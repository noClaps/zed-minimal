mod conflict_set;
pub mod git_traversal;

use crate::{
    ProjectEnvironment, ProjectItem, ProjectPath,
    buffer_store::{BufferStore, BufferStoreEvent},
    worktree_store::{WorktreeStore, WorktreeStoreEvent},
};
use anyhow::{Context as _, Result, anyhow, bail};
use askpass::AskPassDelegate;
use buffer_diff::{BufferDiff, BufferDiffEvent};
use client::ProjectId;
use collections::HashMap;
pub use conflict_set::{ConflictRegion, ConflictSet, ConflictSetSnapshot, ConflictSetUpdate};
use fs::Fs;
use futures::{
    FutureExt, StreamExt as _,
    channel::{mpsc, oneshot},
    future::{self, Shared},
};
use git::{
    blame::Blame,
    repository::{
        Branch, CommitDetails, CommitDiff, CommitOptions, DiffType, GitRepository,
        GitRepositoryCheckpoint, RepoPath, ResetMode,
    },
    status::{
        FileStatus, GitSummary, StatusCode, TrackedStatus, UnmergedStatus, UnmergedStatusCode,
    },
};
use gpui::{
    App, AppContext, AsyncApp, Context, Entity, EventEmitter, SharedString, Subscription, Task,
    WeakEntity,
};
use language::{Buffer, BufferEvent, Language, LanguageRegistry, proto::deserialize_version};
use parking_lot::Mutex;
use postage::stream::Stream as _;
use rpc::{
    AnyProtoClient, TypedEnvelope,
    proto::{self, FromProto, ToProto, git_reset, split_repository_update},
};
use std::{
    cmp::Ordering,
    collections::{BTreeSet, VecDeque},
    future::Future,
    mem,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{self, AtomicU64},
    },
    time::Instant,
};
use sum_tree::{Edit, SumTree, TreeSet};
use text::{Bias, BufferId};
use util::{ResultExt, post_inc};
use worktree::{
    File, PathKey, PathProgress, PathSummary, PathTarget, UpdatedGitRepositoriesSet,
    UpdatedGitRepository,
};

pub struct GitStore {
    state: GitStoreState,
    buffer_store: Entity<BufferStore>,
    worktree_store: Entity<WorktreeStore>,
    repositories: HashMap<RepositoryId, Entity<Repository>>,
    active_repo_id: Option<RepositoryId>,
    #[allow(clippy::type_complexity)]
    loading_diffs:
        HashMap<(BufferId, DiffKind), Shared<Task<Result<Entity<BufferDiff>, Arc<anyhow::Error>>>>>,
    diffs: HashMap<BufferId, Entity<BufferGitState>>,
    shared_diffs: HashMap<proto::PeerId, HashMap<BufferId, SharedDiffs>>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Default)]
struct SharedDiffs {
    unstaged: Option<Entity<BufferDiff>>,
    uncommitted: Option<Entity<BufferDiff>>,
}

struct BufferGitState {
    unstaged_diff: Option<WeakEntity<BufferDiff>>,
    uncommitted_diff: Option<WeakEntity<BufferDiff>>,
    conflict_set: Option<WeakEntity<ConflictSet>>,
    recalculate_diff_task: Option<Task<Result<()>>>,
    reparse_conflict_markers_task: Option<Task<Result<()>>>,
    language: Option<Arc<Language>>,
    language_registry: Option<Arc<LanguageRegistry>>,
    conflict_updated_futures: Vec<oneshot::Sender<()>>,
    recalculating_tx: postage::watch::Sender<bool>,

    /// These operation counts are used to ensure that head and index text
    /// values read from the git repository are up-to-date with any hunk staging
    /// operations that have been performed on the BufferDiff.
    ///
    /// The operation count is incremented immediately when the user initiates a
    /// hunk stage/unstage operation. Then, upon finishing writing the new index
    /// text do disk, the `operation count as of write` is updated to reflect
    /// the operation count that prompted the write.
    hunk_staging_operation_count: usize,
    hunk_staging_operation_count_as_of_write: usize,

    head_text: Option<Arc<String>>,
    index_text: Option<Arc<String>>,
    head_changed: bool,
    index_changed: bool,
    language_changed: bool,
}

#[derive(Clone, Debug)]
enum DiffBasesChange {
    SetIndex(Option<String>),
    SetHead(Option<String>),
    SetEach {
        index: Option<String>,
        head: Option<String>,
    },
    SetBoth(Option<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum DiffKind {
    Unstaged,
    Uncommitted,
}

enum GitStoreState {
    Local {
        next_repository_id: Arc<AtomicU64>,
        downstream: Option<LocalDownstreamState>,
        project_environment: Entity<ProjectEnvironment>,
        fs: Arc<dyn Fs>,
    },
}

enum DownstreamUpdate {
    UpdateRepository(RepositorySnapshot),
    RemoveRepository(RepositoryId),
}

struct LocalDownstreamState {
    client: AnyProtoClient,
    project_id: ProjectId,
    updates_tx: mpsc::UnboundedSender<DownstreamUpdate>,
    _task: Task<Result<()>>,
}

#[derive(Clone, Debug)]
pub struct GitStoreCheckpoint {
    checkpoints_by_work_dir_abs_path: HashMap<Arc<Path>, GitRepositoryCheckpoint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    pub repo_path: RepoPath,
    pub status: FileStatus,
}

impl StatusEntry {
    fn to_proto(&self) -> proto::StatusEntry {
        let simple_status = match self.status {
            FileStatus::Ignored | FileStatus::Untracked => proto::GitStatus::Added as i32,
            FileStatus::Unmerged { .. } => proto::GitStatus::Conflict as i32,
            FileStatus::Tracked(TrackedStatus {
                index_status,
                worktree_status,
            }) => tracked_status_to_proto(if worktree_status != StatusCode::Unmodified {
                worktree_status
            } else {
                index_status
            }),
        };

        proto::StatusEntry {
            repo_path: self.repo_path.as_ref().to_proto(),
            simple_status,
            status: Some(status_to_proto(self.status)),
        }
    }
}

impl TryFrom<proto::StatusEntry> for StatusEntry {
    type Error = anyhow::Error;

    fn try_from(value: proto::StatusEntry) -> Result<Self, Self::Error> {
        let repo_path = RepoPath(Arc::<Path>::from_proto(value.repo_path));
        let status = status_from_proto(value.simple_status, value.status)?;
        Ok(Self { repo_path, status })
    }
}

impl sum_tree::Item for StatusEntry {
    type Summary = PathSummary<GitSummary>;

    fn summary(&self, _: &<Self::Summary as sum_tree::Summary>::Context) -> Self::Summary {
        PathSummary {
            max_path: self.repo_path.0.clone(),
            item_summary: self.status.summary(),
        }
    }
}

impl sum_tree::KeyedItem for StatusEntry {
    type Key = PathKey;

    fn key(&self) -> Self::Key {
        PathKey(self.repo_path.0.clone())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepositoryId(pub u64);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeDetails {
    pub conflicted_paths: TreeSet<RepoPath>,
    pub message: Option<SharedString>,
    pub heads: Vec<Option<SharedString>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositorySnapshot {
    pub id: RepositoryId,
    pub statuses_by_path: SumTree<StatusEntry>,
    pub work_directory_abs_path: Arc<Path>,
    pub branch: Option<Branch>,
    pub head_commit: Option<CommitDetails>,
    pub scan_id: u64,
    pub merge: MergeDetails,
}

type JobId = u64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobInfo {
    pub start: Instant,
    pub message: SharedString,
}

pub struct Repository {
    this: WeakEntity<Self>,
    snapshot: RepositorySnapshot,
    commit_message_buffer: Option<Entity<Buffer>>,
    git_store: WeakEntity<GitStore>,
    // For a local repository, holds paths that have had worktree events since the last status scan completed,
    // and that should be examined during the next status scan.
    paths_needing_status_update: BTreeSet<RepoPath>,
    job_sender: mpsc::UnboundedSender<GitJob>,
    active_jobs: HashMap<JobId, JobInfo>,
    job_id: JobId,
    askpass_delegates: Arc<Mutex<HashMap<u64, AskPassDelegate>>>,
}

impl std::ops::Deref for Repository {
    type Target = RepositorySnapshot;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

#[derive(Clone)]
pub enum RepositoryState {
    Local {
        backend: Arc<dyn GitRepository>,
        environment: Arc<HashMap<String, String>>,
    },
}

#[derive(Clone, Debug)]
pub enum RepositoryEvent {
    Updated { full_scan: bool },
    MergeHeadsChanged,
}

#[derive(Clone, Debug)]
pub struct JobsUpdated;

#[derive(Debug)]
pub enum GitStoreEvent {
    ActiveRepositoryChanged(Option<RepositoryId>),
    RepositoryUpdated(RepositoryId, RepositoryEvent, bool),
    RepositoryAdded(RepositoryId),
    RepositoryRemoved(RepositoryId),
    IndexWriteError(anyhow::Error),
    JobsUpdated,
    ConflictsUpdated,
}

impl EventEmitter<RepositoryEvent> for Repository {}
impl EventEmitter<JobsUpdated> for Repository {}
impl EventEmitter<GitStoreEvent> for GitStore {}

pub struct GitJob {
    job: Box<dyn FnOnce(RepositoryState, &mut AsyncApp) -> Task<()>>,
    key: Option<GitJobKey>,
}

#[derive(PartialEq, Eq)]
enum GitJobKey {
    WriteIndex(RepoPath),
    ReloadBufferDiffBases,
    RefreshStatuses,
}

impl GitStore {
    pub fn local(
        worktree_store: &Entity<WorktreeStore>,
        buffer_store: Entity<BufferStore>,
        environment: Entity<ProjectEnvironment>,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(
            worktree_store.clone(),
            buffer_store,
            GitStoreState::Local {
                next_repository_id: Arc::new(AtomicU64::new(1)),
                downstream: None,
                project_environment: environment,
                fs,
            },
            cx,
        )
    }

    fn new(
        worktree_store: Entity<WorktreeStore>,
        buffer_store: Entity<BufferStore>,
        state: GitStoreState,
        cx: &mut Context<Self>,
    ) -> Self {
        let _subscriptions = vec![
            cx.subscribe(&worktree_store, Self::on_worktree_store_event),
            cx.subscribe(&buffer_store, Self::on_buffer_store_event),
        ];

        GitStore {
            state,
            buffer_store,
            worktree_store,
            repositories: HashMap::default(),
            active_repo_id: None,
            _subscriptions,
            loading_diffs: HashMap::default(),
            shared_diffs: HashMap::default(),
            diffs: HashMap::default(),
        }
    }

    pub fn init(client: &AnyProtoClient) {
        client.add_entity_request_handler(Self::handle_get_branches);
        client.add_entity_request_handler(Self::handle_change_branch);
        client.add_entity_request_handler(Self::handle_create_branch);
        client.add_entity_request_handler(Self::handle_git_init);
        client.add_entity_request_handler(Self::handle_stage);
        client.add_entity_request_handler(Self::handle_unstage);
        client.add_entity_request_handler(Self::handle_commit);
        client.add_entity_request_handler(Self::handle_reset);
        client.add_entity_request_handler(Self::handle_show);
        client.add_entity_request_handler(Self::handle_load_commit_diff);
        client.add_entity_request_handler(Self::handle_checkout_files);
        client.add_entity_request_handler(Self::handle_open_commit_message_buffer);
        client.add_entity_request_handler(Self::handle_set_index_text);
        client.add_entity_request_handler(Self::handle_askpass);
        client.add_entity_request_handler(Self::handle_check_for_pushed_commits);
        client.add_entity_request_handler(Self::handle_git_diff);
        client.add_entity_request_handler(Self::handle_open_unstaged_diff);
        client.add_entity_request_handler(Self::handle_open_uncommitted_diff);
        client.add_entity_message_handler(Self::handle_update_diff_bases);
        client.add_entity_request_handler(Self::handle_blame_buffer);
        client.add_entity_message_handler(Self::handle_update_repository);
        client.add_entity_message_handler(Self::handle_remove_repository);
    }

    pub fn is_local(&self) -> bool {
        matches!(self.state, GitStoreState::Local { .. })
    }

    pub fn shared(&mut self, project_id: u64, client: AnyProtoClient, cx: &mut Context<Self>) {
        match &mut self.state {
            GitStoreState::Local {
                downstream: downstream_client,
                ..
            } => {
                let mut snapshots = HashMap::default();
                let (updates_tx, mut updates_rx) = mpsc::unbounded();
                for repo in self.repositories.values() {
                    updates_tx
                        .unbounded_send(DownstreamUpdate::UpdateRepository(
                            repo.read(cx).snapshot.clone(),
                        ))
                        .ok();
                }
                *downstream_client = Some(LocalDownstreamState {
                    client: client.clone(),
                    project_id: ProjectId(project_id),
                    updates_tx,
                    _task: cx.spawn(async move |this, cx| {
                        cx.background_spawn(async move {
                            while let Some(update) = updates_rx.next().await {
                                match update {
                                    DownstreamUpdate::UpdateRepository(snapshot) => {
                                        if let Some(old_snapshot) = snapshots.get_mut(&snapshot.id)
                                        {
                                            let update =
                                                snapshot.build_update(old_snapshot, project_id);
                                            *old_snapshot = snapshot;
                                            for update in split_repository_update(update) {
                                                client.send(update)?;
                                            }
                                        } else {
                                            let update = snapshot.initial_update(project_id);
                                            for update in split_repository_update(update) {
                                                client.send(update)?;
                                            }
                                            snapshots.insert(snapshot.id, snapshot);
                                        }
                                    }
                                    DownstreamUpdate::RemoveRepository(id) => {
                                        client.send(proto::RemoveRepository {
                                            project_id,
                                            id: id.to_proto(),
                                        })?;
                                    }
                                }
                            }
                            anyhow::Ok(())
                        })
                        .await
                        .ok();
                        this.update(cx, |this, _| {
                            let GitStoreState::Local {
                                downstream: downstream_client,
                                ..
                            } = &mut this.state;
                            downstream_client.take();
                        })
                    }),
                });
            }
        }
    }

    pub fn unshared(&mut self, _cx: &mut Context<Self>) {
        match &mut self.state {
            GitStoreState::Local {
                downstream: downstream_client,
                ..
            } => {
                downstream_client.take();
            }
        }
        self.shared_diffs.clear();
    }

    pub(crate) fn forget_shared_diffs_for(&mut self, peer_id: &proto::PeerId) {
        self.shared_diffs.remove(peer_id);
    }

    pub fn active_repository(&self) -> Option<Entity<Repository>> {
        self.active_repo_id
            .as_ref()
            .map(|id| self.repositories[&id].clone())
    }

    pub fn open_unstaged_diff(
        &mut self,
        buffer: Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<BufferDiff>>> {
        let buffer_id = buffer.read(cx).remote_id();
        if let Some(diff_state) = self.diffs.get(&buffer_id) {
            if let Some(unstaged_diff) = diff_state
                .read(cx)
                .unstaged_diff
                .as_ref()
                .and_then(|weak| weak.upgrade())
            {
                if let Some(task) =
                    diff_state.update(cx, |diff_state, _| diff_state.wait_for_recalculation())
                {
                    return cx.background_executor().spawn(async move {
                        task.await;
                        Ok(unstaged_diff)
                    });
                }
                return Task::ready(Ok(unstaged_diff));
            }
        }

        let Some((repo, repo_path)) =
            self.repository_and_path_for_buffer_id(buffer.read(cx).remote_id(), cx)
        else {
            return Task::ready(Err(anyhow!("failed to find git repository for buffer")));
        };

        let task = self
            .loading_diffs
            .entry((buffer_id, DiffKind::Unstaged))
            .or_insert_with(|| {
                let staged_text = repo.update(cx, |repo, cx| repo.load_staged_text(repo_path, cx));
                cx.spawn(async move |this, cx| {
                    Self::open_diff_internal(
                        this,
                        DiffKind::Unstaged,
                        staged_text.await.map(DiffBasesChange::SetIndex),
                        buffer,
                        cx,
                    )
                    .await
                    .map_err(Arc::new)
                })
                .shared()
            })
            .clone();

        cx.background_spawn(async move { task.await.map_err(|e| anyhow!("{e}")) })
    }

    pub fn open_uncommitted_diff(
        &mut self,
        buffer: Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<BufferDiff>>> {
        let buffer_id = buffer.read(cx).remote_id();

        if let Some(diff_state) = self.diffs.get(&buffer_id) {
            if let Some(uncommitted_diff) = diff_state
                .read(cx)
                .uncommitted_diff
                .as_ref()
                .and_then(|weak| weak.upgrade())
            {
                if let Some(task) =
                    diff_state.update(cx, |diff_state, _| diff_state.wait_for_recalculation())
                {
                    return cx.background_executor().spawn(async move {
                        task.await;
                        Ok(uncommitted_diff)
                    });
                }
                return Task::ready(Ok(uncommitted_diff));
            }
        }

        let Some((repo, repo_path)) =
            self.repository_and_path_for_buffer_id(buffer.read(cx).remote_id(), cx)
        else {
            return Task::ready(Err(anyhow!("failed to find git repository for buffer")));
        };

        let task = self
            .loading_diffs
            .entry((buffer_id, DiffKind::Uncommitted))
            .or_insert_with(|| {
                let changes = repo.update(cx, |repo, cx| repo.load_committed_text(repo_path, cx));

                cx.spawn(async move |this, cx| {
                    Self::open_diff_internal(this, DiffKind::Uncommitted, changes.await, buffer, cx)
                        .await
                        .map_err(Arc::new)
                })
                .shared()
            })
            .clone();

        cx.background_spawn(async move { task.await.map_err(|e| anyhow!("{e}")) })
    }

    async fn open_diff_internal(
        this: WeakEntity<Self>,
        kind: DiffKind,
        texts: Result<DiffBasesChange>,
        buffer_entity: Entity<Buffer>,
        cx: &mut AsyncApp,
    ) -> Result<Entity<BufferDiff>> {
        let diff_bases_change = match texts {
            Err(e) => {
                this.update(cx, |this, cx| {
                    let buffer = buffer_entity.read(cx);
                    let buffer_id = buffer.remote_id();
                    this.loading_diffs.remove(&(buffer_id, kind));
                })?;
                return Err(e);
            }
            Ok(change) => change,
        };

        this.update(cx, |this, cx| {
            let buffer = buffer_entity.read(cx);
            let buffer_id = buffer.remote_id();
            let language = buffer.language().cloned();
            let language_registry = buffer.language_registry();
            let text_snapshot = buffer.text_snapshot();
            this.loading_diffs.remove(&(buffer_id, kind));

            let git_store = cx.weak_entity();
            let diff_state = this
                .diffs
                .entry(buffer_id)
                .or_insert_with(|| cx.new(|_| BufferGitState::new(git_store)));

            let diff = cx.new(|cx| BufferDiff::new(&text_snapshot, cx));

            cx.subscribe(&diff, Self::on_buffer_diff_event).detach();
            diff_state.update(cx, |diff_state, cx| {
                diff_state.language = language;
                diff_state.language_registry = language_registry;

                match kind {
                    DiffKind::Unstaged => diff_state.unstaged_diff = Some(diff.downgrade()),
                    DiffKind::Uncommitted => {
                        let unstaged_diff = if let Some(diff) = diff_state.unstaged_diff() {
                            diff
                        } else {
                            let unstaged_diff = cx.new(|cx| BufferDiff::new(&text_snapshot, cx));
                            diff_state.unstaged_diff = Some(unstaged_diff.downgrade());
                            unstaged_diff
                        };

                        diff.update(cx, |diff, _| diff.set_secondary_diff(unstaged_diff));
                        diff_state.uncommitted_diff = Some(diff.downgrade())
                    }
                }

                diff_state.diff_bases_changed(text_snapshot, Some(diff_bases_change), cx);
                let rx = diff_state.wait_for_recalculation();

                anyhow::Ok(async move {
                    if let Some(rx) = rx {
                        rx.await;
                    }
                    Ok(diff)
                })
            })
        })??
        .await
    }

    pub fn get_unstaged_diff(&self, buffer_id: BufferId, cx: &App) -> Option<Entity<BufferDiff>> {
        let diff_state = self.diffs.get(&buffer_id)?;
        diff_state.read(cx).unstaged_diff.as_ref()?.upgrade()
    }

    pub fn get_uncommitted_diff(
        &self,
        buffer_id: BufferId,
        cx: &App,
    ) -> Option<Entity<BufferDiff>> {
        let diff_state = self.diffs.get(&buffer_id)?;
        diff_state.read(cx).uncommitted_diff.as_ref()?.upgrade()
    }

    pub fn open_conflict_set(
        &mut self,
        buffer: Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Entity<ConflictSet> {
        log::debug!("open conflict set");
        let buffer_id = buffer.read(cx).remote_id();

        if let Some(git_state) = self.diffs.get(&buffer_id) {
            if let Some(conflict_set) = git_state
                .read(cx)
                .conflict_set
                .as_ref()
                .and_then(|weak| weak.upgrade())
            {
                let conflict_set = conflict_set.clone();
                let buffer_snapshot = buffer.read(cx).text_snapshot();

                git_state.update(cx, |state, cx| {
                    let _ = state.reparse_conflict_markers(buffer_snapshot, cx);
                });

                return conflict_set;
            }
        }

        let is_unmerged = self
            .repository_and_path_for_buffer_id(buffer_id, cx)
            .map_or(false, |(repo, path)| {
                repo.read(cx).snapshot.has_conflict(&path)
            });
        let git_store = cx.weak_entity();
        let buffer_git_state = self
            .diffs
            .entry(buffer_id)
            .or_insert_with(|| cx.new(|_| BufferGitState::new(git_store)));
        let conflict_set = cx.new(|cx| ConflictSet::new(buffer_id, is_unmerged, cx));

        self._subscriptions
            .push(cx.subscribe(&conflict_set, |_, _, _, cx| {
                cx.emit(GitStoreEvent::ConflictsUpdated);
            }));

        buffer_git_state.update(cx, |state, cx| {
            state.conflict_set = Some(conflict_set.downgrade());
            let buffer_snapshot = buffer.read(cx).text_snapshot();
            let _ = state.reparse_conflict_markers(buffer_snapshot, cx);
        });

        conflict_set
    }

    pub fn project_path_git_status(
        &self,
        project_path: &ProjectPath,
        cx: &App,
    ) -> Option<FileStatus> {
        let (repo, repo_path) = self.repository_and_path_for_project_path(project_path, cx)?;
        Some(repo.read(cx).status_for_path(&repo_path)?.status)
    }

    pub fn checkpoint(&self, cx: &mut App) -> Task<Result<GitStoreCheckpoint>> {
        let mut work_directory_abs_paths = Vec::new();
        let mut checkpoints = Vec::new();
        for repository in self.repositories.values() {
            repository.update(cx, |repository, _| {
                work_directory_abs_paths.push(repository.snapshot.work_directory_abs_path.clone());
                checkpoints.push(repository.checkpoint().map(|checkpoint| checkpoint?));
            });
        }

        cx.background_executor().spawn(async move {
            let checkpoints = future::try_join_all(checkpoints).await?;
            Ok(GitStoreCheckpoint {
                checkpoints_by_work_dir_abs_path: work_directory_abs_paths
                    .into_iter()
                    .zip(checkpoints)
                    .collect(),
            })
        })
    }

    pub fn restore_checkpoint(
        &self,
        checkpoint: GitStoreCheckpoint,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let repositories_by_work_dir_abs_path = self
            .repositories
            .values()
            .map(|repo| (repo.read(cx).snapshot.work_directory_abs_path.clone(), repo))
            .collect::<HashMap<_, _>>();

        let mut tasks = Vec::new();
        for (work_dir_abs_path, checkpoint) in checkpoint.checkpoints_by_work_dir_abs_path {
            if let Some(repository) = repositories_by_work_dir_abs_path.get(&work_dir_abs_path) {
                let restore = repository.update(cx, |repository, _| {
                    repository.restore_checkpoint(checkpoint)
                });
                tasks.push(async move { restore.await? });
            }
        }
        cx.background_spawn(async move {
            future::try_join_all(tasks).await?;
            Ok(())
        })
    }

    /// Compares two checkpoints, returning true if they are equal.
    pub fn compare_checkpoints(
        &self,
        left: GitStoreCheckpoint,
        mut right: GitStoreCheckpoint,
        cx: &mut App,
    ) -> Task<Result<bool>> {
        let repositories_by_work_dir_abs_path = self
            .repositories
            .values()
            .map(|repo| (repo.read(cx).snapshot.work_directory_abs_path.clone(), repo))
            .collect::<HashMap<_, _>>();

        let mut tasks = Vec::new();
        for (work_dir_abs_path, left_checkpoint) in left.checkpoints_by_work_dir_abs_path {
            if let Some(right_checkpoint) = right
                .checkpoints_by_work_dir_abs_path
                .remove(&work_dir_abs_path)
            {
                if let Some(repository) = repositories_by_work_dir_abs_path.get(&work_dir_abs_path)
                {
                    let compare = repository.update(cx, |repository, _| {
                        repository.compare_checkpoints(left_checkpoint, right_checkpoint)
                    });

                    tasks.push(async move { compare.await? });
                }
            } else {
                return Task::ready(Ok(false));
            }
        }
        cx.background_spawn(async move {
            Ok(future::try_join_all(tasks)
                .await?
                .into_iter()
                .all(|result| result))
        })
    }

    /// Blames a buffer.
    pub fn blame_buffer(
        &self,
        buffer: &Entity<Buffer>,
        version: Option<clock::Global>,
        cx: &mut App,
    ) -> Task<Result<Option<Blame>>> {
        let buffer = buffer.read(cx);
        let Some((repo, repo_path)) =
            self.repository_and_path_for_buffer_id(buffer.remote_id(), cx)
        else {
            return Task::ready(Err(anyhow!("failed to find a git repository for buffer")));
        };
        let content = match &version {
            Some(version) => buffer.rope_for_version(version).clone(),
            None => buffer.as_rope().clone(),
        };

        let rx = repo.update(cx, |repo, _| {
            repo.send_job(None, move |state, _| async move {
                match state {
                    RepositoryState::Local { backend, .. } => backend
                        .blame(repo_path.clone(), content)
                        .await
                        .with_context(|| format!("Failed to blame {:?}", repo_path.0))
                        .map(Some),
                }
            })
        });

        cx.spawn(|_: &mut AsyncApp| async move { rx.await? })
    }

    fn downstream_client(&self) -> Option<(AnyProtoClient, ProjectId)> {
        match &self.state {
            GitStoreState::Local {
                downstream: downstream_client,
                ..
            } => downstream_client
                .as_ref()
                .map(|state| (state.client.clone(), state.project_id)),
        }
    }

    fn on_worktree_store_event(
        &mut self,
        worktree_store: Entity<WorktreeStore>,
        event: &WorktreeStoreEvent,
        cx: &mut Context<Self>,
    ) {
        let GitStoreState::Local {
            project_environment,
            downstream,
            next_repository_id,
            fs,
        } = &self.state;

        match event {
            WorktreeStoreEvent::WorktreeUpdatedEntries(worktree_id, updated_entries) => {
                let mut paths_by_git_repo = HashMap::<_, Vec<_>>::default();
                for (relative_path, _, _) in updated_entries.iter() {
                    let Some((repo, repo_path)) = self.repository_and_path_for_project_path(
                        &(*worktree_id, relative_path.clone()).into(),
                        cx,
                    ) else {
                        continue;
                    };
                    paths_by_git_repo.entry(repo).or_default().push(repo_path)
                }

                for (repo, paths) in paths_by_git_repo {
                    repo.update(cx, |repo, cx| {
                        repo.paths_changed(
                            paths,
                            downstream
                                .as_ref()
                                .map(|downstream| downstream.updates_tx.clone()),
                            cx,
                        );
                    });
                }
            }
            WorktreeStoreEvent::WorktreeUpdatedGitRepositories(worktree_id, changed_repos) => {
                let Some(worktree) = worktree_store.read(cx).worktree_for_id(*worktree_id, cx)
                else {
                    return;
                };
                if !worktree.read(cx).is_visible() {
                    log::debug!(
                        "not adding repositories for local worktree {:?} because it's not visible",
                        worktree.read(cx).abs_path()
                    );
                    return;
                }
                self.update_repositories_from_worktree(
                    project_environment.clone(),
                    next_repository_id.clone(),
                    downstream
                        .as_ref()
                        .map(|downstream| downstream.updates_tx.clone()),
                    changed_repos.clone(),
                    fs.clone(),
                    cx,
                );
                self.local_worktree_git_repos_changed(changed_repos, cx);
            }
            _ => {}
        }
    }

    fn on_repository_event(
        &mut self,
        repo: Entity<Repository>,
        event: &RepositoryEvent,
        cx: &mut Context<Self>,
    ) {
        let id = repo.read(cx).id;
        let repo_snapshot = repo.read(cx).snapshot.clone();
        for (buffer_id, diff) in self.diffs.iter() {
            if let Some((buffer_repo, repo_path)) =
                self.repository_and_path_for_buffer_id(*buffer_id, cx)
            {
                if buffer_repo == repo {
                    diff.update(cx, |diff, cx| {
                        if let Some(conflict_set) = &diff.conflict_set {
                            let conflict_status_changed =
                                conflict_set.update(cx, |conflict_set, cx| {
                                    let has_conflict = repo_snapshot.has_conflict(&repo_path);
                                    conflict_set.set_has_conflict(has_conflict, cx)
                                })?;
                            if conflict_status_changed {
                                let buffer_store = self.buffer_store.read(cx);
                                if let Some(buffer) = buffer_store.get(*buffer_id) {
                                    let _ = diff.reparse_conflict_markers(
                                        buffer.read(cx).text_snapshot(),
                                        cx,
                                    );
                                }
                            }
                        }
                        anyhow::Ok(())
                    })
                    .ok();
                }
            }
        }
        cx.emit(GitStoreEvent::RepositoryUpdated(
            id,
            event.clone(),
            self.active_repo_id == Some(id),
        ))
    }

    fn on_jobs_updated(&mut self, _: Entity<Repository>, _: &JobsUpdated, cx: &mut Context<Self>) {
        cx.emit(GitStoreEvent::JobsUpdated)
    }

    /// Update our list of repositories and schedule git scans in response to a notification from a worktree,
    fn update_repositories_from_worktree(
        &mut self,
        project_environment: Entity<ProjectEnvironment>,
        next_repository_id: Arc<AtomicU64>,
        updates_tx: Option<mpsc::UnboundedSender<DownstreamUpdate>>,
        updated_git_repositories: UpdatedGitRepositoriesSet,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) {
        let mut removed_ids = Vec::new();
        for update in updated_git_repositories.iter() {
            if let Some((id, existing)) = self.repositories.iter().find(|(_, repo)| {
                let existing_work_directory_abs_path =
                    repo.read(cx).work_directory_abs_path.clone();
                Some(&existing_work_directory_abs_path)
                    == update.old_work_directory_abs_path.as_ref()
                    || Some(&existing_work_directory_abs_path)
                        == update.new_work_directory_abs_path.as_ref()
            }) {
                if let Some(new_work_directory_abs_path) =
                    update.new_work_directory_abs_path.clone()
                {
                    existing.update(cx, |existing, _| {
                        existing.snapshot.work_directory_abs_path = new_work_directory_abs_path;
                    });
                } else {
                    removed_ids.push(*id);
                }
            } else if let UpdatedGitRepository {
                new_work_directory_abs_path: Some(work_directory_abs_path),
                dot_git_abs_path: Some(dot_git_abs_path),
                repository_dir_abs_path: Some(repository_dir_abs_path),
                common_dir_abs_path: Some(common_dir_abs_path),
                ..
            } = update
            {
                let id = RepositoryId(next_repository_id.fetch_add(1, atomic::Ordering::Release));
                let git_store = cx.weak_entity();
                let repo = cx.new(|cx| {
                    Repository::local(
                        id,
                        work_directory_abs_path.clone(),
                        dot_git_abs_path.clone(),
                        repository_dir_abs_path.clone(),
                        common_dir_abs_path.clone(),
                        project_environment.downgrade(),
                        fs.clone(),
                        git_store,
                        cx,
                    )
                });
                self._subscriptions
                    .push(cx.subscribe(&repo, Self::on_repository_event));
                self._subscriptions
                    .push(cx.subscribe(&repo, Self::on_jobs_updated));
                self.repositories.insert(id, repo);
                cx.emit(GitStoreEvent::RepositoryAdded(id));
                self.active_repo_id.get_or_insert_with(|| {
                    cx.emit(GitStoreEvent::ActiveRepositoryChanged(Some(id)));
                    id
                });
            }
        }

        for id in removed_ids {
            if self.active_repo_id == Some(id) {
                self.active_repo_id = None;
                cx.emit(GitStoreEvent::ActiveRepositoryChanged(None));
            }
            self.repositories.remove(&id);
            if let Some(updates_tx) = updates_tx.as_ref() {
                updates_tx
                    .unbounded_send(DownstreamUpdate::RemoveRepository(id))
                    .ok();
            }
        }
    }

    fn on_buffer_store_event(
        &mut self,
        _: Entity<BufferStore>,
        event: &BufferStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            BufferStoreEvent::BufferAdded(buffer) => {
                cx.subscribe(&buffer, |this, buffer, event, cx| {
                    if let BufferEvent::LanguageChanged = event {
                        let buffer_id = buffer.read(cx).remote_id();
                        if let Some(diff_state) = this.diffs.get(&buffer_id) {
                            diff_state.update(cx, |diff_state, cx| {
                                diff_state.buffer_language_changed(buffer, cx);
                            });
                        }
                    }
                })
                .detach();
            }
            BufferStoreEvent::SharedBufferClosed(peer_id, buffer_id) => {
                if let Some(diffs) = self.shared_diffs.get_mut(peer_id) {
                    diffs.remove(buffer_id);
                }
            }
            BufferStoreEvent::BufferDropped(buffer_id) => {
                self.diffs.remove(&buffer_id);
                for diffs in self.shared_diffs.values_mut() {
                    diffs.remove(buffer_id);
                }
            }

            _ => {}
        }
    }

    pub fn recalculate_buffer_diffs(
        &mut self,
        buffers: Vec<Entity<Buffer>>,
        cx: &mut Context<Self>,
    ) -> impl Future<Output = ()> + use<> {
        let mut futures = Vec::new();
        for buffer in buffers {
            if let Some(diff_state) = self.diffs.get_mut(&buffer.read(cx).remote_id()) {
                let buffer = buffer.read(cx).text_snapshot();
                diff_state.update(cx, |diff_state, cx| {
                    diff_state.recalculate_diffs(buffer.clone(), cx);
                    futures.extend(diff_state.wait_for_recalculation().map(FutureExt::boxed));
                });
                futures.push(diff_state.update(cx, |diff_state, cx| {
                    diff_state
                        .reparse_conflict_markers(buffer, cx)
                        .map(|_| {})
                        .boxed()
                }));
            }
        }
        async move {
            futures::future::join_all(futures).await;
        }
    }

    fn on_buffer_diff_event(
        &mut self,
        diff: Entity<buffer_diff::BufferDiff>,
        event: &BufferDiffEvent,
        cx: &mut Context<Self>,
    ) {
        if let BufferDiffEvent::HunksStagedOrUnstaged(new_index_text) = event {
            let buffer_id = diff.read(cx).buffer_id;
            if let Some(diff_state) = self.diffs.get(&buffer_id) {
                let hunk_staging_operation_count = diff_state.update(cx, |diff_state, _| {
                    diff_state.hunk_staging_operation_count += 1;
                    diff_state.hunk_staging_operation_count
                });
                if let Some((repo, path)) = self.repository_and_path_for_buffer_id(buffer_id, cx) {
                    let recv = repo.update(cx, |repo, cx| {
                        log::debug!("hunks changed for {}", path.display());
                        repo.spawn_set_index_text_job(
                            path,
                            new_index_text.as_ref().map(|rope| rope.to_string()),
                            Some(hunk_staging_operation_count),
                            cx,
                        )
                    });
                    let diff = diff.downgrade();
                    cx.spawn(async move |this, cx| {
                        if let Ok(Err(error)) = cx.background_spawn(recv).await {
                            diff.update(cx, |diff, cx| {
                                diff.clear_pending_hunks(cx);
                            })
                            .ok();
                            this.update(cx, |_, cx| cx.emit(GitStoreEvent::IndexWriteError(error)))
                                .ok();
                        }
                    })
                    .detach();
                }
            }
        }
    }

    fn local_worktree_git_repos_changed(
        &mut self,
        changed_repos: &UpdatedGitRepositoriesSet,
        cx: &mut Context<Self>,
    ) {
        log::debug!("local worktree repos changed");

        for repository in self.repositories.values() {
            repository.update(cx, |repository, cx| {
                let repo_abs_path = &repository.work_directory_abs_path;
                if changed_repos.iter().any(|update| {
                    update.old_work_directory_abs_path.as_ref() == Some(&repo_abs_path)
                        || update.new_work_directory_abs_path.as_ref() == Some(&repo_abs_path)
                }) {
                    repository.reload_buffer_diff_bases(cx);
                }
            });
        }
    }

    pub fn repositories(&self) -> &HashMap<RepositoryId, Entity<Repository>> {
        &self.repositories
    }

    pub fn status_for_buffer_id(&self, buffer_id: BufferId, cx: &App) -> Option<FileStatus> {
        let (repo, path) = self.repository_and_path_for_buffer_id(buffer_id, cx)?;
        let status = repo.read(cx).snapshot.status_for_path(&path)?;
        Some(status.status)
    }

    pub fn repository_and_path_for_buffer_id(
        &self,
        buffer_id: BufferId,
        cx: &App,
    ) -> Option<(Entity<Repository>, RepoPath)> {
        let buffer = self.buffer_store.read(cx).get(buffer_id)?;
        let project_path = buffer.read(cx).project_path(cx)?;
        self.repository_and_path_for_project_path(&project_path, cx)
    }

    pub fn repository_and_path_for_project_path(
        &self,
        path: &ProjectPath,
        cx: &App,
    ) -> Option<(Entity<Repository>, RepoPath)> {
        let abs_path = self.worktree_store.read(cx).absolutize(path, cx)?;
        self.repositories
            .values()
            .filter_map(|repo| {
                let repo_path = repo.read(cx).abs_path_to_repo_path(&abs_path)?;
                Some((repo.clone(), repo_path))
            })
            .max_by_key(|(repo, _)| repo.read(cx).work_directory_abs_path.clone())
    }

    pub fn git_init(
        &self,
        path: Arc<Path>,
        fallback_branch_name: String,
        cx: &App,
    ) -> Task<Result<()>> {
        match &self.state {
            GitStoreState::Local { fs, .. } => {
                let fs = fs.clone();
                cx.background_executor()
                    .spawn(async move { fs.git_init(&path, fallback_branch_name) })
            }
        }
    }

    async fn handle_update_repository(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::UpdateRepository>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        this.update(&mut cx, |this, cx| {
            let mut update = envelope.payload;

            let id = RepositoryId::from_proto(update.id);

            this.active_repo_id.get_or_insert_with(|| {
                cx.emit(GitStoreEvent::ActiveRepositoryChanged(Some(id)));
                id
            });

            if let Some((client, project_id)) = this.downstream_client() {
                update.project_id = project_id.to_proto();
                client.send(update).log_err();
            }
            Ok(())
        })?
    }

    async fn handle_remove_repository(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::RemoveRepository>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        this.update(&mut cx, |this, cx| {
            let mut update = envelope.payload;
            let id = RepositoryId::from_proto(update.id);
            this.repositories.remove(&id);
            if let Some((client, project_id)) = this.downstream_client() {
                update.project_id = project_id.to_proto();
                client.send(update).log_err();
            }
            if this.active_repo_id == Some(id) {
                this.active_repo_id = None;
                cx.emit(GitStoreEvent::ActiveRepositoryChanged(None));
            }
            cx.emit(GitStoreEvent::RepositoryRemoved(id));
        })
    }

    async fn handle_git_init(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitInit>,
        cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let path: Arc<Path> = PathBuf::from(envelope.payload.abs_path).into();
        let name = envelope.payload.fallback_branch_name;
        cx.update(|cx| this.read(cx).git_init(path, name, cx))?
            .await?;

        Ok(proto::Ack {})
    }

    async fn handle_stage(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::Stage>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let entries = envelope
            .payload
            .paths
            .into_iter()
            .map(PathBuf::from)
            .map(RepoPath::new)
            .collect();

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.stage_entries(entries, cx)
            })?
            .await?;
        Ok(proto::Ack {})
    }

    async fn handle_unstage(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::Unstage>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let entries = envelope
            .payload
            .paths
            .into_iter()
            .map(PathBuf::from)
            .map(RepoPath::new)
            .collect();

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.unstage_entries(entries, cx)
            })?
            .await?;

        Ok(proto::Ack {})
    }

    async fn handle_set_index_text(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SetIndexText>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let repo_path = RepoPath::from_str(&envelope.payload.path);

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.spawn_set_index_text_job(
                    repo_path,
                    envelope.payload.text,
                    None,
                    cx,
                )
            })?
            .await??;
        Ok(proto::Ack {})
    }

    async fn handle_commit(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::Commit>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let message = SharedString::from(envelope.payload.message);
        let name = envelope.payload.name.map(SharedString::from);
        let email = envelope.payload.email.map(SharedString::from);
        let options = envelope.payload.options.unwrap_or_default();

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.commit(
                    message,
                    name.zip(email),
                    CommitOptions {
                        amend: options.amend,
                    },
                    cx,
                )
            })?
            .await??;
        Ok(proto::Ack {})
    }

    async fn handle_get_branches(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitGetBranches>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitBranchesResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let branches = repository_handle
            .update(&mut cx, |repository_handle, _| repository_handle.branches())?
            .await??;

        Ok(proto::GitBranchesResponse {
            branches: branches
                .into_iter()
                .map(|branch| branch_to_proto(&branch))
                .collect::<Vec<_>>(),
        })
    }
    async fn handle_create_branch(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitCreateBranch>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let branch_name = envelope.payload.branch_name;

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.create_branch(branch_name)
            })?
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_change_branch(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitChangeBranch>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let branch_name = envelope.payload.branch_name;

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.change_branch(branch_name)
            })?
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_show(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitShow>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitCommitDetails> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let commit = repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.show(envelope.payload.commit)
            })?
            .await??;
        Ok(proto::GitCommitDetails {
            sha: commit.sha.into(),
            message: commit.message.into(),
            commit_timestamp: commit.commit_timestamp,
            author_email: commit.author_email.into(),
            author_name: commit.author_name.into(),
        })
    }

    async fn handle_load_commit_diff(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::LoadCommitDiff>,
        mut cx: AsyncApp,
    ) -> Result<proto::LoadCommitDiffResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let commit_diff = repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.load_commit_diff(envelope.payload.commit)
            })?
            .await??;
        Ok(proto::LoadCommitDiffResponse {
            files: commit_diff
                .files
                .into_iter()
                .map(|file| proto::CommitFile {
                    path: file.path.to_string(),
                    old_text: file.old_text,
                    new_text: file.new_text,
                })
                .collect(),
        })
    }

    async fn handle_reset(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitReset>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let mode = match envelope.payload.mode() {
            git_reset::ResetMode::Soft => ResetMode::Soft,
            git_reset::ResetMode::Mixed => ResetMode::Mixed,
        };

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.reset(envelope.payload.commit, mode, cx)
            })?
            .await??;
        Ok(proto::Ack {})
    }

    async fn handle_checkout_files(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitCheckoutFiles>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let paths = envelope
            .payload
            .paths
            .iter()
            .map(|s| RepoPath::from_str(s))
            .collect();

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.checkout_files(&envelope.payload.commit, paths, cx)
            })?
            .await??;
        Ok(proto::Ack {})
    }

    async fn handle_open_commit_message_buffer(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::OpenCommitMessageBuffer>,
        mut cx: AsyncApp,
    ) -> Result<proto::OpenBufferResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let buffer = repository
            .update(&mut cx, |repository, cx| {
                repository.open_commit_buffer(None, this.read(cx).buffer_store.clone(), cx)
            })?
            .await?;

        let buffer_id = buffer.read_with(&cx, |buffer, _| buffer.remote_id())?;
        this.update(&mut cx, |this, cx| {
            this.buffer_store.update(cx, |buffer_store, cx| {
                buffer_store
                    .create_buffer_for_peer(
                        &buffer,
                        envelope.original_sender_id.unwrap_or(envelope.sender_id),
                        cx,
                    )
                    .detach_and_log_err(cx);
            })
        })?;

        Ok(proto::OpenBufferResponse {
            buffer_id: buffer_id.to_proto(),
        })
    }

    async fn handle_askpass(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::AskPassRequest>,
        mut cx: AsyncApp,
    ) -> Result<proto::AskPassResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let delegates = cx.update(|cx| repository.read(cx).askpass_delegates.clone())?;
        let Some(mut askpass) = delegates.lock().remove(&envelope.payload.askpass_id) else {
            anyhow::bail!("no askpass found");
        };

        let response = askpass.ask_password(envelope.payload.prompt).await?;

        delegates
            .lock()
            .insert(envelope.payload.askpass_id, askpass);

        Ok(proto::AskPassResponse { response })
    }

    async fn handle_check_for_pushed_commits(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::CheckForPushedCommits>,
        mut cx: AsyncApp,
    ) -> Result<proto::CheckForPushedCommitsResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let branches = repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.check_for_pushed_commits()
            })?
            .await??;
        Ok(proto::CheckForPushedCommitsResponse {
            pushed_to: branches
                .into_iter()
                .map(|commit| commit.to_string())
                .collect(),
        })
    }

    async fn handle_git_diff(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitDiff>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitDiffResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let diff_type = match envelope.payload.diff_type() {
            proto::git_diff::DiffType::HeadToIndex => DiffType::HeadToIndex,
            proto::git_diff::DiffType::HeadToWorktree => DiffType::HeadToWorktree,
        };

        let mut diff = repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.diff(diff_type, cx)
            })?
            .await??;
        const ONE_MB: usize = 1_000_000;
        if diff.len() > ONE_MB {
            diff = diff.chars().take(ONE_MB).collect()
        }

        Ok(proto::GitDiffResponse { diff })
    }

    async fn handle_open_unstaged_diff(
        this: Entity<Self>,
        request: TypedEnvelope<proto::OpenUnstagedDiff>,
        mut cx: AsyncApp,
    ) -> Result<proto::OpenUnstagedDiffResponse> {
        let buffer_id = BufferId::new(request.payload.buffer_id)?;
        let diff = this
            .update(&mut cx, |this, cx| {
                let buffer = this.buffer_store.read(cx).get(buffer_id)?;
                Some(this.open_unstaged_diff(buffer, cx))
            })?
            .context("missing buffer")?
            .await?;
        this.update(&mut cx, |this, _| {
            let shared_diffs = this
                .shared_diffs
                .entry(request.original_sender_id.unwrap_or(request.sender_id))
                .or_default();
            shared_diffs.entry(buffer_id).or_default().unstaged = Some(diff.clone());
        })?;
        let staged_text = diff.read_with(&cx, |diff, _| diff.base_text_string())?;
        Ok(proto::OpenUnstagedDiffResponse { staged_text })
    }

    async fn handle_open_uncommitted_diff(
        this: Entity<Self>,
        request: TypedEnvelope<proto::OpenUncommittedDiff>,
        mut cx: AsyncApp,
    ) -> Result<proto::OpenUncommittedDiffResponse> {
        let buffer_id = BufferId::new(request.payload.buffer_id)?;
        let diff = this
            .update(&mut cx, |this, cx| {
                let buffer = this.buffer_store.read(cx).get(buffer_id)?;
                Some(this.open_uncommitted_diff(buffer, cx))
            })?
            .context("missing buffer")?
            .await?;
        this.update(&mut cx, |this, _| {
            let shared_diffs = this
                .shared_diffs
                .entry(request.original_sender_id.unwrap_or(request.sender_id))
                .or_default();
            shared_diffs.entry(buffer_id).or_default().uncommitted = Some(diff.clone());
        })?;
        diff.read_with(&cx, |diff, cx| {
            use proto::open_uncommitted_diff_response::Mode;

            let unstaged_diff = diff.secondary_diff();
            let index_snapshot = unstaged_diff.and_then(|diff| {
                let diff = diff.read(cx);
                diff.base_text_exists().then(|| diff.base_text())
            });

            let mode;
            let staged_text;
            let committed_text;
            if diff.base_text_exists() {
                let committed_snapshot = diff.base_text();
                committed_text = Some(committed_snapshot.text());
                if let Some(index_text) = index_snapshot {
                    if index_text.remote_id() == committed_snapshot.remote_id() {
                        mode = Mode::IndexMatchesHead;
                        staged_text = None;
                    } else {
                        mode = Mode::IndexAndHead;
                        staged_text = Some(index_text.text());
                    }
                } else {
                    mode = Mode::IndexAndHead;
                    staged_text = None;
                }
            } else {
                mode = Mode::IndexAndHead;
                committed_text = None;
                staged_text = index_snapshot.as_ref().map(|buffer| buffer.text());
            }

            proto::OpenUncommittedDiffResponse {
                committed_text,
                staged_text,
                mode: mode.into(),
            }
        })
    }

    async fn handle_update_diff_bases(
        this: Entity<Self>,
        request: TypedEnvelope<proto::UpdateDiffBases>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        let buffer_id = BufferId::new(request.payload.buffer_id)?;
        this.update(&mut cx, |this, cx| {
            if let Some(diff_state) = this.diffs.get_mut(&buffer_id) {
                if let Some(buffer) = this.buffer_store.read(cx).get(buffer_id) {
                    let buffer = buffer.read(cx).text_snapshot();
                    diff_state.update(cx, |diff_state, cx| {
                        diff_state.handle_base_texts_updated(buffer, request.payload, cx);
                    })
                }
            }
        })
    }

    async fn handle_blame_buffer(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::BlameBuffer>,
        mut cx: AsyncApp,
    ) -> Result<proto::BlameBufferResponse> {
        let buffer_id = BufferId::new(envelope.payload.buffer_id)?;
        let version = deserialize_version(&envelope.payload.version);
        let buffer = this.read_with(&cx, |this, cx| {
            this.buffer_store.read(cx).get_existing(buffer_id)
        })??;
        buffer
            .update(&mut cx, |buffer, _| {
                buffer.wait_for_version(version.clone())
            })?
            .await?;
        let blame = this
            .update(&mut cx, |this, cx| {
                this.blame_buffer(&buffer, Some(version), cx)
            })?
            .await?;
        Ok(serialize_blame_buffer_response(blame))
    }

    fn repository_for_request(
        this: &Entity<Self>,
        id: RepositoryId,
        cx: &mut AsyncApp,
    ) -> Result<Entity<Repository>> {
        this.read_with(cx, |this, _| {
            this.repositories
                .get(&id)
                .context("missing repository handle")
                .cloned()
        })?
    }

    pub fn repo_snapshots(&self, cx: &App) -> HashMap<RepositoryId, RepositorySnapshot> {
        self.repositories
            .iter()
            .map(|(id, repo)| (*id, repo.read(cx).snapshot.clone()))
            .collect()
    }
}

impl BufferGitState {
    fn new(_git_store: WeakEntity<GitStore>) -> Self {
        Self {
            unstaged_diff: Default::default(),
            uncommitted_diff: Default::default(),
            recalculate_diff_task: Default::default(),
            language: Default::default(),
            language_registry: Default::default(),
            recalculating_tx: postage::watch::channel_with(false).0,
            hunk_staging_operation_count: 0,
            hunk_staging_operation_count_as_of_write: 0,
            head_text: Default::default(),
            index_text: Default::default(),
            head_changed: Default::default(),
            index_changed: Default::default(),
            language_changed: Default::default(),
            conflict_updated_futures: Default::default(),
            conflict_set: Default::default(),
            reparse_conflict_markers_task: Default::default(),
        }
    }

    fn buffer_language_changed(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        self.language = buffer.read(cx).language().cloned();
        self.language_changed = true;
        let _ = self.recalculate_diffs(buffer.read(cx).text_snapshot(), cx);
    }

    fn reparse_conflict_markers(
        &mut self,
        buffer: text::BufferSnapshot,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();

        let Some(conflict_set) = self
            .conflict_set
            .as_ref()
            .and_then(|conflict_set| conflict_set.upgrade())
        else {
            return rx;
        };

        let old_snapshot = conflict_set.read_with(cx, |conflict_set, _| {
            if conflict_set.has_conflict {
                Some(conflict_set.snapshot())
            } else {
                None
            }
        });

        if let Some(old_snapshot) = old_snapshot {
            self.conflict_updated_futures.push(tx);
            self.reparse_conflict_markers_task = Some(cx.spawn(async move |this, cx| {
                let (snapshot, changed_range) = cx
                    .background_spawn(async move {
                        let new_snapshot = ConflictSet::parse(&buffer);
                        let changed_range = old_snapshot.compare(&new_snapshot, &buffer);
                        (new_snapshot, changed_range)
                    })
                    .await;
                this.update(cx, |this, cx| {
                    if let Some(conflict_set) = &this.conflict_set {
                        conflict_set
                            .update(cx, |conflict_set, cx| {
                                conflict_set.set_snapshot(snapshot, changed_range, cx);
                            })
                            .ok();
                    }
                    let futures = std::mem::take(&mut this.conflict_updated_futures);
                    for tx in futures {
                        tx.send(()).ok();
                    }
                })
            }))
        }

        rx
    }

    fn unstaged_diff(&self) -> Option<Entity<BufferDiff>> {
        self.unstaged_diff.as_ref().and_then(|set| set.upgrade())
    }

    fn uncommitted_diff(&self) -> Option<Entity<BufferDiff>> {
        self.uncommitted_diff.as_ref().and_then(|set| set.upgrade())
    }

    fn handle_base_texts_updated(
        &mut self,
        buffer: text::BufferSnapshot,
        message: proto::UpdateDiffBases,
        cx: &mut Context<Self>,
    ) {
        use proto::update_diff_bases::Mode;

        let Some(mode) = Mode::from_i32(message.mode) else {
            return;
        };

        let diff_bases_change = match mode {
            Mode::HeadOnly => DiffBasesChange::SetHead(message.committed_text),
            Mode::IndexOnly => DiffBasesChange::SetIndex(message.staged_text),
            Mode::IndexMatchesHead => DiffBasesChange::SetBoth(message.committed_text),
            Mode::IndexAndHead => DiffBasesChange::SetEach {
                index: message.staged_text,
                head: message.committed_text,
            },
        };

        self.diff_bases_changed(buffer, Some(diff_bases_change), cx);
    }

    pub fn wait_for_recalculation(&mut self) -> Option<impl Future<Output = ()> + use<>> {
        if *self.recalculating_tx.borrow() {
            let mut rx = self.recalculating_tx.subscribe();
            return Some(async move {
                loop {
                    let is_recalculating = rx.recv().await;
                    if is_recalculating != Some(true) {
                        break;
                    }
                }
            });
        } else {
            None
        }
    }

    fn diff_bases_changed(
        &mut self,
        buffer: text::BufferSnapshot,
        diff_bases_change: Option<DiffBasesChange>,
        cx: &mut Context<Self>,
    ) {
        match diff_bases_change {
            Some(DiffBasesChange::SetIndex(index)) => {
                self.index_text = index.map(|mut index| {
                    text::LineEnding::normalize(&mut index);
                    Arc::new(index)
                });
                self.index_changed = true;
            }
            Some(DiffBasesChange::SetHead(head)) => {
                self.head_text = head.map(|mut head| {
                    text::LineEnding::normalize(&mut head);
                    Arc::new(head)
                });
                self.head_changed = true;
            }
            Some(DiffBasesChange::SetBoth(text)) => {
                let text = text.map(|mut text| {
                    text::LineEnding::normalize(&mut text);
                    Arc::new(text)
                });
                self.head_text = text.clone();
                self.index_text = text;
                self.head_changed = true;
                self.index_changed = true;
            }
            Some(DiffBasesChange::SetEach { index, head }) => {
                self.index_text = index.map(|mut index| {
                    text::LineEnding::normalize(&mut index);
                    Arc::new(index)
                });
                self.index_changed = true;
                self.head_text = head.map(|mut head| {
                    text::LineEnding::normalize(&mut head);
                    Arc::new(head)
                });
                self.head_changed = true;
            }
            None => {}
        }

        self.recalculate_diffs(buffer, cx)
    }

    fn recalculate_diffs(&mut self, buffer: text::BufferSnapshot, cx: &mut Context<Self>) {
        *self.recalculating_tx.borrow_mut() = true;

        let language = self.language.clone();
        let language_registry = self.language_registry.clone();
        let unstaged_diff = self.unstaged_diff();
        let uncommitted_diff = self.uncommitted_diff();
        let head = self.head_text.clone();
        let index = self.index_text.clone();
        let index_changed = self.index_changed;
        let head_changed = self.head_changed;
        let language_changed = self.language_changed;
        let prev_hunk_staging_operation_count = self.hunk_staging_operation_count_as_of_write;
        let index_matches_head = match (self.index_text.as_ref(), self.head_text.as_ref()) {
            (Some(index), Some(head)) => Arc::ptr_eq(index, head),
            (None, None) => true,
            _ => false,
        };
        self.recalculate_diff_task = Some(cx.spawn(async move |this, cx| {
            log::debug!(
                "start recalculating diffs for buffer {}",
                buffer.remote_id()
            );

            let mut new_unstaged_diff = None;
            if let Some(unstaged_diff) = &unstaged_diff {
                new_unstaged_diff = Some(
                    BufferDiff::update_diff(
                        unstaged_diff.clone(),
                        buffer.clone(),
                        index,
                        index_changed,
                        language_changed,
                        language.clone(),
                        language_registry.clone(),
                        cx,
                    )
                    .await?,
                );
            }

            let mut new_uncommitted_diff = None;
            if let Some(uncommitted_diff) = &uncommitted_diff {
                new_uncommitted_diff = if index_matches_head {
                    new_unstaged_diff.clone()
                } else {
                    Some(
                        BufferDiff::update_diff(
                            uncommitted_diff.clone(),
                            buffer.clone(),
                            head,
                            head_changed,
                            language_changed,
                            language.clone(),
                            language_registry.clone(),
                            cx,
                        )
                        .await?,
                    )
                }
            }

            let cancel = this.update(cx, |this, _| {
                // This checks whether all pending stage/unstage operations
                // have quiesced (i.e. both the corresponding write and the
                // read of that write have completed). If not, then we cancel
                // this recalculation attempt to avoid invalidating pending
                // state too quickly; another recalculation will come along
                // later and clear the pending state once the state of the index has settled.
                if this.hunk_staging_operation_count > prev_hunk_staging_operation_count {
                    *this.recalculating_tx.borrow_mut() = false;
                    true
                } else {
                    false
                }
            })?;
            if cancel {
                log::debug!(
                    concat!(
                        "aborting recalculating diffs for buffer {}",
                        "due to subsequent hunk operations",
                    ),
                    buffer.remote_id()
                );
                return Ok(());
            }

            let unstaged_changed_range = if let Some((unstaged_diff, new_unstaged_diff)) =
                unstaged_diff.as_ref().zip(new_unstaged_diff.clone())
            {
                unstaged_diff.update(cx, |diff, cx| {
                    if language_changed {
                        diff.language_changed(cx);
                    }
                    diff.set_snapshot(new_unstaged_diff, &buffer, cx)
                })?
            } else {
                None
            };

            if let Some((uncommitted_diff, new_uncommitted_diff)) =
                uncommitted_diff.as_ref().zip(new_uncommitted_diff.clone())
            {
                uncommitted_diff.update(cx, |diff, cx| {
                    if language_changed {
                        diff.language_changed(cx);
                    }
                    diff.set_snapshot_with_secondary(
                        new_uncommitted_diff,
                        &buffer,
                        unstaged_changed_range,
                        true,
                        cx,
                    );
                })?;
            }

            log::debug!(
                "finished recalculating diffs for buffer {}",
                buffer.remote_id()
            );

            if let Some(this) = this.upgrade() {
                this.update(cx, |this, _| {
                    this.index_changed = false;
                    this.head_changed = false;
                    this.language_changed = false;
                    *this.recalculating_tx.borrow_mut() = false;
                })?;
            }

            Ok(())
        }));
    }
}

impl RepositoryId {
    pub fn to_proto(self) -> u64 {
        self.0
    }

    pub fn from_proto(id: u64) -> Self {
        RepositoryId(id)
    }
}

impl RepositorySnapshot {
    fn empty(id: RepositoryId, work_directory_abs_path: Arc<Path>) -> Self {
        Self {
            id,
            statuses_by_path: Default::default(),
            work_directory_abs_path,
            branch: None,
            head_commit: None,
            scan_id: 0,
            merge: Default::default(),
        }
    }

    fn initial_update(&self, project_id: u64) -> proto::UpdateRepository {
        proto::UpdateRepository {
            branch_summary: self.branch.as_ref().map(branch_to_proto),
            head_commit_details: self.head_commit.as_ref().map(commit_details_to_proto),
            updated_statuses: self
                .statuses_by_path
                .iter()
                .map(|entry| entry.to_proto())
                .collect(),
            removed_statuses: Default::default(),
            current_merge_conflicts: self
                .merge
                .conflicted_paths
                .iter()
                .map(|repo_path| repo_path.to_proto())
                .collect(),
            project_id,
            id: self.id.to_proto(),
            abs_path: self.work_directory_abs_path.to_proto(),
            entry_ids: vec![self.id.to_proto()],
            scan_id: self.scan_id,
            is_last_update: true,
        }
    }

    fn build_update(&self, old: &Self, project_id: u64) -> proto::UpdateRepository {
        let mut updated_statuses: Vec<proto::StatusEntry> = Vec::new();
        let mut removed_statuses: Vec<String> = Vec::new();

        let mut new_statuses = self.statuses_by_path.iter().peekable();
        let mut old_statuses = old.statuses_by_path.iter().peekable();

        let mut current_new_entry = new_statuses.next();
        let mut current_old_entry = old_statuses.next();
        loop {
            match (current_new_entry, current_old_entry) {
                (Some(new_entry), Some(old_entry)) => {
                    match new_entry.repo_path.cmp(&old_entry.repo_path) {
                        Ordering::Less => {
                            updated_statuses.push(new_entry.to_proto());
                            current_new_entry = new_statuses.next();
                        }
                        Ordering::Equal => {
                            if new_entry.status != old_entry.status {
                                updated_statuses.push(new_entry.to_proto());
                            }
                            current_old_entry = old_statuses.next();
                            current_new_entry = new_statuses.next();
                        }
                        Ordering::Greater => {
                            removed_statuses.push(old_entry.repo_path.as_ref().to_proto());
                            current_old_entry = old_statuses.next();
                        }
                    }
                }
                (None, Some(old_entry)) => {
                    removed_statuses.push(old_entry.repo_path.as_ref().to_proto());
                    current_old_entry = old_statuses.next();
                }
                (Some(new_entry), None) => {
                    updated_statuses.push(new_entry.to_proto());
                    current_new_entry = new_statuses.next();
                }
                (None, None) => break,
            }
        }

        proto::UpdateRepository {
            branch_summary: self.branch.as_ref().map(branch_to_proto),
            head_commit_details: self.head_commit.as_ref().map(commit_details_to_proto),
            updated_statuses,
            removed_statuses,
            current_merge_conflicts: self
                .merge
                .conflicted_paths
                .iter()
                .map(|path| path.as_ref().to_proto())
                .collect(),
            project_id,
            id: self.id.to_proto(),
            abs_path: self.work_directory_abs_path.to_proto(),
            entry_ids: vec![],
            scan_id: self.scan_id,
            is_last_update: true,
        }
    }

    pub fn status(&self) -> impl Iterator<Item = StatusEntry> + '_ {
        self.statuses_by_path.iter().cloned()
    }

    pub fn status_summary(&self) -> GitSummary {
        self.statuses_by_path.summary().item_summary
    }

    pub fn status_for_path(&self, path: &RepoPath) -> Option<StatusEntry> {
        self.statuses_by_path
            .get(&PathKey(path.0.clone()), &())
            .cloned()
    }

    pub fn abs_path_to_repo_path(&self, abs_path: &Path) -> Option<RepoPath> {
        abs_path
            .strip_prefix(&self.work_directory_abs_path)
            .map(RepoPath::from)
            .ok()
    }

    pub fn had_conflict_on_last_merge_head_change(&self, repo_path: &RepoPath) -> bool {
        self.merge.conflicted_paths.contains(&repo_path)
    }

    pub fn has_conflict(&self, repo_path: &RepoPath) -> bool {
        let had_conflict_on_last_merge_head_change =
            self.merge.conflicted_paths.contains(&repo_path);
        let has_conflict_currently = self
            .status_for_path(&repo_path)
            .map_or(false, |entry| entry.status.is_conflicted());
        had_conflict_on_last_merge_head_change || has_conflict_currently
    }

    /// This is the name that will be displayed in the repository selector for this repository.
    pub fn display_name(&self) -> SharedString {
        self.work_directory_abs_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
            .into()
    }
}

impl Repository {
    pub fn snapshot(&self) -> RepositorySnapshot {
        self.snapshot.clone()
    }

    fn local(
        id: RepositoryId,
        work_directory_abs_path: Arc<Path>,
        dot_git_abs_path: Arc<Path>,
        repository_dir_abs_path: Arc<Path>,
        common_dir_abs_path: Arc<Path>,
        project_environment: WeakEntity<ProjectEnvironment>,
        fs: Arc<dyn Fs>,
        git_store: WeakEntity<GitStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        let snapshot = RepositorySnapshot::empty(id, work_directory_abs_path.clone());
        Repository {
            this: cx.weak_entity(),
            git_store,
            snapshot,
            commit_message_buffer: None,
            askpass_delegates: Default::default(),
            paths_needing_status_update: Default::default(),
            job_sender: Repository::spawn_local_git_worker(
                work_directory_abs_path,
                dot_git_abs_path,
                repository_dir_abs_path,
                common_dir_abs_path,
                project_environment,
                fs,
                cx,
            ),
            job_id: 0,
            active_jobs: Default::default(),
        }
    }

    pub fn git_store(&self) -> Option<Entity<GitStore>> {
        self.git_store.upgrade()
    }

    fn reload_buffer_diff_bases(&mut self, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        let git_store = self.git_store.clone();
        let _ = self.send_keyed_job(
            Some(GitJobKey::ReloadBufferDiffBases),
            None,
            |state, mut cx| async move {
                let RepositoryState::Local { backend, .. } = state;

                let Some(this) = this.upgrade() else {
                    return Ok(());
                };

                let repo_diff_state_updates = this.update(&mut cx, |this, cx| {
                    git_store.update(cx, |git_store, cx| {
                        git_store
                            .diffs
                            .iter()
                            .filter_map(|(buffer_id, diff_state)| {
                                let buffer_store = git_store.buffer_store.read(cx);
                                let buffer = buffer_store.get(*buffer_id)?;
                                let file = File::from_dyn(buffer.read(cx).file())?;
                                let abs_path =
                                    file.worktree.read(cx).absolutize(&file.path).ok()?;
                                let repo_path = this.abs_path_to_repo_path(&abs_path)?;
                                log::debug!(
                                    "start reload diff bases for repo path {}",
                                    repo_path.0.display()
                                );
                                diff_state.update(cx, |diff_state, _| {
                                    let has_unstaged_diff = diff_state
                                        .unstaged_diff
                                        .as_ref()
                                        .is_some_and(|diff| diff.is_upgradable());
                                    let has_uncommitted_diff = diff_state
                                        .uncommitted_diff
                                        .as_ref()
                                        .is_some_and(|set| set.is_upgradable());

                                    Some((
                                        buffer,
                                        repo_path,
                                        has_unstaged_diff.then(|| diff_state.index_text.clone()),
                                        has_uncommitted_diff.then(|| diff_state.head_text.clone()),
                                    ))
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                })??;

                let buffer_diff_base_changes = cx
                    .background_spawn(async move {
                        let mut changes = Vec::new();
                        for (buffer, repo_path, current_index_text, current_head_text) in
                            &repo_diff_state_updates
                        {
                            let index_text = if current_index_text.is_some() {
                                backend.load_index_text(repo_path.clone()).await
                            } else {
                                None
                            };
                            let head_text = if current_head_text.is_some() {
                                backend.load_committed_text(repo_path.clone()).await
                            } else {
                                None
                            };

                            let change =
                                match (current_index_text.as_ref(), current_head_text.as_ref()) {
                                    (Some(current_index), Some(current_head)) => {
                                        let index_changed =
                                            index_text.as_ref() != current_index.as_deref();
                                        let head_changed =
                                            head_text.as_ref() != current_head.as_deref();
                                        if index_changed && head_changed {
                                            if index_text == head_text {
                                                Some(DiffBasesChange::SetBoth(head_text))
                                            } else {
                                                Some(DiffBasesChange::SetEach {
                                                    index: index_text,
                                                    head: head_text,
                                                })
                                            }
                                        } else if index_changed {
                                            Some(DiffBasesChange::SetIndex(index_text))
                                        } else if head_changed {
                                            Some(DiffBasesChange::SetHead(head_text))
                                        } else {
                                            None
                                        }
                                    }
                                    (Some(current_index), None) => {
                                        let index_changed =
                                            index_text.as_ref() != current_index.as_deref();
                                        index_changed
                                            .then_some(DiffBasesChange::SetIndex(index_text))
                                    }
                                    (None, Some(current_head)) => {
                                        let head_changed =
                                            head_text.as_ref() != current_head.as_deref();
                                        head_changed.then_some(DiffBasesChange::SetHead(head_text))
                                    }
                                    (None, None) => None,
                                };

                            changes.push((buffer.clone(), change))
                        }
                        changes
                    })
                    .await;

                git_store.update(&mut cx, |git_store, cx| {
                    for (buffer, diff_bases_change) in buffer_diff_base_changes {
                        let buffer_snapshot = buffer.read(cx).text_snapshot();
                        let buffer_id = buffer_snapshot.remote_id();
                        let Some(diff_state) = git_store.diffs.get(&buffer_id) else {
                            continue;
                        };

                        let downstream_client = git_store.downstream_client();
                        diff_state.update(cx, |diff_state, cx| {
                            use proto::update_diff_bases::Mode;

                            if let Some((diff_bases_change, (client, project_id))) =
                                diff_bases_change.clone().zip(downstream_client)
                            {
                                let (staged_text, committed_text, mode) = match diff_bases_change {
                                    DiffBasesChange::SetIndex(index) => {
                                        (index, None, Mode::IndexOnly)
                                    }
                                    DiffBasesChange::SetHead(head) => (None, head, Mode::HeadOnly),
                                    DiffBasesChange::SetEach { index, head } => {
                                        (index, head, Mode::IndexAndHead)
                                    }
                                    DiffBasesChange::SetBoth(text) => {
                                        (None, text, Mode::IndexMatchesHead)
                                    }
                                };
                                client
                                    .send(proto::UpdateDiffBases {
                                        project_id: project_id.to_proto(),
                                        buffer_id: buffer_id.to_proto(),
                                        staged_text,
                                        committed_text,
                                        mode: mode as i32,
                                    })
                                    .log_err();
                            }

                            diff_state.diff_bases_changed(buffer_snapshot, diff_bases_change, cx);
                        });
                    }
                })
            },
        );
    }

    pub fn send_job<F, Fut, R>(
        &mut self,
        status: Option<SharedString>,
        job: F,
    ) -> oneshot::Receiver<R>
    where
        F: FnOnce(RepositoryState, AsyncApp) -> Fut + 'static,
        Fut: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        self.send_keyed_job(None, status, job)
    }

    fn send_keyed_job<F, Fut, R>(
        &mut self,
        key: Option<GitJobKey>,
        status: Option<SharedString>,
        job: F,
    ) -> oneshot::Receiver<R>
    where
        F: FnOnce(RepositoryState, AsyncApp) -> Fut + 'static,
        Fut: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        let (result_tx, result_rx) = futures::channel::oneshot::channel();
        let job_id = post_inc(&mut self.job_id);
        let this = self.this.clone();
        self.job_sender
            .unbounded_send(GitJob {
                key,
                job: Box::new(move |state, cx: &mut AsyncApp| {
                    let job = job(state, cx.clone());
                    cx.spawn(async move |cx| {
                        if let Some(s) = status.clone() {
                            this.update(cx, |this, cx| {
                                this.active_jobs.insert(
                                    job_id,
                                    JobInfo {
                                        start: Instant::now(),
                                        message: s.clone(),
                                    },
                                );

                                cx.notify();
                            })
                            .ok();
                        }
                        let result = job.await;

                        this.update(cx, |this, cx| {
                            this.active_jobs.remove(&job_id);
                            cx.notify();
                        })
                        .ok();

                        result_tx.send(result).ok();
                    })
                }),
            })
            .ok();
        result_rx
    }

    pub fn set_as_active_repository(&self, cx: &mut Context<Self>) {
        let Some(git_store) = self.git_store.upgrade() else {
            return;
        };
        let entity = cx.entity();
        git_store.update(cx, |git_store, cx| {
            let Some((&id, _)) = git_store
                .repositories
                .iter()
                .find(|(_, handle)| *handle == &entity)
            else {
                return;
            };
            git_store.active_repo_id = Some(id);
            cx.emit(GitStoreEvent::ActiveRepositoryChanged(Some(id)));
        });
    }

    pub fn cached_status(&self) -> impl '_ + Iterator<Item = StatusEntry> {
        self.snapshot.status()
    }

    pub fn repo_path_to_project_path(&self, path: &RepoPath, cx: &App) -> Option<ProjectPath> {
        let git_store = self.git_store.upgrade()?;
        let worktree_store = git_store.read(cx).worktree_store.read(cx);
        let abs_path = self.snapshot.work_directory_abs_path.join(&path.0);
        let (worktree, relative_path) = worktree_store.find_worktree(abs_path, cx)?;
        Some(ProjectPath {
            worktree_id: worktree.read(cx).id(),
            path: relative_path.into(),
        })
    }

    pub fn project_path_to_repo_path(&self, path: &ProjectPath, cx: &App) -> Option<RepoPath> {
        let git_store = self.git_store.upgrade()?;
        let worktree_store = git_store.read(cx).worktree_store.read(cx);
        let abs_path = worktree_store.absolutize(path, cx)?;
        self.snapshot.abs_path_to_repo_path(&abs_path)
    }

    pub fn contains_sub_repo(&self, other: &Entity<Self>, cx: &App) -> bool {
        other
            .read(cx)
            .snapshot
            .work_directory_abs_path
            .starts_with(&self.snapshot.work_directory_abs_path)
    }

    pub fn open_commit_buffer(
        &mut self,
        languages: Option<Arc<LanguageRegistry>>,
        buffer_store: Entity<BufferStore>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Buffer>>> {
        if let Some(buffer) = self.commit_message_buffer.clone() {
            return Task::ready(Ok(buffer));
        }
        let this = cx.weak_entity();

        let rx = self.send_job(None, move |_, mut cx| async move {
            let Some(this) = this.upgrade() else {
                bail!("git store was dropped");
            };
            this.update(&mut cx, |_, cx| {
                Self::open_local_commit_buffer(languages, buffer_store, cx)
            })?
            .await
        });

        cx.spawn(|_, _: &mut AsyncApp| async move { rx.await? })
    }

    fn open_local_commit_buffer(
        language_registry: Option<Arc<LanguageRegistry>>,
        buffer_store: Entity<BufferStore>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Buffer>>> {
        cx.spawn(async move |repository, cx| {
            let buffer = buffer_store
                .update(cx, |buffer_store, cx| buffer_store.create_buffer(cx))?
                .await?;

            if let Some(language_registry) = language_registry {
                let git_commit_language = language_registry.language_for_name("Git Commit").await?;
                buffer.update(cx, |buffer, cx| {
                    buffer.set_language(Some(git_commit_language), cx);
                })?;
            }

            repository.update(cx, |repository, _| {
                repository.commit_message_buffer = Some(buffer.clone());
            })?;
            Ok(buffer)
        })
    }

    pub fn checkout_files(
        &mut self,
        commit: &str,
        paths: Vec<RepoPath>,
        _cx: &mut App,
    ) -> oneshot::Receiver<Result<()>> {
        let commit = commit.to_string();

        self.send_job(
            Some(format!("git checkout {}", commit).into()),
            move |git_repo, _| async move {
                match git_repo {
                    RepositoryState::Local {
                        backend,
                        environment,
                        ..
                    } => {
                        backend
                            .checkout_files(commit, paths, environment.clone())
                            .await
                    }
                }
            },
        )
    }

    pub fn reset(
        &mut self,
        commit: String,
        reset_mode: ResetMode,
        _cx: &mut App,
    ) -> oneshot::Receiver<Result<()>> {
        let commit = commit.to_string();

        self.send_job(None, move |git_repo, _| async move {
            match git_repo {
                RepositoryState::Local {
                    backend,
                    environment,
                    ..
                } => backend.reset(commit, reset_mode, environment).await,
            }
        })
    }

    pub fn show(&mut self, commit: String) -> oneshot::Receiver<Result<CommitDetails>> {
        self.send_job(None, move |git_repo, _cx| async move {
            match git_repo {
                RepositoryState::Local { backend, .. } => backend.show(commit).await,
            }
        })
    }

    pub fn load_commit_diff(&mut self, commit: String) -> oneshot::Receiver<Result<CommitDiff>> {
        self.send_job(None, move |git_repo, cx| async move {
            match git_repo {
                RepositoryState::Local { backend, .. } => backend.load_commit(commit, cx).await,
            }
        })
    }

    fn buffer_store(&self, cx: &App) -> Option<Entity<BufferStore>> {
        Some(self.git_store.upgrade()?.read(cx).buffer_store.clone())
    }

    pub fn stage_entries(
        &self,
        entries: Vec<RepoPath>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        if entries.is_empty() {
            return Task::ready(Ok(()));
        }

        let mut save_futures = Vec::new();
        if let Some(buffer_store) = self.buffer_store(cx) {
            buffer_store.update(cx, |buffer_store, cx| {
                for path in &entries {
                    let Some(project_path) = self.repo_path_to_project_path(path, cx) else {
                        continue;
                    };
                    if let Some(buffer) = buffer_store.get_by_path(&project_path, cx) {
                        if buffer
                            .read(cx)
                            .file()
                            .map_or(false, |file| file.disk_state().exists())
                        {
                            save_futures.push(buffer_store.save_buffer(buffer, cx));
                        }
                    }
                }
            })
        }

        cx.spawn(async move |this, cx| {
            for save_future in save_futures {
                save_future.await?;
            }

            this.update(cx, |this, _| {
                this.send_job(None, move |git_repo, _cx| async move {
                    match git_repo {
                        RepositoryState::Local {
                            backend,
                            environment,
                            ..
                        } => backend.stage_paths(entries, environment.clone()).await,
                    }
                })
            })?
            .await??;

            Ok(())
        })
    }

    pub fn unstage_entries(
        &self,
        entries: Vec<RepoPath>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        if entries.is_empty() {
            return Task::ready(Ok(()));
        }

        let mut save_futures = Vec::new();
        if let Some(buffer_store) = self.buffer_store(cx) {
            buffer_store.update(cx, |buffer_store, cx| {
                for path in &entries {
                    let Some(project_path) = self.repo_path_to_project_path(path, cx) else {
                        continue;
                    };
                    if let Some(buffer) = buffer_store.get_by_path(&project_path, cx) {
                        if buffer
                            .read(cx)
                            .file()
                            .map_or(false, |file| file.disk_state().exists())
                        {
                            save_futures.push(buffer_store.save_buffer(buffer, cx));
                        }
                    }
                }
            })
        }

        cx.spawn(async move |this, cx| {
            for save_future in save_futures {
                save_future.await?;
            }

            this.update(cx, |this, _| {
                this.send_job(None, move |git_repo, _cx| async move {
                    match git_repo {
                        RepositoryState::Local {
                            backend,
                            environment,
                            ..
                        } => backend.unstage_paths(entries, environment).await,
                    }
                })
            })?
            .await??;

            Ok(())
        })
    }

    pub fn stage_all(&self, cx: &mut Context<Self>) -> Task<anyhow::Result<()>> {
        let to_stage = self
            .cached_status()
            .filter(|entry| !entry.status.staging().is_fully_staged())
            .map(|entry| entry.repo_path.clone())
            .collect();
        self.stage_entries(to_stage, cx)
    }

    pub fn unstage_all(&self, cx: &mut Context<Self>) -> Task<anyhow::Result<()>> {
        let to_unstage = self
            .cached_status()
            .filter(|entry| entry.status.staging().has_staged())
            .map(|entry| entry.repo_path.clone())
            .collect();
        self.unstage_entries(to_unstage, cx)
    }

    pub fn commit(
        &mut self,
        message: SharedString,
        name_and_email: Option<(SharedString, SharedString)>,
        options: CommitOptions,
        _cx: &mut App,
    ) -> oneshot::Receiver<Result<()>> {
        self.send_job(Some("git commit".into()), move |git_repo, _cx| async move {
            match git_repo {
                RepositoryState::Local {
                    backend,
                    environment,
                    ..
                } => {
                    backend
                        .commit(message, name_and_email, options, environment)
                        .await
                }
            }
        })
    }

    fn spawn_set_index_text_job(
        &mut self,
        path: RepoPath,
        content: Option<String>,
        hunk_staging_operation_count: Option<usize>,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<anyhow::Result<()>> {
        let this = cx.weak_entity();
        let git_store = self.git_store.clone();
        self.send_keyed_job(
            Some(GitJobKey::WriteIndex(path.clone())),
            None,
            move |git_repo, mut cx| async move {
                log::debug!("start updating index text for buffer {}", path.display());
                match git_repo {
                    RepositoryState::Local {
                        backend,
                        environment,
                        ..
                    } => {
                        backend
                            .set_index_text(path.clone(), content, environment.clone())
                            .await?;
                    }
                }
                log::debug!("finish updating index text for buffer {}", path.display());

                if let Some(hunk_staging_operation_count) = hunk_staging_operation_count {
                    let project_path = this
                        .read_with(&cx, |this, cx| this.repo_path_to_project_path(&path, cx))
                        .ok()
                        .flatten();
                    git_store.update(&mut cx, |git_store, cx| {
                        let buffer_id = git_store
                            .buffer_store
                            .read(cx)
                            .get_by_path(&project_path?, cx)?
                            .read(cx)
                            .remote_id();
                        let diff_state = git_store.diffs.get(&buffer_id)?;
                        diff_state.update(cx, |diff_state, _| {
                            diff_state.hunk_staging_operation_count_as_of_write =
                                hunk_staging_operation_count;
                        });
                        Some(())
                    })?;
                }
                Ok(())
            },
        )
    }

    pub fn branches(&mut self) -> oneshot::Receiver<Result<Vec<Branch>>> {
        self.send_job(None, move |repo, _| async move {
            match repo {
                RepositoryState::Local { backend, .. } => backend.branches().await,
            }
        })
    }

    pub fn diff(&mut self, diff_type: DiffType, _cx: &App) -> oneshot::Receiver<Result<String>> {
        self.send_job(None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local { backend, .. } => backend.diff(diff_type).await,
            }
        })
    }

    pub fn create_branch(&mut self, branch_name: String) -> oneshot::Receiver<Result<()>> {
        self.send_job(
            Some(format!("git switch -c {branch_name}").into()),
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local { backend, .. } => {
                        backend.create_branch(branch_name).await
                    }
                }
            },
        )
    }

    pub fn change_branch(&mut self, branch_name: String) -> oneshot::Receiver<Result<()>> {
        self.send_job(
            Some(format!("git switch {branch_name}").into()),
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local { backend, .. } => {
                        backend.change_branch(branch_name).await
                    }
                }
            },
        )
    }

    pub fn check_for_pushed_commits(&mut self) -> oneshot::Receiver<Result<Vec<SharedString>>> {
        self.send_job(None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local { backend, .. } => backend.check_for_pushed_commit().await,
            }
        })
    }

    pub fn checkpoint(&mut self) -> oneshot::Receiver<Result<GitRepositoryCheckpoint>> {
        self.send_job(None, |repo, _cx| async move {
            match repo {
                RepositoryState::Local { backend, .. } => backend.checkpoint().await,
            }
        })
    }

    pub fn restore_checkpoint(
        &mut self,
        checkpoint: GitRepositoryCheckpoint,
    ) -> oneshot::Receiver<Result<()>> {
        self.send_job(None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local { backend, .. } => {
                    backend.restore_checkpoint(checkpoint).await
                }
            }
        })
    }

    pub fn compare_checkpoints(
        &mut self,
        left: GitRepositoryCheckpoint,
        right: GitRepositoryCheckpoint,
    ) -> oneshot::Receiver<Result<bool>> {
        self.send_job(None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local { backend, .. } => {
                    backend.compare_checkpoints(left, right).await
                }
            }
        })
    }

    pub fn diff_checkpoints(
        &mut self,
        base_checkpoint: GitRepositoryCheckpoint,
        target_checkpoint: GitRepositoryCheckpoint,
    ) -> oneshot::Receiver<Result<String>> {
        self.send_job(None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local { backend, .. } => {
                    backend
                        .diff_checkpoints(base_checkpoint, target_checkpoint)
                        .await
                }
            }
        })
    }

    fn spawn_local_git_worker(
        work_directory_abs_path: Arc<Path>,
        dot_git_abs_path: Arc<Path>,
        _repository_dir_abs_path: Arc<Path>,
        _common_dir_abs_path: Arc<Path>,
        project_environment: WeakEntity<ProjectEnvironment>,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> mpsc::UnboundedSender<GitJob> {
        let (job_tx, mut job_rx) = mpsc::unbounded::<GitJob>();

        cx.spawn(async move |_, cx| {
            let environment = project_environment
                .upgrade()
                .context("missing project environment")?
                .update(cx, |project_environment, cx| {
                    project_environment.get_directory_environment(work_directory_abs_path.clone(), cx)
                })?
                .await
                .unwrap_or_else(|| {
                    log::error!("failed to get working directory environment for repository {work_directory_abs_path:?}");
                    HashMap::default()
                });
            let backend = cx
                .background_spawn(async move {
                    fs.open_repo(&dot_git_abs_path)
                        .with_context(|| format!("opening repository at {dot_git_abs_path:?}"))
                })
                .await?;


            let state = RepositoryState::Local {
                backend,
                environment: Arc::new(environment),
            };
            let mut jobs = VecDeque::new();
            loop {
                while let Ok(Some(next_job)) = job_rx.try_next() {
                    jobs.push_back(next_job);
                }

                if let Some(job) = jobs.pop_front() {
                    if let Some(current_key) = &job.key {
                        if jobs
                            .iter()
                            .any(|other_job| other_job.key.as_ref() == Some(current_key))
                        {
                            continue;
                        }
                    }
                    (job.job)(state.clone(), cx).await;
                } else if let Some(job) = job_rx.next().await {
                    jobs.push_back(job);
                } else {
                    break;
                }
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);

        job_tx
    }

    fn load_staged_text(&mut self, repo_path: RepoPath, cx: &App) -> Task<Result<Option<String>>> {
        let rx = self.send_job(None, move |state, _| async move {
            match state {
                RepositoryState::Local { backend, .. } => {
                    anyhow::Ok(backend.load_index_text(repo_path).await)
                }
            }
        });
        cx.spawn(|_: &mut AsyncApp| async move { rx.await? })
    }

    fn load_committed_text(
        &mut self,
        repo_path: RepoPath,
        cx: &App,
    ) -> Task<Result<DiffBasesChange>> {
        let rx = self.send_job(None, move |state, _| async move {
            match state {
                RepositoryState::Local { backend, .. } => {
                    let committed_text = backend.load_committed_text(repo_path.clone()).await;
                    let staged_text = backend.load_index_text(repo_path).await;
                    let diff_bases_change = if committed_text == staged_text {
                        DiffBasesChange::SetBoth(committed_text)
                    } else {
                        DiffBasesChange::SetEach {
                            index: staged_text,
                            head: committed_text,
                        }
                    };
                    anyhow::Ok(diff_bases_change)
                }
            }
        });

        cx.spawn(|_: &mut AsyncApp| async move { rx.await? })
    }

    fn paths_changed(
        &mut self,
        paths: Vec<RepoPath>,
        updates_tx: Option<mpsc::UnboundedSender<DownstreamUpdate>>,
        cx: &mut Context<Self>,
    ) {
        self.paths_needing_status_update.extend(paths);

        let this = cx.weak_entity();
        let _ = self.send_keyed_job(
            Some(GitJobKey::RefreshStatuses),
            None,
            |state, mut cx| async move {
                let (prev_snapshot, mut changed_paths) = this.update(&mut cx, |this, _| {
                    (
                        this.snapshot.clone(),
                        mem::take(&mut this.paths_needing_status_update),
                    )
                })?;
                let RepositoryState::Local { backend, .. } = state;

                let paths = changed_paths.iter().cloned().collect::<Vec<_>>();
                let statuses = backend.status(&paths).await?;

                let changed_path_statuses = cx
                    .background_spawn(async move {
                        let mut changed_path_statuses = Vec::new();
                        let prev_statuses = prev_snapshot.statuses_by_path.clone();
                        let mut cursor = prev_statuses.cursor::<PathProgress>(&());

                        for (repo_path, status) in &*statuses.entries {
                            changed_paths.remove(repo_path);
                            if cursor.seek_forward(&PathTarget::Path(repo_path), Bias::Left, &()) {
                                if cursor.item().is_some_and(|entry| entry.status == *status) {
                                    continue;
                                }
                            }

                            changed_path_statuses.push(Edit::Insert(StatusEntry {
                                repo_path: repo_path.clone(),
                                status: *status,
                            }));
                        }
                        let mut cursor = prev_statuses.cursor::<PathProgress>(&());
                        for path in changed_paths.into_iter() {
                            if cursor.seek_forward(&PathTarget::Path(&path), Bias::Left, &()) {
                                changed_path_statuses.push(Edit::Remove(PathKey(path.0)));
                            }
                        }
                        changed_path_statuses
                    })
                    .await;

                this.update(&mut cx, |this, cx| {
                    if !changed_path_statuses.is_empty() {
                        this.snapshot
                            .statuses_by_path
                            .edit(changed_path_statuses, &());
                        this.snapshot.scan_id += 1;
                        if let Some(updates_tx) = updates_tx {
                            updates_tx
                                .unbounded_send(DownstreamUpdate::UpdateRepository(
                                    this.snapshot.clone(),
                                ))
                                .ok();
                        }
                    }
                    cx.emit(RepositoryEvent::Updated { full_scan: false });
                })
            },
        );
    }

    /// currently running git command and when it started
    pub fn current_job(&self) -> Option<JobInfo> {
        self.active_jobs.values().next().cloned()
    }

    pub fn barrier(&mut self) -> oneshot::Receiver<()> {
        self.send_job(None, |_, _| async {})
    }
}

fn serialize_blame_buffer_response(blame: Option<git::blame::Blame>) -> proto::BlameBufferResponse {
    let Some(blame) = blame else {
        return proto::BlameBufferResponse {
            blame_response: None,
        };
    };

    let entries = blame
        .entries
        .into_iter()
        .map(|entry| proto::BlameEntry {
            sha: entry.sha.as_bytes().into(),
            start_line: entry.range.start,
            end_line: entry.range.end,
            original_line_number: entry.original_line_number,
            author: entry.author.clone(),
            author_mail: entry.author_mail.clone(),
            author_time: entry.author_time,
            author_tz: entry.author_tz.clone(),
            committer: entry.committer_name.clone(),
            committer_mail: entry.committer_email.clone(),
            committer_time: entry.committer_time,
            committer_tz: entry.committer_tz.clone(),
            summary: entry.summary.clone(),
            previous: entry.previous.clone(),
            filename: entry.filename.clone(),
        })
        .collect::<Vec<_>>();

    let messages = blame
        .messages
        .into_iter()
        .map(|(oid, message)| proto::CommitMessage {
            oid: oid.as_bytes().into(),
            message,
        })
        .collect::<Vec<_>>();

    proto::BlameBufferResponse {
        blame_response: Some(proto::blame_buffer_response::BlameResponse {
            entries,
            messages,
            remote_url: blame.remote_url,
        }),
    }
}

fn branch_to_proto(branch: &git::repository::Branch) -> proto::Branch {
    proto::Branch {
        is_head: branch.is_head,
        ref_name: branch.ref_name.to_string(),
        unix_timestamp: branch
            .most_recent_commit
            .as_ref()
            .map(|commit| commit.commit_timestamp as u64),
        upstream: branch.upstream.as_ref().map(|upstream| proto::GitUpstream {
            ref_name: upstream.ref_name.to_string(),
            tracking: None,
        }),
        most_recent_commit: branch
            .most_recent_commit
            .as_ref()
            .map(|commit| proto::CommitSummary {
                sha: commit.sha.to_string(),
                subject: commit.subject.to_string(),
                commit_timestamp: commit.commit_timestamp,
            }),
    }
}

fn commit_details_to_proto(commit: &CommitDetails) -> proto::GitCommitDetails {
    proto::GitCommitDetails {
        sha: commit.sha.to_string(),
        message: commit.message.to_string(),
        commit_timestamp: commit.commit_timestamp,
        author_email: commit.author_email.to_string(),
        author_name: commit.author_name.to_string(),
    }
}

fn status_from_proto(
    simple_status: i32,
    status: Option<proto::GitFileStatus>,
) -> anyhow::Result<FileStatus> {
    use proto::git_file_status::Variant;

    let Some(variant) = status.and_then(|status| status.variant) else {
        let code = proto::GitStatus::from_i32(simple_status)
            .with_context(|| format!("Invalid git status code: {simple_status}"))?;
        let result = match code {
            proto::GitStatus::Added => TrackedStatus {
                worktree_status: StatusCode::Added,
                index_status: StatusCode::Unmodified,
            }
            .into(),
            proto::GitStatus::Modified => TrackedStatus {
                worktree_status: StatusCode::Modified,
                index_status: StatusCode::Unmodified,
            }
            .into(),
            proto::GitStatus::Conflict => UnmergedStatus {
                first_head: UnmergedStatusCode::Updated,
                second_head: UnmergedStatusCode::Updated,
            }
            .into(),
            proto::GitStatus::Deleted => TrackedStatus {
                worktree_status: StatusCode::Deleted,
                index_status: StatusCode::Unmodified,
            }
            .into(),
            _ => anyhow::bail!("Invalid code for simple status: {simple_status}"),
        };
        return Ok(result);
    };

    let result = match variant {
        Variant::Untracked(_) => FileStatus::Untracked,
        Variant::Ignored(_) => FileStatus::Ignored,
        Variant::Unmerged(unmerged) => {
            let [first_head, second_head] =
                [unmerged.first_head, unmerged.second_head].map(|head| {
                    let code = proto::GitStatus::from_i32(head)
                        .with_context(|| format!("Invalid git status code: {head}"))?;
                    let result = match code {
                        proto::GitStatus::Added => UnmergedStatusCode::Added,
                        proto::GitStatus::Updated => UnmergedStatusCode::Updated,
                        proto::GitStatus::Deleted => UnmergedStatusCode::Deleted,
                        _ => anyhow::bail!("Invalid code for unmerged status: {code:?}"),
                    };
                    Ok(result)
                });
            let [first_head, second_head] = [first_head?, second_head?];
            UnmergedStatus {
                first_head,
                second_head,
            }
            .into()
        }
        Variant::Tracked(tracked) => {
            let [index_status, worktree_status] = [tracked.index_status, tracked.worktree_status]
                .map(|status| {
                    let code = proto::GitStatus::from_i32(status)
                        .with_context(|| format!("Invalid git status code: {status}"))?;
                    let result = match code {
                        proto::GitStatus::Modified => StatusCode::Modified,
                        proto::GitStatus::TypeChanged => StatusCode::TypeChanged,
                        proto::GitStatus::Added => StatusCode::Added,
                        proto::GitStatus::Deleted => StatusCode::Deleted,
                        proto::GitStatus::Renamed => StatusCode::Renamed,
                        proto::GitStatus::Copied => StatusCode::Copied,
                        proto::GitStatus::Unmodified => StatusCode::Unmodified,
                        _ => anyhow::bail!("Invalid code for tracked status: {code:?}"),
                    };
                    Ok(result)
                });
            let [index_status, worktree_status] = [index_status?, worktree_status?];
            TrackedStatus {
                index_status,
                worktree_status,
            }
            .into()
        }
    };
    Ok(result)
}

fn status_to_proto(status: FileStatus) -> proto::GitFileStatus {
    use proto::git_file_status::{Tracked, Unmerged, Variant};

    let variant = match status {
        FileStatus::Untracked => Variant::Untracked(Default::default()),
        FileStatus::Ignored => Variant::Ignored(Default::default()),
        FileStatus::Unmerged(UnmergedStatus {
            first_head,
            second_head,
        }) => Variant::Unmerged(Unmerged {
            first_head: unmerged_status_to_proto(first_head),
            second_head: unmerged_status_to_proto(second_head),
        }),
        FileStatus::Tracked(TrackedStatus {
            index_status,
            worktree_status,
        }) => Variant::Tracked(Tracked {
            index_status: tracked_status_to_proto(index_status),
            worktree_status: tracked_status_to_proto(worktree_status),
        }),
    };
    proto::GitFileStatus {
        variant: Some(variant),
    }
}

fn unmerged_status_to_proto(code: UnmergedStatusCode) -> i32 {
    match code {
        UnmergedStatusCode::Added => proto::GitStatus::Added as _,
        UnmergedStatusCode::Deleted => proto::GitStatus::Deleted as _,
        UnmergedStatusCode::Updated => proto::GitStatus::Updated as _,
    }
}

fn tracked_status_to_proto(code: StatusCode) -> i32 {
    match code {
        StatusCode::Added => proto::GitStatus::Added as _,
        StatusCode::Deleted => proto::GitStatus::Deleted as _,
        StatusCode::Modified => proto::GitStatus::Modified as _,
        StatusCode::Renamed => proto::GitStatus::Renamed as _,
        StatusCode::TypeChanged => proto::GitStatus::TypeChanged as _,
        StatusCode::Copied => proto::GitStatus::Copied as _,
        StatusCode::Unmodified => proto::GitStatus::Unmodified as _,
    }
}

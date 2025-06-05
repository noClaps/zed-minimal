use crate::Editor;
use anyhow::Result;
use collections::HashMap;
use git::{
    GitHostingProviderRegistry, GitRemote, Oid,
    blame::{Blame, BlameEntry, ParsedCommitMessage},
    parse_git_remote_url,
};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, Hsla, ScrollHandle, Subscription, Task,
    TextStyle, WeakEntity, Window,
};
use language::{Bias, Buffer, BufferSnapshot, Edit};
use markdown::Markdown;
use multi_buffer::RowInfo;
use project::{
    Project, ProjectItem,
    git_store::{GitStoreEvent, Repository, RepositoryEvent},
};
use smallvec::SmallVec;
use std::{sync::Arc, time::Duration};
use sum_tree::SumTree;
use workspace::Workspace;

#[derive(Clone, Debug, Default)]
pub struct GitBlameEntry {
    pub rows: u32,
    pub blame: Option<BlameEntry>,
}

#[derive(Clone, Debug, Default)]
pub struct GitBlameEntrySummary {
    rows: u32,
}

impl sum_tree::Item for GitBlameEntry {
    type Summary = GitBlameEntrySummary;

    fn summary(&self, _cx: &()) -> Self::Summary {
        GitBlameEntrySummary { rows: self.rows }
    }
}

impl sum_tree::Summary for GitBlameEntrySummary {
    type Context = ();

    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &Self, _cx: &()) {
        self.rows += summary.rows;
    }
}

impl<'a> sum_tree::Dimension<'a, GitBlameEntrySummary> for u32 {
    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a GitBlameEntrySummary, _cx: &()) {
        *self += summary.rows;
    }
}

pub struct GitBlame {
    project: Entity<Project>,
    buffer: Entity<Buffer>,
    entries: SumTree<GitBlameEntry>,
    commit_details: HashMap<Oid, ParsedCommitMessage>,
    buffer_snapshot: BufferSnapshot,
    buffer_edits: text::Subscription,
    task: Task<Result<()>>,
    focused: bool,
    generated: bool,
    changed_while_blurred: bool,
    user_triggered: bool,
    regenerate_on_edit_task: Task<Result<()>>,
    _regenerate_subscriptions: Vec<Subscription>,
}

pub trait BlameRenderer {
    fn max_author_length(&self) -> usize;

    fn render_blame_entry(
        &self,
        _: &TextStyle,
        _: BlameEntry,
        _: Option<ParsedCommitMessage>,
        _: Entity<Repository>,
        _: WeakEntity<Workspace>,
        _: Entity<Editor>,
        _: usize,
        _: Hsla,
        _: &mut App,
    ) -> Option<AnyElement>;

    fn render_inline_blame_entry(
        &self,
        _: &TextStyle,
        _: BlameEntry,
        _: &mut App,
    ) -> Option<AnyElement>;

    fn render_blame_entry_popover(
        &self,
        _: BlameEntry,
        _: ScrollHandle,
        _: Option<ParsedCommitMessage>,
        _: Entity<Markdown>,
        _: Entity<Repository>,
        _: WeakEntity<Workspace>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<AnyElement>;

    fn open_blame_commit(
        &self,
        _: BlameEntry,
        _: Entity<Repository>,
        _: WeakEntity<Workspace>,
        _: &mut Window,
        _: &mut App,
    );
}

impl BlameRenderer for () {
    fn max_author_length(&self) -> usize {
        0
    }

    fn render_blame_entry(
        &self,
        _: &TextStyle,
        _: BlameEntry,
        _: Option<ParsedCommitMessage>,
        _: Entity<Repository>,
        _: WeakEntity<Workspace>,
        _: Entity<Editor>,
        _: usize,
        _: Hsla,
        _: &mut App,
    ) -> Option<AnyElement> {
        None
    }

    fn render_inline_blame_entry(
        &self,
        _: &TextStyle,
        _: BlameEntry,
        _: &mut App,
    ) -> Option<AnyElement> {
        None
    }

    fn render_blame_entry_popover(
        &self,
        _: BlameEntry,
        _: ScrollHandle,
        _: Option<ParsedCommitMessage>,
        _: Entity<Markdown>,
        _: Entity<Repository>,
        _: WeakEntity<Workspace>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<AnyElement> {
        None
    }

    fn open_blame_commit(
        &self,
        _: BlameEntry,
        _: Entity<Repository>,
        _: WeakEntity<Workspace>,
        _: &mut Window,
        _: &mut App,
    ) {
    }
}

pub(crate) struct GlobalBlameRenderer(pub Arc<dyn BlameRenderer>);

impl gpui::Global for GlobalBlameRenderer {}

impl GitBlame {
    pub fn new(
        buffer: Entity<Buffer>,
        project: Entity<Project>,
        user_triggered: bool,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let entries = SumTree::from_item(
            GitBlameEntry {
                rows: buffer.read(cx).max_point().row + 1,
                blame: None,
            },
            &(),
        );

        let buffer_subscriptions = cx.subscribe(&buffer, |this, buffer, event, cx| match event {
            language::BufferEvent::DirtyChanged => {
                if !buffer.read(cx).is_dirty() {
                    this.generate(cx);
                }
            }
            language::BufferEvent::Edited => {
                this.regenerate_on_edit(cx);
            }
            _ => {}
        });

        let project_subscription = cx.subscribe(&project, {
            let buffer = buffer.clone();

            move |this, _, event, cx| match event {
                project::Event::WorktreeUpdatedEntries(_, updated) => {
                    let project_entry_id = buffer.read(cx).entry_id(cx);
                    if updated
                        .iter()
                        .any(|(_, entry_id, _)| project_entry_id == Some(*entry_id))
                    {
                        log::debug!("Updated buffers. Regenerating blame data...",);
                        this.generate(cx);
                    }
                }
                _ => {}
            }
        });

        let git_store = project.read(cx).git_store().clone();
        let git_store_subscription =
            cx.subscribe(&git_store, move |this, _, event, cx| match event {
                GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::Updated { .. }, _)
                | GitStoreEvent::RepositoryAdded(_)
                | GitStoreEvent::RepositoryRemoved(_) => {
                    log::debug!("Status of git repositories updated. Regenerating blame data...",);
                    this.generate(cx);
                }
                _ => {}
            });

        let buffer_snapshot = buffer.read(cx).snapshot();
        let buffer_edits = buffer.update(cx, |buffer, _| buffer.subscribe());

        let mut this = Self {
            project,
            buffer,
            buffer_snapshot,
            entries,
            buffer_edits,
            user_triggered,
            focused,
            changed_while_blurred: false,
            commit_details: HashMap::default(),
            task: Task::ready(Ok(())),
            generated: false,
            regenerate_on_edit_task: Task::ready(Ok(())),
            _regenerate_subscriptions: vec![
                buffer_subscriptions,
                project_subscription,
                git_store_subscription,
            ],
        };
        this.generate(cx);
        this
    }

    pub fn repository(&self, cx: &App) -> Option<Entity<Repository>> {
        self.project
            .read(cx)
            .git_store()
            .read(cx)
            .repository_and_path_for_buffer_id(self.buffer.read(cx).remote_id(), cx)
            .map(|(repo, _)| repo)
    }

    pub fn has_generated_entries(&self) -> bool {
        self.generated
    }

    pub fn details_for_entry(&self, entry: &BlameEntry) -> Option<ParsedCommitMessage> {
        self.commit_details.get(&entry.sha).cloned()
    }

    pub fn blame_for_rows<'a>(
        &'a mut self,
        rows: &'a [RowInfo],
        cx: &App,
    ) -> impl 'a + Iterator<Item = Option<BlameEntry>> {
        self.sync(cx);

        let buffer_id = self.buffer_snapshot.remote_id();
        let mut cursor = self.entries.cursor::<u32>(&());
        rows.into_iter().map(move |info| {
            let row = info
                .buffer_row
                .filter(|_| info.buffer_id == Some(buffer_id))?;
            cursor.seek_forward(&row, Bias::Right, &());
            cursor.item()?.blame.clone()
        })
    }

    pub fn max_author_length(&mut self, cx: &App) -> usize {
        self.sync(cx);

        let mut max_author_length = 0;

        for entry in self.entries.iter() {
            let author_len = entry
                .blame
                .as_ref()
                .and_then(|entry| entry.author.as_ref())
                .map(|author| author.len());
            if let Some(author_len) = author_len {
                if author_len > max_author_length {
                    max_author_length = author_len;
                }
            }
        }

        max_author_length
    }

    pub fn blur(&mut self, _: &mut Context<Self>) {
        self.focused = false;
    }

    pub fn focus(&mut self, cx: &mut Context<Self>) {
        if self.focused {
            return;
        }
        self.focused = true;
        if self.changed_while_blurred {
            self.changed_while_blurred = false;
            self.generate(cx);
        }
    }

    fn sync(&mut self, cx: &App) {
        let edits = self.buffer_edits.consume();
        let new_snapshot = self.buffer.read(cx).snapshot();

        let mut row_edits = edits
            .into_iter()
            .map(|edit| {
                let old_point_range = self.buffer_snapshot.offset_to_point(edit.old.start)
                    ..self.buffer_snapshot.offset_to_point(edit.old.end);
                let new_point_range = new_snapshot.offset_to_point(edit.new.start)
                    ..new_snapshot.offset_to_point(edit.new.end);

                if old_point_range.start.column
                    == self.buffer_snapshot.line_len(old_point_range.start.row)
                    && (new_snapshot.chars_at(edit.new.start).next() == Some('\n')
                        || self.buffer_snapshot.line_len(old_point_range.end.row) == 0)
                {
                    Edit {
                        old: old_point_range.start.row + 1..old_point_range.end.row + 1,
                        new: new_point_range.start.row + 1..new_point_range.end.row + 1,
                    }
                } else if old_point_range.start.column == 0
                    && old_point_range.end.column == 0
                    && new_point_range.end.column == 0
                {
                    Edit {
                        old: old_point_range.start.row..old_point_range.end.row,
                        new: new_point_range.start.row..new_point_range.end.row,
                    }
                } else {
                    Edit {
                        old: old_point_range.start.row..old_point_range.end.row + 1,
                        new: new_point_range.start.row..new_point_range.end.row + 1,
                    }
                }
            })
            .peekable();

        let mut new_entries = SumTree::default();
        let mut cursor = self.entries.cursor::<u32>(&());

        while let Some(mut edit) = row_edits.next() {
            while let Some(next_edit) = row_edits.peek() {
                if edit.old.end >= next_edit.old.start {
                    edit.old.end = next_edit.old.end;
                    edit.new.end = next_edit.new.end;
                    row_edits.next();
                } else {
                    break;
                }
            }

            new_entries.append(cursor.slice(&edit.old.start, Bias::Right, &()), &());

            if edit.new.start > new_entries.summary().rows {
                new_entries.push(
                    GitBlameEntry {
                        rows: edit.new.start - new_entries.summary().rows,
                        blame: cursor.item().and_then(|entry| entry.blame.clone()),
                    },
                    &(),
                );
            }

            cursor.seek(&edit.old.end, Bias::Right, &());
            if !edit.new.is_empty() {
                new_entries.push(
                    GitBlameEntry {
                        rows: edit.new.len() as u32,
                        blame: None,
                    },
                    &(),
                );
            }

            let old_end = cursor.end(&());
            if row_edits
                .peek()
                .map_or(true, |next_edit| next_edit.old.start >= old_end)
            {
                if let Some(entry) = cursor.item() {
                    if old_end > edit.old.end {
                        new_entries.push(
                            GitBlameEntry {
                                rows: cursor.end(&()) - edit.old.end,
                                blame: entry.blame.clone(),
                            },
                            &(),
                        );
                    }

                    cursor.next(&());
                }
            }
        }
        new_entries.append(cursor.suffix(&()), &());
        drop(cursor);

        self.buffer_snapshot = new_snapshot;
        self.entries = new_entries;
    }

    fn generate(&mut self, cx: &mut Context<Self>) {
        if !self.focused {
            self.changed_while_blurred = true;
            return;
        }
        let buffer_edits = self.buffer.update(cx, |buffer, _| buffer.subscribe());
        let snapshot = self.buffer.read(cx).snapshot();
        let blame = self.project.update(cx, |project, cx| {
            project.blame_buffer(&self.buffer, None, cx)
        });
        let provider_registry = GitHostingProviderRegistry::default_global(cx);

        self.task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let snapshot = snapshot.clone();
                    async move {
                        let Some(Blame {
                            entries,
                            messages,
                            remote_url,
                        }) = blame.await?
                        else {
                            return Ok(None);
                        };

                        let entries = build_blame_entry_sum_tree(entries, snapshot.max_point().row);
                        let commit_details =
                            parse_commit_messages(messages, remote_url, provider_registry).await;

                        anyhow::Ok(Some((entries, commit_details)))
                    }
                })
                .await;

            this.update(cx, |this, cx| match result {
                Ok(None) => {
                    // Nothing to do, e.g. no repository found
                }
                Ok(Some((entries, commit_details))) => {
                    this.buffer_edits = buffer_edits;
                    this.buffer_snapshot = snapshot;
                    this.entries = entries;
                    this.commit_details = commit_details;
                    this.generated = true;
                    cx.notify();
                }
                Err(error) => this.project.update(cx, |_, cx| {
                    if this.user_triggered {
                        log::error!("failed to get git blame data: {error:?}");
                        let notification = format!("{:#}", error).trim().to_string();
                        cx.emit(project::Event::Toast {
                            notification_id: "git-blame".into(),
                            message: notification,
                        });
                    } else {
                        // If we weren't triggered by a user, we just log errors in the background, instead of sending
                        // notifications.
                        log::debug!("failed to get git blame data: {error:?}");
                    }
                }),
            })
        });
    }

    fn regenerate_on_edit(&mut self, cx: &mut Context<Self>) {
        self.regenerate_on_edit_task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(REGENERATE_ON_EDIT_DEBOUNCE_INTERVAL)
                .await;

            this.update(cx, |this, cx| {
                this.generate(cx);
            })
        })
    }
}

const REGENERATE_ON_EDIT_DEBOUNCE_INTERVAL: Duration = Duration::from_secs(2);

fn build_blame_entry_sum_tree(entries: Vec<BlameEntry>, max_row: u32) -> SumTree<GitBlameEntry> {
    let mut current_row = 0;
    let mut entries = SumTree::from_iter(
        entries.into_iter().flat_map(|entry| {
            let mut entries = SmallVec::<[GitBlameEntry; 2]>::new();

            if entry.range.start > current_row {
                let skipped_rows = entry.range.start - current_row;
                entries.push(GitBlameEntry {
                    rows: skipped_rows,
                    blame: None,
                });
            }
            entries.push(GitBlameEntry {
                rows: entry.range.len() as u32,
                blame: Some(entry.clone()),
            });

            current_row = entry.range.end;
            entries
        }),
        &(),
    );

    if max_row >= current_row {
        entries.push(
            GitBlameEntry {
                rows: (max_row + 1) - current_row,
                blame: None,
            },
            &(),
        );
    }

    entries
}

async fn parse_commit_messages(
    messages: impl IntoIterator<Item = (Oid, String)>,
    remote_url: Option<String>,
    provider_registry: Arc<GitHostingProviderRegistry>,
) -> HashMap<Oid, ParsedCommitMessage> {
    let mut commit_details = HashMap::default();

    let parsed_remote_url = remote_url
        .as_deref()
        .and_then(|remote_url| parse_git_remote_url(provider_registry, remote_url));

    for (oid, message) in messages {
        let permalink = if let Some((provider, git_remote)) = parsed_remote_url.as_ref() {
            Some(provider.build_commit_permalink(
                git_remote,
                git::BuildCommitPermalinkParams {
                    sha: oid.to_string().as_str(),
                },
            ))
        } else {
            None
        };

        let remote = parsed_remote_url
            .as_ref()
            .map(|(provider, remote)| GitRemote {
                host: provider.clone(),
                owner: remote.owner.to_string(),
                repo: remote.repo.to_string(),
            });

        let pull_request = parsed_remote_url
            .as_ref()
            .and_then(|(provider, remote)| provider.extract_pull_request(remote, &message));

        commit_details.insert(
            oid,
            ParsedCommitMessage {
                message: message.into(),
                permalink,
                remote,
                pull_request,
            },
        );
    }

    commit_details
}

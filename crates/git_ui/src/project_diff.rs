use crate::{
    conflict_view::ConflictAddon,
    git_panel::{GitPanel, GitPanelAddon, GitStatusEntry},
    git_panel_settings::GitPanelSettings,
};
use anyhow::Result;
use buffer_diff::{BufferDiff, DiffHunkSecondaryStatus};
use collections::HashSet;
use editor::{
    Editor, EditorEvent,
    actions::{GoToHunk, GoToPreviousHunk},
    scroll::Autoscroll,
};
use futures::StreamExt;
use git::{
    Commit, StageAll, StageAndNext, ToggleStaged, UnstageAll, UnstageAndNext, repository::Branch,
    status::FileStatus,
};
use gpui::{
    Action, AnyElement, AnyView, App, AppContext as _, AsyncWindowContext, Entity, EventEmitter,
    FocusHandle, Focusable, Render, Subscription, Task, WeakEntity, actions,
};
use language::{Anchor, Buffer, Capability, OffsetRangeExt};
use multi_buffer::{MultiBuffer, PathKey};
use project::{
    Project, ProjectPath,
    git_store::{GitStore, GitStoreEvent, RepositoryEvent},
};
use settings::{Settings, SettingsStore};
use std::any::{Any, TypeId};
use std::ops::Range;
use theme::ActiveTheme;
use ui::{KeyBinding, Tooltip, prelude::*, vertical_divider};
use util::ResultExt as _;
use workspace::{
    CloseActiveItem, ItemNavHistory, SerializableItem, ToolbarItemEvent, ToolbarItemLocation,
    ToolbarItemView, Workspace,
    item::{BreadcrumbText, Item, ItemEvent, ItemHandle, TabContentParams},
    searchable::SearchableItemHandle,
};

actions!(git, [Diff, Add]);

pub struct ProjectDiff {
    project: Entity<Project>,
    multibuffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    git_store: Entity<GitStore>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    update_needed: postage::watch::Sender<()>,
    pending_scroll: Option<PathKey>,
    _task: Task<Result<()>>,
    _subscription: Subscription,
}

#[derive(Debug)]
struct DiffBuffer {
    path_key: PathKey,
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    file_status: FileStatus,
}

const CONFLICT_NAMESPACE: u32 = 1;
const TRACKED_NAMESPACE: u32 = 2;
const NEW_NAMESPACE: u32 = 3;

impl ProjectDiff {
    pub(crate) fn register(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
        workspace.register_action(Self::deploy);
        workspace.register_action(|workspace, _: &Add, window, cx| {
            Self::deploy(workspace, &Diff, window, cx);
        });
        workspace::register_serializable_item::<ProjectDiff>(cx);
    }

    fn deploy(
        workspace: &mut Workspace,
        _: &Diff,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::deploy_at(workspace, None, window, cx)
    }

    pub fn deploy_at(
        workspace: &mut Workspace,
        entry: Option<GitStatusEntry>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        telemetry::event!(
            "Git Diff Opened",
            source = if entry.is_some() {
                "Git Panel"
            } else {
                "Action"
            }
        );
        let project_diff = if let Some(existing) = workspace.item_of_type::<Self>(cx) {
            workspace.activate_item(&existing, true, true, window, cx);
            existing
        } else {
            let workspace_handle = cx.entity();
            let project_diff =
                cx.new(|cx| Self::new(workspace.project().clone(), workspace_handle, window, cx));
            workspace.add_item_to_active_pane(
                Box::new(project_diff.clone()),
                None,
                true,
                window,
                cx,
            );
            project_diff
        };
        if let Some(entry) = entry {
            project_diff.update(cx, |project_diff, cx| {
                project_diff.move_to_entry(entry, window, cx);
            })
        }
    }

    pub fn autoscroll(&self, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.request_autoscroll(Autoscroll::fit(), cx);
        })
    }

    fn new(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let multibuffer = cx.new(|_| MultiBuffer::new(Capability::ReadWrite));

        let editor = cx.new(|cx| {
            let mut diff_display_editor =
                Editor::for_multibuffer(multibuffer.clone(), Some(project.clone()), window, cx);
            diff_display_editor.disable_inline_diagnostics();
            diff_display_editor.set_expand_all_diff_hunks(cx);
            diff_display_editor.register_addon(GitPanelAddon {
                workspace: workspace.downgrade(),
            });
            diff_display_editor
        });
        window.defer(cx, {
            let workspace = workspace.clone();
            let editor = editor.clone();
            move |window, cx| {
                workspace.update(cx, |workspace, cx| {
                    editor.update(cx, |editor, cx| {
                        editor.added_to_workspace(workspace, window, cx);
                    })
                });
            }
        });
        cx.subscribe_in(&editor, window, Self::handle_editor_event)
            .detach();

        let git_store = project.read(cx).git_store().clone();
        let git_store_subscription = cx.subscribe_in(
            &git_store,
            window,
            move |this, _git_store, event, _window, _cx| match event {
                GitStoreEvent::ActiveRepositoryChanged(_)
                | GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::Updated { .. }, true)
                | GitStoreEvent::ConflictsUpdated => {
                    *this.update_needed.borrow_mut() = ();
                }
                _ => {}
            },
        );

        let mut was_sort_by_path = GitPanelSettings::get_global(cx).sort_by_path;
        cx.observe_global::<SettingsStore>(move |this, cx| {
            let is_sort_by_path = GitPanelSettings::get_global(cx).sort_by_path;
            if is_sort_by_path != was_sort_by_path {
                *this.update_needed.borrow_mut() = ();
            }
            was_sort_by_path = is_sort_by_path
        })
        .detach();

        let (mut send, recv) = postage::watch::channel::<()>();
        let worker = window.spawn(cx, {
            let this = cx.weak_entity();
            async |cx| Self::handle_status_updates(this, recv, cx).await
        });
        // Kick off a refresh immediately
        *send.borrow_mut() = ();

        Self {
            project,
            git_store: git_store.clone(),
            workspace: workspace.downgrade(),
            focus_handle,
            editor,
            multibuffer,
            pending_scroll: None,
            update_needed: send,
            _task: worker,
            _subscription: git_store_subscription,
        }
    }

    pub fn move_to_entry(
        &mut self,
        entry: GitStatusEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(git_repo) = self.git_store.read(cx).active_repository() else {
            return;
        };
        let repo = git_repo.read(cx);

        let namespace = if repo.had_conflict_on_last_merge_head_change(&entry.repo_path) {
            CONFLICT_NAMESPACE
        } else if entry.status.is_created() {
            NEW_NAMESPACE
        } else {
            TRACKED_NAMESPACE
        };

        let path_key = PathKey::namespaced(namespace, entry.repo_path.0.clone());

        self.move_to_path(path_key, window, cx)
    }

    pub fn active_path(&self, cx: &App) -> Option<ProjectPath> {
        let editor = self.editor.read(cx);
        let position = editor.selections.newest_anchor().head();
        let multi_buffer = editor.buffer().read(cx);
        let (_, buffer, _) = multi_buffer.excerpt_containing(position, cx)?;

        let file = buffer.read(cx).file()?;
        Some(ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path().clone(),
        })
    }

    fn move_to_path(&mut self, path_key: PathKey, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(position) = self.multibuffer.read(cx).location_for_path(&path_key, cx) {
            self.editor.update(cx, |editor, cx| {
                editor.change_selections(Some(Autoscroll::focused()), window, cx, |s| {
                    s.select_ranges([position..position]);
                })
            });
        } else {
            self.pending_scroll = Some(path_key);
        }
    }

    fn button_states(&self, cx: &App) -> ButtonStates {
        let editor = self.editor.read(cx);
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let prev_next = snapshot.diff_hunks().skip(1).next().is_some();
        let mut selection = true;

        let mut ranges = editor
            .selections
            .disjoint_anchor_ranges()
            .collect::<Vec<_>>();
        if !ranges.iter().any(|range| range.start != range.end) {
            selection = false;
            if let Some((excerpt_id, buffer, range)) = self.editor.read(cx).active_excerpt(cx) {
                ranges = vec![multi_buffer::Anchor::range_in_buffer(
                    excerpt_id,
                    buffer.read(cx).remote_id(),
                    range,
                )];
            } else {
                ranges = Vec::default();
            }
        }
        let mut has_staged_hunks = false;
        let mut has_unstaged_hunks = false;
        for hunk in editor.diff_hunks_in_ranges(&ranges, &snapshot) {
            match hunk.secondary_status {
                DiffHunkSecondaryStatus::HasSecondaryHunk
                | DiffHunkSecondaryStatus::SecondaryHunkAdditionPending => {
                    has_unstaged_hunks = true;
                }
                DiffHunkSecondaryStatus::OverlapsWithSecondaryHunk => {
                    has_staged_hunks = true;
                    has_unstaged_hunks = true;
                }
                DiffHunkSecondaryStatus::NoSecondaryHunk
                | DiffHunkSecondaryStatus::SecondaryHunkRemovalPending => {
                    has_staged_hunks = true;
                }
            }
        }
        let mut stage_all = false;
        let mut unstage_all = false;
        self.workspace
            .read_with(cx, |workspace, cx| {
                if let Some(git_panel) = workspace.panel::<GitPanel>(cx) {
                    let git_panel = git_panel.read(cx);
                    stage_all = git_panel.can_stage_all();
                    unstage_all = git_panel.can_unstage_all();
                }
            })
            .ok();

        return ButtonStates {
            stage: has_unstaged_hunks,
            unstage: has_staged_hunks,
            prev_next,
            selection,
            stage_all,
            unstage_all,
        };
    }

    fn handle_editor_event(
        &mut self,
        editor: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::SelectionsChanged { local: true } => {
                let Some(project_path) = self.active_path(cx) else {
                    return;
                };
                self.workspace
                    .update(cx, |workspace, cx| {
                        if let Some(git_panel) = workspace.panel::<GitPanel>(cx) {
                            git_panel.update(cx, |git_panel, cx| {
                                git_panel.select_entry_by_path(project_path, window, cx)
                            })
                        }
                    })
                    .ok();
            }
            _ => {}
        }
        if editor.focus_handle(cx).contains_focused(window, cx) {
            if self.multibuffer.read(cx).is_empty() {
                self.focus_handle.focus(window)
            }
        }
    }

    fn load_buffers(&mut self, cx: &mut Context<Self>) -> Vec<Task<Result<DiffBuffer>>> {
        let Some(repo) = self.git_store.read(cx).active_repository() else {
            self.multibuffer.update(cx, |multibuffer, cx| {
                multibuffer.clear(cx);
            });
            return vec![];
        };

        let mut previous_paths = self.multibuffer.read(cx).paths().collect::<HashSet<_>>();

        let mut result = vec![];
        repo.update(cx, |repo, cx| {
            for entry in repo.cached_status() {
                if !entry.status.has_changes() {
                    continue;
                }
                let Some(project_path) = repo.repo_path_to_project_path(&entry.repo_path, cx)
                else {
                    continue;
                };
                let namespace = if GitPanelSettings::get_global(cx).sort_by_path {
                    TRACKED_NAMESPACE
                } else if repo.had_conflict_on_last_merge_head_change(&entry.repo_path) {
                    CONFLICT_NAMESPACE
                } else if entry.status.is_created() {
                    NEW_NAMESPACE
                } else {
                    TRACKED_NAMESPACE
                };
                let path_key = PathKey::namespaced(namespace, entry.repo_path.0.clone());

                previous_paths.remove(&path_key);
                let load_buffer = self
                    .project
                    .update(cx, |project, cx| project.open_buffer(project_path, cx));

                let project = self.project.clone();
                result.push(cx.spawn(async move |_, cx| {
                    let buffer = load_buffer.await?;
                    let changes = project
                        .update(cx, |project, cx| {
                            project.open_uncommitted_diff(buffer.clone(), cx)
                        })?
                        .await?;
                    Ok(DiffBuffer {
                        path_key,
                        buffer,
                        diff: changes,
                        file_status: entry.status,
                    })
                }));
            }
        });
        self.multibuffer.update(cx, |multibuffer, cx| {
            for path in previous_paths {
                multibuffer.remove_excerpts_for_path(path, cx);
            }
        });
        result
    }

    fn register_buffer(
        &mut self,
        diff_buffer: DiffBuffer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path_key = diff_buffer.path_key;
        let buffer = diff_buffer.buffer;
        let diff = diff_buffer.diff;

        let conflict_addon = self
            .editor
            .read(cx)
            .addon::<ConflictAddon>()
            .expect("project diff editor should have a conflict addon");

        let snapshot = buffer.read(cx).snapshot();
        let diff = diff.read(cx);
        let diff_hunk_ranges = diff
            .hunks_intersecting_range(Anchor::MIN..Anchor::MAX, &snapshot, cx)
            .map(|diff_hunk| diff_hunk.buffer_range.clone());
        let conflicts = conflict_addon
            .conflict_set(snapshot.remote_id())
            .map(|conflict_set| conflict_set.read(cx).snapshot().conflicts.clone())
            .unwrap_or_default();
        let conflicts = conflicts.iter().map(|conflict| conflict.range.clone());

        let excerpt_ranges = merge_anchor_ranges(diff_hunk_ranges, conflicts, &snapshot)
            .map(|range| range.to_point(&snapshot))
            .collect::<Vec<_>>();

        let (was_empty, is_excerpt_newly_added) = self.multibuffer.update(cx, |multibuffer, cx| {
            let was_empty = multibuffer.is_empty();
            let (_, is_newly_added) = multibuffer.set_excerpts_for_path(
                path_key.clone(),
                buffer,
                excerpt_ranges,
                editor::DEFAULT_MULTIBUFFER_CONTEXT,
                cx,
            );
            (was_empty, is_newly_added)
        });

        self.editor.update(cx, |editor, cx| {
            if was_empty {
                editor.change_selections(None, window, cx, |selections| {
                    // TODO select the very beginning (possibly inside a deletion)
                    selections.select_ranges([0..0])
                });
            }
            if is_excerpt_newly_added && diff_buffer.file_status.is_deleted() {
                editor.fold_buffer(snapshot.text.remote_id(), cx)
            }
        });

        if self.multibuffer.read(cx).is_empty()
            && self
                .editor
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
        {
            self.focus_handle.focus(window);
        } else if self.focus_handle.is_focused(window) && !self.multibuffer.read(cx).is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.focus_handle(cx).focus(window);
            });
        }
        if self.pending_scroll.as_ref() == Some(&path_key) {
            self.move_to_path(path_key, window, cx);
        }
    }

    pub async fn handle_status_updates(
        this: WeakEntity<Self>,
        mut recv: postage::watch::Receiver<()>,
        cx: &mut AsyncWindowContext,
    ) -> Result<()> {
        while let Some(_) = recv.next().await {
            let buffers_to_load = this.update(cx, |this, cx| this.load_buffers(cx))?;
            for buffer_to_load in buffers_to_load {
                if let Some(buffer) = buffer_to_load.await.log_err() {
                    cx.update(|window, cx| {
                        this.update(cx, |this, cx| this.register_buffer(buffer, window, cx))
                            .ok();
                    })?;
                }
            }
            this.update(cx, |this, cx| {
                this.pending_scroll.take();
                cx.notify();
            })?;
        }

        Ok(())
    }
}

impl EventEmitter<EditorEvent> for ProjectDiff {}

impl Focusable for ProjectDiff {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.multibuffer.read(cx).is_empty() {
            self.focus_handle.clone()
        } else {
            self.editor.focus_handle(cx)
        }
    }
}

impl Item for ProjectDiff {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch).color(Color::Muted))
    }

    fn to_item_events(event: &EditorEvent, f: impl FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn navigate(
        &mut self,
        data: Box<dyn Any>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some("Project Diff".into())
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, _: &App) -> AnyElement {
        Label::new("Uncommitted Changes")
            .color(if params.selected {
                Color::Default
            } else {
                Color::Muted
            })
            .into_any_element()
    }

    fn tab_content_text(&self, _detail: usize, _: &App) -> SharedString {
        "Uncommitted Changes".into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Project Diff Opened")
    }

    fn as_searchable(&self, _: &Entity<Self>) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.for_each_project_item(cx, f)
    }

    fn is_singleton(&self, _: &App) -> bool {
        false
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, _| {
            editor.set_nav_history(Some(nav_history));
        });
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Self>>
    where
        Self: Sized,
    {
        let workspace = self.workspace.upgrade()?;
        Some(cx.new(|cx| ProjectDiff::new(self.project.clone(), workspace, window, cx)))
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.multibuffer.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.multibuffer.read(cx).has_conflict(cx)
    }

    fn can_save(&self, _: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        format: bool,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.save(format, project, window, cx)
    }

    fn save_as(
        &mut self,
        _: Entity<Project>,
        _: ProjectPath,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> Task<Result<()>> {
        unreachable!()
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.reload(project, window, cx)
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<AnyView> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.to_any())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.to_any())
        } else {
            None
        }
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(&self, theme: &theme::Theme, cx: &App) -> Option<Vec<BreadcrumbText>> {
        self.editor.breadcrumbs(theme, cx)
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }
}

impl Render for ProjectDiff {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_empty = self.multibuffer.read(cx).is_empty();

        div()
            .track_focus(&self.focus_handle)
            .key_context(if is_empty { "EmptyPane" } else { "GitDiff" })
            .bg(cx.theme().colors().editor_background)
            .flex()
            .items_center()
            .justify_center()
            .size_full()
            .when(is_empty, |el| {
                let keybinding_focus_handle = self.focus_handle(cx).clone();
                el.child(
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .justify_around()
                                .child(Label::new("No uncommitted changes")),
                        )
                        .child(
                            h_flex().justify_around().mt_1().child(
                                Button::new("project-diff-close-button", "Close")
                                    // .style(ButtonStyle::Transparent)
                                    .key_binding(KeyBinding::for_action_in(
                                        &CloseActiveItem::default(),
                                        &keybinding_focus_handle,
                                        window,
                                        cx,
                                    ))
                                    .on_click(move |_, window, cx| {
                                        window.focus(&keybinding_focus_handle);
                                        window.dispatch_action(
                                            Box::new(CloseActiveItem::default()),
                                            cx,
                                        );
                                    }),
                            ),
                        ),
                )
            })
            .when(!is_empty, |el| el.child(self.editor.clone()))
    }
}

impl SerializableItem for ProjectDiff {
    fn serialized_item_kind() -> &'static str {
        "ProjectDiff"
    }

    fn cleanup(
        _: workspace::WorkspaceId,
        _: Vec<workspace::ItemId>,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn deserialize(
        _project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        _workspace_id: workspace::WorkspaceId,
        _item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        window.spawn(cx, async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                let workspace_handle = cx.entity();
                cx.new(|cx| Self::new(workspace.project().clone(), workspace_handle, window, cx))
            })
        })
    }

    fn serialize(
        &mut self,
        _workspace: &mut Workspace,
        _item_id: workspace::ItemId,
        _closing: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        None
    }

    fn should_serialize(&self, _: &Self::Event) -> bool {
        false
    }
}

pub struct ProjectDiffToolbar {
    project_diff: Option<WeakEntity<ProjectDiff>>,
    workspace: WeakEntity<Workspace>,
}

impl ProjectDiffToolbar {
    pub fn new(workspace: &Workspace, _: &mut Context<Self>) -> Self {
        Self {
            project_diff: None,
            workspace: workspace.weak_handle(),
        }
    }

    fn project_diff(&self, _: &App) -> Option<Entity<ProjectDiff>> {
        self.project_diff.as_ref()?.upgrade()
    }

    fn dispatch_action(&self, action: &dyn Action, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(project_diff) = self.project_diff(cx) {
            project_diff.focus_handle(cx).focus(window);
        }
        let action = action.boxed_clone();
        cx.defer(move |cx| {
            cx.dispatch_action(action.as_ref());
        })
    }

    fn stage_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                if let Some(panel) = workspace.panel::<GitPanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        panel.stage_all(&Default::default(), window, cx);
                    });
                }
            })
            .ok();
    }

    fn unstage_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                let Some(panel) = workspace.panel::<GitPanel>(cx) else {
                    return;
                };
                panel.update(cx, |panel, cx| {
                    panel.unstage_all(&Default::default(), window, cx);
                });
            })
            .ok();
    }
}

impl EventEmitter<ToolbarItemEvent> for ProjectDiffToolbar {}

impl ToolbarItemView for ProjectDiffToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.project_diff = active_pane_item
            .and_then(|item| item.act_as::<ProjectDiff>(cx))
            .map(|entity| entity.downgrade());
        if self.project_diff.is_some() {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn pane_focus_update(
        &mut self,
        _pane_focused: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

struct ButtonStates {
    stage: bool,
    unstage: bool,
    prev_next: bool,
    selection: bool,
    stage_all: bool,
    unstage_all: bool,
}

impl Render for ProjectDiffToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(project_diff) = self.project_diff(cx) else {
            return div();
        };
        let focus_handle = project_diff.focus_handle(cx);
        let button_states = project_diff.read(cx).button_states(cx);

        h_group_xl()
            .my_neg_1()
            .py_1()
            .items_center()
            .flex_wrap()
            .justify_between()
            .child(
                h_group_sm()
                    .when(button_states.selection, |el| {
                        el.child(
                            Button::new("stage", "Toggle Staged")
                                .tooltip(Tooltip::for_action_title_in(
                                    "Toggle Staged",
                                    &ToggleStaged,
                                    &focus_handle,
                                ))
                                .disabled(!button_states.stage && !button_states.unstage)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.dispatch_action(&ToggleStaged, window, cx)
                                })),
                        )
                    })
                    .when(!button_states.selection, |el| {
                        el.child(
                            Button::new("stage", "Stage")
                                .tooltip(Tooltip::for_action_title_in(
                                    "Stage and go to next hunk",
                                    &StageAndNext,
                                    &focus_handle,
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.dispatch_action(&StageAndNext, window, cx)
                                })),
                        )
                        .child(
                            Button::new("unstage", "Unstage")
                                .tooltip(Tooltip::for_action_title_in(
                                    "Unstage and go to next hunk",
                                    &UnstageAndNext,
                                    &focus_handle,
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.dispatch_action(&UnstageAndNext, window, cx)
                                })),
                        )
                    }),
            )
            // n.b. the only reason these arrows are here is because we don't
            // support "undo" for staging so we need a way to go back.
            .child(
                h_group_sm()
                    .child(
                        IconButton::new("up", IconName::ArrowUp)
                            .shape(ui::IconButtonShape::Square)
                            .tooltip(Tooltip::for_action_title_in(
                                "Go to previous hunk",
                                &GoToPreviousHunk,
                                &focus_handle,
                            ))
                            .disabled(!button_states.prev_next)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dispatch_action(&GoToPreviousHunk, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new("down", IconName::ArrowDown)
                            .shape(ui::IconButtonShape::Square)
                            .tooltip(Tooltip::for_action_title_in(
                                "Go to next hunk",
                                &GoToHunk,
                                &focus_handle,
                            ))
                            .disabled(!button_states.prev_next)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dispatch_action(&GoToHunk, window, cx)
                            })),
                    ),
            )
            .child(vertical_divider())
            .child(
                h_group_sm()
                    .when(
                        button_states.unstage_all && !button_states.stage_all,
                        |el| {
                            el.child(
                                Button::new("unstage-all", "Unstage All")
                                    .tooltip(Tooltip::for_action_title_in(
                                        "Unstage all changes",
                                        &UnstageAll,
                                        &focus_handle,
                                    ))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.unstage_all(window, cx)
                                    })),
                            )
                        },
                    )
                    .when(
                        !button_states.unstage_all || button_states.stage_all,
                        |el| {
                            el.child(
                                // todo make it so that changing to say "Unstaged"
                                // doesn't change the position.
                                div().child(
                                    Button::new("stage-all", "Stage All")
                                        .disabled(!button_states.stage_all)
                                        .tooltip(Tooltip::for_action_title_in(
                                            "Stage all changes",
                                            &StageAll,
                                            &focus_handle,
                                        ))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.stage_all(window, cx)
                                        })),
                                ),
                            )
                        },
                    )
                    .child(
                        Button::new("commit", "Commit")
                            .tooltip(Tooltip::for_action_title_in(
                                "Commit",
                                &Commit,
                                &focus_handle,
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dispatch_action(&Commit, window, cx);
                            })),
                    ),
            )
    }
}

#[derive(IntoElement, RegisterComponent)]
pub struct ProjectDiffEmptyState {
    pub no_repo: bool,
    pub can_push_and_pull: bool,
    pub focus_handle: Option<FocusHandle>,
    pub current_branch: Option<Branch>,
    // has_pending_commits: bool,
    // ahead_of_remote: bool,
    // no_git_repository: bool,
}

impl RenderOnce for ProjectDiffEmptyState {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .child(v_flex().gap_1().when(self.no_repo, |this| {
                // TODO: add git init
                this.text_center()
                    .child(Label::new("No Repository").color(Color::Muted))
            }))
    }
}

mod preview {
    use ui::prelude::*;

    use super::ProjectDiffEmptyState;

    // View this component preview using `workspace: open component-preview`
    impl Component for ProjectDiffEmptyState {
        fn scope() -> ComponentScope {
            ComponentScope::VersionControl
        }

        fn preview(_window: &mut Window, _cx: &mut App) -> Option<AnyElement> {
            let no_repo_state = ProjectDiffEmptyState {
                no_repo: true,
                can_push_and_pull: false,
                focus_handle: None,
                current_branch: None,
            };

            let (width, height) = (px(480.), px(320.));

            Some(
                v_flex()
                    .gap_6()
                    .children(vec![
                        example_group(vec![single_example(
                            "No Repo",
                            div()
                                .w(width)
                                .h(height)
                                .child(no_repo_state)
                                .into_any_element(),
                        )])
                        .vertical(),
                    ])
                    .into_any_element(),
            )
        }
    }
}

fn merge_anchor_ranges<'a>(
    left: impl 'a + Iterator<Item = Range<Anchor>>,
    right: impl 'a + Iterator<Item = Range<Anchor>>,
    snapshot: &'a language::BufferSnapshot,
) -> impl 'a + Iterator<Item = Range<Anchor>> {
    let mut left = left.fuse().peekable();
    let mut right = right.fuse().peekable();

    std::iter::from_fn(move || {
        let Some(left_range) = left.peek() else {
            return right.next();
        };
        let Some(right_range) = right.peek() else {
            return left.next();
        };

        let mut next_range = if left_range.start.cmp(&right_range.start, snapshot).is_lt() {
            left.next().unwrap()
        } else {
            right.next().unwrap()
        };

        // Extend the basic range while there's overlap with a range from either stream.
        loop {
            if let Some(left_range) = left
                .peek()
                .filter(|range| range.start.cmp(&next_range.end, &snapshot).is_le())
                .cloned()
            {
                left.next();
                next_range.end = left_range.end;
            } else if let Some(right_range) = right
                .peek()
                .filter(|range| range.start.cmp(&next_range.end, &snapshot).is_le())
                .cloned()
            {
                right.next();
                next_range.end = right_range.end;
            } else {
                break;
            }
        }

        Some(next_range)
    })
}

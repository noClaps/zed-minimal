use crate::{
    CollaboratorId, DelayedDebouncedEditAction, FollowableViewRegistry, ItemNavHistory,
    SerializableItemRegistry, ToolbarItemLocation, ViewId, Workspace, WorkspaceId,
    pane::{self, Pane},
    persistence::model::ItemId,
    searchable::SearchableItemHandle,
    workspace_settings::{AutosaveSetting, WorkspaceSettings},
};
use anyhow::Result;
use client::{Client, proto};
use futures::channel::mpsc;
use gpui::{
    Action, AnyElement, AnyView, App, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, Font, HighlightStyle, Pixels, Point, Render, SharedString, Task, WeakEntity, Window,
};
use project::{Project, ProjectEntryId, ProjectPath};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsLocation, SettingsSources};
use smallvec::SmallVec;
use std::{
    any::{Any, TypeId},
    cell::RefCell,
    ops::Range,
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use theme::Theme;
use ui::{Color, Icon, IntoElement, Label, LabelCommon};
use util::ResultExt;

pub const LEADER_UPDATE_THROTTLE: Duration = Duration::from_millis(200);

#[derive(Deserialize)]
pub struct ItemSettings {
    pub git_status: bool,
    pub close_position: ClosePosition,
    pub activate_on_close: ActivateOnClose,
    pub file_icons: bool,
    pub show_diagnostics: ShowDiagnostics,
    pub show_close_button: ShowCloseButton,
}

#[derive(Deserialize)]
pub struct PreviewTabsSettings {
    pub enabled: bool,
    pub enable_preview_from_file_finder: bool,
    pub enable_preview_from_code_navigation: bool,
}

#[derive(Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ClosePosition {
    Left,
    #[default]
    Right,
}

#[derive(Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ShowCloseButton {
    Always,
    #[default]
    Hover,
    Hidden,
}

#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShowDiagnostics {
    #[default]
    Off,
    Errors,
    All,
}

#[derive(Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActivateOnClose {
    #[default]
    History,
    Neighbour,
    LeftNeighbour,
}

#[derive(Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ItemSettingsContent {
    /// Whether to show the Git file status on a tab item.
    ///
    /// Default: false
    git_status: Option<bool>,
    /// Position of the close button in a tab.
    ///
    /// Default: right
    close_position: Option<ClosePosition>,
    /// Whether to show the file icon for a tab.
    ///
    /// Default: false
    file_icons: Option<bool>,
    /// What to do after closing the current tab.
    ///
    /// Default: history
    pub activate_on_close: Option<ActivateOnClose>,
    /// Which files containing diagnostic errors/warnings to mark in the tabs.
    /// This setting can take the following three values:
    ///
    /// Default: off
    show_diagnostics: Option<ShowDiagnostics>,
    /// Whether to always show the close button on tabs.
    ///
    /// Default: false
    show_close_button: Option<ShowCloseButton>,
}

#[derive(Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct PreviewTabsSettingsContent {
    /// Whether to show opened editors as preview tabs.
    /// Preview tabs do not stay open, are reused until explicitly set to be kept open opened (via double-click or editing) and show file names in italic.
    ///
    /// Default: true
    enabled: Option<bool>,
    /// Whether to open tabs in preview mode when selected from the file finder.
    ///
    /// Default: false
    enable_preview_from_file_finder: Option<bool>,
    /// Whether a preview tab gets replaced when code navigation is used to navigate away from the tab.
    ///
    /// Default: false
    enable_preview_from_code_navigation: Option<bool>,
}

impl Settings for ItemSettings {
    const KEY: Option<&'static str> = Some("tabs");

    type FileContent = ItemSettingsContent;

    fn load(sources: SettingsSources<Self::FileContent>, _: &mut App) -> Result<Self> {
        sources.json_merge()
    }
}

impl Settings for PreviewTabsSettings {
    const KEY: Option<&'static str> = Some("preview_tabs");

    type FileContent = PreviewTabsSettingsContent;

    fn load(sources: SettingsSources<Self::FileContent>, _: &mut App) -> Result<Self> {
        sources.json_merge()
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
pub enum ItemEvent {
    CloseItem,
    UpdateTab,
    UpdateBreadcrumbs,
    Edit,
}

// TODO: Combine this with existing HighlightedText struct?
pub struct BreadcrumbText {
    pub text: String,
    pub highlights: Option<Vec<(Range<usize>, HighlightStyle)>>,
    pub font: Option<Font>,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct TabContentParams {
    pub detail: Option<usize>,
    pub selected: bool,
    pub preview: bool,
    /// Tab content should be deemphasized when active pane does not have focus.
    pub deemphasized: bool,
}

impl TabContentParams {
    /// Returns the text color to be used for the tab content.
    pub fn text_color(&self) -> Color {
        if self.deemphasized {
            if self.selected {
                Color::Muted
            } else {
                Color::Hidden
            }
        } else if self.selected {
            Color::Default
        } else {
            Color::Muted
        }
    }
}

pub enum TabTooltipContent {
    Text(SharedString),
    Custom(Box<dyn Fn(&mut Window, &mut App) -> AnyView>),
}

pub trait Item: Focusable + EventEmitter<Self::Event> + Render + Sized {
    type Event;

    /// Returns the tab contents.
    ///
    /// By default this returns a [`Label`] that displays that text from
    /// `tab_content_text`.
    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let text = self.tab_content_text(params.detail.unwrap_or_default(), cx);

        Label::new(text)
            .color(params.text_color())
            .into_any_element()
    }

    /// Returns the textual contents of the tab.
    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        None
    }

    /// Returns the tab tooltip text.
    ///
    /// Use this if you don't need to customize the tab tooltip content.
    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        None
    }

    /// Returns the tab tooltip content.
    ///
    /// By default this returns a Tooltip text from
    /// `tab_tooltip_text`.
    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        self.tab_tooltip_text(cx).map(TabTooltipContent::Text)
    }

    fn to_item_events(_event: &Self::Event, _f: impl FnMut(ItemEvent)) {}

    fn deactivated(&mut self, _window: &mut Window, _: &mut Context<Self>) {}
    fn discarded(&self, _project: Entity<Project>, _window: &mut Window, _cx: &mut Context<Self>) {}
    fn workspace_deactivated(&mut self, _window: &mut Window, _: &mut Context<Self>) {}
    fn navigate(&mut self, _: Box<dyn Any>, _window: &mut Window, _: &mut Context<Self>) -> bool {
        false
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    /// (model id, Item)
    fn for_each_project_item(
        &self,
        _: &App,
        _: &mut dyn FnMut(EntityId, &dyn project::ProjectItem),
    ) {
    }
    fn is_singleton(&self, _cx: &App) -> bool {
        false
    }
    fn set_nav_history(&mut self, _: ItemNavHistory, _window: &mut Window, _: &mut Context<Self>) {}
    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Entity<Self>>
    where
        Self: Sized,
    {
        None
    }
    fn is_dirty(&self, _: &App) -> bool {
        false
    }
    fn has_deleted_file(&self, _: &App) -> bool {
        false
    }
    fn has_conflict(&self, _: &App) -> bool {
        false
    }
    fn can_save(&self, _cx: &App) -> bool {
        false
    }
    fn can_save_as(&self, _: &App) -> bool {
        false
    }
    fn save(
        &mut self,
        _format: bool,
        _project: Entity<Project>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        unimplemented!("save() must be implemented if can_save() returns true")
    }
    fn save_as(
        &mut self,
        _project: Entity<Project>,
        _path: ProjectPath,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        unimplemented!("save_as() must be implemented if can_save() returns true")
    }
    fn reload(
        &mut self,
        _project: Entity<Project>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        unimplemented!("reload() must be implemented if can_save() returns true")
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<AnyView> {
        if TypeId::of::<Self>() == type_id {
            Some(self_handle.clone().into())
        } else {
            None
        }
    }

    fn as_searchable(&self, _: &Entity<Self>) -> Option<Box<dyn SearchableItemHandle>> {
        None
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::Hidden
    }

    fn breadcrumbs(&self, _theme: &Theme, _cx: &App) -> Option<Vec<BreadcrumbText>> {
        None
    }

    fn added_to_workspace(
        &mut self,
        _workspace: &mut Workspace,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn show_toolbar(&self) -> bool {
        true
    }

    fn pixel_position_of_cursor(&self, _: &App) -> Option<Point<Pixels>> {
        None
    }

    fn preserve_preview(&self, _cx: &App) -> bool {
        false
    }

    fn include_in_nav_history() -> bool {
        true
    }
}

pub trait SerializableItem: Item {
    fn serialized_item_kind() -> &'static str;

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>>;

    fn deserialize(
        _project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        _workspace_id: WorkspaceId,
        _item_id: ItemId,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<Entity<Self>>>;

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        closing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>>;

    fn should_serialize(&self, event: &Self::Event) -> bool;
}

pub trait SerializableItemHandle: ItemHandle {
    fn serialized_item_kind(&self) -> &'static str;
    fn serialize(
        &self,
        workspace: &mut Workspace,
        closing: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Task<Result<()>>>;
    fn should_serialize(&self, event: &dyn Any, cx: &App) -> bool;
}

impl<T> SerializableItemHandle for Entity<T>
where
    T: SerializableItem,
{
    fn serialized_item_kind(&self) -> &'static str {
        T::serialized_item_kind()
    }

    fn serialize(
        &self,
        workspace: &mut Workspace,
        closing: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Task<Result<()>>> {
        self.update(cx, |this, cx| {
            this.serialize(workspace, cx.entity_id().as_u64(), closing, window, cx)
        })
    }

    fn should_serialize(&self, event: &dyn Any, cx: &App) -> bool {
        event
            .downcast_ref::<T::Event>()
            .map_or(false, |event| self.read(cx).should_serialize(event))
    }
}

pub trait ItemHandle: 'static + Send {
    fn item_focus_handle(&self, cx: &App) -> FocusHandle;
    fn subscribe_to_item_events(
        &self,
        window: &mut Window,
        cx: &mut App,
        handler: Box<dyn Fn(ItemEvent, &mut Window, &mut App)>,
    ) -> gpui::Subscription;
    fn tab_content(&self, params: TabContentParams, window: &Window, cx: &App) -> AnyElement;
    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString;
    fn tab_icon(&self, window: &Window, cx: &App) -> Option<Icon>;
    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString>;
    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent>;
    fn telemetry_event_text(&self, cx: &App) -> Option<&'static str>;
    fn dragged_tab_content(
        &self,
        params: TabContentParams,
        window: &Window,
        cx: &App,
    ) -> AnyElement;
    fn project_path(&self, cx: &App) -> Option<ProjectPath>;
    fn project_entry_ids(&self, cx: &App) -> SmallVec<[ProjectEntryId; 3]>;
    fn project_paths(&self, cx: &App) -> SmallVec<[ProjectPath; 3]>;
    fn project_item_model_ids(&self, cx: &App) -> SmallVec<[EntityId; 3]>;
    fn for_each_project_item(
        &self,
        _: &App,
        _: &mut dyn FnMut(EntityId, &dyn project::ProjectItem),
    );
    fn is_singleton(&self, cx: &App) -> bool;
    fn boxed_clone(&self) -> Box<dyn ItemHandle>;
    fn clone_on_split(
        &self,
        workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Box<dyn ItemHandle>>;
    fn added_to_pane(
        &self,
        workspace: &mut Workspace,
        pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    );
    fn deactivated(&self, window: &mut Window, cx: &mut App);
    fn discarded(&self, project: Entity<Project>, window: &mut Window, cx: &mut App);
    fn workspace_deactivated(&self, window: &mut Window, cx: &mut App);
    fn navigate(&self, data: Box<dyn Any>, window: &mut Window, cx: &mut App) -> bool;
    fn item_id(&self) -> EntityId;
    fn to_any(&self) -> AnyView;
    fn is_dirty(&self, cx: &App) -> bool;
    fn has_deleted_file(&self, cx: &App) -> bool;
    fn has_conflict(&self, cx: &App) -> bool;
    fn can_save(&self, cx: &App) -> bool;
    fn can_save_as(&self, cx: &App) -> bool;
    fn save(
        &self,
        format: bool,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>>;
    fn save_as(
        &self,
        project: Entity<Project>,
        path: ProjectPath,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>>;
    fn reload(
        &self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>>;
    fn act_as_type(&self, type_id: TypeId, cx: &App) -> Option<AnyView>;
    fn to_followable_item_handle(&self, cx: &App) -> Option<Box<dyn FollowableItemHandle>>;
    fn to_serializable_item_handle(&self, cx: &App) -> Option<Box<dyn SerializableItemHandle>>;
    fn on_release(
        &self,
        cx: &mut App,
        callback: Box<dyn FnOnce(&mut App) + Send>,
    ) -> gpui::Subscription;
    fn to_searchable_item_handle(&self, cx: &App) -> Option<Box<dyn SearchableItemHandle>>;
    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation;
    fn breadcrumbs(&self, theme: &Theme, cx: &App) -> Option<Vec<BreadcrumbText>>;
    fn show_toolbar(&self, cx: &App) -> bool;
    fn pixel_position_of_cursor(&self, cx: &App) -> Option<Point<Pixels>>;
    fn downgrade_item(&self) -> Box<dyn WeakItemHandle>;
    fn workspace_settings<'a>(&self, cx: &'a App) -> &'a WorkspaceSettings;
    fn preserve_preview(&self, cx: &App) -> bool;
    fn include_in_nav_history(&self) -> bool;
    fn relay_action(&self, action: Box<dyn Action>, window: &mut Window, cx: &mut App);
    fn can_autosave(&self, cx: &App) -> bool {
        let is_deleted = self.project_entry_ids(cx).is_empty();
        self.is_dirty(cx) && !self.has_conflict(cx) && self.can_save(cx) && !is_deleted
    }
}

pub trait WeakItemHandle: Send + Sync {
    fn id(&self) -> EntityId;
    fn boxed_clone(&self) -> Box<dyn WeakItemHandle>;
    fn upgrade(&self) -> Option<Box<dyn ItemHandle>>;
}

impl dyn ItemHandle {
    pub fn downcast<V: 'static>(&self) -> Option<Entity<V>> {
        self.to_any().downcast().ok()
    }

    pub fn act_as<V: 'static>(&self, cx: &App) -> Option<Entity<V>> {
        self.act_as_type(TypeId::of::<V>(), cx)
            .and_then(|t| t.downcast().ok())
    }
}

impl<T: Item> ItemHandle for Entity<T> {
    fn subscribe_to_item_events(
        &self,
        window: &mut Window,
        cx: &mut App,
        handler: Box<dyn Fn(ItemEvent, &mut Window, &mut App)>,
    ) -> gpui::Subscription {
        window.subscribe(self, cx, move |_, event, window, cx| {
            T::to_item_events(event, |item_event| handler(item_event, window, cx));
        })
    }

    fn item_focus_handle(&self, cx: &App) -> FocusHandle {
        self.read(cx).focus_handle(cx)
    }

    fn telemetry_event_text(&self, cx: &App) -> Option<&'static str> {
        self.read(cx).telemetry_event_text()
    }

    fn tab_content(&self, params: TabContentParams, window: &Window, cx: &App) -> AnyElement {
        self.read(cx).tab_content(params, window, cx)
    }
    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString {
        self.read(cx).tab_content_text(detail, cx)
    }

    fn tab_icon(&self, window: &Window, cx: &App) -> Option<Icon> {
        self.read(cx).tab_icon(window, cx)
    }

    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        self.read(cx).tab_tooltip_content(cx)
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        self.read(cx).tab_tooltip_text(cx)
    }

    fn dragged_tab_content(
        &self,
        params: TabContentParams,
        window: &Window,
        cx: &App,
    ) -> AnyElement {
        self.read(cx).tab_content(
            TabContentParams {
                selected: true,
                ..params
            },
            window,
            cx,
        )
    }

    fn project_path(&self, cx: &App) -> Option<ProjectPath> {
        let this = self.read(cx);
        let mut result = None;
        if this.is_singleton(cx) {
            this.for_each_project_item(cx, &mut |_, item| {
                result = item.project_path(cx);
            });
        }
        result
    }

    fn workspace_settings<'a>(&self, cx: &'a App) -> &'a WorkspaceSettings {
        if let Some(project_path) = self.project_path(cx) {
            WorkspaceSettings::get(
                Some(SettingsLocation {
                    worktree_id: project_path.worktree_id,
                    path: &project_path.path,
                }),
                cx,
            )
        } else {
            WorkspaceSettings::get_global(cx)
        }
    }

    fn project_entry_ids(&self, cx: &App) -> SmallVec<[ProjectEntryId; 3]> {
        let mut result = SmallVec::new();
        self.read(cx).for_each_project_item(cx, &mut |_, item| {
            if let Some(id) = item.entry_id(cx) {
                result.push(id);
            }
        });
        result
    }

    fn project_paths(&self, cx: &App) -> SmallVec<[ProjectPath; 3]> {
        let mut result = SmallVec::new();
        self.read(cx).for_each_project_item(cx, &mut |_, item| {
            if let Some(id) = item.project_path(cx) {
                result.push(id);
            }
        });
        result
    }

    fn project_item_model_ids(&self, cx: &App) -> SmallVec<[EntityId; 3]> {
        let mut result = SmallVec::new();
        self.read(cx).for_each_project_item(cx, &mut |id, _| {
            result.push(id);
        });
        result
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(EntityId, &dyn project::ProjectItem),
    ) {
        self.read(cx).for_each_project_item(cx, f)
    }

    fn is_singleton(&self, cx: &App) -> bool {
        self.read(cx).is_singleton(cx)
    }

    fn boxed_clone(&self) -> Box<dyn ItemHandle> {
        Box::new(self.clone())
    }

    fn clone_on_split(
        &self,
        workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Box<dyn ItemHandle>> {
        self.update(cx, |item, cx| item.clone_on_split(workspace_id, window, cx))
            .map(|handle| Box::new(handle) as Box<dyn ItemHandle>)
    }

    fn added_to_pane(
        &self,
        workspace: &mut Workspace,
        pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let weak_item = self.downgrade();
        let history = pane.read(cx).nav_history_for_item(self);
        self.update(cx, |this, cx| {
            this.set_nav_history(history, window, cx);
            this.added_to_workspace(workspace, window, cx);
        });

        if let Some(serializable_item) = self.to_serializable_item_handle(cx) {
            workspace
                .enqueue_item_serialization(serializable_item)
                .log_err();
        }

        if workspace
            .panes_by_item
            .insert(self.item_id(), pane.downgrade())
            .is_none()
        {
            let mut pending_autosave = DelayedDebouncedEditAction::new();
            let (pending_update_tx, _) = mpsc::unbounded();
            let pending_update = Rc::new(RefCell::new(None));

            let mut event_subscription = Some(cx.subscribe_in(
                self,
                window,
                move |workspace, item: &Entity<T>, event, window, cx| {
                    let pane = if let Some(pane) = workspace
                        .panes_by_item
                        .get(&item.item_id())
                        .and_then(|pane| pane.upgrade())
                    {
                        pane
                    } else {
                        return;
                    };

                    if let Some(item) = item.to_followable_item_handle(cx) {
                        let leader_id = workspace.leader_for_pane(&pane);

                        if let Some(leader_id) = leader_id {
                            if let Some(FollowEvent::Unfollow) = item.to_follow_event(event) {
                                workspace.unfollow(leader_id, window, cx);
                            }
                        }

                        if item.item_focus_handle(cx).contains_focused(window, cx) {
                            match leader_id {
                                Some(CollaboratorId::PeerId(leader_peer_id)) => {
                                    item.add_event_to_update_proto(
                                        event,
                                        &mut pending_update.borrow_mut(),
                                        window,
                                        cx,
                                    );
                                    pending_update_tx.unbounded_send(Some(leader_peer_id)).ok();
                                }
                                None => {
                                    item.add_event_to_update_proto(
                                        event,
                                        &mut pending_update.borrow_mut(),
                                        window,
                                        cx,
                                    );
                                    pending_update_tx.unbounded_send(None).ok();
                                }
                            }
                        }
                    }

                    if let Some(item) = item.to_serializable_item_handle(cx) {
                        if item.should_serialize(event, cx) {
                            workspace.enqueue_item_serialization(item).ok();
                        }
                    }

                    T::to_item_events(event, |event| match event {
                        ItemEvent::CloseItem => {
                            pane.update(cx, |pane, cx| {
                                pane.close_item_by_id(
                                    item.item_id(),
                                    crate::SaveIntent::Close,
                                    window,
                                    cx,
                                )
                            })
                            .detach_and_log_err(cx);
                        }

                        ItemEvent::UpdateTab => {
                            workspace.update_item_dirty_state(item, window, cx);

                            if item.has_deleted_file(cx)
                                && !item.is_dirty(cx)
                                && item.workspace_settings(cx).close_on_file_delete
                            {
                                let item_id = item.item_id();
                                let close_item_task = pane.update(cx, |pane, cx| {
                                    pane.close_item_by_id(
                                        item_id,
                                        crate::SaveIntent::Close,
                                        window,
                                        cx,
                                    )
                                });
                                cx.spawn_in(window, {
                                    let pane = pane.clone();
                                    async move |_workspace, cx| {
                                        close_item_task.await?;
                                        pane.update(cx, |pane, _cx| {
                                            pane.nav_history_mut().remove_item(item_id);
                                        })
                                    }
                                })
                                .detach_and_log_err(cx);
                            } else {
                                pane.update(cx, |_, cx| {
                                    cx.emit(pane::Event::ChangeItemTitle);
                                    cx.notify();
                                });
                            }
                        }

                        ItemEvent::Edit => {
                            let autosave = item.workspace_settings(cx).autosave;

                            if let AutosaveSetting::AfterDelay { milliseconds } = autosave {
                                let delay = Duration::from_millis(milliseconds);
                                let item = item.clone();
                                pending_autosave.fire_new(
                                    delay,
                                    window,
                                    cx,
                                    move |workspace, window, cx| {
                                        Pane::autosave_item(
                                            &item,
                                            workspace.project().clone(),
                                            window,
                                            cx,
                                        )
                                    },
                                );
                            }
                            pane.update(cx, |pane, cx| pane.handle_item_edit(item.item_id(), cx));
                        }

                        _ => {}
                    });
                },
            ));

            cx.on_blur(
                &self.read(cx).focus_handle(cx),
                window,
                move |workspace, window, cx| {
                    if let Some(item) = weak_item.upgrade() {
                        if item.workspace_settings(cx).autosave == AutosaveSetting::OnFocusChange {
                            Pane::autosave_item(&item, workspace.project.clone(), window, cx)
                                .detach_and_log_err(cx);
                        }
                    }
                },
            )
            .detach();

            let item_id = self.item_id();
            workspace.update_item_dirty_state(self, window, cx);
            cx.observe_release_in(self, window, move |workspace, _, _, _| {
                workspace.panes_by_item.remove(&item_id);
                event_subscription.take();
            })
            .detach();
        }

        cx.defer_in(window, |workspace, window, cx| {
            workspace.serialize_workspace(window, cx);
        });
    }

    fn discarded(&self, project: Entity<Project>, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| this.discarded(project, window, cx));
    }

    fn deactivated(&self, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| this.deactivated(window, cx));
    }

    fn workspace_deactivated(&self, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| this.workspace_deactivated(window, cx));
    }

    fn navigate(&self, data: Box<dyn Any>, window: &mut Window, cx: &mut App) -> bool {
        self.update(cx, |this, cx| this.navigate(data, window, cx))
    }

    fn item_id(&self) -> EntityId {
        self.entity_id()
    }

    fn to_any(&self) -> AnyView {
        self.clone().into()
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.read(cx).is_dirty(cx)
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.read(cx).has_deleted_file(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.read(cx).has_conflict(cx)
    }

    fn can_save(&self, cx: &App) -> bool {
        self.read(cx).can_save(cx)
    }

    fn can_save_as(&self, cx: &App) -> bool {
        self.read(cx).can_save_as(cx)
    }

    fn save(
        &self,
        format: bool,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        self.update(cx, |item, cx| item.save(format, project, window, cx))
    }

    fn save_as(
        &self,
        project: Entity<Project>,
        path: ProjectPath,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        self.update(cx, |item, cx| item.save_as(project, path, window, cx))
    }

    fn reload(
        &self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        self.update(cx, |item, cx| item.reload(project, window, cx))
    }

    fn act_as_type<'a>(&'a self, type_id: TypeId, cx: &'a App) -> Option<AnyView> {
        self.read(cx).act_as_type(type_id, self, cx)
    }

    fn to_followable_item_handle(&self, cx: &App) -> Option<Box<dyn FollowableItemHandle>> {
        FollowableViewRegistry::to_followable_view(self.clone(), cx)
    }

    fn on_release(
        &self,
        cx: &mut App,
        callback: Box<dyn FnOnce(&mut App) + Send>,
    ) -> gpui::Subscription {
        cx.observe_release(self, move |_, cx| callback(cx))
    }

    fn to_searchable_item_handle(&self, cx: &App) -> Option<Box<dyn SearchableItemHandle>> {
        self.read(cx).as_searchable(self)
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        self.read(cx).breadcrumb_location(cx)
    }

    fn breadcrumbs(&self, theme: &Theme, cx: &App) -> Option<Vec<BreadcrumbText>> {
        self.read(cx).breadcrumbs(theme, cx)
    }

    fn show_toolbar(&self, cx: &App) -> bool {
        self.read(cx).show_toolbar()
    }

    fn pixel_position_of_cursor(&self, cx: &App) -> Option<Point<Pixels>> {
        self.read(cx).pixel_position_of_cursor(cx)
    }

    fn downgrade_item(&self) -> Box<dyn WeakItemHandle> {
        Box::new(self.downgrade())
    }

    fn to_serializable_item_handle(&self, cx: &App) -> Option<Box<dyn SerializableItemHandle>> {
        SerializableItemRegistry::view_to_serializable_item_handle(self.to_any(), cx)
    }

    fn preserve_preview(&self, cx: &App) -> bool {
        self.read(cx).preserve_preview(cx)
    }

    fn include_in_nav_history(&self) -> bool {
        T::include_in_nav_history()
    }

    fn relay_action(&self, action: Box<dyn Action>, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| {
            this.focus_handle(cx).focus(window);
            window.dispatch_action(action, cx);
        })
    }
}

impl From<Box<dyn ItemHandle>> for AnyView {
    fn from(val: Box<dyn ItemHandle>) -> Self {
        val.to_any()
    }
}

impl From<&Box<dyn ItemHandle>> for AnyView {
    fn from(val: &Box<dyn ItemHandle>) -> Self {
        val.to_any()
    }
}

impl Clone for Box<dyn ItemHandle> {
    fn clone(&self) -> Box<dyn ItemHandle> {
        self.boxed_clone()
    }
}

impl<T: Item> WeakItemHandle for WeakEntity<T> {
    fn id(&self) -> EntityId {
        self.entity_id()
    }

    fn boxed_clone(&self) -> Box<dyn WeakItemHandle> {
        Box::new(self.clone())
    }

    fn upgrade(&self) -> Option<Box<dyn ItemHandle>> {
        self.upgrade().map(|v| Box::new(v) as Box<dyn ItemHandle>)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectItemKind(pub &'static str);

pub trait ProjectItem: Item {
    type Item: project::ProjectItem;

    fn project_item_kind() -> Option<ProjectItemKind> {
        None
    }

    fn for_project_item(
        project: Entity<Project>,
        pane: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self
    where
        Self: Sized;
}

#[derive(Debug)]
pub enum FollowEvent {
    Unfollow,
}

pub enum Dedup {
    KeepExisting,
    ReplaceExisting,
}

pub trait FollowableItem: Item {
    fn remote_id(&self) -> Option<ViewId>;
    fn to_state_proto(&self, window: &Window, cx: &App) -> Option<proto::view::Variant>;
    fn from_state_proto(
        project: Entity<Workspace>,
        id: ViewId,
        state: &mut Option<proto::view::Variant>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>>;
    fn to_follow_event(event: &Self::Event) -> Option<FollowEvent>;
    fn add_event_to_update_proto(
        &self,
        event: &Self::Event,
        update: &mut Option<proto::update_view::Variant>,
        window: &Window,
        cx: &App,
    ) -> bool;
    fn apply_update_proto(
        &mut self,
        project: &Entity<Project>,
        message: proto::update_view::Variant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>>;
    fn is_project_item(&self, window: &Window, cx: &App) -> bool;
    fn set_leader_id(
        &mut self,
        leader_peer_id: Option<CollaboratorId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    );
    fn dedup(&self, existing: &Self, window: &Window, cx: &App) -> Option<Dedup>;
}

pub trait FollowableItemHandle: ItemHandle {
    fn remote_id(&self, client: &Arc<Client>, window: &mut Window, cx: &mut App) -> Option<ViewId>;
    fn downgrade(&self) -> Box<dyn WeakFollowableItemHandle>;
    fn set_leader_id(
        &self,
        leader_peer_id: Option<CollaboratorId>,
        window: &mut Window,
        cx: &mut App,
    );
    fn to_state_proto(&self, window: &mut Window, cx: &mut App) -> Option<proto::view::Variant>;
    fn add_event_to_update_proto(
        &self,
        event: &dyn Any,
        update: &mut Option<proto::update_view::Variant>,
        window: &mut Window,
        cx: &mut App,
    ) -> bool;
    fn to_follow_event(&self, event: &dyn Any) -> Option<FollowEvent>;
    fn apply_update_proto(
        &self,
        project: &Entity<Project>,
        message: proto::update_view::Variant,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>>;
    fn is_project_item(&self, window: &mut Window, cx: &mut App) -> bool;
    fn dedup(
        &self,
        existing: &dyn FollowableItemHandle,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Dedup>;
}

impl<T: FollowableItem> FollowableItemHandle for Entity<T> {
    fn remote_id(&self, client: &Arc<Client>, _: &mut Window, cx: &mut App) -> Option<ViewId> {
        self.read(cx).remote_id().or_else(|| {
            client.peer_id().map(|creator| ViewId {
                creator: CollaboratorId::PeerId(creator),
                id: self.item_id().as_u64(),
            })
        })
    }

    fn downgrade(&self) -> Box<dyn WeakFollowableItemHandle> {
        Box::new(self.downgrade())
    }

    fn set_leader_id(&self, leader_id: Option<CollaboratorId>, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| this.set_leader_id(leader_id, window, cx))
    }

    fn to_state_proto(&self, window: &mut Window, cx: &mut App) -> Option<proto::view::Variant> {
        self.read(cx).to_state_proto(window, cx)
    }

    fn add_event_to_update_proto(
        &self,
        event: &dyn Any,
        update: &mut Option<proto::update_view::Variant>,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        if let Some(event) = event.downcast_ref() {
            self.read(cx)
                .add_event_to_update_proto(event, update, window, cx)
        } else {
            false
        }
    }

    fn to_follow_event(&self, event: &dyn Any) -> Option<FollowEvent> {
        T::to_follow_event(event.downcast_ref()?)
    }

    fn apply_update_proto(
        &self,
        project: &Entity<Project>,
        message: proto::update_view::Variant,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        self.update(cx, |this, cx| {
            this.apply_update_proto(project, message, window, cx)
        })
    }

    fn is_project_item(&self, window: &mut Window, cx: &mut App) -> bool {
        self.read(cx).is_project_item(window, cx)
    }

    fn dedup(
        &self,
        existing: &dyn FollowableItemHandle,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Dedup> {
        let existing = existing.to_any().downcast::<T>().ok()?;
        self.read(cx).dedup(existing.read(cx), window, cx)
    }
}

pub trait WeakFollowableItemHandle: Send + Sync {
    fn upgrade(&self) -> Option<Box<dyn FollowableItemHandle>>;
}

impl<T: FollowableItem> WeakFollowableItemHandle for WeakEntity<T> {
    fn upgrade(&self) -> Option<Box<dyn FollowableItemHandle>> {
        Some(Box::new(self.upgrade()?))
    }
}

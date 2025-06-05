mod outline_panel_settings;

use std::{
    cmp,
    collections::BTreeMap,
    hash::Hash,
    ops::Range,
    path::{MAIN_SEPARATOR_STR, Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{self, AtomicBool},
    },
    time::Duration,
    u32,
};

use anyhow::Context as _;
use collections::{BTreeSet, HashMap, HashSet, hash_map};
use db::kvp::KEY_VALUE_STORE;
use editor::{
    AnchorRangeExt, Bias, DisplayPoint, Editor, EditorEvent, EditorSettings, ExcerptId,
    ExcerptRange, MultiBufferSnapshot, RangeToAnchorExt, ShowScrollbar,
    display_map::ToDisplayPoint,
    items::{entry_git_aware_label_color, entry_label_color},
    scroll::{Autoscroll, AutoscrollStrategy, ScrollAnchor, ScrollbarAutoHide},
};
use file_icons::FileIcons;
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    Action, AnyElement, App, AppContext as _, AsyncWindowContext, Bounds, ClipboardItem, Context,
    DismissEvent, Div, ElementId, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle,
    InteractiveElement, IntoElement, KeyContext, ListHorizontalSizingBehavior, ListSizingBehavior,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Point, Render, ScrollStrategy,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled, Subscription, Task,
    UniformListScrollHandle, WeakEntity, Window, actions, anchored, deferred, div, point, px, size,
    uniform_list,
};
use itertools::Itertools;
use language::{BufferId, BufferSnapshot, OffsetRangeExt, OutlineItem};
use menu::{Cancel, SelectFirst, SelectLast, SelectNext, SelectPrevious};

use outline_panel_settings::{OutlinePanelDockPosition, OutlinePanelSettings, ShowIndentGuides};
use project::{File, Fs, GitEntry, GitTraversal, Project, ProjectItem};
use search::{BufferSearchBar, ProjectSearchView};
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsStore};
use smol::channel;
use theme::{SyntaxTheme, ThemeSettings};
use ui::{DynamicSpacing, IndentGuideColors, IndentGuideLayout};
use util::{RangeExt, ResultExt, TryFutureExt};
use workspace::{
    OpenInTerminal, WeakItemHandle, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    item::ItemHandle,
    searchable::{SearchEvent, SearchableItem},
    ui::{
        ActiveTheme, ButtonCommon, Clickable, Color, ContextMenu, FluentBuilder, HighlightedLabel,
        Icon, IconButton, IconButtonShape, IconName, IconSize, Label, LabelCommon, ListItem,
        Scrollbar, ScrollbarState, StyledExt, StyledTypography, Toggleable, Tooltip, h_flex,
        v_flex,
    },
};
use worktree::{Entry, ProjectEntryId, WorktreeId};

actions!(
    outline_panel,
    [
        CollapseAllEntries,
        CollapseSelectedEntry,
        ExpandAllEntries,
        ExpandSelectedEntry,
        FoldDirectory,
        OpenSelectedEntry,
        RevealInFileManager,
        SelectParent,
        ToggleActiveEditorPin,
        ToggleFocus,
        UnfoldDirectory,
    ]
);

const OUTLINE_PANEL_KEY: &str = "OutlinePanel";
const UPDATE_DEBOUNCE: Duration = Duration::from_millis(50);

type Outline = OutlineItem<language::Anchor>;
type HighlightStyleData = Arc<OnceLock<Vec<(Range<usize>, HighlightStyle)>>>;

pub struct OutlinePanel {
    fs: Arc<dyn Fs>,
    width: Option<Pixels>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    active: bool,
    pinned: bool,
    scroll_handle: UniformListScrollHandle,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    focus_handle: FocusHandle,
    pending_serialization: Task<Option<()>>,
    fs_entries_depth: HashMap<(WorktreeId, ProjectEntryId), usize>,
    fs_entries: Vec<FsEntry>,
    fs_children_count: HashMap<WorktreeId, HashMap<Arc<Path>, FsChildren>>,
    collapsed_entries: HashSet<CollapsedEntry>,
    unfolded_dirs: HashMap<WorktreeId, BTreeSet<ProjectEntryId>>,
    selected_entry: SelectedEntry,
    active_item: Option<ActiveItem>,
    _subscriptions: Vec<Subscription>,
    updating_fs_entries: bool,
    updating_cached_entries: bool,
    new_entries_for_fs_update: HashSet<ExcerptId>,
    fs_entries_update_task: Task<()>,
    cached_entries_update_task: Task<()>,
    reveal_selection_task: Task<anyhow::Result<()>>,
    outline_fetch_tasks: HashMap<(BufferId, ExcerptId), Task<()>>,
    excerpts: HashMap<BufferId, HashMap<ExcerptId, Excerpt>>,
    cached_entries: Vec<CachedEntry>,
    filter_editor: Entity<Editor>,
    mode: ItemsDisplayMode,
    show_scrollbar: bool,
    vertical_scrollbar_state: ScrollbarState,
    horizontal_scrollbar_state: ScrollbarState,
    hide_scrollbar_task: Option<Task<()>>,
    max_width_item_index: Option<usize>,
    preserve_selection_on_buffer_fold_toggles: HashSet<BufferId>,
}

#[derive(Debug)]
enum ItemsDisplayMode {
    Search(SearchState),
    Outline,
}

#[derive(Debug)]
struct SearchState {
    kind: SearchKind,
    query: String,
    matches: Vec<(Range<editor::Anchor>, Arc<OnceLock<SearchData>>)>,
    highlight_search_match_tx: channel::Sender<HighlightArguments>,
    _search_match_highlighter: Task<()>,
    _search_match_notify: Task<()>,
}

struct HighlightArguments {
    multi_buffer_snapshot: MultiBufferSnapshot,
    match_range: Range<editor::Anchor>,
    search_data: Arc<OnceLock<SearchData>>,
}

impl SearchState {
    fn new(
        kind: SearchKind,
        query: String,
        previous_matches: HashMap<Range<editor::Anchor>, Arc<OnceLock<SearchData>>>,
        new_matches: Vec<Range<editor::Anchor>>,
        theme: Arc<SyntaxTheme>,
        window: &mut Window,
        cx: &mut Context<OutlinePanel>,
    ) -> Self {
        let (highlight_search_match_tx, highlight_search_match_rx) = channel::unbounded();
        let (notify_tx, notify_rx) = channel::unbounded::<()>();
        Self {
            kind,
            query,
            matches: new_matches
                .into_iter()
                .map(|range| {
                    let search_data = previous_matches
                        .get(&range)
                        .map(Arc::clone)
                        .unwrap_or_default();
                    (range, search_data)
                })
                .collect(),
            highlight_search_match_tx,
            _search_match_highlighter: cx.background_spawn(async move {
                while let Ok(highlight_arguments) = highlight_search_match_rx.recv().await {
                    let needs_init = highlight_arguments.search_data.get().is_none();
                    let search_data = highlight_arguments.search_data.get_or_init(|| {
                        SearchData::new(
                            &highlight_arguments.match_range,
                            &highlight_arguments.multi_buffer_snapshot,
                        )
                    });
                    if needs_init {
                        notify_tx.try_send(()).ok();
                    }

                    let highlight_data = &search_data.highlights_data;
                    if highlight_data.get().is_some() {
                        continue;
                    }
                    let mut left_whitespaces_count = 0;
                    let mut non_whitespace_symbol_occurred = false;
                    let context_offset_range = search_data
                        .context_range
                        .to_offset(&highlight_arguments.multi_buffer_snapshot);
                    let mut offset = context_offset_range.start;
                    let mut context_text = String::new();
                    let mut highlight_ranges = Vec::new();
                    for mut chunk in highlight_arguments
                        .multi_buffer_snapshot
                        .chunks(context_offset_range.start..context_offset_range.end, true)
                    {
                        if !non_whitespace_symbol_occurred {
                            for c in chunk.text.chars() {
                                if c.is_whitespace() {
                                    left_whitespaces_count += c.len_utf8();
                                } else {
                                    non_whitespace_symbol_occurred = true;
                                    break;
                                }
                            }
                        }

                        if chunk.text.len() > context_offset_range.end - offset {
                            chunk.text = &chunk.text[0..(context_offset_range.end - offset)];
                            offset = context_offset_range.end;
                        } else {
                            offset += chunk.text.len();
                        }
                        let style = chunk
                            .syntax_highlight_id
                            .and_then(|highlight| highlight.style(&theme));
                        if let Some(style) = style {
                            let start = context_text.len();
                            let end = start + chunk.text.len();
                            highlight_ranges.push((start..end, style));
                        }
                        context_text.push_str(chunk.text);
                        if offset >= context_offset_range.end {
                            break;
                        }
                    }

                    highlight_ranges.iter_mut().for_each(|(range, _)| {
                        range.start = range.start.saturating_sub(left_whitespaces_count);
                        range.end = range.end.saturating_sub(left_whitespaces_count);
                    });
                    if highlight_data.set(highlight_ranges).ok().is_some() {
                        notify_tx.try_send(()).ok();
                    }

                    let trimmed_text = context_text[left_whitespaces_count..].to_owned();
                    debug_assert_eq!(
                        trimmed_text, search_data.context_text,
                        "Highlighted text that does not match the buffer text"
                    );
                }
            }),
            _search_match_notify: cx.spawn_in(window, async move |outline_panel, cx| {
                loop {
                    match notify_rx.recv().await {
                        Ok(()) => {}
                        Err(_) => break,
                    };
                    while let Ok(()) = notify_rx.try_recv() {
                        //
                    }
                    let update_result = outline_panel.update(cx, |_, cx| {
                        cx.notify();
                    });
                    if update_result.is_err() {
                        break;
                    }
                }
            }),
        }
    }
}

#[derive(Debug)]
enum SelectedEntry {
    Invalidated(Option<PanelEntry>),
    Valid(PanelEntry, usize),
    None,
}

impl SelectedEntry {
    fn invalidate(&mut self) {
        match std::mem::replace(self, SelectedEntry::None) {
            Self::Valid(entry, _) => *self = Self::Invalidated(Some(entry)),
            Self::None => *self = Self::Invalidated(None),
            other => *self = other,
        }
    }

    fn is_invalidated(&self) -> bool {
        matches!(self, Self::Invalidated(_))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct FsChildren {
    files: usize,
    dirs: usize,
}

impl FsChildren {
    fn may_be_fold_part(&self) -> bool {
        self.dirs == 0 || (self.dirs == 1 && self.files == 0)
    }
}

#[derive(Clone, Debug)]
struct CachedEntry {
    depth: usize,
    string_match: Option<StringMatch>,
    entry: PanelEntry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum CollapsedEntry {
    Dir(WorktreeId, ProjectEntryId),
    File(WorktreeId, BufferId),
    ExternalFile(BufferId),
    Excerpt(BufferId, ExcerptId),
}

#[derive(Debug)]
struct Excerpt {
    range: ExcerptRange<language::Anchor>,
    outlines: ExcerptOutlines,
}

impl Excerpt {
    fn invalidate_outlines(&mut self) {
        if let ExcerptOutlines::Outlines(valid_outlines) = &mut self.outlines {
            self.outlines = ExcerptOutlines::Invalidated(std::mem::take(valid_outlines));
        }
    }

    fn iter_outlines(&self) -> impl Iterator<Item = &Outline> {
        match &self.outlines {
            ExcerptOutlines::Outlines(outlines) => outlines.iter(),
            ExcerptOutlines::Invalidated(outlines) => outlines.iter(),
            ExcerptOutlines::NotFetched => [].iter(),
        }
    }

    fn should_fetch_outlines(&self) -> bool {
        match &self.outlines {
            ExcerptOutlines::Outlines(_) => false,
            ExcerptOutlines::Invalidated(_) => true,
            ExcerptOutlines::NotFetched => true,
        }
    }
}

#[derive(Debug)]
enum ExcerptOutlines {
    Outlines(Vec<Outline>),
    Invalidated(Vec<Outline>),
    NotFetched,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FoldedDirsEntry {
    worktree_id: WorktreeId,
    entries: Vec<GitEntry>,
}

// TODO: collapse the inner enums into panel entry
#[derive(Clone, Debug)]
enum PanelEntry {
    Fs(FsEntry),
    FoldedDirs(FoldedDirsEntry),
    Outline(OutlineEntry),
    Search(SearchEntry),
}

#[derive(Clone, Debug)]
struct SearchEntry {
    match_range: Range<editor::Anchor>,
    kind: SearchKind,
    render_data: Arc<OnceLock<SearchData>>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum SearchKind {
    Project,
    Buffer,
}

#[derive(Clone, Debug)]
struct SearchData {
    context_range: Range<editor::Anchor>,
    context_text: String,
    truncated_left: bool,
    truncated_right: bool,
    search_match_indices: Vec<Range<usize>>,
    highlights_data: HighlightStyleData,
}

impl PartialEq for PanelEntry {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Fs(a), Self::Fs(b)) => a == b,
            (
                Self::FoldedDirs(FoldedDirsEntry {
                    worktree_id: worktree_id_a,
                    entries: entries_a,
                }),
                Self::FoldedDirs(FoldedDirsEntry {
                    worktree_id: worktree_id_b,
                    entries: entries_b,
                }),
            ) => worktree_id_a == worktree_id_b && entries_a == entries_b,
            (Self::Outline(a), Self::Outline(b)) => a == b,
            (
                Self::Search(SearchEntry {
                    match_range: match_range_a,
                    kind: kind_a,
                    ..
                }),
                Self::Search(SearchEntry {
                    match_range: match_range_b,
                    kind: kind_b,
                    ..
                }),
            ) => match_range_a == match_range_b && kind_a == kind_b,
            _ => false,
        }
    }
}

impl Eq for PanelEntry {}

const SEARCH_MATCH_CONTEXT_SIZE: u32 = 40;
const TRUNCATED_CONTEXT_MARK: &str = "…";

impl SearchData {
    fn new(
        match_range: &Range<editor::Anchor>,
        multi_buffer_snapshot: &MultiBufferSnapshot,
    ) -> Self {
        let match_point_range = match_range.to_point(multi_buffer_snapshot);
        let context_left_border = multi_buffer_snapshot.clip_point(
            language::Point::new(
                match_point_range.start.row,
                match_point_range
                    .start
                    .column
                    .saturating_sub(SEARCH_MATCH_CONTEXT_SIZE),
            ),
            Bias::Left,
        );
        let context_right_border = multi_buffer_snapshot.clip_point(
            language::Point::new(
                match_point_range.end.row,
                match_point_range.end.column + SEARCH_MATCH_CONTEXT_SIZE,
            ),
            Bias::Right,
        );

        let context_anchor_range =
            (context_left_border..context_right_border).to_anchors(multi_buffer_snapshot);
        let context_offset_range = context_anchor_range.to_offset(multi_buffer_snapshot);
        let match_offset_range = match_range.to_offset(multi_buffer_snapshot);

        let mut search_match_indices = vec![
            multi_buffer_snapshot.clip_offset(
                match_offset_range.start - context_offset_range.start,
                Bias::Left,
            )
                ..multi_buffer_snapshot.clip_offset(
                    match_offset_range.end - context_offset_range.start,
                    Bias::Right,
                ),
        ];

        let entire_context_text = multi_buffer_snapshot
            .text_for_range(context_offset_range.clone())
            .collect::<String>();
        let left_whitespaces_offset = entire_context_text
            .chars()
            .take_while(|c| c.is_whitespace())
            .map(|c| c.len_utf8())
            .sum::<usize>();

        let mut extended_context_left_border = context_left_border;
        extended_context_left_border.column = extended_context_left_border.column.saturating_sub(1);
        let extended_context_left_border =
            multi_buffer_snapshot.clip_point(extended_context_left_border, Bias::Left);
        let mut extended_context_right_border = context_right_border;
        extended_context_right_border.column += 1;
        let extended_context_right_border =
            multi_buffer_snapshot.clip_point(extended_context_right_border, Bias::Right);

        let truncated_left = left_whitespaces_offset == 0
            && extended_context_left_border < context_left_border
            && multi_buffer_snapshot
                .chars_at(extended_context_left_border)
                .last()
                .map_or(false, |c| !c.is_whitespace());
        let truncated_right = entire_context_text
            .chars()
            .last()
            .map_or(true, |c| !c.is_whitespace())
            && extended_context_right_border > context_right_border
            && multi_buffer_snapshot
                .chars_at(extended_context_right_border)
                .next()
                .map_or(false, |c| !c.is_whitespace());
        search_match_indices.iter_mut().for_each(|range| {
            range.start = multi_buffer_snapshot.clip_offset(
                range.start.saturating_sub(left_whitespaces_offset),
                Bias::Left,
            );
            range.end = multi_buffer_snapshot.clip_offset(
                range.end.saturating_sub(left_whitespaces_offset),
                Bias::Right,
            );
        });

        let trimmed_row_offset_range =
            context_offset_range.start + left_whitespaces_offset..context_offset_range.end;
        let trimmed_text = entire_context_text[left_whitespaces_offset..].to_owned();
        Self {
            highlights_data: Arc::default(),
            search_match_indices,
            context_range: trimmed_row_offset_range.to_anchors(multi_buffer_snapshot),
            context_text: trimmed_text,
            truncated_left,
            truncated_right,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct OutlineEntryExcerpt {
    id: ExcerptId,
    buffer_id: BufferId,
    range: ExcerptRange<language::Anchor>,
}

#[derive(Clone, Debug, Eq)]
struct OutlineEntryOutline {
    buffer_id: BufferId,
    excerpt_id: ExcerptId,
    outline: Outline,
}

impl PartialEq for OutlineEntryOutline {
    fn eq(&self, other: &Self) -> bool {
        self.buffer_id == other.buffer_id
            && self.excerpt_id == other.excerpt_id
            && self.outline.depth == other.outline.depth
            && self.outline.range == other.outline.range
            && self.outline.text == other.outline.text
    }
}

impl Hash for OutlineEntryOutline {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (
            self.buffer_id,
            self.excerpt_id,
            self.outline.depth,
            &self.outline.range,
            &self.outline.text,
        )
            .hash(state);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum OutlineEntry {
    Excerpt(OutlineEntryExcerpt),
    Outline(OutlineEntryOutline),
}

impl OutlineEntry {
    fn ids(&self) -> (BufferId, ExcerptId) {
        match self {
            OutlineEntry::Excerpt(excerpt) => (excerpt.buffer_id, excerpt.id),
            OutlineEntry::Outline(outline) => (outline.buffer_id, outline.excerpt_id),
        }
    }
}

#[derive(Debug, Clone, Eq)]
struct FsEntryFile {
    worktree_id: WorktreeId,
    entry: GitEntry,
    buffer_id: BufferId,
    excerpts: Vec<ExcerptId>,
}

impl PartialEq for FsEntryFile {
    fn eq(&self, other: &Self) -> bool {
        self.worktree_id == other.worktree_id
            && self.entry.id == other.entry.id
            && self.buffer_id == other.buffer_id
    }
}

impl Hash for FsEntryFile {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self.buffer_id, self.entry.id, self.worktree_id).hash(state);
    }
}

#[derive(Debug, Clone, Eq)]
struct FsEntryDirectory {
    worktree_id: WorktreeId,
    entry: GitEntry,
}

impl PartialEq for FsEntryDirectory {
    fn eq(&self, other: &Self) -> bool {
        self.worktree_id == other.worktree_id && self.entry.id == other.entry.id
    }
}

impl Hash for FsEntryDirectory {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self.worktree_id, self.entry.id).hash(state);
    }
}

#[derive(Debug, Clone, Eq)]
struct FsEntryExternalFile {
    buffer_id: BufferId,
    excerpts: Vec<ExcerptId>,
}

impl PartialEq for FsEntryExternalFile {
    fn eq(&self, other: &Self) -> bool {
        self.buffer_id == other.buffer_id
    }
}

impl Hash for FsEntryExternalFile {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.buffer_id.hash(state);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FsEntry {
    ExternalFile(FsEntryExternalFile),
    Directory(FsEntryDirectory),
    File(FsEntryFile),
}

struct ActiveItem {
    item_handle: Box<dyn WeakItemHandle>,
    active_editor: WeakEntity<Editor>,
    _buffer_search_subscription: Subscription,
    _editor_subscription: Subscription,
}

#[derive(Debug)]
pub enum Event {
    Focus,
}

#[derive(Serialize, Deserialize)]
struct SerializedOutlinePanel {
    width: Option<Pixels>,
    active: Option<bool>,
}

pub fn init_settings(cx: &mut App) {
    OutlinePanelSettings::register(cx);
}

pub fn init(cx: &mut App) {
    init_settings(cx);

    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<OutlinePanel>(window, cx);
        });
    })
    .detach();
}

impl OutlinePanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let serialized_panel = match workspace
            .read_with(&cx, |workspace, _| {
                OutlinePanel::serialization_key(workspace)
            })
            .ok()
            .flatten()
        {
            Some(serialization_key) => cx
                .background_spawn(async move { KEY_VALUE_STORE.read_kvp(&serialization_key) })
                .await
                .context("loading outline panel")
                .log_err()
                .flatten()
                .map(|panel| serde_json::from_str::<SerializedOutlinePanel>(&panel))
                .transpose()
                .log_err()
                .flatten(),
            None => None,
        };

        workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = Self::new(workspace, window, cx);
            if let Some(serialized_panel) = serialized_panel {
                panel.update(cx, |panel, cx| {
                    panel.width = serialized_panel.width.map(|px| px.round());
                    panel.active = serialized_panel.active.unwrap_or(false);
                    cx.notify();
                });
            }
            panel
        })
    }

    fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let workspace_handle = cx.entity().downgrade();
        let outline_panel = cx.new(|cx| {
            let filter_editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Filter...", cx);
                editor
            });
            let filter_update_subscription = cx.subscribe_in(
                &filter_editor,
                window,
                |outline_panel: &mut Self, _, event, window, cx| {
                    if let editor::EditorEvent::BufferEdited = event {
                        outline_panel.update_cached_entries(Some(UPDATE_DEBOUNCE), window, cx);
                    }
                },
            );

            let focus_handle = cx.focus_handle();
            let focus_subscription = cx.on_focus(&focus_handle, window, Self::focus_in);
            let focus_out_subscription =
                cx.on_focus_out(&focus_handle, window, |outline_panel, _, window, cx| {
                    outline_panel.hide_scrollbar(window, cx);
                });
            let workspace_subscription = cx.subscribe_in(
                &workspace
                    .weak_handle()
                    .upgrade()
                    .expect("have a &mut Workspace"),
                window,
                move |outline_panel, workspace, event, window, cx| {
                    if let workspace::Event::ActiveItemChanged = event {
                        if let Some((new_active_item, new_active_editor)) =
                            workspace_active_editor(workspace.read(cx), cx)
                        {
                            if outline_panel.should_replace_active_item(new_active_item.as_ref()) {
                                outline_panel.replace_active_editor(
                                    new_active_item,
                                    new_active_editor,
                                    window,
                                    cx,
                                );
                            }
                        } else {
                            outline_panel.clear_previous(window, cx);
                            cx.notify();
                        }
                    }
                },
            );

            let icons_subscription = cx.observe_global::<FileIcons>(|_, cx| {
                cx.notify();
            });

            let mut outline_panel_settings = *OutlinePanelSettings::get_global(cx);
            let mut current_theme = ThemeSettings::get_global(cx).clone();
            let settings_subscription =
                cx.observe_global_in::<SettingsStore>(window, move |outline_panel, window, cx| {
                    let new_settings = OutlinePanelSettings::get_global(cx);
                    let new_theme = ThemeSettings::get_global(cx);
                    if &current_theme != new_theme {
                        outline_panel_settings = *new_settings;
                        current_theme = new_theme.clone();
                        for excerpts in outline_panel.excerpts.values_mut() {
                            for excerpt in excerpts.values_mut() {
                                excerpt.invalidate_outlines();
                            }
                        }
                        let update_cached_items = outline_panel.update_non_fs_items(window, cx);
                        if update_cached_items {
                            outline_panel.update_cached_entries(Some(UPDATE_DEBOUNCE), window, cx);
                        }
                    } else if &outline_panel_settings != new_settings {
                        outline_panel_settings = *new_settings;
                        cx.notify();
                    }
                });

            let scroll_handle = UniformListScrollHandle::new();

            let mut outline_panel = Self {
                mode: ItemsDisplayMode::Outline,
                active: false,
                pinned: false,
                workspace: workspace_handle,
                project,
                fs: workspace.app_state().fs.clone(),
                show_scrollbar: !Self::should_autohide_scrollbar(cx),
                hide_scrollbar_task: None,
                vertical_scrollbar_state: ScrollbarState::new(scroll_handle.clone())
                    .parent_entity(&cx.entity()),
                horizontal_scrollbar_state: ScrollbarState::new(scroll_handle.clone())
                    .parent_entity(&cx.entity()),
                max_width_item_index: None,
                scroll_handle,
                focus_handle,
                filter_editor,
                fs_entries: Vec::new(),
                fs_entries_depth: HashMap::default(),
                fs_children_count: HashMap::default(),
                collapsed_entries: HashSet::default(),
                unfolded_dirs: HashMap::default(),
                selected_entry: SelectedEntry::None,
                context_menu: None,
                width: None,
                active_item: None,
                pending_serialization: Task::ready(None),
                updating_fs_entries: false,
                updating_cached_entries: false,
                new_entries_for_fs_update: HashSet::default(),
                preserve_selection_on_buffer_fold_toggles: HashSet::default(),
                fs_entries_update_task: Task::ready(()),
                cached_entries_update_task: Task::ready(()),
                reveal_selection_task: Task::ready(Ok(())),
                outline_fetch_tasks: HashMap::default(),
                excerpts: HashMap::default(),
                cached_entries: Vec::new(),
                _subscriptions: vec![
                    settings_subscription,
                    icons_subscription,
                    focus_subscription,
                    focus_out_subscription,
                    workspace_subscription,
                    filter_update_subscription,
                ],
            };
            if let Some((item, editor)) = workspace_active_editor(workspace, cx) {
                outline_panel.replace_active_editor(item, editor, window, cx);
            }
            outline_panel
        });

        outline_panel
    }

    fn serialization_key(workspace: &Workspace) -> Option<String> {
        workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())
            .map(|id| format!("{}-{:?}", OUTLINE_PANEL_KEY, id))
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        let Some(serialization_key) = self
            .workspace
            .read_with(cx, |workspace, _| {
                OutlinePanel::serialization_key(workspace)
            })
            .ok()
            .flatten()
        else {
            return;
        };
        let width = self.width;
        let active = Some(self.active);
        self.pending_serialization = cx.background_spawn(
            async move {
                KEY_VALUE_STORE
                    .write_kvp(
                        serialization_key,
                        serde_json::to_string(&SerializedOutlinePanel { width, active })?,
                    )
                    .await?;
                anyhow::Ok(())
            }
            .log_err(),
        );
    }

    fn dispatch_context(&self, window: &mut Window, cx: &mut Context<Self>) -> KeyContext {
        let mut dispatch_context = KeyContext::new_with_defaults();
        dispatch_context.add("OutlinePanel");
        dispatch_context.add("menu");
        let identifier = if self.filter_editor.focus_handle(cx).is_focused(window) {
            "editing"
        } else {
            "not_editing"
        };
        dispatch_context.add(identifier);
        dispatch_context
    }

    fn unfold_directory(
        &mut self,
        _: &UnfoldDirectory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(PanelEntry::FoldedDirs(FoldedDirsEntry {
            worktree_id,
            entries,
            ..
        })) = self.selected_entry().cloned()
        {
            self.unfolded_dirs
                .entry(worktree_id)
                .or_default()
                .extend(entries.iter().map(|entry| entry.id));
            self.update_cached_entries(None, window, cx);
        }
    }

    fn fold_directory(&mut self, _: &FoldDirectory, window: &mut Window, cx: &mut Context<Self>) {
        let (worktree_id, entry) = match self.selected_entry().cloned() {
            Some(PanelEntry::Fs(FsEntry::Directory(directory))) => {
                (directory.worktree_id, Some(directory.entry))
            }
            Some(PanelEntry::FoldedDirs(folded_dirs)) => {
                (folded_dirs.worktree_id, folded_dirs.entries.last().cloned())
            }
            _ => return,
        };
        let Some(entry) = entry else {
            return;
        };
        let unfolded_dirs = self.unfolded_dirs.get_mut(&worktree_id);
        let worktree = self
            .project
            .read(cx)
            .worktree_for_id(worktree_id, cx)
            .map(|w| w.read(cx).snapshot());
        let Some((_, unfolded_dirs)) = worktree.zip(unfolded_dirs) else {
            return;
        };

        unfolded_dirs.remove(&entry.id);
        self.update_cached_entries(None, window, cx);
    }

    fn open_selected_entry(
        &mut self,
        _: &OpenSelectedEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.filter_editor.focus_handle(cx).is_focused(window) {
            cx.propagate()
        } else if let Some(selected_entry) = self.selected_entry().cloned() {
            self.toggle_expanded(&selected_entry, window, cx);
            self.scroll_editor_to_entry(&selected_entry, true, true, window, cx);
        }
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_editor.focus_handle(cx).is_focused(window) {
            self.focus_handle.focus(window);
        } else {
            self.filter_editor.focus_handle(cx).focus(window);
        }

        if self.context_menu.is_some() {
            self.context_menu.take();
            cx.notify();
        }
    }

    fn open_excerpts(
        &mut self,
        action: &editor::OpenExcerpts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.filter_editor.focus_handle(cx).is_focused(window) {
            cx.propagate()
        } else if let Some((active_editor, selected_entry)) =
            self.active_editor().zip(self.selected_entry().cloned())
        {
            self.scroll_editor_to_entry(&selected_entry, true, true, window, cx);
            active_editor.update(cx, |editor, cx| editor.open_excerpts(action, window, cx));
        }
    }

    fn open_excerpts_split(
        &mut self,
        action: &editor::OpenExcerptsSplit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.filter_editor.focus_handle(cx).is_focused(window) {
            cx.propagate()
        } else if let Some((active_editor, selected_entry)) =
            self.active_editor().zip(self.selected_entry().cloned())
        {
            self.scroll_editor_to_entry(&selected_entry, true, true, window, cx);
            active_editor.update(cx, |editor, cx| {
                editor.open_excerpts_in_split(action, window, cx)
            });
        }
    }

    fn scroll_editor_to_entry(
        &mut self,
        entry: &PanelEntry,
        prefer_selection_change: bool,
        prefer_focus_change: bool,
        window: &mut Window,
        cx: &mut Context<OutlinePanel>,
    ) {
        let Some(active_editor) = self.active_editor() else {
            return;
        };
        let active_multi_buffer = active_editor.read(cx).buffer().clone();
        let multi_buffer_snapshot = active_multi_buffer.read(cx).snapshot(cx);
        let mut change_selection = prefer_selection_change;
        let mut change_focus = prefer_focus_change;
        let mut scroll_to_buffer = None;
        let scroll_target = match entry {
            PanelEntry::FoldedDirs(..) | PanelEntry::Fs(FsEntry::Directory(..)) => {
                change_focus = false;
                None
            }
            PanelEntry::Fs(FsEntry::ExternalFile(file)) => {
                change_selection = false;
                scroll_to_buffer = Some(file.buffer_id);
                multi_buffer_snapshot.excerpts().find_map(
                    |(excerpt_id, buffer_snapshot, excerpt_range)| {
                        if buffer_snapshot.remote_id() == file.buffer_id {
                            multi_buffer_snapshot
                                .anchor_in_excerpt(excerpt_id, excerpt_range.context.start)
                        } else {
                            None
                        }
                    },
                )
            }

            PanelEntry::Fs(FsEntry::File(file)) => {
                change_selection = false;
                scroll_to_buffer = Some(file.buffer_id);
                self.project
                    .update(cx, |project, cx| {
                        project
                            .path_for_entry(file.entry.id, cx)
                            .and_then(|path| project.get_open_buffer(&path, cx))
                    })
                    .map(|buffer| {
                        active_multi_buffer
                            .read(cx)
                            .excerpts_for_buffer(buffer.read(cx).remote_id(), cx)
                    })
                    .and_then(|excerpts| {
                        let (excerpt_id, excerpt_range) = excerpts.first()?;
                        multi_buffer_snapshot
                            .anchor_in_excerpt(*excerpt_id, excerpt_range.context.start)
                    })
            }
            PanelEntry::Outline(OutlineEntry::Outline(outline)) => multi_buffer_snapshot
                .anchor_in_excerpt(outline.excerpt_id, outline.outline.range.start)
                .or_else(|| {
                    multi_buffer_snapshot
                        .anchor_in_excerpt(outline.excerpt_id, outline.outline.range.end)
                }),
            PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => {
                change_selection = false;
                change_focus = false;
                multi_buffer_snapshot.anchor_in_excerpt(excerpt.id, excerpt.range.context.start)
            }
            PanelEntry::Search(search_entry) => Some(search_entry.match_range.start),
        };

        if let Some(anchor) = scroll_target {
            let activate = self
                .workspace
                .update(cx, |workspace, cx| match self.active_item() {
                    Some(active_item) => workspace.activate_item(
                        active_item.as_ref(),
                        true,
                        change_focus,
                        window,
                        cx,
                    ),
                    None => workspace.activate_item(&active_editor, true, change_focus, window, cx),
                });

            if activate.is_ok() {
                self.select_entry(entry.clone(), true, window, cx);
                if change_selection {
                    active_editor.update(cx, |editor, cx| {
                        editor.change_selections(
                            Some(Autoscroll::Strategy(AutoscrollStrategy::Center, None)),
                            window,
                            cx,
                            |s| s.select_ranges(Some(anchor..anchor)),
                        );
                    });
                } else {
                    let mut offset = Point::default();
                    let expand_excerpt_control_height = 1.0;
                    if let Some(buffer_id) = scroll_to_buffer {
                        let current_folded = active_editor.read(cx).is_buffer_folded(buffer_id, cx);
                        if current_folded {
                            let previous_buffer_id = self
                                .fs_entries
                                .iter()
                                .rev()
                                .filter_map(|entry| match entry {
                                    FsEntry::File(file) => Some(file.buffer_id),
                                    FsEntry::ExternalFile(external_file) => {
                                        Some(external_file.buffer_id)
                                    }
                                    FsEntry::Directory(..) => None,
                                })
                                .skip_while(|id| *id != buffer_id)
                                .nth(1);
                            if let Some(previous_buffer_id) = previous_buffer_id {
                                if !active_editor
                                    .read(cx)
                                    .is_buffer_folded(previous_buffer_id, cx)
                                {
                                    offset.y += expand_excerpt_control_height;
                                }
                            }
                        } else {
                            if multi_buffer_snapshot.as_singleton().is_none() {
                                offset.y = -(active_editor.read(cx).file_header_size() as f32);
                            }
                            offset.y -= expand_excerpt_control_height;
                        }
                    }
                    active_editor.update(cx, |editor, cx| {
                        editor.set_scroll_anchor(ScrollAnchor { offset, anchor }, window, cx);
                    });
                }

                if change_focus {
                    active_editor.focus_handle(cx).focus(window);
                } else {
                    self.focus_handle.focus(window);
                }
            }
        }
    }

    fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry_to_select) = self.selected_entry().and_then(|selected_entry| {
            self.cached_entries
                .iter()
                .map(|cached_entry| &cached_entry.entry)
                .skip_while(|entry| entry != &selected_entry)
                .nth(1)
                .cloned()
        }) {
            self.select_entry(entry_to_select, true, window, cx);
        } else {
            self.select_first(&SelectFirst {}, window, cx)
        }
        if let Some(selected_entry) = self.selected_entry().cloned() {
            self.scroll_editor_to_entry(&selected_entry, true, false, window, cx);
        }
    }

    fn select_previous(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry_to_select) = self.selected_entry().and_then(|selected_entry| {
            self.cached_entries
                .iter()
                .rev()
                .map(|cached_entry| &cached_entry.entry)
                .skip_while(|entry| entry != &selected_entry)
                .nth(1)
                .cloned()
        }) {
            self.select_entry(entry_to_select, true, window, cx);
        } else {
            self.select_last(&SelectLast, window, cx)
        }
        if let Some(selected_entry) = self.selected_entry().cloned() {
            self.scroll_editor_to_entry(&selected_entry, true, false, window, cx);
        }
    }

    fn select_parent(&mut self, _: &SelectParent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry_to_select) = self.selected_entry().and_then(|selected_entry| {
            let mut previous_entries = self
                .cached_entries
                .iter()
                .rev()
                .map(|cached_entry| &cached_entry.entry)
                .skip_while(|entry| entry != &selected_entry)
                .skip(1);
            match &selected_entry {
                PanelEntry::Fs(fs_entry) => match fs_entry {
                    FsEntry::ExternalFile(..) => None,
                    FsEntry::File(FsEntryFile {
                        worktree_id, entry, ..
                    })
                    | FsEntry::Directory(FsEntryDirectory {
                        worktree_id, entry, ..
                    }) => entry.path.parent().and_then(|parent_path| {
                        previous_entries.find(|entry| match entry {
                            PanelEntry::Fs(FsEntry::Directory(directory)) => {
                                directory.worktree_id == *worktree_id
                                    && directory.entry.path.as_ref() == parent_path
                            }
                            PanelEntry::FoldedDirs(FoldedDirsEntry {
                                worktree_id: dirs_worktree_id,
                                entries: dirs,
                                ..
                            }) => {
                                dirs_worktree_id == worktree_id
                                    && dirs
                                        .last()
                                        .map_or(false, |dir| dir.path.as_ref() == parent_path)
                            }
                            _ => false,
                        })
                    }),
                },
                PanelEntry::FoldedDirs(folded_dirs) => folded_dirs
                    .entries
                    .first()
                    .and_then(|entry| entry.path.parent())
                    .and_then(|parent_path| {
                        previous_entries.find(|entry| {
                            if let PanelEntry::Fs(FsEntry::Directory(directory)) = entry {
                                directory.worktree_id == folded_dirs.worktree_id
                                    && directory.entry.path.as_ref() == parent_path
                            } else {
                                false
                            }
                        })
                    }),
                PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => {
                    previous_entries.find(|entry| match entry {
                        PanelEntry::Fs(FsEntry::File(file)) => {
                            file.buffer_id == excerpt.buffer_id
                                && file.excerpts.contains(&excerpt.id)
                        }
                        PanelEntry::Fs(FsEntry::ExternalFile(external_file)) => {
                            external_file.buffer_id == excerpt.buffer_id
                                && external_file.excerpts.contains(&excerpt.id)
                        }
                        _ => false,
                    })
                }
                PanelEntry::Outline(OutlineEntry::Outline(outline)) => {
                    previous_entries.find(|entry| {
                        if let PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) = entry {
                            outline.buffer_id == excerpt.buffer_id
                                && outline.excerpt_id == excerpt.id
                        } else {
                            false
                        }
                    })
                }
                PanelEntry::Search(_) => {
                    previous_entries.find(|entry| !matches!(entry, PanelEntry::Search(_)))
                }
            }
        }) {
            self.select_entry(entry_to_select.clone(), true, window, cx);
        } else {
            self.select_first(&SelectFirst {}, window, cx);
        }
    }

    fn select_first(&mut self, _: &SelectFirst, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(first_entry) = self.cached_entries.first() {
            self.select_entry(first_entry.entry.clone(), true, window, cx);
        }
    }

    fn select_last(&mut self, _: &SelectLast, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(new_selection) = self
            .cached_entries
            .iter()
            .rev()
            .map(|cached_entry| &cached_entry.entry)
            .next()
        {
            self.select_entry(new_selection.clone(), true, window, cx);
        }
    }

    fn autoscroll(&mut self, cx: &mut Context<Self>) {
        if let Some(selected_entry) = self.selected_entry() {
            let index = self
                .cached_entries
                .iter()
                .position(|cached_entry| &cached_entry.entry == selected_entry);
            if let Some(index) = index {
                self.scroll_handle
                    .scroll_to_item(index, ScrollStrategy::Center);
                cx.notify();
            }
        }
    }

    fn focus_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_handle.contains_focused(window, cx) {
            cx.emit(Event::Focus);
        }
    }

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        entry: PanelEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_entry(entry.clone(), true, window, cx);
        let is_root = match &entry {
            PanelEntry::Fs(FsEntry::File(FsEntryFile {
                worktree_id, entry, ..
            }))
            | PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory {
                worktree_id, entry, ..
            })) => self
                .project
                .read(cx)
                .worktree_for_id(*worktree_id, cx)
                .map(|worktree| {
                    worktree.read(cx).root_entry().map(|entry| entry.id) == Some(entry.id)
                })
                .unwrap_or(false),
            PanelEntry::FoldedDirs(FoldedDirsEntry {
                worktree_id,
                entries,
                ..
            }) => entries
                .first()
                .and_then(|entry| {
                    self.project
                        .read(cx)
                        .worktree_for_id(*worktree_id, cx)
                        .map(|worktree| {
                            worktree.read(cx).root_entry().map(|entry| entry.id) == Some(entry.id)
                        })
                })
                .unwrap_or(false),
            PanelEntry::Fs(FsEntry::ExternalFile(..)) => false,
            PanelEntry::Outline(..) => {
                cx.notify();
                return;
            }
            PanelEntry::Search(_) => {
                cx.notify();
                return;
            }
        };
        let auto_fold_dirs = OutlinePanelSettings::get_global(cx).auto_fold_dirs;
        let is_foldable = auto_fold_dirs && !is_root && self.is_foldable(&entry);
        let is_unfoldable = auto_fold_dirs && !is_root && self.is_unfoldable(&entry);

        let context_menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.context(self.focus_handle.clone())
                .action("Reveal in Finder", Box::new(RevealInFileManager))
                .action("Open in Terminal", Box::new(OpenInTerminal))
                .when(is_unfoldable, |menu| {
                    menu.action("Unfold Directory", Box::new(UnfoldDirectory))
                })
                .when(is_foldable, |menu| {
                    menu.action("Fold Directory", Box::new(FoldDirectory))
                })
                .separator()
                .action("Copy Path", Box::new(zed_actions::workspace::CopyPath))
                .action(
                    "Copy Relative Path",
                    Box::new(zed_actions::workspace::CopyRelativePath),
                )
        });
        window.focus(&context_menu.focus_handle(cx));
        let subscription = cx.subscribe(&context_menu, |outline_panel, _, _: &DismissEvent, cx| {
            outline_panel.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    fn is_unfoldable(&self, entry: &PanelEntry) -> bool {
        matches!(entry, PanelEntry::FoldedDirs(..))
    }

    fn is_foldable(&self, entry: &PanelEntry) -> bool {
        let (directory_worktree, directory_entry) = match entry {
            PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory {
                worktree_id,
                entry: directory_entry,
                ..
            })) => (*worktree_id, Some(directory_entry)),
            _ => return false,
        };
        let Some(directory_entry) = directory_entry else {
            return false;
        };

        if self
            .unfolded_dirs
            .get(&directory_worktree)
            .map_or(true, |unfolded_dirs| {
                !unfolded_dirs.contains(&directory_entry.id)
            })
        {
            return false;
        }

        let children = self
            .fs_children_count
            .get(&directory_worktree)
            .and_then(|entries| entries.get(&directory_entry.path))
            .copied()
            .unwrap_or_default();

        children.may_be_fold_part() && children.dirs > 0
    }

    fn expand_selected_entry(
        &mut self,
        _: &ExpandSelectedEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_editor) = self.active_editor() else {
            return;
        };
        let Some(selected_entry) = self.selected_entry().cloned() else {
            return;
        };
        let mut buffers_to_unfold = HashSet::default();
        let entry_to_expand = match &selected_entry {
            PanelEntry::FoldedDirs(FoldedDirsEntry {
                entries: dir_entries,
                worktree_id,
                ..
            }) => dir_entries.last().map(|entry| {
                buffers_to_unfold.extend(self.buffers_inside_directory(*worktree_id, entry));
                CollapsedEntry::Dir(*worktree_id, entry.id)
            }),
            PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory {
                worktree_id, entry, ..
            })) => {
                buffers_to_unfold.extend(self.buffers_inside_directory(*worktree_id, entry));
                Some(CollapsedEntry::Dir(*worktree_id, entry.id))
            }
            PanelEntry::Fs(FsEntry::File(FsEntryFile {
                worktree_id,
                buffer_id,
                ..
            })) => {
                buffers_to_unfold.insert(*buffer_id);
                Some(CollapsedEntry::File(*worktree_id, *buffer_id))
            }
            PanelEntry::Fs(FsEntry::ExternalFile(external_file)) => {
                buffers_to_unfold.insert(external_file.buffer_id);
                Some(CollapsedEntry::ExternalFile(external_file.buffer_id))
            }
            PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => {
                Some(CollapsedEntry::Excerpt(excerpt.buffer_id, excerpt.id))
            }
            PanelEntry::Search(_) | PanelEntry::Outline(..) => return,
        };
        let Some(collapsed_entry) = entry_to_expand else {
            return;
        };
        let expanded = self.collapsed_entries.remove(&collapsed_entry);
        if expanded {
            if let CollapsedEntry::Dir(worktree_id, dir_entry_id) = collapsed_entry {
                let task = self.project.update(cx, |project, cx| {
                    project.expand_entry(worktree_id, dir_entry_id, cx)
                });
                if let Some(task) = task {
                    task.detach_and_log_err(cx);
                }
            };

            active_editor.update(cx, |editor, cx| {
                buffers_to_unfold.retain(|buffer_id| editor.is_buffer_folded(*buffer_id, cx));
            });
            self.select_entry(selected_entry, true, window, cx);
            if buffers_to_unfold.is_empty() {
                self.update_cached_entries(None, window, cx);
            } else {
                self.toggle_buffers_fold(buffers_to_unfold, false, window, cx)
                    .detach();
            }
        } else {
            self.select_next(&SelectNext, window, cx)
        }
    }

    fn collapse_selected_entry(
        &mut self,
        _: &CollapseSelectedEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_editor) = self.active_editor() else {
            return;
        };
        let Some(selected_entry) = self.selected_entry().cloned() else {
            return;
        };

        let mut buffers_to_fold = HashSet::default();
        let collapsed = match &selected_entry {
            PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory {
                worktree_id, entry, ..
            })) => {
                if self
                    .collapsed_entries
                    .insert(CollapsedEntry::Dir(*worktree_id, entry.id))
                {
                    buffers_to_fold.extend(self.buffers_inside_directory(*worktree_id, entry));
                    true
                } else {
                    false
                }
            }
            PanelEntry::Fs(FsEntry::File(FsEntryFile {
                worktree_id,
                buffer_id,
                ..
            })) => {
                if self
                    .collapsed_entries
                    .insert(CollapsedEntry::File(*worktree_id, *buffer_id))
                {
                    buffers_to_fold.insert(*buffer_id);
                    true
                } else {
                    false
                }
            }
            PanelEntry::Fs(FsEntry::ExternalFile(external_file)) => {
                if self
                    .collapsed_entries
                    .insert(CollapsedEntry::ExternalFile(external_file.buffer_id))
                {
                    buffers_to_fold.insert(external_file.buffer_id);
                    true
                } else {
                    false
                }
            }
            PanelEntry::FoldedDirs(folded_dirs) => {
                let mut folded = false;
                if let Some(dir_entry) = folded_dirs.entries.last() {
                    if self
                        .collapsed_entries
                        .insert(CollapsedEntry::Dir(folded_dirs.worktree_id, dir_entry.id))
                    {
                        folded = true;
                        buffers_to_fold.extend(
                            self.buffers_inside_directory(folded_dirs.worktree_id, dir_entry),
                        );
                    }
                }
                folded
            }
            PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => self
                .collapsed_entries
                .insert(CollapsedEntry::Excerpt(excerpt.buffer_id, excerpt.id)),
            PanelEntry::Search(_) | PanelEntry::Outline(..) => false,
        };

        if collapsed {
            active_editor.update(cx, |editor, cx| {
                buffers_to_fold.retain(|buffer_id| !editor.is_buffer_folded(*buffer_id, cx));
            });
            self.select_entry(selected_entry, true, window, cx);
            if buffers_to_fold.is_empty() {
                self.update_cached_entries(None, window, cx);
            } else {
                self.toggle_buffers_fold(buffers_to_fold, true, window, cx)
                    .detach();
            }
        } else {
            self.select_parent(&SelectParent, window, cx);
        }
    }

    pub fn expand_all_entries(
        &mut self,
        _: &ExpandAllEntries,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_editor) = self.active_editor() else {
            return;
        };
        let mut buffers_to_unfold = HashSet::default();
        let expanded_entries =
            self.fs_entries
                .iter()
                .fold(HashSet::default(), |mut entries, fs_entry| {
                    match fs_entry {
                        FsEntry::ExternalFile(external_file) => {
                            buffers_to_unfold.insert(external_file.buffer_id);
                            entries.insert(CollapsedEntry::ExternalFile(external_file.buffer_id));
                            entries.extend(
                                self.excerpts
                                    .get(&external_file.buffer_id)
                                    .into_iter()
                                    .flat_map(|excerpts| {
                                        excerpts.keys().map(|excerpt_id| {
                                            CollapsedEntry::Excerpt(
                                                external_file.buffer_id,
                                                *excerpt_id,
                                            )
                                        })
                                    }),
                            );
                        }
                        FsEntry::Directory(directory) => {
                            entries.insert(CollapsedEntry::Dir(
                                directory.worktree_id,
                                directory.entry.id,
                            ));
                        }
                        FsEntry::File(file) => {
                            buffers_to_unfold.insert(file.buffer_id);
                            entries.insert(CollapsedEntry::File(file.worktree_id, file.buffer_id));
                            entries.extend(
                                self.excerpts.get(&file.buffer_id).into_iter().flat_map(
                                    |excerpts| {
                                        excerpts.keys().map(|excerpt_id| {
                                            CollapsedEntry::Excerpt(file.buffer_id, *excerpt_id)
                                        })
                                    },
                                ),
                            );
                        }
                    };
                    entries
                });
        self.collapsed_entries
            .retain(|entry| !expanded_entries.contains(entry));
        active_editor.update(cx, |editor, cx| {
            buffers_to_unfold.retain(|buffer_id| editor.is_buffer_folded(*buffer_id, cx));
        });
        if buffers_to_unfold.is_empty() {
            self.update_cached_entries(None, window, cx);
        } else {
            self.toggle_buffers_fold(buffers_to_unfold, false, window, cx)
                .detach();
        }
    }

    pub fn collapse_all_entries(
        &mut self,
        _: &CollapseAllEntries,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_editor) = self.active_editor() else {
            return;
        };
        let mut buffers_to_fold = HashSet::default();
        let new_entries = self
            .cached_entries
            .iter()
            .flat_map(|cached_entry| match &cached_entry.entry {
                PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory {
                    worktree_id, entry, ..
                })) => Some(CollapsedEntry::Dir(*worktree_id, entry.id)),
                PanelEntry::Fs(FsEntry::File(FsEntryFile {
                    worktree_id,
                    buffer_id,
                    ..
                })) => {
                    buffers_to_fold.insert(*buffer_id);
                    Some(CollapsedEntry::File(*worktree_id, *buffer_id))
                }
                PanelEntry::Fs(FsEntry::ExternalFile(external_file)) => {
                    buffers_to_fold.insert(external_file.buffer_id);
                    Some(CollapsedEntry::ExternalFile(external_file.buffer_id))
                }
                PanelEntry::FoldedDirs(FoldedDirsEntry {
                    worktree_id,
                    entries,
                    ..
                }) => Some(CollapsedEntry::Dir(*worktree_id, entries.last()?.id)),
                PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => {
                    Some(CollapsedEntry::Excerpt(excerpt.buffer_id, excerpt.id))
                }
                PanelEntry::Search(_) | PanelEntry::Outline(..) => None,
            })
            .collect::<Vec<_>>();
        self.collapsed_entries.extend(new_entries);

        active_editor.update(cx, |editor, cx| {
            buffers_to_fold.retain(|buffer_id| !editor.is_buffer_folded(*buffer_id, cx));
        });
        if buffers_to_fold.is_empty() {
            self.update_cached_entries(None, window, cx);
        } else {
            self.toggle_buffers_fold(buffers_to_fold, true, window, cx)
                .detach();
        }
    }

    fn toggle_expanded(&mut self, entry: &PanelEntry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active_editor) = self.active_editor() else {
            return;
        };
        let mut fold = false;
        let mut buffers_to_toggle = HashSet::default();
        match entry {
            PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory {
                worktree_id,
                entry: dir_entry,
                ..
            })) => {
                let entry_id = dir_entry.id;
                let collapsed_entry = CollapsedEntry::Dir(*worktree_id, entry_id);
                buffers_to_toggle.extend(self.buffers_inside_directory(*worktree_id, dir_entry));
                if self.collapsed_entries.remove(&collapsed_entry) {
                    self.project
                        .update(cx, |project, cx| {
                            project.expand_entry(*worktree_id, entry_id, cx)
                        })
                        .unwrap_or_else(|| Task::ready(Ok(())))
                        .detach_and_log_err(cx);
                } else {
                    self.collapsed_entries.insert(collapsed_entry);
                    fold = true;
                }
            }
            PanelEntry::Fs(FsEntry::File(FsEntryFile {
                worktree_id,
                buffer_id,
                ..
            })) => {
                let collapsed_entry = CollapsedEntry::File(*worktree_id, *buffer_id);
                buffers_to_toggle.insert(*buffer_id);
                if !self.collapsed_entries.remove(&collapsed_entry) {
                    self.collapsed_entries.insert(collapsed_entry);
                    fold = true;
                }
            }
            PanelEntry::Fs(FsEntry::ExternalFile(external_file)) => {
                let collapsed_entry = CollapsedEntry::ExternalFile(external_file.buffer_id);
                buffers_to_toggle.insert(external_file.buffer_id);
                if !self.collapsed_entries.remove(&collapsed_entry) {
                    self.collapsed_entries.insert(collapsed_entry);
                    fold = true;
                }
            }
            PanelEntry::FoldedDirs(FoldedDirsEntry {
                worktree_id,
                entries: dir_entries,
                ..
            }) => {
                if let Some(dir_entry) = dir_entries.first() {
                    let entry_id = dir_entry.id;
                    let collapsed_entry = CollapsedEntry::Dir(*worktree_id, entry_id);
                    buffers_to_toggle
                        .extend(self.buffers_inside_directory(*worktree_id, dir_entry));
                    if self.collapsed_entries.remove(&collapsed_entry) {
                        self.project
                            .update(cx, |project, cx| {
                                project.expand_entry(*worktree_id, entry_id, cx)
                            })
                            .unwrap_or_else(|| Task::ready(Ok(())))
                            .detach_and_log_err(cx);
                    } else {
                        self.collapsed_entries.insert(collapsed_entry);
                        fold = true;
                    }
                }
            }
            PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => {
                let collapsed_entry = CollapsedEntry::Excerpt(excerpt.buffer_id, excerpt.id);
                if !self.collapsed_entries.remove(&collapsed_entry) {
                    self.collapsed_entries.insert(collapsed_entry);
                }
            }
            PanelEntry::Search(_) | PanelEntry::Outline(..) => return,
        }

        active_editor.update(cx, |editor, cx| {
            buffers_to_toggle.retain(|buffer_id| {
                let folded = editor.is_buffer_folded(*buffer_id, cx);
                if fold { !folded } else { folded }
            });
        });

        self.select_entry(entry.clone(), true, window, cx);
        if buffers_to_toggle.is_empty() {
            self.update_cached_entries(None, window, cx);
        } else {
            self.toggle_buffers_fold(buffers_to_toggle, fold, window, cx)
                .detach();
        }
    }

    fn toggle_buffers_fold(
        &self,
        buffers: HashSet<BufferId>,
        fold: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let Some(active_editor) = self.active_editor() else {
            return Task::ready(());
        };
        cx.spawn_in(window, async move |outline_panel, cx| {
            outline_panel
                .update_in(cx, |outline_panel, window, cx| {
                    active_editor.update(cx, |editor, cx| {
                        for buffer_id in buffers {
                            outline_panel
                                .preserve_selection_on_buffer_fold_toggles
                                .insert(buffer_id);
                            if fold {
                                editor.fold_buffer(buffer_id, cx);
                            } else {
                                editor.unfold_buffer(buffer_id, cx);
                            }
                        }
                    });
                    if let Some(selection) = outline_panel.selected_entry().cloned() {
                        outline_panel.scroll_editor_to_entry(&selection, false, false, window, cx);
                    }
                })
                .ok();
        })
    }

    fn copy_path(
        &mut self,
        _: &zed_actions::workspace::CopyPath,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(clipboard_text) = self
            .selected_entry()
            .and_then(|entry| self.abs_path(entry, cx))
            .map(|p| p.to_string_lossy().to_string())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(clipboard_text));
        }
    }

    fn copy_relative_path(
        &mut self,
        _: &zed_actions::workspace::CopyRelativePath,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(clipboard_text) = self
            .selected_entry()
            .and_then(|entry| match entry {
                PanelEntry::Fs(entry) => self.relative_path(entry, cx),
                PanelEntry::FoldedDirs(folded_dirs) => {
                    folded_dirs.entries.last().map(|entry| entry.path.clone())
                }
                PanelEntry::Search(_) | PanelEntry::Outline(..) => None,
            })
            .map(|p| p.to_string_lossy().to_string())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(clipboard_text));
        }
    }

    fn reveal_in_finder(
        &mut self,
        _: &RevealInFileManager,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(abs_path) = self
            .selected_entry()
            .and_then(|entry| self.abs_path(entry, cx))
        {
            cx.reveal_path(&abs_path);
        }
    }

    fn open_in_terminal(
        &mut self,
        _: &OpenInTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected_entry = self.selected_entry();
        let abs_path = selected_entry.and_then(|entry| self.abs_path(entry, cx));
        let working_directory = if let (
            Some(abs_path),
            Some(PanelEntry::Fs(FsEntry::File(..) | FsEntry::ExternalFile(..))),
        ) = (&abs_path, selected_entry)
        {
            abs_path.parent().map(|p| p.to_owned())
        } else {
            abs_path
        };

        if let Some(working_directory) = working_directory {
            window.dispatch_action(
                workspace::OpenTerminal { working_directory }.boxed_clone(),
                cx,
            )
        }
    }

    fn reveal_entry_for_selection(
        &mut self,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.active
            || !OutlinePanelSettings::get_global(cx).auto_reveal_entries
            || self.focus_handle.contains_focused(window, cx)
        {
            return;
        }
        let project = self.project.clone();
        self.reveal_selection_task = cx.spawn_in(window, async move |outline_panel, cx| {
            cx.background_executor().timer(UPDATE_DEBOUNCE).await;
            let entry_with_selection =
                outline_panel.update_in(cx, |outline_panel, window, cx| {
                    outline_panel.location_for_editor_selection(&editor, window, cx)
                })?;
            let Some(entry_with_selection) = entry_with_selection else {
                outline_panel.update(cx, |outline_panel, cx| {
                    outline_panel.selected_entry = SelectedEntry::None;
                    cx.notify();
                })?;
                return Ok(());
            };
            let related_buffer_entry = match &entry_with_selection {
                PanelEntry::Fs(FsEntry::File(FsEntryFile {
                    worktree_id,
                    buffer_id,
                    ..
                })) => project.update(cx, |project, cx| {
                    let entry_id = project
                        .buffer_for_id(*buffer_id, cx)
                        .and_then(|buffer| buffer.read(cx).entry_id(cx));
                    project
                        .worktree_for_id(*worktree_id, cx)
                        .zip(entry_id)
                        .and_then(|(worktree, entry_id)| {
                            let entry = worktree.read(cx).entry_for_id(entry_id)?.clone();
                            Some((worktree, entry))
                        })
                })?,
                PanelEntry::Outline(outline_entry) => {
                    let (buffer_id, excerpt_id) = outline_entry.ids();
                    outline_panel.update(cx, |outline_panel, cx| {
                        outline_panel
                            .collapsed_entries
                            .remove(&CollapsedEntry::ExternalFile(buffer_id));
                        outline_panel
                            .collapsed_entries
                            .remove(&CollapsedEntry::Excerpt(buffer_id, excerpt_id));
                        let project = outline_panel.project.read(cx);
                        let entry_id = project
                            .buffer_for_id(buffer_id, cx)
                            .and_then(|buffer| buffer.read(cx).entry_id(cx));

                        entry_id.and_then(|entry_id| {
                            project
                                .worktree_for_entry(entry_id, cx)
                                .and_then(|worktree| {
                                    let worktree_id = worktree.read(cx).id();
                                    outline_panel
                                        .collapsed_entries
                                        .remove(&CollapsedEntry::File(worktree_id, buffer_id));
                                    let entry = worktree.read(cx).entry_for_id(entry_id)?.clone();
                                    Some((worktree, entry))
                                })
                        })
                    })?
                }
                PanelEntry::Fs(FsEntry::ExternalFile(..)) => None,
                PanelEntry::Search(SearchEntry { match_range, .. }) => match_range
                    .start
                    .buffer_id
                    .or(match_range.end.buffer_id)
                    .map(|buffer_id| {
                        outline_panel.update(cx, |outline_panel, cx| {
                            outline_panel
                                .collapsed_entries
                                .remove(&CollapsedEntry::ExternalFile(buffer_id));
                            let project = project.read(cx);
                            let entry_id = project
                                .buffer_for_id(buffer_id, cx)
                                .and_then(|buffer| buffer.read(cx).entry_id(cx));

                            entry_id.and_then(|entry_id| {
                                project
                                    .worktree_for_entry(entry_id, cx)
                                    .and_then(|worktree| {
                                        let worktree_id = worktree.read(cx).id();
                                        outline_panel
                                            .collapsed_entries
                                            .remove(&CollapsedEntry::File(worktree_id, buffer_id));
                                        let entry =
                                            worktree.read(cx).entry_for_id(entry_id)?.clone();
                                        Some((worktree, entry))
                                    })
                            })
                        })
                    })
                    .transpose()?
                    .flatten(),
                _ => return anyhow::Ok(()),
            };
            if let Some((worktree, buffer_entry)) = related_buffer_entry {
                outline_panel.update(cx, |outline_panel, cx| {
                    let worktree_id = worktree.read(cx).id();
                    let mut dirs_to_expand = Vec::new();
                    {
                        let mut traversal = worktree.read(cx).traverse_from_path(
                            true,
                            true,
                            true,
                            buffer_entry.path.as_ref(),
                        );
                        let mut current_entry = buffer_entry;
                        loop {
                            if current_entry.is_dir()
                                && outline_panel
                                    .collapsed_entries
                                    .remove(&CollapsedEntry::Dir(worktree_id, current_entry.id))
                            {
                                dirs_to_expand.push(current_entry.id);
                            }

                            if traversal.back_to_parent() {
                                if let Some(parent_entry) = traversal.entry() {
                                    current_entry = parent_entry.clone();
                                    continue;
                                }
                            }
                            break;
                        }
                    }
                    for dir_to_expand in dirs_to_expand {
                        project
                            .update(cx, |project, cx| {
                                project.expand_entry(worktree_id, dir_to_expand, cx)
                            })
                            .unwrap_or_else(|| Task::ready(Ok(())))
                            .detach_and_log_err(cx)
                    }
                })?
            }

            outline_panel.update_in(cx, |outline_panel, window, cx| {
                outline_panel.select_entry(entry_with_selection, false, window, cx);
                outline_panel.update_cached_entries(None, window, cx);
            })?;

            anyhow::Ok(())
        });
    }

    fn render_excerpt(
        &self,
        excerpt: &OutlineEntryExcerpt,
        depth: usize,
        window: &mut Window,
        cx: &mut Context<OutlinePanel>,
    ) -> Option<Stateful<Div>> {
        let item_id = ElementId::from(excerpt.id.to_proto() as usize);
        let is_active = match self.selected_entry() {
            Some(PanelEntry::Outline(OutlineEntry::Excerpt(selected_excerpt))) => {
                selected_excerpt.buffer_id == excerpt.buffer_id && selected_excerpt.id == excerpt.id
            }
            _ => false,
        };
        let has_outlines = self
            .excerpts
            .get(&excerpt.buffer_id)
            .and_then(|excerpts| match &excerpts.get(&excerpt.id)?.outlines {
                ExcerptOutlines::Outlines(outlines) => Some(outlines),
                ExcerptOutlines::Invalidated(outlines) => Some(outlines),
                ExcerptOutlines::NotFetched => None,
            })
            .map_or(false, |outlines| !outlines.is_empty());
        let is_expanded = !self
            .collapsed_entries
            .contains(&CollapsedEntry::Excerpt(excerpt.buffer_id, excerpt.id));
        let color = entry_label_color(is_active);
        let icon = if has_outlines {
            FileIcons::get_chevron_icon(is_expanded, cx)
                .map(|icon_path| Icon::from_path(icon_path).color(color).into_any_element())
        } else {
            None
        }
        .unwrap_or_else(empty_icon);

        let label = self.excerpt_label(excerpt.buffer_id, &excerpt.range, cx)?;
        let label_element = Label::new(label)
            .single_line()
            .color(color)
            .into_any_element();

        Some(self.entry_element(
            PanelEntry::Outline(OutlineEntry::Excerpt(excerpt.clone())),
            item_id,
            depth,
            Some(icon),
            is_active,
            label_element,
            window,
            cx,
        ))
    }

    fn excerpt_label(
        &self,
        buffer_id: BufferId,
        range: &ExcerptRange<language::Anchor>,
        cx: &App,
    ) -> Option<String> {
        let buffer_snapshot = self.buffer_snapshot_for_id(buffer_id, cx)?;
        let excerpt_range = range.context.to_point(&buffer_snapshot);
        Some(format!(
            "Lines {}- {}",
            excerpt_range.start.row + 1,
            excerpt_range.end.row + 1,
        ))
    }

    fn render_outline(
        &self,
        outline: &OutlineEntryOutline,
        depth: usize,
        string_match: Option<&StringMatch>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let item_id = ElementId::from(SharedString::from(format!(
            "{:?}|{:?}{:?}|{:?}",
            outline.buffer_id, outline.excerpt_id, outline.outline.range, &outline.outline.text,
        )));

        let label_element = outline::render_item(
            &outline.outline,
            string_match
                .map(|string_match| string_match.ranges().collect::<Vec<_>>())
                .unwrap_or_default(),
            cx,
        )
        .into_any_element();

        let is_active = match self.selected_entry() {
            Some(PanelEntry::Outline(OutlineEntry::Outline(selected))) => {
                outline == selected && outline.outline == selected.outline
            }
            _ => false,
        };

        let icon = if self.is_singleton_active(cx) {
            None
        } else {
            Some(empty_icon())
        };

        self.entry_element(
            PanelEntry::Outline(OutlineEntry::Outline(outline.clone())),
            item_id,
            depth,
            icon,
            is_active,
            label_element,
            window,
            cx,
        )
    }

    fn render_entry(
        &self,
        rendered_entry: &FsEntry,
        depth: usize,
        string_match: Option<&StringMatch>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let settings = OutlinePanelSettings::get_global(cx);
        let is_active = match self.selected_entry() {
            Some(PanelEntry::Fs(selected_entry)) => selected_entry == rendered_entry,
            _ => false,
        };
        let (item_id, label_element, icon) = match rendered_entry {
            FsEntry::File(FsEntryFile {
                worktree_id, entry, ..
            }) => {
                let name = self.entry_name(worktree_id, entry, cx);
                let color =
                    entry_git_aware_label_color(entry.git_summary, entry.is_ignored, is_active);
                let icon = if settings.file_icons {
                    FileIcons::get_icon(&entry.path, cx)
                        .map(|icon_path| Icon::from_path(icon_path).color(color).into_any_element())
                } else {
                    None
                };
                (
                    ElementId::from(entry.id.to_proto() as usize),
                    HighlightedLabel::new(
                        name,
                        string_match
                            .map(|string_match| string_match.positions.clone())
                            .unwrap_or_default(),
                    )
                    .color(color)
                    .into_any_element(),
                    icon.unwrap_or_else(empty_icon),
                )
            }
            FsEntry::Directory(directory) => {
                let name = self.entry_name(&directory.worktree_id, &directory.entry, cx);

                let is_expanded = !self.collapsed_entries.contains(&CollapsedEntry::Dir(
                    directory.worktree_id,
                    directory.entry.id,
                ));
                let color = entry_git_aware_label_color(
                    directory.entry.git_summary,
                    directory.entry.is_ignored,
                    is_active,
                );
                let icon = if settings.folder_icons {
                    FileIcons::get_folder_icon(is_expanded, cx)
                } else {
                    FileIcons::get_chevron_icon(is_expanded, cx)
                }
                .map(Icon::from_path)
                .map(|icon| icon.color(color).into_any_element());
                (
                    ElementId::from(directory.entry.id.to_proto() as usize),
                    HighlightedLabel::new(
                        name,
                        string_match
                            .map(|string_match| string_match.positions.clone())
                            .unwrap_or_default(),
                    )
                    .color(color)
                    .into_any_element(),
                    icon.unwrap_or_else(empty_icon),
                )
            }
            FsEntry::ExternalFile(external_file) => {
                let color = entry_label_color(is_active);
                let (icon, name) = match self.buffer_snapshot_for_id(external_file.buffer_id, cx) {
                    Some(buffer_snapshot) => match buffer_snapshot.file() {
                        Some(file) => {
                            let path = file.path();
                            let icon = if settings.file_icons {
                                FileIcons::get_icon(path.as_ref(), cx)
                            } else {
                                None
                            }
                            .map(Icon::from_path)
                            .map(|icon| icon.color(color).into_any_element());
                            (icon, file_name(path.as_ref()))
                        }
                        None => (None, "Untitled".to_string()),
                    },
                    None => (None, "Unknown buffer".to_string()),
                };
                (
                    ElementId::from(external_file.buffer_id.to_proto() as usize),
                    HighlightedLabel::new(
                        name,
                        string_match
                            .map(|string_match| string_match.positions.clone())
                            .unwrap_or_default(),
                    )
                    .color(color)
                    .into_any_element(),
                    icon.unwrap_or_else(empty_icon),
                )
            }
        };

        self.entry_element(
            PanelEntry::Fs(rendered_entry.clone()),
            item_id,
            depth,
            Some(icon),
            is_active,
            label_element,
            window,
            cx,
        )
    }

    fn render_folded_dirs(
        &self,
        folded_dir: &FoldedDirsEntry,
        depth: usize,
        string_match: Option<&StringMatch>,
        window: &mut Window,
        cx: &mut Context<OutlinePanel>,
    ) -> Stateful<Div> {
        let settings = OutlinePanelSettings::get_global(cx);
        let is_active = match self.selected_entry() {
            Some(PanelEntry::FoldedDirs(selected_dirs)) => {
                selected_dirs.worktree_id == folded_dir.worktree_id
                    && selected_dirs.entries == folded_dir.entries
            }
            _ => false,
        };
        let (item_id, label_element, icon) = {
            let name = self.dir_names_string(&folded_dir.entries, folded_dir.worktree_id, cx);

            let is_expanded = folded_dir.entries.iter().all(|dir| {
                !self
                    .collapsed_entries
                    .contains(&CollapsedEntry::Dir(folded_dir.worktree_id, dir.id))
            });
            let is_ignored = folded_dir.entries.iter().any(|entry| entry.is_ignored);
            let git_status = folded_dir
                .entries
                .first()
                .map(|entry| entry.git_summary)
                .unwrap_or_default();
            let color = entry_git_aware_label_color(git_status, is_ignored, is_active);
            let icon = if settings.folder_icons {
                FileIcons::get_folder_icon(is_expanded, cx)
            } else {
                FileIcons::get_chevron_icon(is_expanded, cx)
            }
            .map(Icon::from_path)
            .map(|icon| icon.color(color).into_any_element());
            (
                ElementId::from(
                    folded_dir
                        .entries
                        .last()
                        .map(|entry| entry.id.to_proto())
                        .unwrap_or_else(|| folded_dir.worktree_id.to_proto())
                        as usize,
                ),
                HighlightedLabel::new(
                    name,
                    string_match
                        .map(|string_match| string_match.positions.clone())
                        .unwrap_or_default(),
                )
                .color(color)
                .into_any_element(),
                icon.unwrap_or_else(empty_icon),
            )
        };

        self.entry_element(
            PanelEntry::FoldedDirs(folded_dir.clone()),
            item_id,
            depth,
            Some(icon),
            is_active,
            label_element,
            window,
            cx,
        )
    }

    fn render_search_match(
        &mut self,
        multi_buffer_snapshot: Option<&MultiBufferSnapshot>,
        match_range: &Range<editor::Anchor>,
        render_data: &Arc<OnceLock<SearchData>>,
        kind: SearchKind,
        depth: usize,
        string_match: Option<&StringMatch>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Stateful<Div>> {
        let search_data = match render_data.get() {
            Some(search_data) => search_data,
            None => {
                if let ItemsDisplayMode::Search(search_state) = &mut self.mode {
                    if let Some(multi_buffer_snapshot) = multi_buffer_snapshot {
                        search_state
                            .highlight_search_match_tx
                            .try_send(HighlightArguments {
                                multi_buffer_snapshot: multi_buffer_snapshot.clone(),
                                match_range: match_range.clone(),
                                search_data: Arc::clone(render_data),
                            })
                            .ok();
                    }
                }
                return None;
            }
        };
        let search_matches = string_match
            .iter()
            .flat_map(|string_match| string_match.ranges())
            .collect::<Vec<_>>();
        let match_ranges = if search_matches.is_empty() {
            &search_data.search_match_indices
        } else {
            &search_matches
        };
        let label_element = outline::render_item(
            &OutlineItem {
                depth,
                annotation_range: None,
                range: search_data.context_range.clone(),
                text: search_data.context_text.clone(),
                highlight_ranges: search_data
                    .highlights_data
                    .get()
                    .cloned()
                    .unwrap_or_default(),
                name_ranges: search_data.search_match_indices.clone(),
                body_range: Some(search_data.context_range.clone()),
            },
            match_ranges.iter().cloned(),
            cx,
        );
        let truncated_contents_label = || Label::new(TRUNCATED_CONTEXT_MARK);
        let entire_label = h_flex()
            .justify_center()
            .p_0()
            .when(search_data.truncated_left, |parent| {
                parent.child(truncated_contents_label())
            })
            .child(label_element)
            .when(search_data.truncated_right, |parent| {
                parent.child(truncated_contents_label())
            })
            .into_any_element();

        let is_active = match self.selected_entry() {
            Some(PanelEntry::Search(SearchEntry {
                match_range: selected_match_range,
                ..
            })) => match_range == selected_match_range,
            _ => false,
        };
        Some(self.entry_element(
            PanelEntry::Search(SearchEntry {
                kind,
                match_range: match_range.clone(),
                render_data: render_data.clone(),
            }),
            ElementId::from(SharedString::from(format!("search-{match_range:?}"))),
            depth,
            None,
            is_active,
            entire_label,
            window,
            cx,
        ))
    }

    fn entry_element(
        &self,
        rendered_entry: PanelEntry,
        item_id: ElementId,
        depth: usize,
        icon_element: Option<AnyElement>,
        is_active: bool,
        label_element: gpui::AnyElement,
        window: &mut Window,
        cx: &mut Context<OutlinePanel>,
    ) -> Stateful<Div> {
        let settings = OutlinePanelSettings::get_global(cx);
        div()
            .text_ui(cx)
            .id(item_id.clone())
            .on_click({
                let clicked_entry = rendered_entry.clone();
                cx.listener(move |outline_panel, event: &gpui::ClickEvent, window, cx| {
                    if event.down.button == MouseButton::Right || event.down.first_mouse {
                        return;
                    }
                    let change_focus = event.down.click_count > 1;
                    outline_panel.toggle_expanded(&clicked_entry, window, cx);
                    outline_panel.scroll_editor_to_entry(
                        &clicked_entry,
                        true,
                        change_focus,
                        window,
                        cx,
                    );
                })
            })
            .cursor_pointer()
            .child(
                ListItem::new(item_id)
                    .indent_level(depth)
                    .indent_step_size(px(settings.indent_size))
                    .toggle_state(is_active)
                    .when_some(icon_element, |list_item, icon_element| {
                        list_item.child(h_flex().child(icon_element))
                    })
                    .child(h_flex().h_6().child(label_element).ml_1())
                    .on_secondary_mouse_down(cx.listener(
                        move |outline_panel, event: &MouseDownEvent, window, cx| {
                            // Stop propagation to prevent the catch-all context menu for the project
                            // panel from being deployed.
                            cx.stop_propagation();
                            outline_panel.deploy_context_menu(
                                event.position,
                                rendered_entry.clone(),
                                window,
                                cx,
                            )
                        },
                    )),
            )
            .border_1()
            .border_r_2()
            .rounded_none()
            .hover(|style| {
                if is_active {
                    style
                } else {
                    let hover_color = cx.theme().colors().ghost_element_hover;
                    style.bg(hover_color).border_color(hover_color)
                }
            })
            .when(
                is_active && self.focus_handle.contains_focused(window, cx),
                |div| div.border_color(Color::Selected.color(cx)),
            )
    }

    fn entry_name(&self, worktree_id: &WorktreeId, entry: &Entry, cx: &App) -> String {
        let name = match self.project.read(cx).worktree_for_id(*worktree_id, cx) {
            Some(worktree) => {
                let worktree = worktree.read(cx);
                match worktree.snapshot().root_entry() {
                    Some(root_entry) => {
                        if root_entry.id == entry.id {
                            file_name(worktree.abs_path().as_ref())
                        } else {
                            let path = worktree.absolutize(entry.path.as_ref()).ok();
                            let path = path.as_deref().unwrap_or_else(|| entry.path.as_ref());
                            file_name(path)
                        }
                    }
                    None => {
                        let path = worktree.absolutize(entry.path.as_ref()).ok();
                        let path = path.as_deref().unwrap_or_else(|| entry.path.as_ref());
                        file_name(path)
                    }
                }
            }
            None => file_name(entry.path.as_ref()),
        };
        name
    }

    fn update_fs_entries(
        &mut self,
        active_editor: Entity<Editor>,
        debounce: Option<Duration>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.active {
            return;
        }

        let auto_fold_dirs = OutlinePanelSettings::get_global(cx).auto_fold_dirs;
        let active_multi_buffer = active_editor.read(cx).buffer().clone();
        let new_entries = self.new_entries_for_fs_update.clone();
        let repo_snapshots = self.project.update(cx, |project, cx| {
            project.git_store().read(cx).repo_snapshots(cx)
        });
        self.updating_fs_entries = true;
        self.fs_entries_update_task = cx.spawn_in(window, async move |outline_panel, cx| {
            if let Some(debounce) = debounce {
                cx.background_executor().timer(debounce).await;
            }

            let mut new_collapsed_entries = HashSet::default();
            let mut new_unfolded_dirs = HashMap::default();
            let mut root_entries = HashSet::default();
            let mut new_excerpts = HashMap::<BufferId, HashMap<ExcerptId, Excerpt>>::default();
            let Ok(buffer_excerpts) = outline_panel.update(cx, |outline_panel, cx| {
                let git_store = outline_panel.project.read(cx).git_store().clone();
                new_collapsed_entries = outline_panel.collapsed_entries.clone();
                new_unfolded_dirs = outline_panel.unfolded_dirs.clone();
                let multi_buffer_snapshot = active_multi_buffer.read(cx).snapshot(cx);
                let buffer_excerpts = multi_buffer_snapshot.excerpts().fold(
                    HashMap::default(),
                    |mut buffer_excerpts, (excerpt_id, buffer_snapshot, excerpt_range)| {
                        let buffer_id = buffer_snapshot.remote_id();
                        let file = File::from_dyn(buffer_snapshot.file());
                        let entry_id = file.and_then(|file| file.project_entry_id(cx));
                        let worktree = file.map(|file| file.worktree.read(cx).snapshot());
                        let is_new = new_entries.contains(&excerpt_id)
                            || !outline_panel.excerpts.contains_key(&buffer_id);
                        let is_folded = active_editor.read(cx).is_buffer_folded(buffer_id, cx);
                        let status = git_store
                            .read(cx)
                            .repository_and_path_for_buffer_id(buffer_id, cx)
                            .and_then(|(repo, path)| {
                                Some(repo.read(cx).status_for_path(&path)?.status)
                            });
                        buffer_excerpts
                            .entry(buffer_id)
                            .or_insert_with(|| {
                                (is_new, is_folded, Vec::new(), entry_id, worktree, status)
                            })
                            .2
                            .push(excerpt_id);

                        let outlines = match outline_panel
                            .excerpts
                            .get(&buffer_id)
                            .and_then(|excerpts| excerpts.get(&excerpt_id))
                        {
                            Some(old_excerpt) => match &old_excerpt.outlines {
                                ExcerptOutlines::Outlines(outlines) => {
                                    ExcerptOutlines::Outlines(outlines.clone())
                                }
                                ExcerptOutlines::Invalidated(_) => ExcerptOutlines::NotFetched,
                                ExcerptOutlines::NotFetched => ExcerptOutlines::NotFetched,
                            },
                            None => ExcerptOutlines::NotFetched,
                        };
                        new_excerpts.entry(buffer_id).or_default().insert(
                            excerpt_id,
                            Excerpt {
                                range: excerpt_range,
                                outlines,
                            },
                        );
                        buffer_excerpts
                    },
                );
                buffer_excerpts
            }) else {
                return;
            };

            let Some((
                new_collapsed_entries,
                new_unfolded_dirs,
                new_fs_entries,
                new_depth_map,
                new_children_count,
            )) = cx
                .background_spawn(async move {
                    let mut processed_external_buffers = HashSet::default();
                    let mut new_worktree_entries =
                        BTreeMap::<WorktreeId, HashMap<ProjectEntryId, GitEntry>>::default();
                    let mut worktree_excerpts = HashMap::<
                        WorktreeId,
                        HashMap<ProjectEntryId, (BufferId, Vec<ExcerptId>)>,
                    >::default();
                    let mut external_excerpts = HashMap::default();

                    for (buffer_id, (is_new, is_folded, excerpts, entry_id, worktree, status)) in
                        buffer_excerpts
                    {
                        if is_folded {
                            match &worktree {
                                Some(worktree) => {
                                    new_collapsed_entries
                                        .insert(CollapsedEntry::File(worktree.id(), buffer_id));
                                }
                                None => {
                                    new_collapsed_entries
                                        .insert(CollapsedEntry::ExternalFile(buffer_id));
                                }
                            }
                        } else if is_new {
                            match &worktree {
                                Some(worktree) => {
                                    new_collapsed_entries
                                        .remove(&CollapsedEntry::File(worktree.id(), buffer_id));
                                }
                                None => {
                                    new_collapsed_entries
                                        .remove(&CollapsedEntry::ExternalFile(buffer_id));
                                }
                            }
                        }

                        if let Some(worktree) = worktree {
                            let worktree_id = worktree.id();
                            let unfolded_dirs = new_unfolded_dirs.entry(worktree_id).or_default();

                            match entry_id.and_then(|id| worktree.entry_for_id(id)).cloned() {
                                Some(entry) => {
                                    let entry = GitEntry {
                                        git_summary: status
                                            .map(|status| status.summary())
                                            .unwrap_or_default(),
                                        entry,
                                    };
                                    let mut traversal = GitTraversal::new(
                                        &repo_snapshots,
                                        worktree.traverse_from_path(
                                            true,
                                            true,
                                            true,
                                            entry.path.as_ref(),
                                        ),
                                    );

                                    let mut entries_to_add = HashMap::default();
                                    worktree_excerpts
                                        .entry(worktree_id)
                                        .or_default()
                                        .insert(entry.id, (buffer_id, excerpts));
                                    let mut current_entry = entry;
                                    loop {
                                        if current_entry.is_dir() {
                                            let is_root =
                                                worktree.root_entry().map(|entry| entry.id)
                                                    == Some(current_entry.id);
                                            if is_root {
                                                root_entries.insert(current_entry.id);
                                                if auto_fold_dirs {
                                                    unfolded_dirs.insert(current_entry.id);
                                                }
                                            }
                                            if is_new {
                                                new_collapsed_entries.remove(&CollapsedEntry::Dir(
                                                    worktree_id,
                                                    current_entry.id,
                                                ));
                                            }
                                        }

                                        let new_entry_added = entries_to_add
                                            .insert(current_entry.id, current_entry)
                                            .is_none();
                                        if new_entry_added && traversal.back_to_parent() {
                                            if let Some(parent_entry) = traversal.entry() {
                                                current_entry = parent_entry.to_owned();
                                                continue;
                                            }
                                        }
                                        break;
                                    }
                                    new_worktree_entries
                                        .entry(worktree_id)
                                        .or_insert_with(HashMap::default)
                                        .extend(entries_to_add);
                                }
                                None => {
                                    if processed_external_buffers.insert(buffer_id) {
                                        external_excerpts
                                            .entry(buffer_id)
                                            .or_insert_with(Vec::new)
                                            .extend(excerpts);
                                    }
                                }
                            }
                        } else if processed_external_buffers.insert(buffer_id) {
                            external_excerpts
                                .entry(buffer_id)
                                .or_insert_with(Vec::new)
                                .extend(excerpts);
                        }
                    }

                    let mut new_children_count =
                        HashMap::<WorktreeId, HashMap<Arc<Path>, FsChildren>>::default();

                    let worktree_entries = new_worktree_entries
                        .into_iter()
                        .map(|(worktree_id, entries)| {
                            let mut entries = entries.into_values().collect::<Vec<_>>();
                            entries.sort_by(|a, b| a.path.as_ref().cmp(b.path.as_ref()));
                            (worktree_id, entries)
                        })
                        .flat_map(|(worktree_id, entries)| {
                            {
                                entries
                                    .into_iter()
                                    .filter_map(|entry| {
                                        if auto_fold_dirs {
                                            if let Some(parent) = entry.path.parent() {
                                                let children = new_children_count
                                                    .entry(worktree_id)
                                                    .or_default()
                                                    .entry(Arc::from(parent))
                                                    .or_default();
                                                if entry.is_dir() {
                                                    children.dirs += 1;
                                                } else {
                                                    children.files += 1;
                                                }
                                            }
                                        }

                                        if entry.is_dir() {
                                            Some(FsEntry::Directory(FsEntryDirectory {
                                                worktree_id,
                                                entry,
                                            }))
                                        } else {
                                            let (buffer_id, excerpts) = worktree_excerpts
                                                .get_mut(&worktree_id)
                                                .and_then(|worktree_excerpts| {
                                                    worktree_excerpts.remove(&entry.id)
                                                })?;
                                            Some(FsEntry::File(FsEntryFile {
                                                worktree_id,
                                                buffer_id,
                                                entry,
                                                excerpts,
                                            }))
                                        }
                                    })
                                    .collect::<Vec<_>>()
                            }
                        })
                        .collect::<Vec<_>>();

                    let mut visited_dirs = Vec::new();
                    let mut new_depth_map = HashMap::default();
                    let new_visible_entries = external_excerpts
                        .into_iter()
                        .sorted_by_key(|(id, _)| *id)
                        .map(|(buffer_id, excerpts)| {
                            FsEntry::ExternalFile(FsEntryExternalFile {
                                buffer_id,
                                excerpts,
                            })
                        })
                        .chain(worktree_entries)
                        .filter(|visible_item| {
                            match visible_item {
                                FsEntry::Directory(directory) => {
                                    let parent_id = back_to_common_visited_parent(
                                        &mut visited_dirs,
                                        &directory.worktree_id,
                                        &directory.entry,
                                    );

                                    let mut depth = 0;
                                    if !root_entries.contains(&directory.entry.id) {
                                        if auto_fold_dirs {
                                            let children = new_children_count
                                                .get(&directory.worktree_id)
                                                .and_then(|children_count| {
                                                    children_count.get(&directory.entry.path)
                                                })
                                                .copied()
                                                .unwrap_or_default();

                                            if !children.may_be_fold_part()
                                                || (children.dirs == 0
                                                    && visited_dirs
                                                        .last()
                                                        .map(|(parent_dir_id, _)| {
                                                            new_unfolded_dirs
                                                                .get(&directory.worktree_id)
                                                                .map_or(true, |unfolded_dirs| {
                                                                    unfolded_dirs
                                                                        .contains(parent_dir_id)
                                                                })
                                                        })
                                                        .unwrap_or(true))
                                            {
                                                new_unfolded_dirs
                                                    .entry(directory.worktree_id)
                                                    .or_default()
                                                    .insert(directory.entry.id);
                                            }
                                        }

                                        depth = parent_id
                                            .and_then(|(worktree_id, id)| {
                                                new_depth_map.get(&(worktree_id, id)).copied()
                                            })
                                            .unwrap_or(0)
                                            + 1;
                                    };
                                    visited_dirs
                                        .push((directory.entry.id, directory.entry.path.clone()));
                                    new_depth_map
                                        .insert((directory.worktree_id, directory.entry.id), depth);
                                }
                                FsEntry::File(FsEntryFile {
                                    worktree_id,
                                    entry: file_entry,
                                    ..
                                }) => {
                                    let parent_id = back_to_common_visited_parent(
                                        &mut visited_dirs,
                                        worktree_id,
                                        file_entry,
                                    );
                                    let depth = if root_entries.contains(&file_entry.id) {
                                        0
                                    } else {
                                        parent_id
                                            .and_then(|(worktree_id, id)| {
                                                new_depth_map.get(&(worktree_id, id)).copied()
                                            })
                                            .unwrap_or(0)
                                            + 1
                                    };
                                    new_depth_map.insert((*worktree_id, file_entry.id), depth);
                                }
                                FsEntry::ExternalFile(..) => {
                                    visited_dirs.clear();
                                }
                            }

                            true
                        })
                        .collect::<Vec<_>>();

                    anyhow::Ok((
                        new_collapsed_entries,
                        new_unfolded_dirs,
                        new_visible_entries,
                        new_depth_map,
                        new_children_count,
                    ))
                })
                .await
                .log_err()
            else {
                return;
            };

            outline_panel
                .update_in(cx, |outline_panel, window, cx| {
                    outline_panel.updating_fs_entries = false;
                    outline_panel.new_entries_for_fs_update.clear();
                    outline_panel.excerpts = new_excerpts;
                    outline_panel.collapsed_entries = new_collapsed_entries;
                    outline_panel.unfolded_dirs = new_unfolded_dirs;
                    outline_panel.fs_entries = new_fs_entries;
                    outline_panel.fs_entries_depth = new_depth_map;
                    outline_panel.fs_children_count = new_children_count;
                    outline_panel.update_non_fs_items(window, cx);
                    outline_panel.update_cached_entries(debounce, window, cx);

                    cx.notify();
                })
                .ok();
        });
    }

    fn replace_active_editor(
        &mut self,
        new_active_item: Box<dyn ItemHandle>,
        new_active_editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_previous(window, cx);
        let buffer_search_subscription = cx.subscribe_in(
            &new_active_editor,
            window,
            |outline_panel: &mut Self,
             _,
             e: &SearchEvent,
             window: &mut Window,
             cx: &mut Context<Self>| {
                if matches!(e, SearchEvent::MatchesInvalidated) {
                    let update_cached_items = outline_panel.update_search_matches(window, cx);
                    if update_cached_items {
                        outline_panel.selected_entry.invalidate();
                        outline_panel.update_cached_entries(Some(UPDATE_DEBOUNCE), window, cx);
                    }
                };
                outline_panel.autoscroll(cx);
            },
        );
        self.active_item = Some(ActiveItem {
            _buffer_search_subscription: buffer_search_subscription,
            _editor_subscription: subscribe_for_editor_events(&new_active_editor, window, cx),
            item_handle: new_active_item.downgrade_item(),
            active_editor: new_active_editor.downgrade(),
        });
        self.new_entries_for_fs_update
            .extend(new_active_editor.read(cx).buffer().read(cx).excerpt_ids());
        self.selected_entry.invalidate();
        self.update_fs_entries(new_active_editor, None, window, cx);
    }

    fn clear_previous(&mut self, window: &mut Window, cx: &mut App) {
        self.fs_entries_update_task = Task::ready(());
        self.outline_fetch_tasks.clear();
        self.cached_entries_update_task = Task::ready(());
        self.reveal_selection_task = Task::ready(Ok(()));
        self.filter_editor
            .update(cx, |editor, cx| editor.clear(window, cx));
        self.collapsed_entries.clear();
        self.unfolded_dirs.clear();
        self.active_item = None;
        self.fs_entries.clear();
        self.fs_entries_depth.clear();
        self.fs_children_count.clear();
        self.excerpts.clear();
        self.cached_entries = Vec::new();
        self.selected_entry = SelectedEntry::None;
        self.pinned = false;
        self.mode = ItemsDisplayMode::Outline;
    }

    fn location_for_editor_selection(
        &self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PanelEntry> {
        let selection = editor.update(cx, |editor, cx| {
            editor.selections.newest::<language::Point>(cx).head()
        });
        let editor_snapshot = editor.update(cx, |editor, cx| editor.snapshot(window, cx));
        let multi_buffer = editor.read(cx).buffer();
        let multi_buffer_snapshot = multi_buffer.read(cx).snapshot(cx);
        let (excerpt_id, buffer, _) = editor
            .read(cx)
            .buffer()
            .read(cx)
            .excerpt_containing(selection, cx)?;
        let buffer_id = buffer.read(cx).remote_id();

        if editor.read(cx).is_buffer_folded(buffer_id, cx) {
            return self
                .fs_entries
                .iter()
                .find(|fs_entry| match fs_entry {
                    FsEntry::Directory(..) => false,
                    FsEntry::File(FsEntryFile {
                        buffer_id: other_buffer_id,
                        ..
                    })
                    | FsEntry::ExternalFile(FsEntryExternalFile {
                        buffer_id: other_buffer_id,
                        ..
                    }) => buffer_id == *other_buffer_id,
                })
                .cloned()
                .map(PanelEntry::Fs);
        }

        let selection_display_point = selection.to_display_point(&editor_snapshot);

        match &self.mode {
            ItemsDisplayMode::Search(search_state) => search_state
                .matches
                .iter()
                .rev()
                .min_by_key(|&(match_range, _)| {
                    let match_display_range =
                        match_range.clone().to_display_points(&editor_snapshot);
                    let start_distance = if selection_display_point < match_display_range.start {
                        match_display_range.start - selection_display_point
                    } else {
                        selection_display_point - match_display_range.start
                    };
                    let end_distance = if selection_display_point < match_display_range.end {
                        match_display_range.end - selection_display_point
                    } else {
                        selection_display_point - match_display_range.end
                    };
                    start_distance + end_distance
                })
                .and_then(|(closest_range, _)| {
                    self.cached_entries.iter().find_map(|cached_entry| {
                        if let PanelEntry::Search(SearchEntry { match_range, .. }) =
                            &cached_entry.entry
                        {
                            if match_range == closest_range {
                                Some(cached_entry.entry.clone())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                }),
            ItemsDisplayMode::Outline => self.outline_location(
                buffer_id,
                excerpt_id,
                multi_buffer_snapshot,
                editor_snapshot,
                selection_display_point,
            ),
        }
    }

    fn outline_location(
        &self,
        buffer_id: BufferId,
        excerpt_id: ExcerptId,
        multi_buffer_snapshot: editor::MultiBufferSnapshot,
        editor_snapshot: editor::EditorSnapshot,
        selection_display_point: DisplayPoint,
    ) -> Option<PanelEntry> {
        let excerpt_outlines = self
            .excerpts
            .get(&buffer_id)
            .and_then(|excerpts| excerpts.get(&excerpt_id))
            .into_iter()
            .flat_map(|excerpt| excerpt.iter_outlines())
            .flat_map(|outline| {
                let start = multi_buffer_snapshot
                    .anchor_in_excerpt(excerpt_id, outline.range.start)?
                    .to_display_point(&editor_snapshot);
                let end = multi_buffer_snapshot
                    .anchor_in_excerpt(excerpt_id, outline.range.end)?
                    .to_display_point(&editor_snapshot);
                Some((start..end, outline))
            })
            .collect::<Vec<_>>();

        let mut matching_outline_indices = Vec::new();
        let mut children = HashMap::default();
        let mut parents_stack = Vec::<(&Range<DisplayPoint>, &&Outline, usize)>::new();

        for (i, (outline_range, outline)) in excerpt_outlines.iter().enumerate() {
            if outline_range
                .to_inclusive()
                .contains(&selection_display_point)
            {
                matching_outline_indices.push(i);
            } else if (outline_range.start.row()..outline_range.end.row())
                .to_inclusive()
                .contains(&selection_display_point.row())
            {
                matching_outline_indices.push(i);
            }

            while let Some((parent_range, parent_outline, _)) = parents_stack.last() {
                if parent_outline.depth >= outline.depth
                    || !parent_range.contains(&outline_range.start)
                {
                    parents_stack.pop();
                } else {
                    break;
                }
            }
            if let Some((_, _, parent_index)) = parents_stack.last_mut() {
                children
                    .entry(*parent_index)
                    .or_insert_with(Vec::new)
                    .push(i);
            }
            parents_stack.push((outline_range, outline, i));
        }

        let outline_item = matching_outline_indices
            .into_iter()
            .flat_map(|i| Some((i, excerpt_outlines.get(i)?)))
            .filter(|(i, _)| {
                children
                    .get(i)
                    .map(|children| {
                        children.iter().all(|child_index| {
                            excerpt_outlines
                                .get(*child_index)
                                .map(|(child_range, _)| child_range.start > selection_display_point)
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(true)
            })
            .min_by_key(|(_, (outline_range, outline))| {
                let distance_from_start = if outline_range.start > selection_display_point {
                    outline_range.start - selection_display_point
                } else {
                    selection_display_point - outline_range.start
                };
                let distance_from_end = if outline_range.end > selection_display_point {
                    outline_range.end - selection_display_point
                } else {
                    selection_display_point - outline_range.end
                };

                (
                    cmp::Reverse(outline.depth),
                    distance_from_start + distance_from_end,
                )
            })
            .map(|(_, (_, outline))| *outline)
            .cloned();

        let closest_container = match outline_item {
            Some(outline) => PanelEntry::Outline(OutlineEntry::Outline(OutlineEntryOutline {
                buffer_id,
                excerpt_id,
                outline,
            })),
            None => {
                self.cached_entries.iter().rev().find_map(|cached_entry| {
                    match &cached_entry.entry {
                        PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => {
                            if excerpt.buffer_id == buffer_id && excerpt.id == excerpt_id {
                                Some(cached_entry.entry.clone())
                            } else {
                                None
                            }
                        }
                        PanelEntry::Fs(
                            FsEntry::ExternalFile(FsEntryExternalFile {
                                buffer_id: file_buffer_id,
                                excerpts: file_excerpts,
                            })
                            | FsEntry::File(FsEntryFile {
                                buffer_id: file_buffer_id,
                                excerpts: file_excerpts,
                                ..
                            }),
                        ) => {
                            if file_buffer_id == &buffer_id && file_excerpts.contains(&excerpt_id) {
                                Some(cached_entry.entry.clone())
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                })?
            }
        };
        Some(closest_container)
    }

    fn fetch_outdated_outlines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let excerpt_fetch_ranges = self.excerpt_fetch_ranges(cx);
        if excerpt_fetch_ranges.is_empty() {
            return;
        }

        let syntax_theme = cx.theme().syntax().clone();
        let first_update = Arc::new(AtomicBool::new(true));
        for (buffer_id, (buffer_snapshot, excerpt_ranges)) in excerpt_fetch_ranges {
            for (excerpt_id, excerpt_range) in excerpt_ranges {
                let syntax_theme = syntax_theme.clone();
                let buffer_snapshot = buffer_snapshot.clone();
                let first_update = first_update.clone();
                self.outline_fetch_tasks.insert(
                    (buffer_id, excerpt_id),
                    cx.spawn_in(window, async move |outline_panel, cx| {
                        let fetched_outlines = cx
                            .background_spawn(async move {
                                buffer_snapshot
                                    .outline_items_containing(
                                        excerpt_range.context,
                                        false,
                                        Some(&syntax_theme),
                                    )
                                    .unwrap_or_default()
                            })
                            .await;
                        outline_panel
                            .update_in(cx, |outline_panel, window, cx| {
                                if let Some(excerpt) = outline_panel
                                    .excerpts
                                    .entry(buffer_id)
                                    .or_default()
                                    .get_mut(&excerpt_id)
                                {
                                    let debounce = if first_update
                                        .fetch_and(false, atomic::Ordering::AcqRel)
                                    {
                                        None
                                    } else {
                                        Some(UPDATE_DEBOUNCE)
                                    };
                                    excerpt.outlines = ExcerptOutlines::Outlines(fetched_outlines);
                                    outline_panel.update_cached_entries(debounce, window, cx);
                                }
                            })
                            .ok();
                    }),
                );
            }
        }
    }

    fn is_singleton_active(&self, cx: &App) -> bool {
        self.active_editor().map_or(false, |active_editor| {
            active_editor.read(cx).buffer().read(cx).is_singleton()
        })
    }

    fn invalidate_outlines(&mut self, ids: &[ExcerptId]) {
        self.outline_fetch_tasks.clear();
        let mut ids = ids.iter().collect::<HashSet<_>>();
        for excerpts in self.excerpts.values_mut() {
            ids.retain(|id| {
                if let Some(excerpt) = excerpts.get_mut(id) {
                    excerpt.invalidate_outlines();
                    false
                } else {
                    true
                }
            });
            if ids.is_empty() {
                break;
            }
        }
    }

    fn excerpt_fetch_ranges(
        &self,
        cx: &App,
    ) -> HashMap<
        BufferId,
        (
            BufferSnapshot,
            HashMap<ExcerptId, ExcerptRange<language::Anchor>>,
        ),
    > {
        self.fs_entries
            .iter()
            .fold(HashMap::default(), |mut excerpts_to_fetch, fs_entry| {
                match fs_entry {
                    FsEntry::File(FsEntryFile {
                        buffer_id,
                        excerpts: file_excerpts,
                        ..
                    })
                    | FsEntry::ExternalFile(FsEntryExternalFile {
                        buffer_id,
                        excerpts: file_excerpts,
                    }) => {
                        let excerpts = self.excerpts.get(buffer_id);
                        for &file_excerpt in file_excerpts {
                            if let Some(excerpt) = excerpts
                                .and_then(|excerpts| excerpts.get(&file_excerpt))
                                .filter(|excerpt| excerpt.should_fetch_outlines())
                            {
                                match excerpts_to_fetch.entry(*buffer_id) {
                                    hash_map::Entry::Occupied(mut o) => {
                                        o.get_mut().1.insert(file_excerpt, excerpt.range.clone());
                                    }
                                    hash_map::Entry::Vacant(v) => {
                                        if let Some(buffer_snapshot) =
                                            self.buffer_snapshot_for_id(*buffer_id, cx)
                                        {
                                            v.insert((buffer_snapshot, HashMap::default()))
                                                .1
                                                .insert(file_excerpt, excerpt.range.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    FsEntry::Directory(..) => {}
                }
                excerpts_to_fetch
            })
    }

    fn buffer_snapshot_for_id(&self, buffer_id: BufferId, cx: &App) -> Option<BufferSnapshot> {
        let editor = self.active_editor()?;
        Some(
            editor
                .read(cx)
                .buffer()
                .read(cx)
                .buffer(buffer_id)?
                .read(cx)
                .snapshot(),
        )
    }

    fn abs_path(&self, entry: &PanelEntry, cx: &App) -> Option<PathBuf> {
        match entry {
            PanelEntry::Fs(
                FsEntry::File(FsEntryFile { buffer_id, .. })
                | FsEntry::ExternalFile(FsEntryExternalFile { buffer_id, .. }),
            ) => self
                .buffer_snapshot_for_id(*buffer_id, cx)
                .and_then(|buffer_snapshot| {
                    let file = File::from_dyn(buffer_snapshot.file())?;
                    file.worktree.read(cx).absolutize(&file.path).ok()
                }),
            PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory {
                worktree_id, entry, ..
            })) => self
                .project
                .read(cx)
                .worktree_for_id(*worktree_id, cx)?
                .read(cx)
                .absolutize(&entry.path)
                .ok(),
            PanelEntry::FoldedDirs(FoldedDirsEntry {
                worktree_id,
                entries: dirs,
                ..
            }) => dirs.last().and_then(|entry| {
                self.project
                    .read(cx)
                    .worktree_for_id(*worktree_id, cx)
                    .and_then(|worktree| worktree.read(cx).absolutize(&entry.path).ok())
            }),
            PanelEntry::Search(_) | PanelEntry::Outline(..) => None,
        }
    }

    fn relative_path(&self, entry: &FsEntry, cx: &App) -> Option<Arc<Path>> {
        match entry {
            FsEntry::ExternalFile(FsEntryExternalFile { buffer_id, .. }) => {
                let buffer_snapshot = self.buffer_snapshot_for_id(*buffer_id, cx)?;
                Some(buffer_snapshot.file()?.path().clone())
            }
            FsEntry::Directory(FsEntryDirectory { entry, .. }) => Some(entry.path.clone()),
            FsEntry::File(FsEntryFile { entry, .. }) => Some(entry.path.clone()),
        }
    }

    fn update_cached_entries(
        &mut self,
        debounce: Option<Duration>,
        window: &mut Window,
        cx: &mut Context<OutlinePanel>,
    ) {
        if !self.active {
            return;
        }

        let is_singleton = self.is_singleton_active(cx);
        let query = self.query(cx);
        self.updating_cached_entries = true;
        self.cached_entries_update_task = cx.spawn_in(window, async move |outline_panel, cx| {
            if let Some(debounce) = debounce {
                cx.background_executor().timer(debounce).await;
            }
            let Some(new_cached_entries) = outline_panel
                .update_in(cx, |outline_panel, window, cx| {
                    outline_panel.generate_cached_entries(is_singleton, query, window, cx)
                })
                .ok()
            else {
                return;
            };
            let (new_cached_entries, max_width_item_index) = new_cached_entries.await;
            outline_panel
                .update_in(cx, |outline_panel, window, cx| {
                    outline_panel.cached_entries = new_cached_entries;
                    outline_panel.max_width_item_index = max_width_item_index;
                    if outline_panel.selected_entry.is_invalidated()
                        || matches!(outline_panel.selected_entry, SelectedEntry::None)
                    {
                        if let Some(new_selected_entry) =
                            outline_panel.active_editor().and_then(|active_editor| {
                                outline_panel.location_for_editor_selection(
                                    &active_editor,
                                    window,
                                    cx,
                                )
                            })
                        {
                            outline_panel.select_entry(new_selected_entry, false, window, cx);
                        }
                    }

                    outline_panel.autoscroll(cx);
                    outline_panel.updating_cached_entries = false;
                    cx.notify();
                })
                .ok();
        });
    }

    fn generate_cached_entries(
        &self,
        is_singleton: bool,
        query: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<(Vec<CachedEntry>, Option<usize>)> {
        let project = self.project.clone();
        let Some(active_editor) = self.active_editor() else {
            return Task::ready((Vec::new(), None));
        };
        cx.spawn_in(window, async move |outline_panel, cx| {
            let mut generation_state = GenerationState::default();

            let Ok(()) = outline_panel.update(cx, |outline_panel, cx| {
                let auto_fold_dirs = OutlinePanelSettings::get_global(cx).auto_fold_dirs;
                let mut folded_dirs_entry = None::<(usize, FoldedDirsEntry)>;
                let track_matches = query.is_some();

                #[derive(Debug)]
                struct ParentStats {
                    path: Arc<Path>,
                    folded: bool,
                    expanded: bool,
                    depth: usize,
                }
                let mut parent_dirs = Vec::<ParentStats>::new();
                for entry in outline_panel.fs_entries.clone() {
                    let is_expanded = outline_panel.is_expanded(&entry);
                    let (depth, should_add) = match &entry {
                        FsEntry::Directory(directory_entry) => {
                            let mut should_add = true;
                            let is_root = project
                                .read(cx)
                                .worktree_for_id(directory_entry.worktree_id, cx)
                                .map_or(false, |worktree| {
                                    worktree.read(cx).root_entry() == Some(&directory_entry.entry)
                                });
                            let folded = auto_fold_dirs
                                && !is_root
                                && outline_panel
                                    .unfolded_dirs
                                    .get(&directory_entry.worktree_id)
                                    .map_or(true, |unfolded_dirs| {
                                        !unfolded_dirs.contains(&directory_entry.entry.id)
                                    });
                            let fs_depth = outline_panel
                                .fs_entries_depth
                                .get(&(directory_entry.worktree_id, directory_entry.entry.id))
                                .copied()
                                .unwrap_or(0);
                            while let Some(parent) = parent_dirs.last() {
                                if !is_root && directory_entry.entry.path.starts_with(&parent.path)
                                {
                                    break;
                                }
                                parent_dirs.pop();
                            }
                            let auto_fold = match parent_dirs.last() {
                                Some(parent) => {
                                    parent.folded
                                        && Some(parent.path.as_ref())
                                            == directory_entry.entry.path.parent()
                                        && outline_panel
                                            .fs_children_count
                                            .get(&directory_entry.worktree_id)
                                            .and_then(|entries| {
                                                entries.get(&directory_entry.entry.path)
                                            })
                                            .copied()
                                            .unwrap_or_default()
                                            .may_be_fold_part()
                                }
                                None => false,
                            };
                            let folded = folded || auto_fold;
                            let (depth, parent_expanded, parent_folded) = match parent_dirs.last() {
                                Some(parent) => {
                                    let parent_folded = parent.folded;
                                    let parent_expanded = parent.expanded;
                                    let new_depth = if parent_folded {
                                        parent.depth
                                    } else {
                                        parent.depth + 1
                                    };
                                    parent_dirs.push(ParentStats {
                                        path: directory_entry.entry.path.clone(),
                                        folded,
                                        expanded: parent_expanded && is_expanded,
                                        depth: new_depth,
                                    });
                                    (new_depth, parent_expanded, parent_folded)
                                }
                                None => {
                                    parent_dirs.push(ParentStats {
                                        path: directory_entry.entry.path.clone(),
                                        folded,
                                        expanded: is_expanded,
                                        depth: fs_depth,
                                    });
                                    (fs_depth, true, false)
                                }
                            };

                            if let Some((folded_depth, mut folded_dirs)) = folded_dirs_entry.take()
                            {
                                if folded
                                    && directory_entry.worktree_id == folded_dirs.worktree_id
                                    && directory_entry.entry.path.parent()
                                        == folded_dirs
                                            .entries
                                            .last()
                                            .map(|entry| entry.path.as_ref())
                                {
                                    folded_dirs.entries.push(directory_entry.entry.clone());
                                    folded_dirs_entry = Some((folded_depth, folded_dirs))
                                } else {
                                    if !is_singleton {
                                        let start_of_collapsed_dir_sequence = !parent_expanded
                                            && parent_dirs
                                                .iter()
                                                .rev()
                                                .nth(folded_dirs.entries.len() + 1)
                                                .map_or(true, |parent| parent.expanded);
                                        if start_of_collapsed_dir_sequence
                                            || parent_expanded
                                            || query.is_some()
                                        {
                                            if parent_folded {
                                                folded_dirs
                                                    .entries
                                                    .push(directory_entry.entry.clone());
                                                should_add = false;
                                            }
                                            let new_folded_dirs =
                                                PanelEntry::FoldedDirs(folded_dirs.clone());
                                            outline_panel.push_entry(
                                                &mut generation_state,
                                                track_matches,
                                                new_folded_dirs,
                                                folded_depth,
                                                cx,
                                            );
                                        }
                                    }

                                    folded_dirs_entry = if parent_folded {
                                        None
                                    } else {
                                        Some((
                                            depth,
                                            FoldedDirsEntry {
                                                worktree_id: directory_entry.worktree_id,
                                                entries: vec![directory_entry.entry.clone()],
                                            },
                                        ))
                                    };
                                }
                            } else if folded {
                                folded_dirs_entry = Some((
                                    depth,
                                    FoldedDirsEntry {
                                        worktree_id: directory_entry.worktree_id,
                                        entries: vec![directory_entry.entry.clone()],
                                    },
                                ));
                            }

                            let should_add =
                                should_add && parent_expanded && folded_dirs_entry.is_none();
                            (depth, should_add)
                        }
                        FsEntry::ExternalFile(..) => {
                            if let Some((folded_depth, folded_dir)) = folded_dirs_entry.take() {
                                let parent_expanded = parent_dirs
                                    .iter()
                                    .rev()
                                    .find(|parent| {
                                        folded_dir
                                            .entries
                                            .iter()
                                            .all(|entry| entry.path != parent.path)
                                    })
                                    .map_or(true, |parent| parent.expanded);
                                if !is_singleton && (parent_expanded || query.is_some()) {
                                    outline_panel.push_entry(
                                        &mut generation_state,
                                        track_matches,
                                        PanelEntry::FoldedDirs(folded_dir),
                                        folded_depth,
                                        cx,
                                    );
                                }
                            }
                            parent_dirs.clear();
                            (0, true)
                        }
                        FsEntry::File(file) => {
                            if let Some((folded_depth, folded_dirs)) = folded_dirs_entry.take() {
                                let parent_expanded = parent_dirs
                                    .iter()
                                    .rev()
                                    .find(|parent| {
                                        folded_dirs
                                            .entries
                                            .iter()
                                            .all(|entry| entry.path != parent.path)
                                    })
                                    .map_or(true, |parent| parent.expanded);
                                if !is_singleton && (parent_expanded || query.is_some()) {
                                    outline_panel.push_entry(
                                        &mut generation_state,
                                        track_matches,
                                        PanelEntry::FoldedDirs(folded_dirs),
                                        folded_depth,
                                        cx,
                                    );
                                }
                            }

                            let fs_depth = outline_panel
                                .fs_entries_depth
                                .get(&(file.worktree_id, file.entry.id))
                                .copied()
                                .unwrap_or(0);
                            while let Some(parent) = parent_dirs.last() {
                                if file.entry.path.starts_with(&parent.path) {
                                    break;
                                }
                                parent_dirs.pop();
                            }
                            match parent_dirs.last() {
                                Some(parent) => {
                                    let new_depth = parent.depth + 1;
                                    (new_depth, parent.expanded)
                                }
                                None => (fs_depth, true),
                            }
                        }
                    };

                    if !is_singleton
                        && (should_add || (query.is_some() && folded_dirs_entry.is_none()))
                    {
                        outline_panel.push_entry(
                            &mut generation_state,
                            track_matches,
                            PanelEntry::Fs(entry.clone()),
                            depth,
                            cx,
                        );
                    }

                    match outline_panel.mode {
                        ItemsDisplayMode::Search(_) => {
                            if is_singleton || query.is_some() || (should_add && is_expanded) {
                                outline_panel.add_search_entries(
                                    &mut generation_state,
                                    &active_editor,
                                    entry.clone(),
                                    depth,
                                    query.clone(),
                                    is_singleton,
                                    cx,
                                );
                            }
                        }
                        ItemsDisplayMode::Outline => {
                            let excerpts_to_consider =
                                if is_singleton || query.is_some() || (should_add && is_expanded) {
                                    match &entry {
                                        FsEntry::File(FsEntryFile {
                                            buffer_id,
                                            excerpts,
                                            ..
                                        })
                                        | FsEntry::ExternalFile(FsEntryExternalFile {
                                            buffer_id,
                                            excerpts,
                                            ..
                                        }) => Some((*buffer_id, excerpts)),
                                        _ => None,
                                    }
                                } else {
                                    None
                                };
                            if let Some((buffer_id, entry_excerpts)) = excerpts_to_consider {
                                if !active_editor.read(cx).is_buffer_folded(buffer_id, cx) {
                                    outline_panel.add_excerpt_entries(
                                        &mut generation_state,
                                        buffer_id,
                                        entry_excerpts,
                                        depth,
                                        track_matches,
                                        is_singleton,
                                        query.as_deref(),
                                        cx,
                                    );
                                }
                            }
                        }
                    }

                    if is_singleton
                        && matches!(entry, FsEntry::File(..) | FsEntry::ExternalFile(..))
                        && !generation_state.entries.iter().any(|item| {
                            matches!(item.entry, PanelEntry::Outline(..) | PanelEntry::Search(_))
                        })
                    {
                        outline_panel.push_entry(
                            &mut generation_state,
                            track_matches,
                            PanelEntry::Fs(entry.clone()),
                            0,
                            cx,
                        );
                    }
                }

                if let Some((folded_depth, folded_dirs)) = folded_dirs_entry.take() {
                    let parent_expanded = parent_dirs
                        .iter()
                        .rev()
                        .find(|parent| {
                            folded_dirs
                                .entries
                                .iter()
                                .all(|entry| entry.path != parent.path)
                        })
                        .map_or(true, |parent| parent.expanded);
                    if parent_expanded || query.is_some() {
                        outline_panel.push_entry(
                            &mut generation_state,
                            track_matches,
                            PanelEntry::FoldedDirs(folded_dirs),
                            folded_depth,
                            cx,
                        );
                    }
                }
            }) else {
                return (Vec::new(), None);
            };

            let Some(query) = query else {
                return (
                    generation_state.entries,
                    generation_state
                        .max_width_estimate_and_index
                        .map(|(_, index)| index),
                );
            };

            let mut matched_ids = match_strings(
                &generation_state.match_candidates,
                &query,
                true,
                usize::MAX,
                &AtomicBool::default(),
                cx.background_executor().clone(),
            )
            .await
            .into_iter()
            .map(|string_match| (string_match.candidate_id, string_match))
            .collect::<HashMap<_, _>>();

            let mut id = 0;
            generation_state.entries.retain_mut(|cached_entry| {
                let retain = match matched_ids.remove(&id) {
                    Some(string_match) => {
                        cached_entry.string_match = Some(string_match);
                        true
                    }
                    None => false,
                };
                id += 1;
                retain
            });

            (
                generation_state.entries,
                generation_state
                    .max_width_estimate_and_index
                    .map(|(_, index)| index),
            )
        })
    }

    fn push_entry(
        &self,
        state: &mut GenerationState,
        track_matches: bool,
        entry: PanelEntry,
        depth: usize,
        cx: &mut App,
    ) {
        let entry = if let PanelEntry::FoldedDirs(folded_dirs_entry) = &entry {
            match folded_dirs_entry.entries.len() {
                0 => {
                    return;
                }
                1 => PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory {
                    worktree_id: folded_dirs_entry.worktree_id,
                    entry: folded_dirs_entry.entries[0].clone(),
                })),
                _ => entry,
            }
        } else {
            entry
        };

        if track_matches {
            let id = state.entries.len();
            match &entry {
                PanelEntry::Fs(fs_entry) => {
                    if let Some(file_name) =
                        self.relative_path(fs_entry, cx).as_deref().map(file_name)
                    {
                        state
                            .match_candidates
                            .push(StringMatchCandidate::new(id, &file_name));
                    }
                }
                PanelEntry::FoldedDirs(folded_dir_entry) => {
                    let dir_names = self.dir_names_string(
                        &folded_dir_entry.entries,
                        folded_dir_entry.worktree_id,
                        cx,
                    );
                    {
                        state
                            .match_candidates
                            .push(StringMatchCandidate::new(id, &dir_names));
                    }
                }
                PanelEntry::Outline(OutlineEntry::Outline(outline_entry)) => state
                    .match_candidates
                    .push(StringMatchCandidate::new(id, &outline_entry.outline.text)),
                PanelEntry::Outline(OutlineEntry::Excerpt(_)) => {}
                PanelEntry::Search(new_search_entry) => {
                    if let Some(search_data) = new_search_entry.render_data.get() {
                        state
                            .match_candidates
                            .push(StringMatchCandidate::new(id, &search_data.context_text));
                    }
                }
            }
        }

        let width_estimate = self.width_estimate(depth, &entry, cx);
        if Some(width_estimate)
            > state
                .max_width_estimate_and_index
                .map(|(estimate, _)| estimate)
        {
            state.max_width_estimate_and_index = Some((width_estimate, state.entries.len()));
        }
        state.entries.push(CachedEntry {
            depth,
            entry,
            string_match: None,
        });
    }

    fn dir_names_string(&self, entries: &[GitEntry], worktree_id: WorktreeId, cx: &App) -> String {
        let dir_names_segment = entries
            .iter()
            .map(|entry| self.entry_name(&worktree_id, entry, cx))
            .collect::<PathBuf>();
        dir_names_segment.to_string_lossy().to_string()
    }

    fn query(&self, cx: &App) -> Option<String> {
        let query = self.filter_editor.read(cx).text(cx);
        if query.trim().is_empty() {
            None
        } else {
            Some(query)
        }
    }

    fn is_expanded(&self, entry: &FsEntry) -> bool {
        let entry_to_check = match entry {
            FsEntry::ExternalFile(FsEntryExternalFile { buffer_id, .. }) => {
                CollapsedEntry::ExternalFile(*buffer_id)
            }
            FsEntry::File(FsEntryFile {
                worktree_id,
                buffer_id,
                ..
            }) => CollapsedEntry::File(*worktree_id, *buffer_id),
            FsEntry::Directory(FsEntryDirectory {
                worktree_id, entry, ..
            }) => CollapsedEntry::Dir(*worktree_id, entry.id),
        };
        !self.collapsed_entries.contains(&entry_to_check)
    }

    fn update_non_fs_items(&mut self, window: &mut Window, cx: &mut Context<OutlinePanel>) -> bool {
        if !self.active {
            return false;
        }

        let mut update_cached_items = false;
        update_cached_items |= self.update_search_matches(window, cx);
        self.fetch_outdated_outlines(window, cx);
        if update_cached_items {
            self.selected_entry.invalidate();
        }
        update_cached_items
    }

    fn update_search_matches(
        &mut self,
        window: &mut Window,
        cx: &mut Context<OutlinePanel>,
    ) -> bool {
        if !self.active {
            return false;
        }

        let project_search = self
            .active_item()
            .and_then(|item| item.downcast::<ProjectSearchView>());
        let project_search_matches = project_search
            .as_ref()
            .map(|project_search| project_search.read(cx).get_matches(cx))
            .unwrap_or_default();

        let buffer_search = self
            .active_item()
            .as_deref()
            .and_then(|active_item| {
                self.workspace
                    .upgrade()
                    .and_then(|workspace| workspace.read(cx).pane_for(active_item))
            })
            .and_then(|pane| {
                pane.read(cx)
                    .toolbar()
                    .read(cx)
                    .item_of_type::<BufferSearchBar>()
            });
        let buffer_search_matches = self
            .active_editor()
            .map(|active_editor| {
                active_editor.update(cx, |editor, cx| editor.get_matches(window, cx))
            })
            .unwrap_or_default();

        let mut update_cached_entries = false;
        if buffer_search_matches.is_empty() && project_search_matches.is_empty() {
            if matches!(self.mode, ItemsDisplayMode::Search(_)) {
                self.mode = ItemsDisplayMode::Outline;
                update_cached_entries = true;
            }
        } else {
            let (kind, new_search_matches, new_search_query) = if buffer_search_matches.is_empty() {
                (
                    SearchKind::Project,
                    project_search_matches,
                    project_search
                        .map(|project_search| project_search.read(cx).search_query_text(cx))
                        .unwrap_or_default(),
                )
            } else {
                (
                    SearchKind::Buffer,
                    buffer_search_matches,
                    buffer_search
                        .map(|buffer_search| buffer_search.read(cx).query(cx))
                        .unwrap_or_default(),
                )
            };

            let mut previous_matches = HashMap::default();
            update_cached_entries = match &mut self.mode {
                ItemsDisplayMode::Search(current_search_state) => {
                    let update = current_search_state.query != new_search_query
                        || current_search_state.kind != kind
                        || current_search_state.matches.is_empty()
                        || current_search_state.matches.iter().enumerate().any(
                            |(i, (match_range, _))| new_search_matches.get(i) != Some(match_range),
                        );
                    if current_search_state.kind == kind {
                        previous_matches.extend(current_search_state.matches.drain(..));
                    }
                    update
                }
                ItemsDisplayMode::Outline => true,
            };
            self.mode = ItemsDisplayMode::Search(SearchState::new(
                kind,
                new_search_query,
                previous_matches,
                new_search_matches,
                cx.theme().syntax().clone(),
                window,
                cx,
            ));
        }
        update_cached_entries
    }

    fn add_excerpt_entries(
        &self,
        state: &mut GenerationState,
        buffer_id: BufferId,
        entries_to_add: &[ExcerptId],
        parent_depth: usize,
        track_matches: bool,
        is_singleton: bool,
        query: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if let Some(excerpts) = self.excerpts.get(&buffer_id) {
            for &excerpt_id in entries_to_add {
                let Some(excerpt) = excerpts.get(&excerpt_id) else {
                    continue;
                };
                let excerpt_depth = parent_depth + 1;
                self.push_entry(
                    state,
                    track_matches,
                    PanelEntry::Outline(OutlineEntry::Excerpt(OutlineEntryExcerpt {
                        buffer_id,
                        id: excerpt_id,
                        range: excerpt.range.clone(),
                    })),
                    excerpt_depth,
                    cx,
                );

                let mut outline_base_depth = excerpt_depth + 1;
                if is_singleton {
                    outline_base_depth = 0;
                    state.clear();
                } else if query.is_none()
                    && self
                        .collapsed_entries
                        .contains(&CollapsedEntry::Excerpt(buffer_id, excerpt_id))
                {
                    continue;
                }

                for outline in excerpt.iter_outlines() {
                    self.push_entry(
                        state,
                        track_matches,
                        PanelEntry::Outline(OutlineEntry::Outline(OutlineEntryOutline {
                            buffer_id,
                            excerpt_id,
                            outline: outline.clone(),
                        })),
                        outline_base_depth + outline.depth,
                        cx,
                    );
                }
            }
        }
    }

    fn add_search_entries(
        &mut self,
        state: &mut GenerationState,
        active_editor: &Entity<Editor>,
        parent_entry: FsEntry,
        parent_depth: usize,
        filter_query: Option<String>,
        is_singleton: bool,
        cx: &mut Context<Self>,
    ) {
        let ItemsDisplayMode::Search(search_state) = &mut self.mode else {
            return;
        };

        let kind = search_state.kind;
        let related_excerpts = match &parent_entry {
            FsEntry::Directory(_) => return,
            FsEntry::ExternalFile(external) => &external.excerpts,
            FsEntry::File(file) => &file.excerpts,
        }
        .iter()
        .copied()
        .collect::<HashSet<_>>();

        let depth = if is_singleton { 0 } else { parent_depth + 1 };
        let new_search_matches = search_state
            .matches
            .iter()
            .filter(|(match_range, _)| {
                related_excerpts.contains(&match_range.start.excerpt_id)
                    || related_excerpts.contains(&match_range.end.excerpt_id)
            })
            .filter(|(match_range, _)| {
                let editor = active_editor.read(cx);
                if let Some(buffer_id) = match_range.start.buffer_id {
                    if editor.is_buffer_folded(buffer_id, cx) {
                        return false;
                    }
                }
                if let Some(buffer_id) = match_range.start.buffer_id {
                    if editor.is_buffer_folded(buffer_id, cx) {
                        return false;
                    }
                }
                true
            });

        let new_search_entries = new_search_matches
            .map(|(match_range, search_data)| SearchEntry {
                match_range: match_range.clone(),
                kind,
                render_data: Arc::clone(search_data),
            })
            .collect::<Vec<_>>();
        for new_search_entry in new_search_entries {
            self.push_entry(
                state,
                filter_query.is_some(),
                PanelEntry::Search(new_search_entry),
                depth,
                cx,
            );
        }
    }

    fn active_editor(&self) -> Option<Entity<Editor>> {
        self.active_item.as_ref()?.active_editor.upgrade()
    }

    fn active_item(&self) -> Option<Box<dyn ItemHandle>> {
        self.active_item.as_ref()?.item_handle.upgrade()
    }

    fn should_replace_active_item(&self, new_active_item: &dyn ItemHandle) -> bool {
        self.active_item().map_or(true, |active_item| {
            !self.pinned && active_item.item_id() != new_active_item.item_id()
        })
    }

    pub fn toggle_active_editor_pin(
        &mut self,
        _: &ToggleActiveEditorPin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pinned = !self.pinned;
        if !self.pinned {
            if let Some((active_item, active_editor)) = self
                .workspace
                .upgrade()
                .and_then(|workspace| workspace_active_editor(workspace.read(cx), cx))
            {
                if self.should_replace_active_item(active_item.as_ref()) {
                    self.replace_active_editor(active_item, active_editor, window, cx);
                }
            }
        }

        cx.notify();
    }

    fn selected_entry(&self) -> Option<&PanelEntry> {
        match &self.selected_entry {
            SelectedEntry::Invalidated(entry) => entry.as_ref(),
            SelectedEntry::Valid(entry, _) => Some(entry),
            SelectedEntry::None => None,
        }
    }

    fn select_entry(
        &mut self,
        entry: PanelEntry,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if focus {
            self.focus_handle.focus(window);
        }
        let ix = self
            .cached_entries
            .iter()
            .enumerate()
            .find(|(_, cached_entry)| &cached_entry.entry == &entry)
            .map(|(i, _)| i)
            .unwrap_or_default();

        self.selected_entry = SelectedEntry::Valid(entry, ix);

        self.autoscroll(cx);
        cx.notify();
    }

    fn render_vertical_scrollbar(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        if !Self::should_show_scrollbar(cx)
            || !(self.show_scrollbar || self.vertical_scrollbar_state.is_dragging())
        {
            return None;
        }
        Some(
            div()
                .occlude()
                .id("project-panel-vertical-scroll")
                .on_mouse_move(cx.listener(|_, _, _, cx| {
                    cx.notify();
                    cx.stop_propagation()
                }))
                .on_hover(|_, _, cx| {
                    cx.stop_propagation();
                })
                .on_any_mouse_down(|_, _, cx| {
                    cx.stop_propagation();
                })
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|outline_panel, _, window, cx| {
                        if !outline_panel.vertical_scrollbar_state.is_dragging()
                            && !outline_panel.focus_handle.contains_focused(window, cx)
                        {
                            outline_panel.hide_scrollbar(window, cx);
                            cx.notify();
                        }

                        cx.stop_propagation();
                    }),
                )
                .on_scroll_wheel(cx.listener(|_, _, _, cx| {
                    cx.notify();
                }))
                .h_full()
                .absolute()
                .right_1()
                .top_1()
                .bottom_0()
                .w(px(12.))
                .cursor_default()
                .children(Scrollbar::vertical(self.vertical_scrollbar_state.clone())),
        )
    }

    fn render_horizontal_scrollbar(
        &self,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Stateful<Div>> {
        if !Self::should_show_scrollbar(cx)
            || !(self.show_scrollbar || self.horizontal_scrollbar_state.is_dragging())
        {
            return None;
        }
        Scrollbar::horizontal(self.horizontal_scrollbar_state.clone()).map(|scrollbar| {
            div()
                .occlude()
                .id("project-panel-horizontal-scroll")
                .on_mouse_move(cx.listener(|_, _, _, cx| {
                    cx.notify();
                    cx.stop_propagation()
                }))
                .on_hover(|_, _, cx| {
                    cx.stop_propagation();
                })
                .on_any_mouse_down(|_, _, cx| {
                    cx.stop_propagation();
                })
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|outline_panel, _, window, cx| {
                        if !outline_panel.horizontal_scrollbar_state.is_dragging()
                            && !outline_panel.focus_handle.contains_focused(window, cx)
                        {
                            outline_panel.hide_scrollbar(window, cx);
                            cx.notify();
                        }

                        cx.stop_propagation();
                    }),
                )
                .on_scroll_wheel(cx.listener(|_, _, _, cx| {
                    cx.notify();
                }))
                .w_full()
                .absolute()
                .right_1()
                .left_1()
                .bottom_0()
                .h(px(12.))
                .cursor_default()
                .child(scrollbar)
        })
    }

    fn should_show_scrollbar(cx: &App) -> bool {
        let show = OutlinePanelSettings::get_global(cx)
            .scrollbar
            .show
            .unwrap_or_else(|| EditorSettings::get_global(cx).scrollbar.show);
        match show {
            ShowScrollbar::Auto => true,
            ShowScrollbar::System => true,
            ShowScrollbar::Always => true,
            ShowScrollbar::Never => false,
        }
    }

    fn should_autohide_scrollbar(cx: &App) -> bool {
        let show = OutlinePanelSettings::get_global(cx)
            .scrollbar
            .show
            .unwrap_or_else(|| EditorSettings::get_global(cx).scrollbar.show);
        match show {
            ShowScrollbar::Auto => true,
            ShowScrollbar::System => cx
                .try_global::<ScrollbarAutoHide>()
                .map_or_else(|| cx.should_auto_hide_scrollbars(), |autohide| autohide.0),
            ShowScrollbar::Always => false,
            ShowScrollbar::Never => true,
        }
    }

    fn hide_scrollbar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        const SCROLLBAR_SHOW_INTERVAL: Duration = Duration::from_secs(1);
        if !Self::should_autohide_scrollbar(cx) {
            return;
        }
        self.hide_scrollbar_task = Some(cx.spawn_in(window, async move |panel, cx| {
            cx.background_executor()
                .timer(SCROLLBAR_SHOW_INTERVAL)
                .await;
            panel
                .update(cx, |panel, cx| {
                    panel.show_scrollbar = false;
                    cx.notify();
                })
                .log_err();
        }))
    }

    fn width_estimate(&self, depth: usize, entry: &PanelEntry, cx: &App) -> u64 {
        let item_text_chars = match entry {
            PanelEntry::Fs(FsEntry::ExternalFile(external)) => self
                .buffer_snapshot_for_id(external.buffer_id, cx)
                .and_then(|snapshot| {
                    Some(snapshot.file()?.path().file_name()?.to_string_lossy().len())
                })
                .unwrap_or_default(),
            PanelEntry::Fs(FsEntry::Directory(directory)) => directory
                .entry
                .path
                .file_name()
                .map(|name| name.to_string_lossy().len())
                .unwrap_or_default(),
            PanelEntry::Fs(FsEntry::File(file)) => file
                .entry
                .path
                .file_name()
                .map(|name| name.to_string_lossy().len())
                .unwrap_or_default(),
            PanelEntry::FoldedDirs(folded_dirs) => {
                folded_dirs
                    .entries
                    .iter()
                    .map(|dir| {
                        dir.path
                            .file_name()
                            .map(|name| name.to_string_lossy().len())
                            .unwrap_or_default()
                    })
                    .sum::<usize>()
                    + folded_dirs.entries.len().saturating_sub(1) * MAIN_SEPARATOR_STR.len()
            }
            PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => self
                .excerpt_label(excerpt.buffer_id, &excerpt.range, cx)
                .map(|label| label.len())
                .unwrap_or_default(),
            PanelEntry::Outline(OutlineEntry::Outline(entry)) => entry.outline.text.len(),
            PanelEntry::Search(search) => search
                .render_data
                .get()
                .map(|data| data.context_text.len())
                .unwrap_or_default(),
        };

        (item_text_chars + depth) as u64
    }

    fn render_main_contents(
        &mut self,
        query: Option<String>,
        show_indent_guides: bool,
        indent_size: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let contents = if self.cached_entries.is_empty() {
            let header = if self.updating_fs_entries || self.updating_cached_entries {
                None
            } else if query.is_some() {
                Some("No matches for query")
            } else {
                Some("No outlines available")
            };

            v_flex()
                .flex_1()
                .justify_center()
                .size_full()
                .when_some(header, |panel, header| {
                    panel
                        .child(h_flex().justify_center().child(Label::new(header)))
                        .when_some(query.clone(), |panel, query| {
                            panel.child(h_flex().justify_center().child(Label::new(query)))
                        })
                        .child(
                            h_flex()
                                .pt(DynamicSpacing::Base04.rems(cx))
                                .justify_center()
                                .child({
                                    let keystroke =
                                        match self.position(window, cx) {
                                            DockPosition::Left => window
                                                .keystroke_text_for(&workspace::ToggleLeftDock),
                                            DockPosition::Bottom => window
                                                .keystroke_text_for(&workspace::ToggleBottomDock),
                                            DockPosition::Right => window
                                                .keystroke_text_for(&workspace::ToggleRightDock),
                                        };
                                    Label::new(format!("Toggle this panel with {keystroke}"))
                                }),
                        )
                })
        } else {
            let list_contents = {
                let items_len = self.cached_entries.len();
                let multi_buffer_snapshot = self
                    .active_editor()
                    .map(|editor| editor.read(cx).buffer().read(cx).snapshot(cx));
                uniform_list(cx.entity().clone(), "entries", items_len, {
                    move |outline_panel, range, window, cx| {
                        let entries = outline_panel.cached_entries.get(range);
                        entries
                            .map(|entries| entries.to_vec())
                            .unwrap_or_default()
                            .into_iter()
                            .filter_map(|cached_entry| match cached_entry.entry {
                                PanelEntry::Fs(entry) => Some(outline_panel.render_entry(
                                    &entry,
                                    cached_entry.depth,
                                    cached_entry.string_match.as_ref(),
                                    window,
                                    cx,
                                )),
                                PanelEntry::FoldedDirs(folded_dirs_entry) => {
                                    Some(outline_panel.render_folded_dirs(
                                        &folded_dirs_entry,
                                        cached_entry.depth,
                                        cached_entry.string_match.as_ref(),
                                        window,
                                        cx,
                                    ))
                                }
                                PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => {
                                    outline_panel.render_excerpt(
                                        &excerpt,
                                        cached_entry.depth,
                                        window,
                                        cx,
                                    )
                                }
                                PanelEntry::Outline(OutlineEntry::Outline(entry)) => {
                                    Some(outline_panel.render_outline(
                                        &entry,
                                        cached_entry.depth,
                                        cached_entry.string_match.as_ref(),
                                        window,
                                        cx,
                                    ))
                                }
                                PanelEntry::Search(SearchEntry {
                                    match_range,
                                    render_data,
                                    kind,
                                    ..
                                }) => outline_panel.render_search_match(
                                    multi_buffer_snapshot.as_ref(),
                                    &match_range,
                                    &render_data,
                                    kind,
                                    cached_entry.depth,
                                    cached_entry.string_match.as_ref(),
                                    window,
                                    cx,
                                ),
                            })
                            .collect()
                    }
                })
                .with_sizing_behavior(ListSizingBehavior::Infer)
                .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                .with_width_from_item(self.max_width_item_index)
                .track_scroll(self.scroll_handle.clone())
                .when(show_indent_guides, |list| {
                    list.with_decoration(
                        ui::indent_guides(
                            cx.entity().clone(),
                            px(indent_size),
                            IndentGuideColors::panel(cx),
                            |outline_panel, range, _, _| {
                                let entries = outline_panel.cached_entries.get(range);
                                if let Some(entries) = entries {
                                    entries.into_iter().map(|item| item.depth).collect()
                                } else {
                                    smallvec::SmallVec::new()
                                }
                            },
                        )
                        .with_render_fn(
                            cx.entity().clone(),
                            move |outline_panel, params, _, _| {
                                const LEFT_OFFSET: Pixels = px(14.);

                                let indent_size = params.indent_size;
                                let item_height = params.item_height;
                                let active_indent_guide_ix = find_active_indent_guide_ix(
                                    outline_panel,
                                    &params.indent_guides,
                                );

                                params
                                    .indent_guides
                                    .into_iter()
                                    .enumerate()
                                    .map(|(ix, layout)| {
                                        let bounds = Bounds::new(
                                            point(
                                                layout.offset.x * indent_size + LEFT_OFFSET,
                                                layout.offset.y * item_height,
                                            ),
                                            size(px(1.), layout.length * item_height),
                                        );
                                        ui::RenderedIndentGuide {
                                            bounds,
                                            layout,
                                            is_active: active_indent_guide_ix == Some(ix),
                                            hitbox: None,
                                        }
                                    })
                                    .collect()
                            },
                        ),
                    )
                })
            };

            v_flex()
                .flex_shrink()
                .size_full()
                .child(list_contents.size_full().flex_shrink())
                .children(self.render_vertical_scrollbar(cx))
                .when_some(
                    self.render_horizontal_scrollbar(window, cx),
                    |this, scrollbar| this.pb_4().child(scrollbar),
                )
        }
        .children(self.context_menu.as_ref().map(|(menu, position, _)| {
            deferred(
                anchored()
                    .position(*position)
                    .anchor(gpui::Corner::TopLeft)
                    .child(menu.clone()),
            )
            .with_priority(1)
        }));

        v_flex().w_full().flex_1().overflow_hidden().child(contents)
    }

    fn render_filter_footer(&mut self, pinned: bool, cx: &mut Context<Self>) -> Div {
        v_flex().flex_none().child(horizontal_separator(cx)).child(
            h_flex()
                .p_2()
                .w_full()
                .child(self.filter_editor.clone())
                .child(
                    div().child(
                        IconButton::new(
                            "outline-panel-menu",
                            if pinned {
                                IconName::Unpin
                            } else {
                                IconName::Pin
                            },
                        )
                        .tooltip(Tooltip::text(if pinned {
                            "Unpin Outline"
                        } else {
                            "Pin Active Outline"
                        }))
                        .shape(IconButtonShape::Square)
                        .on_click(cx.listener(
                            |outline_panel, _, window, cx| {
                                outline_panel.toggle_active_editor_pin(
                                    &ToggleActiveEditorPin,
                                    window,
                                    cx,
                                );
                            },
                        )),
                    ),
                ),
        )
    }

    fn buffers_inside_directory(
        &self,
        dir_worktree: WorktreeId,
        dir_entry: &GitEntry,
    ) -> HashSet<BufferId> {
        if !dir_entry.is_dir() {
            return HashSet::default();
        }

        self.fs_entries
            .iter()
            .skip_while(|fs_entry| match fs_entry {
                FsEntry::Directory(directory) => {
                    directory.worktree_id != dir_worktree || &directory.entry != dir_entry
                }
                _ => true,
            })
            .skip(1)
            .take_while(|fs_entry| match fs_entry {
                FsEntry::ExternalFile(..) => false,
                FsEntry::Directory(directory) => {
                    directory.worktree_id == dir_worktree
                        && directory.entry.path.starts_with(&dir_entry.path)
                }
                FsEntry::File(file) => {
                    file.worktree_id == dir_worktree && file.entry.path.starts_with(&dir_entry.path)
                }
            })
            .filter_map(|fs_entry| match fs_entry {
                FsEntry::File(file) => Some(file.buffer_id),
                _ => None,
            })
            .collect()
    }
}

fn workspace_active_editor(
    workspace: &Workspace,
    cx: &App,
) -> Option<(Box<dyn ItemHandle>, Entity<Editor>)> {
    let active_item = workspace.active_item(cx)?;
    let active_editor = active_item
        .act_as::<Editor>(cx)
        .filter(|editor| editor.read(cx).mode().is_full())?;
    Some((active_item, active_editor))
}

fn back_to_common_visited_parent(
    visited_dirs: &mut Vec<(ProjectEntryId, Arc<Path>)>,
    worktree_id: &WorktreeId,
    new_entry: &Entry,
) -> Option<(WorktreeId, ProjectEntryId)> {
    while let Some((visited_dir_id, visited_path)) = visited_dirs.last() {
        match new_entry.path.parent() {
            Some(parent_path) => {
                if parent_path == visited_path.as_ref() {
                    return Some((*worktree_id, *visited_dir_id));
                }
            }
            None => {
                break;
            }
        }
        visited_dirs.pop();
    }
    None
}

fn file_name(path: &Path) -> String {
    let mut current_path = path;
    loop {
        if let Some(file_name) = current_path.file_name() {
            return file_name.to_string_lossy().into_owned();
        }
        match current_path.parent() {
            Some(parent) => current_path = parent,
            None => return path.to_string_lossy().into_owned(),
        }
    }
}

impl Panel for OutlinePanel {
    fn persistent_name() -> &'static str {
        "Outline Panel"
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        match OutlinePanelSettings::get_global(cx).dock {
            OutlinePanelDockPosition::Left => DockPosition::Left,
            OutlinePanelDockPosition::Right => DockPosition::Right,
        }
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file::<OutlinePanelSettings>(
            self.fs.clone(),
            cx,
            move |settings, _| {
                let dock = match position {
                    DockPosition::Left | DockPosition::Bottom => OutlinePanelDockPosition::Left,
                    DockPosition::Right => OutlinePanelDockPosition::Right,
                };
                settings.dock = Some(dock);
            },
        );
    }

    fn size(&self, _: &Window, cx: &App) -> Pixels {
        self.width
            .unwrap_or_else(|| OutlinePanelSettings::get_global(cx).default_width)
    }

    fn set_size(&mut self, size: Option<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.width = size;
        cx.notify();
        cx.defer_in(window, |this, _, cx| {
            this.serialize(cx);
        });
    }

    fn icon(&self, _: &Window, cx: &App) -> Option<IconName> {
        OutlinePanelSettings::get_global(cx)
            .button
            .then_some(IconName::ListTree)
    }

    fn icon_tooltip(&self, _window: &Window, _: &App) -> Option<&'static str> {
        Some("Outline Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _window: &Window, _: &App) -> bool {
        self.active
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |outline_panel, cx| {
            outline_panel
                .update_in(cx, |outline_panel, window, cx| {
                    let old_active = outline_panel.active;
                    outline_panel.active = active;
                    if old_active != active {
                        if active {
                            if let Some((active_item, active_editor)) =
                                outline_panel.workspace.upgrade().and_then(|workspace| {
                                    workspace_active_editor(workspace.read(cx), cx)
                                })
                            {
                                if outline_panel.should_replace_active_item(active_item.as_ref()) {
                                    outline_panel.replace_active_editor(
                                        active_item,
                                        active_editor,
                                        window,
                                        cx,
                                    );
                                } else {
                                    outline_panel.update_fs_entries(active_editor, None, window, cx)
                                }
                                return;
                            }
                        }

                        if !outline_panel.pinned {
                            outline_panel.clear_previous(window, cx);
                        }
                    }
                    outline_panel.serialize(cx);
                })
                .ok();
        })
        .detach()
    }

    fn activation_priority(&self) -> u32 {
        5
    }
}

impl Focusable for OutlinePanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.filter_editor.focus_handle(cx).clone()
    }
}

impl EventEmitter<Event> for OutlinePanel {}

impl EventEmitter<PanelEvent> for OutlinePanel {}

impl Render for OutlinePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (is_local, is_via_ssh) = self
            .project
            .read_with(cx, |project, _| (project.is_local(), project.is_via_ssh()));
        let query = self.query(cx);
        let pinned = self.pinned;
        let settings = OutlinePanelSettings::get_global(cx);
        let indent_size = settings.indent_size;
        let show_indent_guides = settings.indent_guides.show == ShowIndentGuides::Always;

        let search_query = match &self.mode {
            ItemsDisplayMode::Search(search_query) => Some(search_query),
            _ => None,
        };

        v_flex()
            .id("outline-panel")
            .size_full()
            .overflow_hidden()
            .relative()
            .on_hover(cx.listener(|this, hovered, window, cx| {
                if *hovered {
                    this.show_scrollbar = true;
                    this.hide_scrollbar_task.take();
                    cx.notify();
                } else if !this.focus_handle.contains_focused(window, cx) {
                    this.hide_scrollbar(window, cx);
                }
            }))
            .key_context(self.dispatch_context(window, cx))
            .on_action(cx.listener(Self::open_selected_entry))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::select_parent))
            .on_action(cx.listener(Self::expand_selected_entry))
            .on_action(cx.listener(Self::collapse_selected_entry))
            .on_action(cx.listener(Self::expand_all_entries))
            .on_action(cx.listener(Self::collapse_all_entries))
            .on_action(cx.listener(Self::copy_path))
            .on_action(cx.listener(Self::copy_relative_path))
            .on_action(cx.listener(Self::toggle_active_editor_pin))
            .on_action(cx.listener(Self::unfold_directory))
            .on_action(cx.listener(Self::fold_directory))
            .on_action(cx.listener(Self::open_excerpts))
            .on_action(cx.listener(Self::open_excerpts_split))
            .when(is_local, |el| {
                el.on_action(cx.listener(Self::reveal_in_finder))
            })
            .when(is_local || is_via_ssh, |el| {
                el.on_action(cx.listener(Self::open_in_terminal))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |outline_panel, event: &MouseDownEvent, window, cx| {
                    if let Some(entry) = outline_panel.selected_entry().cloned() {
                        outline_panel.deploy_context_menu(event.position, entry, window, cx)
                    } else if let Some(entry) = outline_panel.fs_entries.first().cloned() {
                        outline_panel.deploy_context_menu(
                            event.position,
                            PanelEntry::Fs(entry),
                            window,
                            cx,
                        )
                    }
                }),
            )
            .track_focus(&self.focus_handle)
            .when_some(search_query, |outline_panel, search_state| {
                outline_panel.child(
                    h_flex()
                        .py_1p5()
                        .px_2()
                        .h(DynamicSpacing::Base32.px(cx))
                        .flex_shrink_0()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .gap_0p5()
                        .child(Label::new("Searching:").color(Color::Muted))
                        .child(Label::new(search_state.query.to_string())),
                )
            })
            .child(self.render_main_contents(query, show_indent_guides, indent_size, window, cx))
            .child(self.render_filter_footer(pinned, cx))
    }
}

fn find_active_indent_guide_ix(
    outline_panel: &OutlinePanel,
    candidates: &[IndentGuideLayout],
) -> Option<usize> {
    let SelectedEntry::Valid(_, target_ix) = &outline_panel.selected_entry else {
        return None;
    };
    let target_depth = outline_panel
        .cached_entries
        .get(*target_ix)
        .map(|cached_entry| cached_entry.depth)?;

    let (target_ix, target_depth) = if let Some(target_depth) = outline_panel
        .cached_entries
        .get(target_ix + 1)
        .filter(|cached_entry| cached_entry.depth > target_depth)
        .map(|entry| entry.depth)
    {
        (target_ix + 1, target_depth.saturating_sub(1))
    } else {
        (*target_ix, target_depth.saturating_sub(1))
    };

    candidates
        .iter()
        .enumerate()
        .find(|(_, guide)| {
            guide.offset.y <= target_ix
                && target_ix < guide.offset.y + guide.length
                && guide.offset.x == target_depth
        })
        .map(|(ix, _)| ix)
}

fn subscribe_for_editor_events(
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut Context<OutlinePanel>,
) -> Subscription {
    let debounce = Some(UPDATE_DEBOUNCE);
    cx.subscribe_in(
        editor,
        window,
        move |outline_panel, editor, e: &EditorEvent, window, cx| {
            if !outline_panel.active {
                return;
            }
            match e {
                EditorEvent::SelectionsChanged { local: true } => {
                    outline_panel.reveal_entry_for_selection(editor.clone(), window, cx);
                    cx.notify();
                }
                EditorEvent::ExcerptsAdded { excerpts, .. } => {
                    outline_panel
                        .new_entries_for_fs_update
                        .extend(excerpts.iter().map(|&(excerpt_id, _)| excerpt_id));
                    outline_panel.update_fs_entries(editor.clone(), debounce, window, cx);
                }
                EditorEvent::ExcerptsRemoved { ids, .. } => {
                    let mut ids = ids.iter().collect::<HashSet<_>>();
                    for excerpts in outline_panel.excerpts.values_mut() {
                        excerpts.retain(|excerpt_id, _| !ids.remove(excerpt_id));
                        if ids.is_empty() {
                            break;
                        }
                    }
                    outline_panel.update_fs_entries(editor.clone(), debounce, window, cx);
                }
                EditorEvent::ExcerptsExpanded { ids } => {
                    outline_panel.invalidate_outlines(ids);
                    let update_cached_items = outline_panel.update_non_fs_items(window, cx);
                    if update_cached_items {
                        outline_panel.update_cached_entries(Some(UPDATE_DEBOUNCE), window, cx);
                    }
                }
                EditorEvent::ExcerptsEdited { ids } => {
                    outline_panel.invalidate_outlines(ids);
                    let update_cached_items = outline_panel.update_non_fs_items(window, cx);
                    if update_cached_items {
                        outline_panel.update_cached_entries(Some(UPDATE_DEBOUNCE), window, cx);
                    }
                }
                EditorEvent::BufferFoldToggled { ids, .. } => {
                    outline_panel.invalidate_outlines(ids);
                    let mut latest_unfolded_buffer_id = None;
                    let mut latest_folded_buffer_id = None;
                    let mut ignore_selections_change = false;
                    outline_panel.new_entries_for_fs_update.extend(
                        ids.iter()
                            .filter(|id| {
                                outline_panel
                                    .excerpts
                                    .iter()
                                    .find_map(|(buffer_id, excerpts)| {
                                        if excerpts.contains_key(id) {
                                            ignore_selections_change |= outline_panel
                                                .preserve_selection_on_buffer_fold_toggles
                                                .remove(buffer_id);
                                            Some(buffer_id)
                                        } else {
                                            None
                                        }
                                    })
                                    .map(|buffer_id| {
                                        if editor.read(cx).is_buffer_folded(*buffer_id, cx) {
                                            latest_folded_buffer_id = Some(*buffer_id);
                                            false
                                        } else {
                                            latest_unfolded_buffer_id = Some(*buffer_id);
                                            true
                                        }
                                    })
                                    .unwrap_or(true)
                            })
                            .copied(),
                    );
                    if !ignore_selections_change {
                        if let Some(entry_to_select) = latest_unfolded_buffer_id
                            .or(latest_folded_buffer_id)
                            .and_then(|toggled_buffer_id| {
                                outline_panel.fs_entries.iter().find_map(
                                    |fs_entry| match fs_entry {
                                        FsEntry::ExternalFile(external) => {
                                            if external.buffer_id == toggled_buffer_id {
                                                Some(fs_entry.clone())
                                            } else {
                                                None
                                            }
                                        }
                                        FsEntry::File(FsEntryFile { buffer_id, .. }) => {
                                            if *buffer_id == toggled_buffer_id {
                                                Some(fs_entry.clone())
                                            } else {
                                                None
                                            }
                                        }
                                        FsEntry::Directory(..) => None,
                                    },
                                )
                            })
                            .map(PanelEntry::Fs)
                        {
                            outline_panel.select_entry(entry_to_select, true, window, cx);
                        }
                    }

                    outline_panel.update_fs_entries(editor.clone(), debounce, window, cx);
                }
                EditorEvent::Reparsed(buffer_id) => {
                    if let Some(excerpts) = outline_panel.excerpts.get_mut(buffer_id) {
                        for (_, excerpt) in excerpts {
                            excerpt.invalidate_outlines();
                        }
                    }
                    let update_cached_items = outline_panel.update_non_fs_items(window, cx);
                    if update_cached_items {
                        outline_panel.update_cached_entries(Some(UPDATE_DEBOUNCE), window, cx);
                    }
                }
                _ => {}
            }
        },
    )
}

fn empty_icon() -> AnyElement {
    h_flex()
        .size(IconSize::default().rems())
        .invisible()
        .flex_none()
        .into_any_element()
}

fn horizontal_separator(cx: &mut App) -> Div {
    div().mx_2().border_primary(cx).border_t_1()
}

#[derive(Debug, Default)]
struct GenerationState {
    entries: Vec<CachedEntry>,
    match_candidates: Vec<StringMatchCandidate>,
    max_width_estimate_and_index: Option<(u64, usize)>,
}

impl GenerationState {
    fn clear(&mut self) {
        self.entries.clear();
        self.match_candidates.clear();
        self.max_width_estimate_and_index = None;
    }
}

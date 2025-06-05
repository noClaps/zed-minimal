use crate::{
    Anchor, Editor, EditorSettings, EditorSnapshot, FindAllReferences, GoToDefinition,
    GoToTypeDefinition, GotoDefinitionKind, InlayId, Navigated, PointForPosition, SelectPhase,
    editor_settings::{GoToDefinitionFallback, MultiCursorModifier},
    hover_popover::{self, InlayHover},
    scroll::ScrollAmount,
};
use gpui::{App, AsyncWindowContext, Context, Entity, Modifiers, Task, Window, px};
use language::{Bias, ToOffset};
use linkify::{LinkFinder, LinkKind};
use lsp::LanguageServerId;
use project::{
    HoverBlock, HoverBlockKind, InlayHintLabelPartTooltip, InlayHintTooltip, LocationLink, Project,
    ResolveState, ResolvedPath,
};
use settings::Settings;
use std::ops::Range;
use theme::ActiveTheme as _;
use util::{ResultExt, TryFutureExt as _, maybe};

#[derive(Debug)]
pub struct HoveredLinkState {
    pub last_trigger_point: TriggerPoint,
    pub preferred_kind: GotoDefinitionKind,
    pub symbol_range: Option<RangeInEditor>,
    pub links: Vec<HoverLink>,
    pub task: Option<Task<Option<()>>>,
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum RangeInEditor {
    Text(Range<Anchor>),
    Inlay(InlayHighlight),
}

impl RangeInEditor {
    pub fn as_text_range(&self) -> Option<Range<Anchor>> {
        match self {
            Self::Text(range) => Some(range.clone()),
            Self::Inlay(_) => None,
        }
    }

    pub fn point_within_range(
        &self,
        trigger_point: &TriggerPoint,
        snapshot: &EditorSnapshot,
    ) -> bool {
        match (self, trigger_point) {
            (Self::Text(range), TriggerPoint::Text(point)) => {
                let point_after_start = range.start.cmp(point, &snapshot.buffer_snapshot).is_le();
                point_after_start && range.end.cmp(point, &snapshot.buffer_snapshot).is_ge()
            }
            (Self::Inlay(highlight), TriggerPoint::InlayHint(point, _, _)) => {
                highlight.inlay == point.inlay
                    && highlight.range.contains(&point.range.start)
                    && highlight.range.contains(&point.range.end)
            }
            (Self::Inlay(_), TriggerPoint::Text(_))
            | (Self::Text(_), TriggerPoint::InlayHint(_, _, _)) => false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum HoverLink {
    Url(String),
    File(ResolvedPath),
    Text(LocationLink),
    InlayHint(lsp::Location, LanguageServerId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHighlight {
    pub inlay: InlayId,
    pub inlay_position: Anchor,
    pub range: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TriggerPoint {
    Text(Anchor),
    InlayHint(InlayHighlight, lsp::Location, LanguageServerId),
}

impl TriggerPoint {
    fn anchor(&self) -> &Anchor {
        match self {
            TriggerPoint::Text(anchor) => anchor,
            TriggerPoint::InlayHint(inlay_range, _, _) => &inlay_range.inlay_position,
        }
    }
}

pub fn exclude_link_to_position(
    buffer: &Entity<language::Buffer>,
    current_position: &text::Anchor,
    location: &LocationLink,
    cx: &App,
) -> bool {
    // Exclude definition links that points back to cursor position.
    // (i.e., currently cursor upon definition).
    let snapshot = buffer.read(cx).snapshot();
    !(buffer == &location.target.buffer
        && current_position
            .bias_right(&snapshot)
            .cmp(&location.target.range.start, &snapshot)
            .is_ge()
        && current_position
            .cmp(&location.target.range.end, &snapshot)
            .is_le())
}

impl Editor {
    pub(crate) fn update_hovered_link(
        &mut self,
        point_for_position: PointForPosition,
        snapshot: &EditorSnapshot,
        modifiers: Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let multi_cursor_setting = EditorSettings::get_global(cx).multi_cursor_modifier;
        let hovered_link_modifier = match multi_cursor_setting {
            MultiCursorModifier::Alt => modifiers.secondary(),
            MultiCursorModifier::CmdOrCtrl => modifiers.alt,
        };
        if !hovered_link_modifier || self.has_pending_selection() {
            self.hide_hovered_link(cx);
            return;
        }

        match point_for_position.as_valid() {
            Some(point) => {
                let trigger_point = TriggerPoint::Text(
                    snapshot
                        .buffer_snapshot
                        .anchor_before(point.to_offset(&snapshot.display_snapshot, Bias::Left)),
                );

                show_link_definition(modifiers.shift, self, trigger_point, snapshot, window, cx);
            }
            None => {
                update_inlay_link_and_hover_points(
                    snapshot,
                    point_for_position,
                    self,
                    hovered_link_modifier,
                    modifiers.shift,
                    window,
                    cx,
                );
            }
        }
    }

    pub(crate) fn hide_hovered_link(&mut self, cx: &mut Context<Self>) {
        self.hovered_link_state.take();
        self.clear_highlights::<HoveredLinkState>(cx);
    }

    pub(crate) fn handle_click_hovered_link(
        &mut self,
        point: PointForPosition,
        modifiers: Modifiers,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        let reveal_task = self.cmd_click_reveal_task(point, modifiers, window, cx);
        cx.spawn_in(window, async move |editor, cx| {
            let definition_revealed = reveal_task.await.log_err().unwrap_or(Navigated::No);
            let find_references = editor
                .update_in(cx, |editor, window, cx| {
                    if definition_revealed == Navigated::Yes {
                        return None;
                    }
                    match EditorSettings::get_global(cx).go_to_definition_fallback {
                        GoToDefinitionFallback::None => None,
                        GoToDefinitionFallback::FindAllReferences => {
                            editor.find_all_references(&FindAllReferences, window, cx)
                        }
                    }
                })
                .ok()
                .flatten();
            if let Some(find_references) = find_references {
                find_references.await.log_err();
            }
        })
        .detach();
    }

    pub fn scroll_hover(
        &mut self,
        amount: &ScrollAmount,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let selection = self.selections.newest_anchor().head();
        let snapshot = self.snapshot(window, cx);

        let Some(popover) = self.hover_state.info_popovers.iter().find(|popover| {
            popover
                .symbol_range
                .point_within_range(&TriggerPoint::Text(selection), &snapshot)
        }) else {
            return false;
        };
        popover.scroll(amount, window, cx);
        true
    }

    fn cmd_click_reveal_task(
        &mut self,
        point: PointForPosition,
        modifiers: Modifiers,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Navigated>> {
        if let Some(hovered_link_state) = self.hovered_link_state.take() {
            self.hide_hovered_link(cx);
            if !hovered_link_state.links.is_empty() {
                if !self.focus_handle.is_focused(window) {
                    window.focus(&self.focus_handle);
                }

                // exclude links pointing back to the current anchor
                let current_position = point
                    .next_valid
                    .to_point(&self.snapshot(window, cx).display_snapshot);
                let Some((buffer, anchor)) = self
                    .buffer()
                    .read(cx)
                    .text_anchor_for_position(current_position, cx)
                else {
                    return Task::ready(Ok(Navigated::No));
                };
                let links = hovered_link_state
                    .links
                    .into_iter()
                    .filter(|link| {
                        if let HoverLink::Text(location) = link {
                            exclude_link_to_position(&buffer, &anchor, location, cx)
                        } else {
                            true
                        }
                    })
                    .collect();
                let navigate_task =
                    self.navigate_to_hover_links(None, links, modifiers.alt, window, cx);
                self.select(SelectPhase::End, window, cx);
                return navigate_task;
            }
        }

        // We don't have the correct kind of link cached, set the selection on
        // click and immediately trigger GoToDefinition.
        self.select(
            SelectPhase::Begin {
                position: point.next_valid,
                add: false,
                click_count: 1,
            },
            window,
            cx,
        );

        let navigate_task = if point.as_valid().is_some() {
            if modifiers.shift {
                self.go_to_type_definition(&GoToTypeDefinition, window, cx)
            } else {
                self.go_to_definition(&GoToDefinition, window, cx)
            }
        } else {
            Task::ready(Ok(Navigated::No))
        };
        self.select(SelectPhase::End, window, cx);
        return navigate_task;
    }
}

pub fn update_inlay_link_and_hover_points(
    snapshot: &EditorSnapshot,
    point_for_position: PointForPosition,
    editor: &mut Editor,
    secondary_held: bool,
    shift_held: bool,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let hovered_offset = if point_for_position.column_overshoot_after_line_end == 0 {
        Some(snapshot.display_point_to_inlay_offset(point_for_position.exact_unclipped, Bias::Left))
    } else {
        None
    };
    let mut go_to_definition_updated = false;
    let mut hover_updated = false;
    if let Some(hovered_offset) = hovered_offset {
        let buffer_snapshot = editor.buffer().read(cx).snapshot(cx);
        let previous_valid_anchor = buffer_snapshot.anchor_at(
            point_for_position.previous_valid.to_point(snapshot),
            Bias::Left,
        );
        let next_valid_anchor = buffer_snapshot.anchor_at(
            point_for_position.next_valid.to_point(snapshot),
            Bias::Right,
        );
        if let Some(hovered_hint) = editor
            .visible_inlay_hints(cx)
            .into_iter()
            .skip_while(|hint| {
                hint.position
                    .cmp(&previous_valid_anchor, &buffer_snapshot)
                    .is_lt()
            })
            .take_while(|hint| {
                hint.position
                    .cmp(&next_valid_anchor, &buffer_snapshot)
                    .is_le()
            })
            .max_by_key(|hint| hint.id)
        {
            let inlay_hint_cache = editor.inlay_hint_cache();
            let excerpt_id = previous_valid_anchor.excerpt_id;
            if let Some(cached_hint) = inlay_hint_cache.hint_by_id(excerpt_id, hovered_hint.id) {
                match cached_hint.resolve_state {
                    ResolveState::CanResolve(_, _) => {
                        if let Some(buffer_id) = previous_valid_anchor.buffer_id {
                            inlay_hint_cache.spawn_hint_resolve(
                                buffer_id,
                                excerpt_id,
                                hovered_hint.id,
                                window,
                                cx,
                            );
                        }
                    }
                    ResolveState::Resolved => {
                        let mut extra_shift_left = 0;
                        let mut extra_shift_right = 0;
                        if cached_hint.padding_left {
                            extra_shift_left += 1;
                            extra_shift_right += 1;
                        }
                        if cached_hint.padding_right {
                            extra_shift_right += 1;
                        }
                        match cached_hint.label {
                            project::InlayHintLabel::String(_) => {
                                if let Some(tooltip) = cached_hint.tooltip {
                                    hover_popover::hover_at_inlay(
                                        editor,
                                        InlayHover {
                                            tooltip: match tooltip {
                                                InlayHintTooltip::String(text) => HoverBlock {
                                                    text,
                                                    kind: HoverBlockKind::PlainText,
                                                },
                                                InlayHintTooltip::MarkupContent(content) => {
                                                    HoverBlock {
                                                        text: content.value,
                                                        kind: content.kind,
                                                    }
                                                }
                                            },
                                            range: InlayHighlight {
                                                inlay: hovered_hint.id,
                                                inlay_position: hovered_hint.position,
                                                range: extra_shift_left
                                                    ..hovered_hint.text.len() + extra_shift_right,
                                            },
                                        },
                                        window,
                                        cx,
                                    );
                                    hover_updated = true;
                                }
                            }
                            project::InlayHintLabel::LabelParts(label_parts) => {
                                let hint_start =
                                    snapshot.anchor_to_inlay_offset(hovered_hint.position);
                                if let Some((hovered_hint_part, part_range)) =
                                    hover_popover::find_hovered_hint_part(
                                        label_parts,
                                        hint_start,
                                        hovered_offset,
                                    )
                                {
                                    let highlight_start =
                                        (part_range.start - hint_start).0 + extra_shift_left;
                                    let highlight_end =
                                        (part_range.end - hint_start).0 + extra_shift_right;
                                    let highlight = InlayHighlight {
                                        inlay: hovered_hint.id,
                                        inlay_position: hovered_hint.position,
                                        range: highlight_start..highlight_end,
                                    };
                                    if let Some(tooltip) = hovered_hint_part.tooltip {
                                        hover_popover::hover_at_inlay(
                                            editor,
                                            InlayHover {
                                                tooltip: match tooltip {
                                                    InlayHintLabelPartTooltip::String(text) => {
                                                        HoverBlock {
                                                            text,
                                                            kind: HoverBlockKind::PlainText,
                                                        }
                                                    }
                                                    InlayHintLabelPartTooltip::MarkupContent(
                                                        content,
                                                    ) => HoverBlock {
                                                        text: content.value,
                                                        kind: content.kind,
                                                    },
                                                },
                                                range: highlight.clone(),
                                            },
                                            window,
                                            cx,
                                        );
                                        hover_updated = true;
                                    }
                                    if let Some((language_server_id, location)) =
                                        hovered_hint_part.location
                                    {
                                        if secondary_held
                                            && !editor.has_pending_nonempty_selection()
                                        {
                                            go_to_definition_updated = true;
                                            show_link_definition(
                                                shift_held,
                                                editor,
                                                TriggerPoint::InlayHint(
                                                    highlight,
                                                    location,
                                                    language_server_id,
                                                ),
                                                snapshot,
                                                window,
                                                cx,
                                            );
                                        }
                                    }
                                }
                            }
                        };
                    }
                    ResolveState::Resolving => {}
                }
            }
        }
    }

    if !go_to_definition_updated {
        editor.hide_hovered_link(cx)
    }
    if !hover_updated {
        hover_popover::hover_at(editor, None, window, cx);
    }
}

pub fn show_link_definition(
    shift_held: bool,
    editor: &mut Editor,
    trigger_point: TriggerPoint,
    snapshot: &EditorSnapshot,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let preferred_kind = match trigger_point {
        TriggerPoint::Text(_) if !shift_held => GotoDefinitionKind::Symbol,
        _ => GotoDefinitionKind::Type,
    };

    let (mut hovered_link_state, is_cached) =
        if let Some(existing) = editor.hovered_link_state.take() {
            (existing, true)
        } else {
            (
                HoveredLinkState {
                    last_trigger_point: trigger_point.clone(),
                    symbol_range: None,
                    preferred_kind,
                    links: vec![],
                    task: None,
                },
                false,
            )
        };

    if editor.pending_rename.is_some() {
        return;
    }

    let trigger_anchor = trigger_point.anchor();
    let Some((buffer, buffer_position)) = editor
        .buffer
        .read(cx)
        .text_anchor_for_position(*trigger_anchor, cx)
    else {
        return;
    };

    let Some((excerpt_id, _, _)) = editor
        .buffer()
        .read(cx)
        .excerpt_containing(*trigger_anchor, cx)
    else {
        return;
    };

    let same_kind = hovered_link_state.preferred_kind == preferred_kind
        || hovered_link_state
            .links
            .first()
            .is_some_and(|d| matches!(d, HoverLink::Url(_)));

    if same_kind {
        if is_cached && (hovered_link_state.last_trigger_point == trigger_point)
            || hovered_link_state
                .symbol_range
                .as_ref()
                .is_some_and(|symbol_range| {
                    symbol_range.point_within_range(&trigger_point, snapshot)
                })
        {
            editor.hovered_link_state = Some(hovered_link_state);
            return;
        }
    } else {
        editor.hide_hovered_link(cx)
    }
    let project = editor.project.clone();
    let provider = editor.semantics_provider.clone();

    let snapshot = snapshot.buffer_snapshot.clone();
    hovered_link_state.task = Some(cx.spawn_in(window, async move |this, cx| {
        async move {
            let result = match &trigger_point {
                TriggerPoint::Text(_) => {
                    if let Some((url_range, url)) = find_url(&buffer, buffer_position, cx.clone()) {
                        this.read_with(cx, |_, _| {
                            let range = maybe!({
                                let start =
                                    snapshot.anchor_in_excerpt(excerpt_id, url_range.start)?;
                                let end = snapshot.anchor_in_excerpt(excerpt_id, url_range.end)?;
                                Some(RangeInEditor::Text(start..end))
                            });
                            (range, vec![HoverLink::Url(url)])
                        })
                        .ok()
                    } else if let Some((filename_range, filename)) =
                        find_file(&buffer, project.clone(), buffer_position, cx).await
                    {
                        let range = maybe!({
                            let start =
                                snapshot.anchor_in_excerpt(excerpt_id, filename_range.start)?;
                            let end = snapshot.anchor_in_excerpt(excerpt_id, filename_range.end)?;
                            Some(RangeInEditor::Text(start..end))
                        });

                        Some((range, vec![HoverLink::File(filename)]))
                    } else if let Some(provider) = provider {
                        let task = cx.update(|_, cx| {
                            provider.definitions(&buffer, buffer_position, preferred_kind, cx)
                        })?;
                        if let Some(task) = task {
                            task.await.ok().map(|definition_result| {
                                (
                                    definition_result.iter().find_map(|link| {
                                        link.origin.as_ref().and_then(|origin| {
                                            let start = snapshot.anchor_in_excerpt(
                                                excerpt_id,
                                                origin.range.start,
                                            )?;
                                            let end = snapshot
                                                .anchor_in_excerpt(excerpt_id, origin.range.end)?;
                                            Some(RangeInEditor::Text(start..end))
                                        })
                                    }),
                                    definition_result.into_iter().map(HoverLink::Text).collect(),
                                )
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                TriggerPoint::InlayHint(highlight, lsp_location, server_id) => Some((
                    Some(RangeInEditor::Inlay(highlight.clone())),
                    vec![HoverLink::InlayHint(lsp_location.clone(), *server_id)],
                )),
            };

            this.update(cx, |editor, cx| {
                // Clear any existing highlights
                editor.clear_highlights::<HoveredLinkState>(cx);
                let Some(hovered_link_state) = editor.hovered_link_state.as_mut() else {
                    editor.hide_hovered_link(cx);
                    return;
                };
                hovered_link_state.preferred_kind = preferred_kind;
                hovered_link_state.symbol_range = result
                    .as_ref()
                    .and_then(|(symbol_range, _)| symbol_range.clone());

                if let Some((symbol_range, definitions)) = result {
                    hovered_link_state.links = definitions;

                    let underline_hovered_link = !hovered_link_state.links.is_empty()
                        || hovered_link_state.symbol_range.is_some();

                    if underline_hovered_link {
                        let style = gpui::HighlightStyle {
                            underline: Some(gpui::UnderlineStyle {
                                thickness: px(1.),
                                ..Default::default()
                            }),
                            color: Some(cx.theme().colors().link_text_hover),
                            ..Default::default()
                        };
                        let highlight_range =
                            symbol_range.unwrap_or_else(|| match &trigger_point {
                                TriggerPoint::Text(trigger_anchor) => {
                                    // If no symbol range returned from language server, use the surrounding word.
                                    let (offset_range, _) =
                                        snapshot.surrounding_word(*trigger_anchor, false);
                                    RangeInEditor::Text(
                                        snapshot.anchor_before(offset_range.start)
                                            ..snapshot.anchor_after(offset_range.end),
                                    )
                                }
                                TriggerPoint::InlayHint(highlight, _, _) => {
                                    RangeInEditor::Inlay(highlight.clone())
                                }
                            });

                        match highlight_range {
                            RangeInEditor::Text(text_range) => editor
                                .highlight_text::<HoveredLinkState>(vec![text_range], style, cx),
                            RangeInEditor::Inlay(highlight) => editor
                                .highlight_inlays::<HoveredLinkState>(vec![highlight], style, cx),
                        }
                    }
                } else {
                    editor.hide_hovered_link(cx);
                }
            })?;

            anyhow::Ok(())
        }
        .log_err()
        .await
    }));

    editor.hovered_link_state = Some(hovered_link_state);
}

pub(crate) fn find_url(
    buffer: &Entity<language::Buffer>,
    position: text::Anchor,
    mut cx: AsyncWindowContext,
) -> Option<(Range<text::Anchor>, String)> {
    const LIMIT: usize = 2048;

    let Ok(snapshot) = buffer.read_with(&mut cx, |buffer, _| buffer.snapshot()) else {
        return None;
    };

    let offset = position.to_offset(&snapshot);
    let mut token_start = offset;
    let mut token_end = offset;
    let mut found_start = false;
    let mut found_end = false;

    for ch in snapshot.reversed_chars_at(offset).take(LIMIT) {
        if ch.is_whitespace() {
            found_start = true;
            break;
        }
        token_start -= ch.len_utf8();
    }
    // Check if we didn't find the starting whitespace or if we didn't reach the start of the buffer
    if !found_start && token_start != 0 {
        return None;
    }

    for ch in snapshot
        .chars_at(offset)
        .take(LIMIT - (offset - token_start))
    {
        if ch.is_whitespace() {
            found_end = true;
            break;
        }
        token_end += ch.len_utf8();
    }
    // Check if we didn't find the ending whitespace or if we read more or equal than LIMIT
    // which at this point would happen only if we reached the end of buffer
    if !found_end && (token_end - token_start >= LIMIT) {
        return None;
    }

    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url]);
    let input = snapshot
        .text_for_range(token_start..token_end)
        .collect::<String>();

    let relative_offset = offset - token_start;
    for link in finder.links(&input) {
        if link.start() <= relative_offset && link.end() >= relative_offset {
            let range = snapshot.anchor_before(token_start + link.start())
                ..snapshot.anchor_after(token_start + link.end());
            return Some((range, link.as_str().to_string()));
        }
    }
    None
}

pub(crate) fn find_url_from_range(
    buffer: &Entity<language::Buffer>,
    range: Range<text::Anchor>,
    mut cx: AsyncWindowContext,
) -> Option<String> {
    const LIMIT: usize = 2048;

    let Ok(snapshot) = buffer.read_with(&mut cx, |buffer, _| buffer.snapshot()) else {
        return None;
    };

    let start_offset = range.start.to_offset(&snapshot);
    let end_offset = range.end.to_offset(&snapshot);

    let mut token_start = start_offset.min(end_offset);
    let mut token_end = start_offset.max(end_offset);

    let range_len = token_end - token_start;

    if range_len >= LIMIT {
        return None;
    }

    // Skip leading whitespace
    for ch in snapshot.chars_at(token_start).take(range_len) {
        if !ch.is_whitespace() {
            break;
        }
        token_start += ch.len_utf8();
    }

    // Skip trailing whitespace
    for ch in snapshot.reversed_chars_at(token_end).take(range_len) {
        if !ch.is_whitespace() {
            break;
        }
        token_end -= ch.len_utf8();
    }

    if token_start >= token_end {
        return None;
    }

    let text = snapshot
        .text_for_range(token_start..token_end)
        .collect::<String>();

    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url]);

    if let Some(link) = finder.links(&text).next() {
        if link.start() == 0 && link.end() == text.len() {
            return Some(link.as_str().to_string());
        }
    }

    None
}

pub(crate) async fn find_file(
    buffer: &Entity<language::Buffer>,
    project: Option<Entity<Project>>,
    position: text::Anchor,
    cx: &mut AsyncWindowContext,
) -> Option<(Range<text::Anchor>, ResolvedPath)> {
    let project = project?;
    let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot()).ok()?;
    let scope = snapshot.language_scope_at(position);
    let (range, candidate_file_path) = surrounding_filename(snapshot, position)?;

    async fn check_path(
        candidate_file_path: &str,
        project: &Entity<Project>,
        buffer: &Entity<language::Buffer>,
        cx: &mut AsyncWindowContext,
    ) -> Option<ResolvedPath> {
        project
            .update(cx, |project, cx| {
                project.resolve_path_in_buffer(&candidate_file_path, buffer, cx)
            })
            .ok()?
            .await
            .filter(|s| s.is_file())
    }

    if let Some(existing_path) = check_path(&candidate_file_path, &project, buffer, cx).await {
        return Some((range, existing_path));
    }

    if let Some(scope) = scope {
        for suffix in scope.path_suffixes() {
            if candidate_file_path.ends_with(format!(".{suffix}").as_str()) {
                continue;
            }

            let suffixed_candidate = format!("{candidate_file_path}.{suffix}");
            if let Some(existing_path) = check_path(&suffixed_candidate, &project, buffer, cx).await
            {
                return Some((range, existing_path));
            }
        }
    }

    None
}

fn surrounding_filename(
    snapshot: language::BufferSnapshot,
    position: text::Anchor,
) -> Option<(Range<text::Anchor>, String)> {
    const LIMIT: usize = 2048;

    let offset = position.to_offset(&snapshot);
    let mut token_start = offset;
    let mut token_end = offset;
    let mut found_start = false;
    let mut found_end = false;
    let mut inside_quotes = false;

    let mut filename = String::new();

    let mut backwards = snapshot.reversed_chars_at(offset).take(LIMIT).peekable();
    while let Some(ch) = backwards.next() {
        // Escaped whitespace
        if ch.is_whitespace() && backwards.peek() == Some(&'\\') {
            filename.push(ch);
            token_start -= ch.len_utf8();
            backwards.next();
            token_start -= '\\'.len_utf8();
            continue;
        }
        if ch.is_whitespace() {
            found_start = true;
            break;
        }
        if (ch == '"' || ch == '\'') && !inside_quotes {
            found_start = true;
            inside_quotes = true;
            break;
        }

        filename.push(ch);
        token_start -= ch.len_utf8();
    }
    if !found_start && token_start != 0 {
        return None;
    }

    filename = filename.chars().rev().collect();

    let mut forwards = snapshot
        .chars_at(offset)
        .take(LIMIT - (offset - token_start))
        .peekable();
    while let Some(ch) = forwards.next() {
        // Skip escaped whitespace
        if ch == '\\' && forwards.peek().map_or(false, |ch| ch.is_whitespace()) {
            token_end += ch.len_utf8();
            let whitespace = forwards.next().unwrap();
            token_end += whitespace.len_utf8();
            filename.push(whitespace);
            continue;
        }

        if ch.is_whitespace() {
            found_end = true;
            break;
        }
        if ch == '"' || ch == '\'' {
            // If we're inside quotes, we stop when we come across the next quote
            if inside_quotes {
                found_end = true;
                break;
            } else {
                // Otherwise, we skip the quote
                inside_quotes = true;
                continue;
            }
        }
        filename.push(ch);
        token_end += ch.len_utf8();
    }

    if !found_end && (token_end - token_start >= LIMIT) {
        return None;
    }

    if filename.is_empty() {
        return None;
    }

    let range = snapshot.anchor_before(token_start)..snapshot.anchor_after(token_end);

    Some((range, filename))
}

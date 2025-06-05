pub mod cursor_position;

use cursor_position::{LineIndicatorFormat, UserCaretPosition};
use editor::{
    Anchor, Editor, MultiBufferSnapshot, RowHighlightOptions, ToOffset, ToPoint, actions::Tab,
    scroll::Autoscroll,
};
use gpui::{
    App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString, Styled,
    Subscription, div, prelude::*,
};
use language::Buffer;
use settings::Settings;
use text::{Bias, Point};
use theme::ActiveTheme;
use ui::prelude::*;
use util::paths::FILE_ROW_COLUMN_DELIMITER;
use workspace::ModalView;

pub fn init(cx: &mut App) {
    LineIndicatorFormat::register(cx);
    cx.observe_new(GoToLine::register).detach();
}

pub struct GoToLine {
    line_editor: Entity<Editor>,
    active_editor: Entity<Editor>,
    current_text: SharedString,
    prev_scroll_position: Option<gpui::Point<f32>>,
    _subscriptions: Vec<Subscription>,
}

impl ModalView for GoToLine {}

impl Focusable for GoToLine {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.line_editor.focus_handle(cx)
    }
}
impl EventEmitter<DismissEvent> for GoToLine {}

enum GoToLineRowHighlights {}

impl GoToLine {
    fn register(editor: &mut Editor, _window: Option<&mut Window>, cx: &mut Context<Editor>) {
        let handle = cx.entity().downgrade();
        editor
            .register_action(move |_: &editor::actions::ToggleGoToLine, window, cx| {
                let Some(editor_handle) = handle.upgrade() else {
                    return;
                };
                let Some(workspace) = editor_handle.read(cx).workspace() else {
                    return;
                };
                let editor = editor_handle.read(cx);
                let Some((_, buffer, _)) = editor.active_excerpt(cx) else {
                    return;
                };
                workspace.update(cx, |workspace, cx| {
                    workspace.toggle_modal(window, cx, move |window, cx| {
                        GoToLine::new(editor_handle, buffer, window, cx)
                    });
                })
            })
            .detach();
    }

    pub fn new(
        active_editor: Entity<Editor>,
        active_buffer: Entity<Buffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (user_caret, last_line, scroll_position) = active_editor.update(cx, |editor, cx| {
            let user_caret = UserCaretPosition::at_selection_end(
                &editor.selections.last::<Point>(cx),
                &editor.buffer().read(cx).snapshot(cx),
            );

            let snapshot = active_buffer.read(cx).snapshot();
            let last_line = editor
                .buffer()
                .read(cx)
                .excerpts_for_buffer(snapshot.remote_id(), cx)
                .into_iter()
                .map(move |(_, range)| text::ToPoint::to_point(&range.context.end, &snapshot).row)
                .max()
                .unwrap_or(0);

            (user_caret, last_line, editor.scroll_position(cx))
        });

        let line = user_caret.line.get();
        let column = user_caret.character.get();

        let line_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            let editor_handle = cx.entity().downgrade();
            editor
                .register_action::<Tab>({
                    move |_, window, cx| {
                        let Some(editor) = editor_handle.upgrade() else {
                            return;
                        };
                        editor.update(cx, |editor, cx| {
                            if let Some(placeholder_text) = editor.placeholder_text() {
                                if editor.text(cx).is_empty() {
                                    let placeholder_text = placeholder_text.to_string();
                                    editor.set_text(placeholder_text, window, cx);
                                }
                            }
                        });
                    }
                })
                .detach();
            editor.set_placeholder_text(format!("{line}{FILE_ROW_COLUMN_DELIMITER}{column}"), cx);
            editor
        });
        let line_editor_change = cx.subscribe_in(&line_editor, window, Self::on_line_editor_event);

        let current_text = format!(
            "Current Line: {} of {} (column {})",
            line,
            last_line + 1,
            column
        );

        Self {
            line_editor,
            active_editor,
            current_text: current_text.into(),
            prev_scroll_position: Some(scroll_position),
            _subscriptions: vec![line_editor_change, cx.on_release_in(window, Self::release)],
        }
    }

    fn release(&mut self, window: &mut Window, cx: &mut App) {
        let scroll_position = self.prev_scroll_position.take();
        self.active_editor.update(cx, |editor, cx| {
            editor.clear_row_highlights::<GoToLineRowHighlights>();
            if let Some(scroll_position) = scroll_position {
                editor.set_scroll_position(scroll_position, window, cx);
            }
            cx.notify();
        })
    }

    fn on_line_editor_event(
        &mut self,
        _: &Entity<Editor>,
        event: &editor::EditorEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            editor::EditorEvent::Blurred => {
                self.prev_scroll_position.take();
                cx.emit(DismissEvent)
            }
            editor::EditorEvent::BufferEdited { .. } => self.highlight_current_line(cx),
            _ => {}
        }
    }

    fn highlight_current_line(&mut self, cx: &mut Context<Self>) {
        self.active_editor.update(cx, |editor, cx| {
            editor.clear_row_highlights::<GoToLineRowHighlights>();
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let Some(start) = self.anchor_from_query(&snapshot, cx) else {
                return;
            };
            let mut start_point = start.to_point(&snapshot);
            start_point.column = 0;
            // Force non-empty range to ensure the line is highlighted.
            let mut end_point = snapshot.clip_point(start_point + Point::new(0, 1), Bias::Left);
            if start_point == end_point {
                end_point = snapshot.clip_point(start_point + Point::new(1, 0), Bias::Left);
            }

            let end = snapshot.anchor_after(end_point);
            editor.highlight_rows::<GoToLineRowHighlights>(
                start..end,
                cx.theme().colors().editor_highlighted_line_background,
                RowHighlightOptions {
                    autoscroll: true,
                    ..Default::default()
                },
                cx,
            );
            editor.request_autoscroll(Autoscroll::center(), cx);
        });
        cx.notify();
    }

    fn anchor_from_query(
        &self,
        snapshot: &MultiBufferSnapshot,
        cx: &Context<Editor>,
    ) -> Option<Anchor> {
        let (query_row, query_char) = self.line_and_char_from_query(cx)?;
        let row = query_row.saturating_sub(1);
        let character = query_char.unwrap_or(0).saturating_sub(1);

        let start_offset = Point::new(row, 0).to_offset(snapshot);
        const MAX_BYTES_IN_UTF_8: u32 = 4;
        let max_end_offset = snapshot
            .clip_point(
                Point::new(row, character * MAX_BYTES_IN_UTF_8 + 1),
                Bias::Right,
            )
            .to_offset(snapshot);

        let mut chars_to_iterate = character;
        let mut end_offset = start_offset;
        'outer: for text_chunk in snapshot.text_for_range(start_offset..max_end_offset) {
            let mut offset_increment = 0;
            for c in text_chunk.chars() {
                if chars_to_iterate == 0 {
                    end_offset += offset_increment;
                    break 'outer;
                } else {
                    chars_to_iterate -= 1;
                    offset_increment += c.len_utf8();
                }
            }
            end_offset += offset_increment;
        }
        Some(snapshot.anchor_before(snapshot.clip_offset(end_offset, Bias::Left)))
    }

    fn line_and_char_from_query(&self, cx: &App) -> Option<(u32, Option<u32>)> {
        let input = self.line_editor.read(cx).text(cx);
        let mut components = input
            .splitn(2, FILE_ROW_COLUMN_DELIMITER)
            .map(str::trim)
            .fuse();
        let row = components.next().and_then(|row| row.parse::<u32>().ok())?;
        let column = components.next().and_then(|col| col.parse::<u32>().ok());
        Some((row, column))
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.active_editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let Some(start) = self.anchor_from_query(&snapshot, cx) else {
                return;
            };
            editor.change_selections(Some(Autoscroll::center()), window, cx, |s| {
                s.select_anchor_ranges([start..start])
            });
            editor.focus_handle(cx).focus(window);
            cx.notify()
        });
        self.prev_scroll_position.take();

        cx.emit(DismissEvent);
    }
}

impl Render for GoToLine {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let help_text = match self.line_and_char_from_query(cx) {
            Some((line, Some(character))) => {
                format!("Go to line {line}, character {character}").into()
            }
            Some((line, None)) => format!("Go to line {line}").into(),
            None => self.current_text.clone(),
        };

        v_flex()
            .w(rems(24.))
            .elevation_2(cx)
            .key_context("GoToLine")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .child(
                div()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .px_2()
                    .py_1()
                    .child(self.line_editor.clone()),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .child(Label::new(help_text).color(Color::Muted)),
            )
    }
}

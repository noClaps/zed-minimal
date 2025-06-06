//! REPL operations on an [`Editor`].

use std::ops::Range;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use editor::Editor;
use gpui::{App, Entity, WeakEntity, Window, prelude::*};
use language::{BufferSnapshot, Language, LanguageName, Point};
use project::{ProjectItem as _, WorktreeId};

use crate::repl_store::ReplStore;
use crate::session::SessionEvent;
use crate::{
    ClearOutputs, Interrupt, JupyterSettings, KernelSpecification, Restart, Session, Shutdown,
};

pub fn assign_kernelspec(
    kernel_specification: KernelSpecification,
    weak_editor: WeakEntity<Editor>,
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    let store = ReplStore::global(cx);
    if !store.read(cx).is_enabled() {
        return Ok(());
    }

    let worktree_id = crate::repl_editor::worktree_id_for_editor(weak_editor.clone(), cx)
        .context("editor is not in a worktree")?;

    store.update(cx, |store, cx| {
        store.set_active_kernelspec(worktree_id, kernel_specification.clone(), cx);
    });

    let fs = store.read(cx).fs().clone();

    if let Some(session) = store.read(cx).get_session(weak_editor.entity_id()).cloned() {
        // Drop previous session, start new one
        session.update(cx, |session, cx| {
            session.clear_outputs(cx);
            session.shutdown(window, cx);
            cx.notify();
        });
    }

    let session =
        cx.new(|cx| Session::new(weak_editor.clone(), fs, kernel_specification, window, cx));

    weak_editor
        .update(cx, |_editor, cx| {
            cx.notify();

            cx.subscribe(&session, {
                let store = store.clone();
                move |_this, _session, event, cx| match event {
                    SessionEvent::Shutdown(shutdown_event) => {
                        store.update(cx, |store, _cx| {
                            store.remove_session(shutdown_event.entity_id());
                        });
                    }
                }
            })
            .detach();
        })
        .ok();

    store.update(cx, |store, _cx| {
        store.insert_session(weak_editor.entity_id(), session.clone());
    });

    Ok(())
}

pub fn run(
    editor: WeakEntity<Editor>,
    move_down: bool,
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    let store = ReplStore::global(cx);
    if !store.read(cx).is_enabled() {
        return Ok(());
    }

    let editor = editor.upgrade().context("editor was dropped")?;
    let selected_range = editor
        .update(cx, |editor, cx| editor.selections.newest_adjusted(cx))
        .range();
    let multibuffer = editor.read(cx).buffer().clone();
    let Some(buffer) = multibuffer.read(cx).as_singleton() else {
        return Ok(());
    };

    let Some(project_path) = buffer.read(cx).project_path(cx) else {
        return Ok(());
    };

    let (runnable_ranges, next_cell_point) =
        runnable_ranges(&buffer.read(cx).snapshot(), selected_range, cx);

    for runnable_range in runnable_ranges {
        let Some(language) = multibuffer.read(cx).language_at(runnable_range.start, cx) else {
            continue;
        };

        let kernel_specification = store
            .read(cx)
            .active_kernelspec(project_path.worktree_id, Some(language.clone()), cx)
            .with_context(|| format!("No kernel found for language: {}", language.name()))?;

        let fs = store.read(cx).fs().clone();

        let session = if let Some(session) = store.read(cx).get_session(editor.entity_id()).cloned()
        {
            session
        } else {
            let weak_editor = editor.downgrade();
            let session =
                cx.new(|cx| Session::new(weak_editor, fs, kernel_specification, window, cx));

            editor.update(cx, |_editor, cx| {
                cx.notify();

                cx.subscribe(&session, {
                    let store = store.clone();
                    move |_this, _session, event, cx| match event {
                        SessionEvent::Shutdown(shutdown_event) => {
                            store.update(cx, |store, _cx| {
                                store.remove_session(shutdown_event.entity_id());
                            });
                        }
                    }
                })
                .detach();
            });

            store.update(cx, |store, _cx| {
                store.insert_session(editor.entity_id(), session.clone());
            });

            session
        };

        let selected_text;
        let anchor_range;
        let next_cursor;
        {
            let snapshot = multibuffer.read(cx).read(cx);
            selected_text = snapshot
                .text_for_range(runnable_range.clone())
                .collect::<String>();
            anchor_range = snapshot.anchor_before(runnable_range.start)
                ..snapshot.anchor_after(runnable_range.end);
            next_cursor = next_cell_point.map(|point| snapshot.anchor_after(point));
        }

        session.update(cx, |session, cx| {
            session.execute(
                selected_text,
                anchor_range,
                next_cursor,
                move_down,
                window,
                cx,
            );
        });
    }

    anyhow::Ok(())
}

pub enum SessionSupport {
    ActiveSession(Entity<Session>),
    Inactive(KernelSpecification),
    RequiresSetup(LanguageName),
    Unsupported,
}

pub fn worktree_id_for_editor(editor: WeakEntity<Editor>, cx: &mut App) -> Option<WorktreeId> {
    editor.upgrade().and_then(|editor| {
        editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()?
            .read(cx)
            .project_path(cx)
            .map(|path| path.worktree_id)
    })
}

pub fn session(editor: WeakEntity<Editor>, cx: &mut App) -> SessionSupport {
    let store = ReplStore::global(cx);
    let entity_id = editor.entity_id();

    if let Some(session) = store.read(cx).get_session(entity_id).cloned() {
        return SessionSupport::ActiveSession(session);
    };

    let Some(language) = get_language(editor.clone(), cx) else {
        return SessionSupport::Unsupported;
    };

    let worktree_id = worktree_id_for_editor(editor.clone(), cx);

    let Some(worktree_id) = worktree_id else {
        return SessionSupport::Unsupported;
    };

    let kernelspec = store
        .read(cx)
        .active_kernelspec(worktree_id, Some(language.clone()), cx);

    match kernelspec {
        Some(kernelspec) => SessionSupport::Inactive(kernelspec),
        None => {
            // For language_supported, need to check available kernels for language
            if language_supported(&language.clone(), cx) {
                SessionSupport::RequiresSetup(language.name())
            } else {
                SessionSupport::Unsupported
            }
        }
    }
}

pub fn clear_outputs(editor: WeakEntity<Editor>, cx: &mut App) {
    let store = ReplStore::global(cx);
    let entity_id = editor.entity_id();
    let Some(session) = store.read(cx).get_session(entity_id).cloned() else {
        return;
    };
    session.update(cx, |session, cx| {
        session.clear_outputs(cx);
        cx.notify();
    });
}

pub fn interrupt(editor: WeakEntity<Editor>, cx: &mut App) {
    let store = ReplStore::global(cx);
    let entity_id = editor.entity_id();
    let Some(session) = store.read(cx).get_session(entity_id).cloned() else {
        return;
    };

    session.update(cx, |session, cx| {
        session.interrupt(cx);
        cx.notify();
    });
}

pub fn shutdown(editor: WeakEntity<Editor>, window: &mut Window, cx: &mut App) {
    let store = ReplStore::global(cx);
    let entity_id = editor.entity_id();
    let Some(session) = store.read(cx).get_session(entity_id).cloned() else {
        return;
    };

    session.update(cx, |session, cx| {
        session.shutdown(window, cx);
        cx.notify();
    });
}

pub fn restart(editor: WeakEntity<Editor>, window: &mut Window, cx: &mut App) {
    let Some(editor) = editor.upgrade() else {
        return;
    };

    let entity_id = editor.entity_id();

    let Some(session) = ReplStore::global(cx)
        .read(cx)
        .get_session(entity_id)
        .cloned()
    else {
        return;
    };

    session.update(cx, |session, cx| {
        session.restart(window, cx);
        cx.notify();
    });
}

pub fn setup_editor_session_actions(editor: &mut Editor, editor_handle: WeakEntity<Editor>) {
    editor
        .register_action({
            let editor_handle = editor_handle.clone();
            move |_: &ClearOutputs, _, cx| {
                if !JupyterSettings::enabled(cx) {
                    return;
                }

                crate::clear_outputs(editor_handle.clone(), cx);
            }
        })
        .detach();

    editor
        .register_action({
            let editor_handle = editor_handle.clone();
            move |_: &Interrupt, _, cx| {
                if !JupyterSettings::enabled(cx) {
                    return;
                }

                crate::interrupt(editor_handle.clone(), cx);
            }
        })
        .detach();

    editor
        .register_action({
            let editor_handle = editor_handle.clone();
            move |_: &Shutdown, window, cx| {
                if !JupyterSettings::enabled(cx) {
                    return;
                }

                crate::shutdown(editor_handle.clone(), window, cx);
            }
        })
        .detach();

    editor
        .register_action({
            let editor_handle = editor_handle.clone();
            move |_: &Restart, window, cx| {
                if !JupyterSettings::enabled(cx) {
                    return;
                }

                crate::restart(editor_handle.clone(), window, cx);
            }
        })
        .detach();
}

fn cell_range(buffer: &BufferSnapshot, start_row: u32, end_row: u32) -> Range<Point> {
    let mut snippet_end_row = end_row;
    while buffer.is_line_blank(snippet_end_row) && snippet_end_row > start_row {
        snippet_end_row -= 1;
    }
    Point::new(start_row, 0)..Point::new(snippet_end_row, buffer.line_len(snippet_end_row))
}

// Returns the ranges of the snippets in the buffer and the next point for moving the cursor to
fn jupytext_cells(
    buffer: &BufferSnapshot,
    range: Range<Point>,
) -> (Vec<Range<Point>>, Option<Point>) {
    let mut current_row = range.start.row;

    let Some(language) = buffer.language() else {
        return (Vec::new(), None);
    };

    let default_scope = language.default_scope();
    let comment_prefixes = default_scope.line_comment_prefixes();
    if comment_prefixes.is_empty() {
        return (Vec::new(), None);
    }

    let jupytext_prefixes = comment_prefixes
        .iter()
        .map(|comment_prefix| format!("{comment_prefix}%%"))
        .collect::<Vec<_>>();

    let mut snippet_start_row = None;
    loop {
        if jupytext_prefixes
            .iter()
            .any(|prefix| buffer.contains_str_at(Point::new(current_row, 0), prefix))
        {
            snippet_start_row = Some(current_row);
            break;
        } else if current_row > 0 {
            current_row -= 1;
        } else {
            break;
        }
    }

    let mut snippets = Vec::new();
    if let Some(mut snippet_start_row) = snippet_start_row {
        for current_row in range.start.row + 1..=buffer.max_point().row {
            if jupytext_prefixes
                .iter()
                .any(|prefix| buffer.contains_str_at(Point::new(current_row, 0), prefix))
            {
                snippets.push(cell_range(buffer, snippet_start_row, current_row - 1));

                if current_row <= range.end.row {
                    snippet_start_row = current_row;
                } else {
                    // Return our snippets as well as the next point for moving the cursor to
                    return (snippets, Some(Point::new(current_row, 0)));
                }
            }
        }

        // Go to the end of the buffer (no more jupytext cells found)
        snippets.push(cell_range(
            buffer,
            snippet_start_row,
            buffer.max_point().row,
        ));
    }

    (snippets, None)
}

fn runnable_ranges(
    buffer: &BufferSnapshot,
    range: Range<Point>,
    cx: &mut App,
) -> (Vec<Range<Point>>, Option<Point>) {
    if let Some(language) = buffer.language() {
        if language.name() == "Markdown".into() {
            return (markdown_code_blocks(buffer, range.clone(), cx), None);
        }
    }

    let (jupytext_snippets, next_cursor) = jupytext_cells(buffer, range.clone());
    if !jupytext_snippets.is_empty() {
        return (jupytext_snippets, next_cursor);
    }

    let snippet_range = cell_range(buffer, range.start.row, range.end.row);
    let start_language = buffer.language_at(snippet_range.start);
    let end_language = buffer.language_at(snippet_range.end);

    if start_language
        .zip(end_language)
        .map_or(false, |(start, end)| start == end)
    {
        (vec![snippet_range], None)
    } else {
        (Vec::new(), None)
    }
}

// We allow markdown code blocks to end in a trailing newline in order to render the output
// below the final code fence. This is different than our behavior for selections and Jupytext cells.
fn markdown_code_blocks(
    buffer: &BufferSnapshot,
    range: Range<Point>,
    cx: &mut App,
) -> Vec<Range<Point>> {
    buffer
        .injections_intersecting_range(range)
        .filter(|(_, language)| language_supported(language, cx))
        .map(|(content_range, _)| {
            buffer.offset_to_point(content_range.start)..buffer.offset_to_point(content_range.end)
        })
        .collect()
}

fn language_supported(language: &Arc<Language>, cx: &mut App) -> bool {
    let store = ReplStore::global(cx);
    let store_read = store.read(cx);

    // Since we're just checking for general language support, we only need to look at
    // the pure Jupyter kernels - these are all the globally available ones
    store_read.pure_jupyter_kernel_specifications().any(|spec| {
        // Convert to lowercase for case-insensitive comparison since kernels might report "python" while our language is "Python"
        spec.language().as_ref().to_lowercase() == language.name().as_ref().to_lowercase()
    })
}

fn get_language(editor: WeakEntity<Editor>, cx: &mut App) -> Option<Arc<Language>> {
    editor
        .update(cx, |editor, cx| {
            let selection = editor.selections.newest::<usize>(cx);
            let buffer = editor.buffer().read(cx).snapshot(cx);
            buffer.language_at(selection.head()).cloned()
        })
        .ok()
        .flatten()
}

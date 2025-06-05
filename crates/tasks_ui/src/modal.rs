use std::sync::Arc;

use crate::TaskContexts;
use editor::Editor;
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, DismissEvent, Entity, EventEmitter,
    Focusable, InteractiveElement, ParentElement, Render, SharedString, Styled, Subscription, Task,
    WeakEntity, Window, rems,
};
use itertools::Itertools;
use picker::{Picker, PickerDelegate, highlighted_match_with_paths::HighlightedMatch};
use project::{TaskSourceKind, task_store::TaskStore};
use task::{ResolvedTask, RevealTarget, TaskContext, TaskTemplate};
use ui::{
    ActiveTheme, Button, ButtonCommon, ButtonSize, Clickable, Color, FluentBuilder as _, Icon,
    IconButton, IconButtonShape, IconName, IconSize, IconWithIndicator, Indicator, IntoElement,
    KeyBinding, Label, LabelSize, ListItem, ListItemSpacing, RenderOnce, Toggleable, Tooltip, div,
    h_flex, v_flex,
};

use util::{ResultExt, truncate_and_trailoff};
use workspace::{ModalView, Workspace};
pub use zed_actions::{Rerun, Spawn};

/// A modal used to spawn new tasks.
pub struct TasksModalDelegate {
    task_store: Entity<TaskStore>,
    candidates: Option<Vec<(TaskSourceKind, ResolvedTask)>>,
    task_overrides: Option<TaskOverrides>,
    last_used_candidate_index: Option<usize>,
    divider_index: Option<usize>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    workspace: WeakEntity<Workspace>,
    prompt: String,
    task_contexts: Arc<TaskContexts>,
    placeholder_text: Arc<str>,
}

/// Task template amendments to do before resolving the context.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskOverrides {
    /// See [`RevealTarget`].
    pub reveal_target: Option<RevealTarget>,
}

impl TasksModalDelegate {
    fn new(
        task_store: Entity<TaskStore>,
        task_contexts: Arc<TaskContexts>,
        task_overrides: Option<TaskOverrides>,
        workspace: WeakEntity<Workspace>,
    ) -> Self {
        let placeholder_text = if let Some(TaskOverrides {
            reveal_target: Some(RevealTarget::Center),
        }) = &task_overrides
        {
            Arc::from("Find a task, or run a command in the central pane")
        } else {
            Arc::from("Find a task, or run a command")
        };
        Self {
            task_store,
            workspace,
            candidates: None,
            matches: Vec::new(),
            last_used_candidate_index: None,
            divider_index: None,
            selected_index: 0,
            prompt: String::default(),
            task_contexts,
            task_overrides,
            placeholder_text,
        }
    }

    fn spawn_oneshot(&mut self) -> Option<(TaskSourceKind, ResolvedTask)> {
        if self.prompt.trim().is_empty() {
            return None;
        }

        let default_context = TaskContext::default();
        let active_context = self
            .task_contexts
            .active_context()
            .unwrap_or(&default_context);
        let source_kind = TaskSourceKind::UserInput;
        let id_base = source_kind.to_id_base();
        let mut new_oneshot = TaskTemplate {
            label: self.prompt.clone(),
            command: self.prompt.clone(),
            ..TaskTemplate::default()
        };
        if let Some(TaskOverrides {
            reveal_target: Some(reveal_target),
        }) = &self.task_overrides
        {
            new_oneshot.reveal_target = *reveal_target;
        }
        Some((
            source_kind,
            new_oneshot.resolve_task(&id_base, active_context)?,
        ))
    }

    fn delete_previously_used(&mut self, ix: usize, cx: &mut App) {
        let Some(candidates) = self.candidates.as_mut() else {
            return;
        };
        let Some(task) = candidates.get(ix).map(|(_, task)| task.clone()) else {
            return;
        };
        // We remove this candidate manually instead of .taking() the candidates, as we already know the index;
        // it doesn't make sense to requery the inventory for new candidates, as that's potentially costly and more often than not it should just return back
        // the original list without a removed entry.
        candidates.remove(ix);
        if let Some(inventory) = self.task_store.read(cx).task_inventory().cloned() {
            inventory.update(cx, |inventory, _| {
                inventory.delete_previously_used(&task.id);
            })
        };
    }
}

pub struct TasksModal {
    pub picker: Entity<Picker<TasksModalDelegate>>,
    _subscription: [Subscription; 1],
}

impl TasksModal {
    pub fn new(
        task_store: Entity<TaskStore>,
        task_contexts: Arc<TaskContexts>,
        task_overrides: Option<TaskOverrides>,
        is_modal: bool,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker = cx.new(|cx| {
            Picker::uniform_list(
                TasksModalDelegate::new(task_store, task_contexts, task_overrides, workspace),
                window,
                cx,
            )
            .modal(is_modal)
        });
        let _subscription = [cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| {
            cx.emit(DismissEvent);
        })];
        Self {
            picker,
            _subscription,
        }
    }

    pub fn task_contexts_loaded(
        &mut self,
        task_contexts: Arc<TaskContexts>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.picker.update(cx, |picker, cx| {
            picker.delegate.task_contexts = task_contexts;
            picker.delegate.candidates = None;
            picker.refresh(window, cx);
            cx.notify();
        })
    }
}

impl Render for TasksModal {
    fn render(
        &mut self,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> impl gpui::prelude::IntoElement {
        v_flex()
            .key_context("TasksModal")
            .w(rems(34.))
            .child(self.picker.clone())
    }
}

impl EventEmitter<DismissEvent> for TasksModal {}

impl Focusable for TasksModal {
    fn focus_handle(&self, cx: &gpui::App) -> gpui::FocusHandle {
        self.picker.read(cx).focus_handle(cx)
    }
}

impl ModalView for TasksModal {}

const MAX_TAGS_LINE_LEN: usize = 30;

impl PickerDelegate for TasksModalDelegate {
    type ListItem = ListItem;

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<picker::Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, _window: &mut Window, _: &mut App) -> Arc<str> {
        self.placeholder_text.clone()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<picker::Picker<Self>>,
    ) -> Task<()> {
        let candidates = match &self.candidates {
            Some(candidates) => Task::ready(string_match_candidates(candidates)),
            None => {
                if let Some(task_inventory) = self.task_store.read(cx).task_inventory().cloned() {
                    let (used, current) = task_inventory
                        .read(cx)
                        .used_and_current_resolved_tasks(&self.task_contexts, cx);
                    let workspace = self.workspace.clone();
                    let lsp_task_sources = self.task_contexts.lsp_task_sources.clone();
                    let task_position = self.task_contexts.latest_selection;
                    cx.spawn(async move |picker, cx| {
                        let Ok((lsp_tasks, prefer_lsp)) = workspace.update(cx, |workspace, cx| {
                            let lsp_tasks = editor::lsp_tasks(
                                workspace.project().clone(),
                                &lsp_task_sources,
                                task_position,
                                cx,
                            );
                            let prefer_lsp = workspace
                                .active_item(cx)
                                .and_then(|item| item.downcast::<Editor>())
                                .map(|editor| {
                                    editor
                                        .read(cx)
                                        .buffer()
                                        .read(cx)
                                        .language_settings(cx)
                                        .tasks
                                        .prefer_lsp
                                })
                                .unwrap_or(false);
                            (lsp_tasks, prefer_lsp)
                        }) else {
                            return Vec::new();
                        };

                        let lsp_tasks = lsp_tasks.await;
                        picker
                            .update(cx, |picker, _| {
                                picker.delegate.last_used_candidate_index = if used.is_empty() {
                                    None
                                } else {
                                    Some(used.len() - 1)
                                };

                                let mut new_candidates = used;
                                let add_current_language_tasks =
                                    !prefer_lsp || lsp_tasks.is_empty();
                                new_candidates.extend(lsp_tasks.into_iter().flat_map(
                                    |(kind, tasks_with_locations)| {
                                        tasks_with_locations
                                            .into_iter()
                                            .sorted_by_key(|(location, task)| {
                                                (location.is_none(), task.resolved_label.clone())
                                            })
                                            .map(move |(_, task)| (kind.clone(), task))
                                    },
                                ));
                                new_candidates.extend(current.into_iter().filter(
                                    |(task_kind, _)| {
                                        add_current_language_tasks
                                            || !matches!(task_kind, TaskSourceKind::Language { .. })
                                    },
                                ));
                                let match_candidates = string_match_candidates(&new_candidates);
                                let _ = picker.delegate.candidates.insert(new_candidates);
                                match_candidates
                            })
                            .ok()
                            .unwrap_or_default()
                    })
                } else {
                    Task::ready(Vec::new())
                }
            }
        };

        cx.spawn_in(window, async move |picker, cx| {
            let candidates = candidates.await;
            let matches = fuzzy::match_strings(
                &candidates,
                &query,
                true,
                1000,
                &Default::default(),
                cx.background_executor().clone(),
            )
            .await;
            picker
                .update(cx, |picker, _| {
                    let delegate = &mut picker.delegate;
                    delegate.matches = matches;
                    if let Some(index) = delegate.last_used_candidate_index {
                        delegate.matches.sort_by_key(|m| m.candidate_id > index);
                    }

                    delegate.prompt = query;
                    delegate.divider_index = delegate.last_used_candidate_index.and_then(|index| {
                        let index = delegate
                            .matches
                            .partition_point(|matching_task| matching_task.candidate_id <= index);
                        Some(index).and_then(|index| (index != 0).then(|| index - 1))
                    });

                    if delegate.matches.is_empty() {
                        delegate.selected_index = 0;
                    } else {
                        delegate.selected_index =
                            delegate.selected_index.min(delegate.matches.len() - 1);
                    }
                })
                .log_err();
        })
    }

    fn confirm(
        &mut self,
        omit_history_entry: bool,
        window: &mut Window,
        cx: &mut Context<picker::Picker<Self>>,
    ) {
        let current_match_index = self.selected_index();
        let task = self
            .matches
            .get(current_match_index)
            .and_then(|current_match| {
                let ix = current_match.candidate_id;
                self.candidates
                    .as_ref()
                    .map(|candidates| candidates[ix].clone())
            });
        let Some((task_source_kind, mut task)) = task else {
            return;
        };
        if let Some(TaskOverrides {
            reveal_target: Some(reveal_target),
        }) = &self.task_overrides
        {
            task.resolved.reveal_target = *reveal_target;
        }

        self.workspace
            .update(cx, |workspace, cx| {
                workspace.schedule_resolved_task(
                    task_source_kind,
                    task,
                    omit_history_entry,
                    window,
                    cx,
                );
            })
            .ok();

        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<picker::Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<picker::Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let candidates = self.candidates.as_ref()?;
        let hit = &self.matches[ix];
        let (source_kind, resolved_task) = &candidates.get(hit.candidate_id)?;
        let template = resolved_task.original_task();
        let display_label = resolved_task.display_label();

        let mut tooltip_label_text = if display_label != &template.label {
            resolved_task.resolved_label.clone()
        } else {
            String::new()
        };

        if resolved_task.resolved.command_label != resolved_task.resolved_label {
            if !tooltip_label_text.trim().is_empty() {
                tooltip_label_text.push('\n');
            }
            tooltip_label_text.push_str(&resolved_task.resolved.command_label);
        }

        if template.tags.len() > 0 {
            tooltip_label_text.push('\n');
            tooltip_label_text.push_str(
                template
                    .tags
                    .iter()
                    .map(|tag| format!("\n#{}", tag))
                    .collect::<Vec<_>>()
                    .join("")
                    .as_str(),
            );
        }
        let tooltip_label = if tooltip_label_text.trim().is_empty() {
            None
        } else {
            Some(Tooltip::simple(tooltip_label_text, cx))
        };

        let highlighted_location = HighlightedMatch {
            text: hit.string.clone(),
            highlight_positions: hit.positions.clone(),
            char_count: hit.string.chars().count(),
            color: Color::Default,
        };
        let icon = match source_kind {
            TaskSourceKind::UserInput => Some(Icon::new(IconName::Terminal)),
            TaskSourceKind::AbsPath { .. } => Some(Icon::new(IconName::Settings)),
            TaskSourceKind::Worktree { .. } => Some(Icon::new(IconName::FileTree)),
            TaskSourceKind::Lsp {
                language_name: name,
                ..
            }
            | TaskSourceKind::Language { name } => file_icons::FileIcons::get(cx)
                .get_icon_for_type(&name.to_lowercase(), cx)
                .map(Icon::from_path),
        }
        .map(|icon| icon.color(Color::Muted).size(IconSize::Small));
        let indicator = if matches!(source_kind, TaskSourceKind::Lsp { .. }) {
            Some(Indicator::icon(
                Icon::new(IconName::Bolt).size(IconSize::Small),
            ))
        } else {
            None
        };
        let icon = icon.map(|icon| {
            IconWithIndicator::new(icon, indicator)
                .indicator_border_color(Some(cx.theme().colors().border_transparent))
        });
        let history_run_icon = if Some(ix) <= self.divider_index {
            Some(
                Icon::new(IconName::HistoryRerun)
                    .color(Color::Muted)
                    .size(IconSize::Small)
                    .into_any_element(),
            )
        } else {
            Some(
                v_flex()
                    .flex_none()
                    .size(IconSize::Small.rems())
                    .into_any_element(),
            )
        };

        Some(
            ListItem::new(SharedString::from(format!("tasks-modal-{ix}")))
                .inset(true)
                .start_slot::<IconWithIndicator>(icon)
                .end_slot::<AnyElement>(
                    h_flex()
                        .gap_1()
                        .child(Label::new(truncate_and_trailoff(
                            &template
                                .tags
                                .iter()
                                .map(|tag| format!("#{}", tag))
                                .collect::<Vec<_>>()
                                .join(" "),
                            MAX_TAGS_LINE_LEN,
                        )))
                        .flex_none()
                        .child(history_run_icon.unwrap())
                        .into_any_element(),
                )
                .spacing(ListItemSpacing::Sparse)
                .when_some(tooltip_label, |list_item, item_label| {
                    list_item.tooltip(move |_, _| item_label.clone())
                })
                .map(|item| {
                    let item = if matches!(source_kind, TaskSourceKind::UserInput)
                        || Some(ix) <= self.divider_index
                    {
                        let task_index = hit.candidate_id;
                        let delete_button = div().child(
                            IconButton::new("delete", IconName::Close)
                                .shape(IconButtonShape::Square)
                                .icon_color(Color::Muted)
                                .size(ButtonSize::None)
                                .icon_size(IconSize::XSmall)
                                .on_click(cx.listener(move |picker, _event, window, cx| {
                                    cx.stop_propagation();
                                    window.prevent_default();

                                    picker.delegate.delete_previously_used(task_index, cx);
                                    picker.delegate.last_used_candidate_index = picker
                                        .delegate
                                        .last_used_candidate_index
                                        .unwrap_or(0)
                                        .checked_sub(1);
                                    picker.refresh(window, cx);
                                }))
                                .tooltip(|_, cx| {
                                    Tooltip::simple("Delete Previously Scheduled Task", cx)
                                }),
                        );
                        item.end_hover_slot(delete_button)
                    } else {
                        item
                    };
                    item
                })
                .toggle_state(selected)
                .child(highlighted_location.render(window, cx)),
        )
    }

    fn confirm_completion(
        &mut self,
        _: String,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<String> {
        let task_index = self.matches.get(self.selected_index())?.candidate_id;
        let tasks = self.candidates.as_ref()?;
        let (_, task) = tasks.get(task_index)?;
        Some(task.resolved.command_label.clone())
    }

    fn confirm_input(
        &mut self,
        omit_history_entry: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let Some((task_source_kind, mut task)) = self.spawn_oneshot() else {
            return;
        };

        if let Some(TaskOverrides {
            reveal_target: Some(reveal_target),
        }) = self.task_overrides
        {
            task.resolved.reveal_target = reveal_target;
        }
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.schedule_resolved_task(
                    task_source_kind,
                    task,
                    omit_history_entry,
                    window,
                    cx,
                )
            })
            .ok();
        cx.emit(DismissEvent);
    }

    fn separators_after_indices(&self) -> Vec<usize> {
        if let Some(i) = self.divider_index {
            vec![i]
        } else {
            Vec::new()
        }
    }

    fn render_footer(
        &self,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<gpui::AnyElement> {
        let is_recent_selected = self.divider_index >= Some(self.selected_index);
        let current_modifiers = window.modifiers();
        let left_button = if self
            .task_store
            .read(cx)
            .task_inventory()?
            .read(cx)
            .last_scheduled_task(None)
            .is_some()
        {
            Some(("Rerun Last Task", Rerun::default().boxed_clone()))
        } else {
            None
        };
        Some(
            h_flex()
                .w_full()
                .h_8()
                .p_2()
                .justify_between()
                .rounded_b_sm()
                .bg(cx.theme().colors().ghost_element_selected)
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    left_button
                        .map(|(label, action)| {
                            let keybind = KeyBinding::for_action(&*action, window, cx);

                            Button::new("edit-current-task", label)
                                .label_size(LabelSize::Small)
                                .when_some(keybind, |this, keybind| this.key_binding(keybind))
                                .on_click(move |_, window, cx| {
                                    window.dispatch_action(action.boxed_clone(), cx);
                                })
                                .into_any_element()
                        })
                        .unwrap_or_else(|| h_flex().into_any_element()),
                )
                .map(|this| {
                    if (current_modifiers.alt || self.matches.is_empty()) && !self.prompt.is_empty()
                    {
                        let action = picker::ConfirmInput {
                            secondary: current_modifiers.secondary(),
                        }
                        .boxed_clone();
                        this.children(KeyBinding::for_action(&*action, window, cx).map(|keybind| {
                            let spawn_oneshot_label = if current_modifiers.secondary() {
                                "Spawn Oneshot Without History"
                            } else {
                                "Spawn Oneshot"
                            };

                            Button::new("spawn-onehshot", spawn_oneshot_label)
                                .label_size(LabelSize::Small)
                                .key_binding(keybind)
                                .on_click(move |_, window, cx| {
                                    window.dispatch_action(action.boxed_clone(), cx)
                                })
                        }))
                    } else if current_modifiers.secondary() {
                        this.children(
                            KeyBinding::for_action(&menu::SecondaryConfirm, window, cx).map(
                                |keybind| {
                                    let label = if is_recent_selected {
                                        "Rerun Without History"
                                    } else {
                                        "Spawn Without History"
                                    };
                                    Button::new("spawn", label)
                                        .label_size(LabelSize::Small)
                                        .key_binding(keybind)
                                        .on_click(move |_, window, cx| {
                                            window.dispatch_action(
                                                menu::SecondaryConfirm.boxed_clone(),
                                                cx,
                                            )
                                        })
                                },
                            ),
                        )
                    } else {
                        this.children(KeyBinding::for_action(&menu::Confirm, window, cx).map(
                            |keybind| {
                                let run_entry_label =
                                    if is_recent_selected { "Rerun" } else { "Spawn" };

                                Button::new("spawn", run_entry_label)
                                    .label_size(LabelSize::Small)
                                    .key_binding(keybind)
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(menu::Confirm.boxed_clone(), cx);
                                    })
                            },
                        ))
                    }
                })
                .into_any_element(),
        )
    }
}

fn string_match_candidates<'a>(
    candidates: impl IntoIterator<Item = &'a (TaskSourceKind, ResolvedTask)> + 'a,
) -> Vec<StringMatchCandidate> {
    candidates
        .into_iter()
        .enumerate()
        .map(|(index, (_, candidate))| StringMatchCandidate::new(index, candidate.display_label()))
        .collect()
}

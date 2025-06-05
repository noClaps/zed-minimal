mod persistence;

use std::{
    cmp::{self, Reverse},
    collections::HashMap,
    sync::Arc,
    time::Duration,
};

use client::parse_zed_link;
use command_palette_hooks::{
    CommandInterceptResult, CommandPaletteFilter, CommandPaletteInterceptor,
};

use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    ParentElement, Render, Styled, Task, WeakEntity, Window,
};
use persistence::COMMAND_PALETTE_HISTORY;
use picker::{Picker, PickerDelegate};
use postage::{sink::Sink, stream::Stream};
use settings::Settings;
use ui::{HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, h_flex, prelude::*, v_flex};
use util::ResultExt;
use workspace::{ModalView, Workspace, WorkspaceSettings};
use zed_actions::{OpenZedUrl, command_palette::Toggle};

pub fn init(cx: &mut App) {
    client::init_settings(cx);
    command_palette_hooks::init(cx);
    cx.observe_new(CommandPalette::register).detach();
}

impl ModalView for CommandPalette {}

pub struct CommandPalette {
    picker: Entity<Picker<CommandPaletteDelegate>>,
}

/// Removes subsequent whitespace characters and double colons from the query.
///
/// This improves the likelihood of a match by either humanized name or keymap-style name.
fn normalize_query(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut last_char = None;

    for char in input.trim().chars() {
        match (last_char, char) {
            (Some(':'), ':') => continue,
            (Some(last_char), char) if last_char.is_whitespace() && char.is_whitespace() => {
                continue;
            }
            _ => {
                last_char = Some(char);
            }
        }
        result.push(char);
    }

    result
}

impl CommandPalette {
    fn register(
        workspace: &mut Workspace,
        _window: Option<&mut Window>,
        _: &mut Context<Workspace>,
    ) {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            Self::toggle(workspace, "", window, cx)
        });
    }

    pub fn toggle(
        workspace: &mut Workspace,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(previous_focus_handle) = window.focused(cx) else {
            return;
        };
        workspace.toggle_modal(window, cx, move |window, cx| {
            CommandPalette::new(previous_focus_handle, query, window, cx)
        });
    }

    fn new(
        previous_focus_handle: FocusHandle,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = CommandPaletteFilter::try_global(cx);

        let commands = window
            .available_actions(cx)
            .into_iter()
            .filter_map(|action| {
                if filter.is_some_and(|filter| filter.is_hidden(&*action)) {
                    return None;
                }

                Some(Command {
                    name: humanize_action_name(action.name()),
                    action,
                })
            })
            .collect();

        let delegate =
            CommandPaletteDelegate::new(cx.entity().downgrade(), commands, previous_focus_handle);

        let picker = cx.new(|cx| {
            let picker = Picker::uniform_list(delegate, window, cx);
            picker.set_query(query, window, cx);
            picker
        });
        Self { picker }
    }

    pub fn set_query(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.picker
            .update(cx, |picker, cx| picker.set_query(query, window, cx))
    }
}

impl EventEmitter<DismissEvent> for CommandPalette {}

impl Focusable for CommandPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

pub struct CommandPaletteDelegate {
    latest_query: String,
    command_palette: WeakEntity<CommandPalette>,
    all_commands: Vec<Command>,
    commands: Vec<Command>,
    matches: Vec<StringMatch>,
    selected_ix: usize,
    previous_focus_handle: FocusHandle,
    updating_matches: Option<(
        Task<()>,
        postage::dispatch::Receiver<(Vec<Command>, Vec<StringMatch>)>,
    )>,
}

struct Command {
    name: String,
    action: Box<dyn Action>,
}

impl Clone for Command {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            action: self.action.boxed_clone(),
        }
    }
}

impl CommandPaletteDelegate {
    fn new(
        command_palette: WeakEntity<CommandPalette>,
        commands: Vec<Command>,
        previous_focus_handle: FocusHandle,
    ) -> Self {
        Self {
            command_palette,
            all_commands: commands.clone(),
            matches: vec![],
            commands,
            selected_ix: 0,
            previous_focus_handle,
            latest_query: String::new(),
            updating_matches: None,
        }
    }

    fn matches_updated(
        &mut self,
        query: String,
        mut commands: Vec<Command>,
        mut matches: Vec<StringMatch>,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.updating_matches.take();
        self.latest_query = query.clone();

        let mut intercept_results = CommandPaletteInterceptor::try_global(cx)
            .map(|interceptor| interceptor.intercept(&query, cx))
            .unwrap_or_default();

        if parse_zed_link(&query, cx).is_some() {
            intercept_results = vec![CommandInterceptResult {
                action: OpenZedUrl { url: query.clone() }.boxed_clone(),
                string: query.clone(),
                positions: vec![],
            }]
        }

        let mut new_matches = Vec::new();

        for CommandInterceptResult {
            action,
            string,
            positions,
        } in intercept_results
        {
            if let Some(idx) = matches
                .iter()
                .position(|m| commands[m.candidate_id].action.partial_eq(&*action))
            {
                matches.remove(idx);
            }
            commands.push(Command {
                name: string.clone(),
                action,
            });
            new_matches.push(StringMatch {
                candidate_id: commands.len() - 1,
                string,
                positions,
                score: 0.0,
            })
        }
        new_matches.append(&mut matches);
        self.commands = commands;
        self.matches = new_matches;
        if self.matches.is_empty() {
            self.selected_ix = 0;
        } else {
            self.selected_ix = cmp::min(self.selected_ix, self.matches.len() - 1);
        }
    }
    ///
    /// Hit count for each command in the palette.
    /// We only account for commands triggered directly via command palette and not by e.g. keystrokes because
    /// if a user already knows a keystroke for a command, they are unlikely to use a command palette to look for it.
    fn hit_counts(&self) -> HashMap<String, u16> {
        if let Ok(commands) = COMMAND_PALETTE_HISTORY.list_commands_used() {
            commands
                .into_iter()
                .map(|command| (command.command_name, command.invocations))
                .collect()
        } else {
            HashMap::new()
        }
    }
}

impl PickerDelegate for CommandPaletteDelegate {
    type ListItem = ListItem;

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Execute a command...".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_ix
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        self.selected_ix = ix;
    }

    fn update_matches(
        &mut self,
        mut query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> gpui::Task<()> {
        let settings = WorkspaceSettings::get_global(cx);
        if let Some(alias) = settings.command_aliases.get(&query) {
            query = alias.to_string();
        }
        let (mut tx, mut rx) = postage::dispatch::channel(1);
        let task = cx.background_spawn({
            let mut commands = self.all_commands.clone();
            let hit_counts = self.hit_counts();
            let executor = cx.background_executor().clone();
            let query = normalize_query(query.as_str());
            async move {
                commands.sort_by_key(|action| {
                    (
                        Reverse(hit_counts.get(&action.name).cloned()),
                        action.name.clone(),
                    )
                });

                let candidates = commands
                    .iter()
                    .enumerate()
                    .map(|(ix, command)| StringMatchCandidate::new(ix, &command.name))
                    .collect::<Vec<_>>();
                let matches = if query.is_empty() {
                    candidates
                        .into_iter()
                        .enumerate()
                        .map(|(index, candidate)| StringMatch {
                            candidate_id: index,
                            string: candidate.string,
                            positions: Vec::new(),
                            score: 0.0,
                        })
                        .collect()
                } else {
                    fuzzy::match_strings(
                        &candidates,
                        &query,
                        true,
                        10000,
                        &Default::default(),
                        executor,
                    )
                    .await
                };

                tx.send((commands, matches)).await.log_err();
            }
        });
        self.updating_matches = Some((task, rx.clone()));

        cx.spawn_in(window, async move |picker, cx| {
            let Some((commands, matches)) = rx.recv().await else {
                return;
            };

            picker
                .update(cx, |picker, cx| {
                    picker
                        .delegate
                        .matches_updated(query, commands, matches, cx)
                })
                .log_err();
        })
    }

    fn finalize_update_matches(
        &mut self,
        query: String,
        duration: Duration,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> bool {
        let Some((task, rx)) = self.updating_matches.take() else {
            return true;
        };

        match cx
            .background_executor()
            .block_with_timeout(duration, rx.clone().recv())
        {
            Ok(Some((commands, matches))) => {
                self.matches_updated(query, commands, matches, cx);
                true
            }
            _ => {
                self.updating_matches = Some((task, rx));
                false
            }
        }
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.command_palette
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if self.matches.is_empty() {
            self.dismissed(window, cx);
            return;
        }
        let action_ix = self.matches[self.selected_ix].candidate_id;
        let command = self.commands.swap_remove(action_ix);
        telemetry::event!(
            "Action Invoked",
            source = "command palette",
            action = command.name
        );
        self.matches.clear();
        self.commands.clear();
        let command_name = command.name.clone();
        let latest_query = self.latest_query.clone();
        cx.background_spawn(async move {
            COMMAND_PALETTE_HISTORY
                .write_command_invocation(command_name, latest_query)
                .await
        })
        .detach_and_log_err(cx);
        let action = command.action;
        window.focus(&self.previous_focus_handle);
        self.dismissed(window, cx);
        window.dispatch_action(action, cx);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let r#match = self.matches.get(ix)?;
        let command = self.commands.get(r#match.candidate_id)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .w_full()
                        .py_px()
                        .justify_between()
                        .child(HighlightedLabel::new(
                            command.name.clone(),
                            r#match.positions.clone(),
                        ))
                        .children(KeyBinding::for_action_in(
                            &*command.action,
                            &self.previous_focus_handle,
                            window,
                            cx,
                        )),
                ),
        )
    }
}

pub fn humanize_action_name(name: &str) -> String {
    let capacity = name.len() + name.chars().filter(|c| c.is_uppercase()).count();
    let mut result = String::with_capacity(capacity);
    for char in name.chars() {
        if char == ':' {
            if result.ends_with(':') {
                result.push(' ');
            } else {
                result.push(':');
            }
        } else if char == '_' {
            result.push(' ');
        } else if char.is_uppercase() {
            if !result.ends_with(' ') {
                result.push(' ');
            }
            result.extend(char.to_lowercase());
        } else {
            result.push(char);
        }
    }
    result
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

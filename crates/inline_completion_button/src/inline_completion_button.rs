use anyhow::Result;
use editor::{
    Editor,
    actions::{ShowEditPrediction, ToggleEditPrediction},
    scroll::Autoscroll,
};
use fs::Fs;
use gpui::{
    App, AsyncWindowContext, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    Subscription, WeakEntity, actions, div,
};
use indoc::indoc;
use language::{
    File, Language,
    language_settings::{self, AllLanguageSettings, all_language_settings},
};
use regex::Regex;
use settings::{Settings, SettingsStore, update_settings_file};
use std::sync::{Arc, LazyLock};
use ui::{ContextMenu, ContextMenuEntry, DocumentationSide, PopoverMenuHandle, prelude::*};
use workspace::{StatusItemView, Workspace, create_and_open_local_file, item::ItemHandle};

actions!(edit_prediction, [ToggleMenu]);

pub struct InlineCompletionButton {
    editor_subscription: Option<(Subscription, usize)>,
    editor_enabled: Option<bool>,
    editor_show_predictions: bool,
    editor_focus_handle: Option<FocusHandle>,
    language: Option<Arc<Language>>,
    file: Option<Arc<dyn File>>,
    edit_prediction_provider: Option<Arc<dyn inline_completion::InlineCompletionProviderHandle>>,
    fs: Arc<dyn Fs>,
    popover_menu_handle: PopoverMenuHandle<ContextMenu>,
}

impl Render for InlineCompletionButton {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl InlineCompletionButton {
    pub fn new(
        fs: Arc<dyn Fs>,
        popover_menu_handle: PopoverMenuHandle<ContextMenu>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe_global::<SettingsStore>(move |_, cx| cx.notify())
            .detach();

        Self {
            editor_subscription: None,
            editor_enabled: None,
            editor_show_predictions: true,
            editor_focus_handle: None,
            language: None,
            file: None,
            edit_prediction_provider: None,
            popover_menu_handle,
            fs,
        }
    }

    pub fn build_language_settings_menu(
        &self,
        mut menu: ContextMenu,
        window: &Window,
        cx: &mut App,
    ) -> ContextMenu {
        let fs = self.fs.clone();
        let line_height = window.line_height();

        menu = menu.header("Show Edit Predictions For");

        let language_state = self.language.as_ref().map(|language| {
            (
                language.clone(),
                language_settings::language_settings(Some(language.name()), None, cx)
                    .show_edit_predictions,
            )
        });

        if let Some(editor_focus_handle) = self.editor_focus_handle.clone() {
            let entry = ContextMenuEntry::new("This Buffer")
                .toggleable(IconPosition::Start, self.editor_show_predictions)
                .action(Box::new(ToggleEditPrediction))
                .handler(move |window, cx| {
                    editor_focus_handle.dispatch_action(&ToggleEditPrediction, window, cx);
                });

            match language_state.clone() {
                Some((language, false)) => {
                    menu = menu.item(
                        entry
                            .disabled(true)
                            .documentation_aside(DocumentationSide::Left, move |_cx| {
                                Label::new(format!("Edit predictions cannot be toggled for this buffer because they are disabled for {}", language.name()))
                                    .into_any_element()
                            })
                    );
                }
                Some(_) | None => menu = menu.item(entry),
            }
        }

        if let Some((language, language_enabled)) = language_state {
            let fs = fs.clone();

            menu = menu.toggleable_entry(
                language.name(),
                language_enabled,
                IconPosition::Start,
                None,
                move |_, cx| {
                    toggle_show_inline_completions_for_language(language.clone(), fs.clone(), cx)
                },
            );
        }

        let settings = AllLanguageSettings::get_global(cx);

        let globally_enabled = settings.show_edit_predictions(None, cx);
        menu = menu.toggleable_entry("All Files", globally_enabled, IconPosition::Start, None, {
            let fs = fs.clone();
            move |_, cx| toggle_inline_completions_globally(fs.clone(), cx)
        });

        menu = menu.separator().header("Privacy Settings");
        if let Some(provider) = &self.edit_prediction_provider {
            let data_collection = provider.data_collection_state(cx);
            if data_collection.is_supported() {
                let provider = provider.clone();
                let enabled = data_collection.is_enabled();
                let is_open_source = data_collection.is_project_open_source();
                let is_collecting = data_collection.is_enabled();
                let (icon_name, icon_color) = if is_open_source && is_collecting {
                    (IconName::Check, Color::Success)
                } else {
                    (IconName::Check, Color::Accent)
                };

                menu = menu.item(
                    ContextMenuEntry::new("Training Data Collection")
                        .toggleable(IconPosition::Start, data_collection.is_enabled())
                        .icon(icon_name)
                        .icon_color(icon_color)
                        .documentation_aside(DocumentationSide::Left, move |cx| {
                            let (msg, label_color, icon_name, icon_color) = match (is_open_source, is_collecting) {
                                (true, true) => (
                                    "Project identified as open source, and you're sharing data.",
                                    Color::Default,
                                    IconName::Check,
                                    Color::Success,
                                ),
                                (true, false) => (
                                    "Project identified as open source, but you're not sharing data.",
                                    Color::Muted,
                                    IconName::Close,
                                    Color::Muted,
                                ),
                                (false, true) => (
                                    "Project not identified as open source. No data captured.",
                                    Color::Muted,
                                    IconName::Close,
                                    Color::Muted,
                                ),
                                (false, false) => (
                                    "Project not identified as open source, and setting turned off.",
                                    Color::Muted,
                                    IconName::Close,
                                    Color::Muted,
                                ),
                            };
                            v_flex()
                                .gap_2()
                                .child(
                                    Label::new(indoc!{
                                        "Help us improve our open dataset model by sharing data from open source repositories. \
                                        Zed must detect a license file in your repo for this setting to take effect."
                                    })
                                )
                                .child(
                                    h_flex()
                                        .items_start()
                                        .pt_2()
                                        .flex_1()
                                        .gap_1p5()
                                        .border_t_1()
                                        .border_color(cx.theme().colors().border_variant)
                                        .child(h_flex().flex_shrink_0().h(line_height).child(Icon::new(icon_name).size(IconSize::XSmall).color(icon_color)))
                                        .child(div().child(msg).w_full().text_sm().text_color(label_color.color(cx)))
                                )
                                .into_any_element()
                        })
                        .handler(move |_, cx| {
                            provider.toggle_data_collection(cx);

                            if !enabled {
                                telemetry::event!(
                                    "Data Collection Enabled",
                                    source = "Edit Prediction Status Menu"
                                );
                            } else {
                                telemetry::event!(
                                    "Data Collection Disabled",
                                    source = "Edit Prediction Status Menu"
                                );
                            }
                        })
                );

                if is_collecting && !is_open_source {
                    menu = menu.item(
                        ContextMenuEntry::new("No data captured.")
                            .disabled(true)
                            .icon(IconName::Close)
                            .icon_color(Color::Error)
                            .icon_size(IconSize::Small),
                    );
                }
            }
        }

        menu = menu.item(
            ContextMenuEntry::new("Configure Excluded Files")
                .icon(IconName::LockOutlined)
                .icon_color(Color::Muted)
                .documentation_aside(DocumentationSide::Left, |_| {
                    Label::new(indoc!{"
                        Open your settings to add sensitive paths for which Zed will never predict edits."}).into_any_element()
                })
                .handler(move |window, cx| {
                    if let Some(workspace) = window.root().flatten() {
                        let workspace = workspace.downgrade();
                        window
                            .spawn(cx, async |cx| {
                                open_disabled_globs_setting_in_editor(
                                    workspace,
                                    cx,
                                ).await
                            })
                            .detach_and_log_err(cx);
                    }
                }),
        );

        if !self.editor_enabled.unwrap_or(true) {
            menu = menu.item(
                ContextMenuEntry::new("This file is excluded.")
                    .disabled(true)
                    .icon_size(IconSize::Small),
            );
        }

        if let Some(editor_focus_handle) = self.editor_focus_handle.clone() {
            menu = menu
                .separator()
                .entry(
                    "Predict Edit at Cursor",
                    Some(Box::new(ShowEditPrediction)),
                    {
                        let editor_focus_handle = editor_focus_handle.clone();
                        move |window, cx| {
                            editor_focus_handle.dispatch_action(&ShowEditPrediction, window, cx);
                        }
                    },
                )
                .context(editor_focus_handle);
        }

        menu
    }

    pub fn update_enabled(&mut self, editor: Entity<Editor>, cx: &mut Context<Self>) {
        let editor = editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let suggestion_anchor = editor.selections.newest_anchor().start;
        let language = snapshot.language_at(suggestion_anchor);
        let file = snapshot.file_at(suggestion_anchor).cloned();
        self.editor_enabled = {
            let file = file.as_ref();
            Some(
                file.map(|file| {
                    all_language_settings(Some(file), cx)
                        .edit_predictions_enabled_for_file(file, cx)
                })
                .unwrap_or(true),
            )
        };
        self.editor_show_predictions = editor.edit_predictions_enabled();
        self.edit_prediction_provider = editor.edit_prediction_provider();
        self.language = language.cloned();
        self.file = file;
        self.editor_focus_handle = Some(editor.focus_handle(cx));

        cx.notify();
    }

    pub fn toggle_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.popover_menu_handle.toggle(window, cx);
    }
}

impl StatusItemView for InlineCompletionButton {
    fn set_active_pane_item(
        &mut self,
        item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = item.and_then(|item| item.act_as::<Editor>(cx)) {
            self.editor_subscription = Some((
                cx.observe(&editor, Self::update_enabled),
                editor.entity_id().as_u64() as usize,
            ));
            self.update_enabled(editor, cx);
        } else {
            self.language = None;
            self.editor_subscription = None;
            self.editor_enabled = None;
        }
        cx.notify();
    }
}

async fn open_disabled_globs_setting_in_editor(
    workspace: WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let settings_editor = workspace
        .update_in(cx, |_, window, cx| {
            create_and_open_local_file(paths::settings_file(), window, cx, || {
                settings::initial_user_settings_content().as_ref().into()
            })
        })?
        .await?
        .downcast::<Editor>()
        .unwrap();

    settings_editor
        .downgrade()
        .update_in(cx, |item, window, cx| {
            let text = item.buffer().read(cx).snapshot(cx).text();

            let settings = cx.global::<SettingsStore>();

            // Ensure that we always have "inline_completions { "disabled_globs": [] }"
            let edits = settings.edits_for_update::<AllLanguageSettings>(&text, |file| {
                file.edit_predictions
                    .get_or_insert_with(Default::default)
                    .disabled_globs
                    .get_or_insert_with(Vec::new);
            });

            if !edits.is_empty() {
                item.edit(edits, cx);
            }

            let text = item.buffer().read(cx).snapshot(cx).text();

            static DISABLED_GLOBS_REGEX: LazyLock<Regex> = LazyLock::new(|| {
                Regex::new(r#""disabled_globs":\s*\[\s*(?P<content>(?:.|\n)*?)\s*\]"#).unwrap()
            });
            // Only capture [...]
            let range = DISABLED_GLOBS_REGEX.captures(&text).and_then(|captures| {
                captures
                    .name("content")
                    .map(|inner_match| inner_match.start()..inner_match.end())
            });
            if let Some(range) = range {
                item.change_selections(Some(Autoscroll::newest()), window, cx, |selections| {
                    selections.select_ranges(vec![range]);
                });
            }
        })?;

    anyhow::Ok(())
}

fn toggle_inline_completions_globally(fs: Arc<dyn Fs>, cx: &mut App) {
    let show_edit_predictions = all_language_settings(None, cx).show_edit_predictions(None, cx);
    update_settings_file::<AllLanguageSettings>(fs, cx, move |file, _| {
        file.defaults.show_edit_predictions = Some(!show_edit_predictions)
    });
}

fn toggle_show_inline_completions_for_language(
    language: Arc<Language>,
    fs: Arc<dyn Fs>,
    cx: &mut App,
) {
    let show_edit_predictions =
        all_language_settings(None, cx).show_edit_predictions(Some(&language), cx);
    update_settings_file::<AllLanguageSettings>(fs, cx, move |file, _| {
        file.languages
            .entry(language.name())
            .or_default()
            .show_edit_predictions = Some(!show_edit_predictions);
    });
}

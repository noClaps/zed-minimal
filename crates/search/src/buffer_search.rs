mod registrar;

use crate::{
    FocusSearch, NextHistoryQuery, PreviousHistoryQuery, ReplaceAll, ReplaceNext, SearchOptions,
    SelectAllMatches, SelectNextMatch, SelectPreviousMatch, ToggleCaseSensitive, ToggleRegex,
    ToggleReplace, ToggleSelection, ToggleWholeWord, search_bar::render_nav_button,
};
use any_vec::AnyVec;
use anyhow::Context as _;
use collections::HashMap;
use editor::{
    DisplayPoint, Editor, EditorElement, EditorSettings, EditorStyle,
    actions::{Backtab, Tab},
};
use futures::channel::oneshot;
use gpui::{
    Action, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyContext, ParentElement as _, Render, ScrollHandle,
    Styled, Subscription, Task, TextStyle, Window, actions, div, impl_actions,
};
use language::{Language, LanguageRegistry};
use project::{
    search::SearchQuery,
    search_history::{SearchHistory, SearchHistoryCursor},
};
use schemars::JsonSchema;
use serde::Deserialize;
use settings::Settings;
use std::sync::Arc;
use theme::ThemeSettings;
use zed_actions::outline::ToggleOutline;

use ui::{
    BASE_REM_SIZE_IN_PX, IconButton, IconButtonShape, IconName, Tooltip, h_flex, prelude::*,
    utils::SearchInputWidth,
};
use util::ResultExt;
use workspace::{
    ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace,
    item::ItemHandle,
    searchable::{Direction, SearchEvent, SearchableItemHandle, WeakSearchableItemHandle},
};

pub use registrar::DivRegistrar;
use registrar::{ForDeployed, ForDismissed, SearchActionsRegistrar, WithResults};

const MAX_BUFFER_SEARCH_HISTORY_SIZE: usize = 50;

#[derive(PartialEq, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Deploy {
    #[serde(default = "util::serde::default_true")]
    pub focus: bool,
    #[serde(default)]
    pub replace_enabled: bool,
    #[serde(default)]
    pub selection_search_enabled: bool,
}

impl_actions!(buffer_search, [Deploy]);

actions!(buffer_search, [DeployReplace, Dismiss, FocusEditor]);

impl Deploy {
    pub fn find() -> Self {
        Self {
            focus: true,
            replace_enabled: false,
            selection_search_enabled: false,
        }
    }

    pub fn replace() -> Self {
        Self {
            focus: true,
            replace_enabled: true,
            selection_search_enabled: false,
        }
    }
}

pub enum Event {
    UpdateLocation,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| BufferSearchBar::register(workspace))
        .detach();
}

pub struct BufferSearchBar {
    query_editor: Entity<Editor>,
    query_editor_focused: bool,
    replacement_editor: Entity<Editor>,
    replacement_editor_focused: bool,
    active_searchable_item: Option<Box<dyn SearchableItemHandle>>,
    active_match_index: Option<usize>,
    active_searchable_item_subscription: Option<Subscription>,
    active_search: Option<Arc<SearchQuery>>,
    searchable_items_with_matches: HashMap<Box<dyn WeakSearchableItemHandle>, AnyVec<dyn Send>>,
    pending_search: Option<Task<()>>,
    search_options: SearchOptions,
    default_options: SearchOptions,
    configured_options: SearchOptions,
    query_contains_error: bool,
    dismissed: bool,
    search_history: SearchHistory,
    search_history_cursor: SearchHistoryCursor,
    replace_enabled: bool,
    selection_search_enabled: bool,
    scroll_handle: ScrollHandle,
    editor_scroll_handle: ScrollHandle,
    editor_needed_width: Pixels,
    regex_language: Option<Arc<Language>>,
}

impl BufferSearchBar {
    fn render_text_input(
        &self,
        editor: &Entity<Editor>,
        color_override: Option<Color>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (color, use_syntax) = if editor.read(cx).read_only(cx) {
            (cx.theme().colors().text_disabled, false)
        } else {
            match color_override {
                Some(color_override) => (color_override.color(cx), false),
                None => (cx.theme().colors().text, true),
            }
        };

        let settings = ThemeSettings::get_global(cx);
        let text_style = TextStyle {
            color,
            font_family: settings.buffer_font.family.clone(),
            font_features: settings.buffer_font.features.clone(),
            font_fallbacks: settings.buffer_font.fallbacks.clone(),
            font_size: rems(0.875).into(),
            font_weight: settings.buffer_font.weight,
            line_height: relative(1.3),
            ..TextStyle::default()
        };

        let mut editor_style = EditorStyle {
            background: cx.theme().colors().toolbar_background,
            local_player: cx.theme().players().local(),
            text: text_style,
            ..EditorStyle::default()
        };
        if use_syntax {
            editor_style.syntax = cx.theme().syntax().clone();
        }

        EditorElement::new(editor, editor_style)
    }

    pub fn query_editor_focused(&self) -> bool {
        self.query_editor_focused
    }
}

impl EventEmitter<Event> for BufferSearchBar {}
impl EventEmitter<workspace::ToolbarItemEvent> for BufferSearchBar {}
impl Render for BufferSearchBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.dismissed {
            return div().id("search_bar");
        }

        let focus_handle = self.focus_handle(cx);

        let narrow_mode =
            self.scroll_handle.bounds().size.width / window.rem_size() < 340. / BASE_REM_SIZE_IN_PX;
        let hide_inline_icons = self.editor_needed_width
            > self.editor_scroll_handle.bounds().size.width - window.rem_size() * 6.;

        let supported_options = self.supported_options(cx);

        if self.query_editor.update(cx, |query_editor, _cx| {
            query_editor.placeholder_text().is_none()
        }) {
            self.query_editor.update(cx, |editor, cx| {
                editor.set_placeholder_text("Search…", cx);
            });
        }

        self.replacement_editor.update(cx, |editor, cx| {
            editor.set_placeholder_text("Replace with…", cx);
        });

        let mut color_override = None;
        let match_text = self
            .active_searchable_item
            .as_ref()
            .and_then(|searchable_item| {
                if self.query(cx).is_empty() {
                    return None;
                }
                let matches_count = self
                    .searchable_items_with_matches
                    .get(&searchable_item.downgrade())
                    .map(AnyVec::len)
                    .unwrap_or(0);
                if let Some(match_ix) = self.active_match_index {
                    Some(format!("{}/{}", match_ix + 1, matches_count))
                } else {
                    color_override = Some(Color::Error); // No matches found
                    None
                }
            })
            .unwrap_or_else(|| "0/0".to_string());
        let should_show_replace_input = self.replace_enabled && supported_options.replacement;
        let in_replace = self.replacement_editor.focus_handle(cx).is_focused(window);

        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("BufferSearchBar");
        if in_replace {
            key_context.add("in_replace");
        }
        let editor_border = if self.query_contains_error {
            Color::Error.color(cx)
        } else {
            cx.theme().colors().border
        };

        let container_width = window.viewport_size().width;
        let input_width = SearchInputWidth::calc_width(container_width);

        let input_base_styles = || {
            h_flex()
                .min_w_32()
                .w(input_width)
                .h_8()
                .pl_2()
                .pr_1()
                .py_1()
                .border_1()
                .border_color(editor_border)
                .rounded_lg()
        };

        let search_line = h_flex()
            .gap_2()
            .when(supported_options.find_in_results, |el| {
                el.child(Label::new("Find in results").color(Color::Hint))
            })
            .child(
                input_base_styles()
                    .id("editor-scroll")
                    .track_scroll(&self.editor_scroll_handle)
                    .child(self.render_text_input(&self.query_editor, color_override, cx))
                    .when(!hide_inline_icons, |div| {
                        div.child(
                            h_flex()
                                .gap_1()
                                .children(supported_options.case.then(|| {
                                    self.render_search_option_button(
                                        SearchOptions::CASE_SENSITIVE,
                                        focus_handle.clone(),
                                        cx.listener(|this, _, window, cx| {
                                            this.toggle_case_sensitive(
                                                &ToggleCaseSensitive,
                                                window,
                                                cx,
                                            )
                                        }),
                                    )
                                }))
                                .children(supported_options.word.then(|| {
                                    self.render_search_option_button(
                                        SearchOptions::WHOLE_WORD,
                                        focus_handle.clone(),
                                        cx.listener(|this, _, window, cx| {
                                            this.toggle_whole_word(&ToggleWholeWord, window, cx)
                                        }),
                                    )
                                }))
                                .children(supported_options.regex.then(|| {
                                    self.render_search_option_button(
                                        SearchOptions::REGEX,
                                        focus_handle.clone(),
                                        cx.listener(|this, _, window, cx| {
                                            this.toggle_regex(&ToggleRegex, window, cx)
                                        }),
                                    )
                                })),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_1()
                    .min_w_64()
                    .when(supported_options.replacement, |this| {
                        this.child(
                            IconButton::new(
                                "buffer-search-bar-toggle-replace-button",
                                IconName::Replace,
                            )
                            .style(ButtonStyle::Subtle)
                            .shape(IconButtonShape::Square)
                            .when(self.replace_enabled, |button| {
                                button.style(ButtonStyle::Filled)
                            })
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.toggle_replace(&ToggleReplace, window, cx);
                            }))
                            .toggle_state(self.replace_enabled)
                            .tooltip({
                                let focus_handle = focus_handle.clone();
                                move |window, cx| {
                                    Tooltip::for_action_in(
                                        "Toggle Replace",
                                        &ToggleReplace,
                                        &focus_handle,
                                        window,
                                        cx,
                                    )
                                }
                            }),
                        )
                    })
                    .when(supported_options.selection, |this| {
                        this.child(
                            IconButton::new(
                                "buffer-search-bar-toggle-search-selection-button",
                                IconName::SearchSelection,
                            )
                            .style(ButtonStyle::Subtle)
                            .shape(IconButtonShape::Square)
                            .when(self.selection_search_enabled, |button| {
                                button.style(ButtonStyle::Filled)
                            })
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.toggle_selection(&ToggleSelection, window, cx);
                            }))
                            .toggle_state(self.selection_search_enabled)
                            .tooltip({
                                let focus_handle = focus_handle.clone();
                                move |window, cx| {
                                    Tooltip::for_action_in(
                                        "Toggle Search Selection",
                                        &ToggleSelection,
                                        &focus_handle,
                                        window,
                                        cx,
                                    )
                                }
                            }),
                        )
                    })
                    .when(!supported_options.find_in_results, |el| {
                        el.child(
                            IconButton::new("select-all", ui::IconName::SelectAll)
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(SelectAllMatches.boxed_clone(), cx)
                                })
                                .shape(IconButtonShape::Square)
                                .tooltip({
                                    let focus_handle = focus_handle.clone();
                                    move |window, cx| {
                                        Tooltip::for_action_in(
                                            "Select All Matches",
                                            &SelectAllMatches,
                                            &focus_handle,
                                            window,
                                            cx,
                                        )
                                    }
                                }),
                        )
                        .child(
                            h_flex()
                                .pl_2()
                                .ml_1()
                                .border_l_1()
                                .border_color(cx.theme().colors().border_variant)
                                .child(render_nav_button(
                                    ui::IconName::ChevronLeft,
                                    self.active_match_index.is_some(),
                                    "Select Previous Match",
                                    &SelectPreviousMatch,
                                    focus_handle.clone(),
                                ))
                                .child(render_nav_button(
                                    ui::IconName::ChevronRight,
                                    self.active_match_index.is_some(),
                                    "Select Next Match",
                                    &SelectNextMatch,
                                    focus_handle.clone(),
                                )),
                        )
                        .when(!narrow_mode, |this| {
                            this.child(h_flex().ml_2().min_w(rems_from_px(40.)).child(
                                Label::new(match_text).size(LabelSize::Small).color(
                                    if self.active_match_index.is_some() {
                                        Color::Default
                                    } else {
                                        Color::Disabled
                                    },
                                ),
                            ))
                        })
                    })
                    .when(supported_options.find_in_results, |el| {
                        el.child(
                            IconButton::new(SharedString::from("Close"), IconName::Close)
                                .shape(IconButtonShape::Square)
                                .tooltip(move |window, cx| {
                                    Tooltip::for_action("Close Search Bar", &Dismiss, window, cx)
                                })
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.dismiss(&Dismiss, window, cx)
                                })),
                        )
                    }),
            );

        let replace_line = should_show_replace_input.then(|| {
            h_flex()
                .gap_2()
                .child(input_base_styles().child(self.render_text_input(
                    &self.replacement_editor,
                    None,
                    cx,
                )))
                .child(
                    h_flex()
                        .min_w_64()
                        .gap_1()
                        .child(
                            IconButton::new("search-replace-next", ui::IconName::ReplaceNext)
                                .shape(IconButtonShape::Square)
                                .tooltip({
                                    let focus_handle = focus_handle.clone();
                                    move |window, cx| {
                                        Tooltip::for_action_in(
                                            "Replace Next Match",
                                            &ReplaceNext,
                                            &focus_handle,
                                            window,
                                            cx,
                                        )
                                    }
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.replace_next(&ReplaceNext, window, cx)
                                })),
                        )
                        .child(
                            IconButton::new("search-replace-all", ui::IconName::ReplaceAll)
                                .shape(IconButtonShape::Square)
                                .tooltip({
                                    let focus_handle = focus_handle.clone();
                                    move |window, cx| {
                                        Tooltip::for_action_in(
                                            "Replace All Matches",
                                            &ReplaceAll,
                                            &focus_handle,
                                            window,
                                            cx,
                                        )
                                    }
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.replace_all(&ReplaceAll, window, cx)
                                })),
                        ),
                )
        });

        v_flex()
            .id("buffer_search")
            .gap_2()
            .py(px(1.0))
            .track_scroll(&self.scroll_handle)
            .key_context(key_context)
            .capture_action(cx.listener(Self::tab))
            .capture_action(cx.listener(Self::backtab))
            .on_action(cx.listener(Self::previous_history_query))
            .on_action(cx.listener(Self::next_history_query))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::select_next_match))
            .on_action(cx.listener(Self::select_prev_match))
            .on_action(cx.listener(|this, _: &ToggleOutline, window, cx| {
                if let Some(active_searchable_item) = &mut this.active_searchable_item {
                    active_searchable_item.relay_action(Box::new(ToggleOutline), window, cx);
                }
            }))
            .when(self.supported_options(cx).replacement, |this| {
                this.on_action(cx.listener(Self::toggle_replace))
                    .when(in_replace, |this| {
                        this.on_action(cx.listener(Self::replace_next))
                            .on_action(cx.listener(Self::replace_all))
                    })
            })
            .when(self.supported_options(cx).case, |this| {
                this.on_action(cx.listener(Self::toggle_case_sensitive))
            })
            .when(self.supported_options(cx).word, |this| {
                this.on_action(cx.listener(Self::toggle_whole_word))
            })
            .when(self.supported_options(cx).regex, |this| {
                this.on_action(cx.listener(Self::toggle_regex))
            })
            .when(self.supported_options(cx).selection, |this| {
                this.on_action(cx.listener(Self::toggle_selection))
            })
            .child(h_flex().relative().child(search_line.w_full()).when(
                !narrow_mode && !supported_options.find_in_results,
                |div| {
                    div.child(
                        h_flex().absolute().right_0().child(
                            IconButton::new(SharedString::from("Close"), IconName::Close)
                                .shape(IconButtonShape::Square)
                                .tooltip(move |window, cx| {
                                    Tooltip::for_action("Close Search Bar", &Dismiss, window, cx)
                                })
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.dismiss(&Dismiss, window, cx)
                                })),
                        ),
                    )
                    .w_full()
                },
            ))
            .children(replace_line)
    }
}

impl Focusable for BufferSearchBar {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.query_editor.focus_handle(cx)
    }
}

impl ToolbarItemView for BufferSearchBar {
    fn set_active_pane_item(
        &mut self,
        item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        cx.notify();
        self.active_searchable_item_subscription.take();
        self.active_searchable_item.take();

        self.pending_search.take();

        if let Some(searchable_item_handle) =
            item.and_then(|item| item.to_searchable_item_handle(cx))
        {
            let this = cx.entity().downgrade();

            self.active_searchable_item_subscription =
                Some(searchable_item_handle.subscribe_to_search_events(
                    window,
                    cx,
                    Box::new(move |search_event, window, cx| {
                        if let Some(this) = this.upgrade() {
                            this.update(cx, |this, cx| {
                                this.on_active_searchable_item_event(search_event, window, cx)
                            });
                        }
                    }),
                ));

            let is_project_search = searchable_item_handle.supported_options(cx).find_in_results;
            self.active_searchable_item = Some(searchable_item_handle);
            drop(self.update_matches(true, window, cx));
            if !self.dismissed {
                if is_project_search {
                    self.dismiss(&Default::default(), window, cx);
                } else {
                    return ToolbarItemLocation::Secondary;
                }
            }
        }
        ToolbarItemLocation::Hidden
    }
}

impl BufferSearchBar {
    pub fn register(registrar: &mut impl SearchActionsRegistrar) {
        registrar.register_handler(ForDeployed(|this, _: &FocusSearch, window, cx| {
            this.query_editor.focus_handle(cx).focus(window);
            this.select_query(window, cx);
        }));
        registrar.register_handler(ForDeployed(
            |this, action: &ToggleCaseSensitive, window, cx| {
                if this.supported_options(cx).case {
                    this.toggle_case_sensitive(action, window, cx);
                }
            },
        ));
        registrar.register_handler(ForDeployed(|this, action: &ToggleWholeWord, window, cx| {
            if this.supported_options(cx).word {
                this.toggle_whole_word(action, window, cx);
            }
        }));
        registrar.register_handler(ForDeployed(|this, action: &ToggleRegex, window, cx| {
            if this.supported_options(cx).regex {
                this.toggle_regex(action, window, cx);
            }
        }));
        registrar.register_handler(ForDeployed(|this, action: &ToggleSelection, window, cx| {
            if this.supported_options(cx).selection {
                this.toggle_selection(action, window, cx);
            } else {
                cx.propagate();
            }
        }));
        registrar.register_handler(ForDeployed(|this, action: &ToggleReplace, window, cx| {
            if this.supported_options(cx).replacement {
                this.toggle_replace(action, window, cx);
            } else {
                cx.propagate();
            }
        }));
        registrar.register_handler(WithResults(|this, action: &SelectNextMatch, window, cx| {
            if this.supported_options(cx).find_in_results {
                cx.propagate();
            } else {
                this.select_next_match(action, window, cx);
            }
        }));
        registrar.register_handler(WithResults(
            |this, action: &SelectPreviousMatch, window, cx| {
                if this.supported_options(cx).find_in_results {
                    cx.propagate();
                } else {
                    this.select_prev_match(action, window, cx);
                }
            },
        ));
        registrar.register_handler(WithResults(
            |this, action: &SelectAllMatches, window, cx| {
                if this.supported_options(cx).find_in_results {
                    cx.propagate();
                } else {
                    this.select_all_matches(action, window, cx);
                }
            },
        ));
        registrar.register_handler(ForDeployed(
            |this, _: &editor::actions::Cancel, window, cx| {
                this.dismiss(&Dismiss, window, cx);
            },
        ));
        registrar.register_handler(ForDeployed(|this, _: &Dismiss, window, cx| {
            this.dismiss(&Dismiss, window, cx);
        }));

        // register deploy buffer search for both search bar states, since we want to focus into the search bar
        // when the deploy action is triggered in the buffer.
        registrar.register_handler(ForDeployed(|this, deploy, window, cx| {
            this.deploy(deploy, window, cx);
        }));
        registrar.register_handler(ForDismissed(|this, deploy, window, cx| {
            this.deploy(deploy, window, cx);
        }));
        registrar.register_handler(ForDeployed(|this, _: &DeployReplace, window, cx| {
            if this.supported_options(cx).find_in_results {
                cx.propagate();
            } else {
                this.deploy(&Deploy::replace(), window, cx);
            }
        }));
        registrar.register_handler(ForDismissed(|this, _: &DeployReplace, window, cx| {
            if this.supported_options(cx).find_in_results {
                cx.propagate();
            } else {
                this.deploy(&Deploy::replace(), window, cx);
            }
        }));
    }

    pub fn new(
        languages: Option<Arc<LanguageRegistry>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query_editor = cx.new(|cx| Editor::single_line(window, cx));
        cx.subscribe_in(&query_editor, window, Self::on_query_editor_event)
            .detach();
        let replacement_editor = cx.new(|cx| Editor::single_line(window, cx));
        cx.subscribe(&replacement_editor, Self::on_replacement_editor_event)
            .detach();

        let search_options = SearchOptions::from_settings(&EditorSettings::get_global(cx).search);
        if let Some(languages) = languages {
            let query_buffer = query_editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .expect("query editor should be backed by a singleton buffer");
            query_buffer
                .read(cx)
                .set_language_registry(languages.clone());

            cx.spawn(async move |buffer_search_bar, cx| {
                let regex_language = languages
                    .language_for_name("regex")
                    .await
                    .context("loading regex language")?;
                buffer_search_bar
                    .update(cx, |buffer_search_bar, cx| {
                        buffer_search_bar.regex_language = Some(regex_language);
                        buffer_search_bar.adjust_query_regex_language(cx);
                    })
                    .ok();
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        }

        Self {
            query_editor,
            query_editor_focused: false,
            replacement_editor,
            replacement_editor_focused: false,
            active_searchable_item: None,
            active_searchable_item_subscription: None,
            active_match_index: None,
            searchable_items_with_matches: Default::default(),
            default_options: search_options,
            configured_options: search_options,
            search_options,
            pending_search: None,
            query_contains_error: false,
            dismissed: true,
            search_history: SearchHistory::new(
                Some(MAX_BUFFER_SEARCH_HISTORY_SIZE),
                project::search_history::QueryInsertionBehavior::ReplacePreviousIfContains,
            ),
            search_history_cursor: Default::default(),
            active_search: None,
            replace_enabled: false,
            selection_search_enabled: false,
            scroll_handle: ScrollHandle::new(),
            editor_scroll_handle: ScrollHandle::new(),
            editor_needed_width: px(0.),
            regex_language: None,
        }
    }

    pub fn is_dismissed(&self) -> bool {
        self.dismissed
    }

    pub fn dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        self.dismissed = true;
        for searchable_item in self.searchable_items_with_matches.keys() {
            if let Some(searchable_item) =
                WeakSearchableItemHandle::upgrade(searchable_item.as_ref(), cx)
            {
                searchable_item.clear_matches(window, cx);
            }
        }
        if let Some(active_editor) = self.active_searchable_item.as_mut() {
            self.selection_search_enabled = false;
            self.replace_enabled = false;
            active_editor.search_bar_visibility_changed(false, window, cx);
            active_editor.toggle_filtered_search_ranges(false, window, cx);
            let handle = active_editor.item_focus_handle(cx);
            self.focus(&handle, window, cx);
        }
        cx.emit(Event::UpdateLocation);
        cx.emit(ToolbarItemEvent::ChangeLocation(
            ToolbarItemLocation::Hidden,
        ));
        cx.notify();
    }

    pub fn deploy(&mut self, deploy: &Deploy, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.show(window, cx) {
            if let Some(active_item) = self.active_searchable_item.as_mut() {
                active_item.toggle_filtered_search_ranges(
                    deploy.selection_search_enabled,
                    window,
                    cx,
                );
            }
            self.search_suggested(window, cx);
            self.smartcase(window, cx);
            self.replace_enabled = deploy.replace_enabled;
            self.selection_search_enabled = deploy.selection_search_enabled;
            if deploy.focus {
                let mut handle = self.query_editor.focus_handle(cx).clone();
                let mut select_query = true;
                if deploy.replace_enabled && handle.is_focused(window) {
                    handle = self.replacement_editor.focus_handle(cx).clone();
                    select_query = false;
                };

                if select_query {
                    self.select_query(window, cx);
                }

                window.focus(&handle);
            }
            return true;
        }

        cx.propagate();
        false
    }

    pub fn toggle(&mut self, action: &Deploy, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_dismissed() {
            self.deploy(action, window, cx);
        } else {
            self.dismiss(&Dismiss, window, cx);
        }
    }

    pub fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(handle) = self.active_searchable_item.as_ref() else {
            return false;
        };

        self.configured_options =
            SearchOptions::from_settings(&EditorSettings::get_global(cx).search);
        if self.dismissed
            && (self.configured_options != self.default_options
                || self.configured_options != self.search_options)
        {
            self.search_options = self.configured_options;
            self.default_options = self.configured_options;
        }

        self.dismissed = false;
        self.adjust_query_regex_language(cx);
        handle.search_bar_visibility_changed(true, window, cx);
        cx.notify();
        cx.emit(Event::UpdateLocation);
        cx.emit(ToolbarItemEvent::ChangeLocation(
            ToolbarItemLocation::Secondary,
        ));
        true
    }

    fn supported_options(&self, cx: &mut Context<Self>) -> workspace::searchable::SearchOptions {
        self.active_searchable_item
            .as_ref()
            .map(|item| item.supported_options(cx))
            .unwrap_or_default()
    }

    pub fn search_suggested(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let search = self
            .query_suggestion(window, cx)
            .map(|suggestion| self.search(&suggestion, Some(self.default_options), window, cx));

        if let Some(search) = search {
            cx.spawn_in(window, async move |this, cx| {
                search.await?;
                this.update_in(cx, |this, window, cx| {
                    this.activate_current_match(window, cx)
                })
            })
            .detach_and_log_err(cx);
        }
    }

    pub fn activate_current_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(match_ix) = self.active_match_index {
            if let Some(active_searchable_item) = self.active_searchable_item.as_ref() {
                if let Some(matches) = self
                    .searchable_items_with_matches
                    .get(&active_searchable_item.downgrade())
                {
                    active_searchable_item.activate_match(match_ix, matches, window, cx)
                }
            }
        }
    }

    pub fn select_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.query_editor.update(cx, |query_editor, cx| {
            query_editor.select_all(&Default::default(), window, cx);
        });
    }

    pub fn query(&self, cx: &App) -> String {
        self.query_editor.read(cx).text(cx)
    }

    pub fn replacement(&self, cx: &mut App) -> String {
        self.replacement_editor.read(cx).text(cx)
    }

    pub fn query_suggestion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        self.active_searchable_item
            .as_ref()
            .map(|searchable_item| searchable_item.query_suggestion(window, cx))
            .filter(|suggestion| !suggestion.is_empty())
    }

    pub fn set_replacement(&mut self, replacement: Option<&str>, cx: &mut Context<Self>) {
        if replacement.is_none() {
            self.replace_enabled = false;
            return;
        }
        self.replace_enabled = true;
        self.replacement_editor
            .update(cx, |replacement_editor, cx| {
                replacement_editor
                    .buffer()
                    .update(cx, |replacement_buffer, cx| {
                        let len = replacement_buffer.len(cx);
                        replacement_buffer.edit([(0..len, replacement.unwrap())], None, cx);
                    });
            });
    }

    pub fn search(
        &mut self,
        query: &str,
        options: Option<SearchOptions>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<()> {
        let options = options.unwrap_or(self.default_options);
        let updated = query != self.query(cx) || self.search_options != options;
        if updated {
            self.query_editor.update(cx, |query_editor, cx| {
                query_editor.buffer().update(cx, |query_buffer, cx| {
                    let len = query_buffer.len(cx);
                    query_buffer.edit([(0..len, query)], None, cx);
                });
            });
            self.set_search_options(options, cx);
            self.clear_matches(window, cx);
            cx.notify();
        }
        self.update_matches(!updated, window, cx)
    }

    fn render_search_option_button<Action: Fn(&ClickEvent, &mut Window, &mut App) + 'static>(
        &self,
        option: SearchOptions,
        focus_handle: FocusHandle,
        action: Action,
    ) -> impl IntoElement + use<Action> {
        let is_active = self.search_options.contains(option);
        option.as_button(is_active, focus_handle, action)
    }

    pub fn focus_editor(&mut self, _: &FocusEditor, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(active_editor) = self.active_searchable_item.as_ref() {
            let handle = active_editor.item_focus_handle(cx);
            window.focus(&handle);
        }
    }

    pub fn toggle_search_option(
        &mut self,
        search_option: SearchOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_options.toggle(search_option);
        self.default_options = self.search_options;
        drop(self.update_matches(false, window, cx));
        self.adjust_query_regex_language(cx);
        cx.notify();
    }

    pub fn has_search_option(&mut self, search_option: SearchOptions) -> bool {
        self.search_options.contains(search_option)
    }

    pub fn enable_search_option(
        &mut self,
        search_option: SearchOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.search_options.contains(search_option) {
            self.toggle_search_option(search_option, window, cx)
        }
    }

    pub fn set_search_options(&mut self, search_options: SearchOptions, cx: &mut Context<Self>) {
        self.search_options = search_options;
        self.adjust_query_regex_language(cx);
        cx.notify();
    }

    pub fn clear_search_within_ranges(
        &mut self,
        search_options: SearchOptions,
        cx: &mut Context<Self>,
    ) {
        self.search_options = search_options;
        self.adjust_query_regex_language(cx);
        cx.notify();
    }

    fn select_next_match(
        &mut self,
        _: &SelectNextMatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_match(Direction::Next, 1, window, cx);
    }

    fn select_prev_match(
        &mut self,
        _: &SelectPreviousMatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_match(Direction::Prev, 1, window, cx);
    }

    fn select_all_matches(
        &mut self,
        _: &SelectAllMatches,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.dismissed && self.active_match_index.is_some() {
            if let Some(searchable_item) = self.active_searchable_item.as_ref() {
                if let Some(matches) = self
                    .searchable_items_with_matches
                    .get(&searchable_item.downgrade())
                {
                    searchable_item.select_matches(matches, window, cx);
                    self.focus_editor(&FocusEditor, window, cx);
                }
            }
        }
    }

    pub fn select_match(
        &mut self,
        direction: Direction,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self.active_match_index {
            if let Some(searchable_item) = self.active_searchable_item.as_ref() {
                if let Some(matches) = self
                    .searchable_items_with_matches
                    .get(&searchable_item.downgrade())
                    .filter(|matches| !matches.is_empty())
                {
                    // If 'wrapscan' is disabled, searches do not wrap around the end of the file.
                    if !EditorSettings::get_global(cx).search_wrap
                        && ((direction == Direction::Next && index + count >= matches.len())
                            || (direction == Direction::Prev && index < count))
                    {
                        crate::show_no_more_matches(window, cx);
                        return;
                    }
                    let new_match_index = searchable_item
                        .match_index_for_direction(matches, index, direction, count, window, cx);

                    searchable_item.update_matches(matches, window, cx);
                    searchable_item.activate_match(new_match_index, matches, window, cx);
                }
            }
        }
    }

    pub fn select_last_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(searchable_item) = self.active_searchable_item.as_ref() {
            if let Some(matches) = self
                .searchable_items_with_matches
                .get(&searchable_item.downgrade())
            {
                if matches.is_empty() {
                    return;
                }
                let new_match_index = matches.len() - 1;
                searchable_item.update_matches(matches, window, cx);
                searchable_item.activate_match(new_match_index, matches, window, cx);
            }
        }
    }

    fn on_query_editor_event(
        &mut self,
        editor: &Entity<Editor>,
        event: &editor::EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            editor::EditorEvent::Focused => self.query_editor_focused = true,
            editor::EditorEvent::Blurred => self.query_editor_focused = false,
            editor::EditorEvent::Edited { .. } => {
                self.smartcase(window, cx);
                self.clear_matches(window, cx);
                let search = self.update_matches(false, window, cx);

                let width = editor.update(cx, |editor, cx| {
                    let text_layout_details = editor.text_layout_details(window);
                    let snapshot = editor.snapshot(window, cx).display_snapshot;

                    snapshot.x_for_display_point(snapshot.max_point(), &text_layout_details)
                        - snapshot.x_for_display_point(DisplayPoint::zero(), &text_layout_details)
                });
                self.editor_needed_width = width;
                cx.notify();

                cx.spawn_in(window, async move |this, cx| {
                    search.await?;
                    this.update_in(cx, |this, window, cx| {
                        this.activate_current_match(window, cx)
                    })
                })
                .detach_and_log_err(cx);
            }
            _ => {}
        }
    }

    fn on_replacement_editor_event(
        &mut self,
        _: Entity<Editor>,
        event: &editor::EditorEvent,
        _: &mut Context<Self>,
    ) {
        match event {
            editor::EditorEvent::Focused => self.replacement_editor_focused = true,
            editor::EditorEvent::Blurred => self.replacement_editor_focused = false,
            _ => {}
        }
    }

    fn on_active_searchable_item_event(
        &mut self,
        event: &SearchEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SearchEvent::MatchesInvalidated => {
                drop(self.update_matches(false, window, cx));
            }
            SearchEvent::ActiveMatchChanged => self.update_match_index(window, cx),
        }
    }

    fn toggle_case_sensitive(
        &mut self,
        _: &ToggleCaseSensitive,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_search_option(SearchOptions::CASE_SENSITIVE, window, cx)
    }

    fn toggle_whole_word(
        &mut self,
        _: &ToggleWholeWord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_search_option(SearchOptions::WHOLE_WORD, window, cx)
    }

    fn toggle_selection(
        &mut self,
        _: &ToggleSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(active_item) = self.active_searchable_item.as_mut() {
            self.selection_search_enabled = !self.selection_search_enabled;
            active_item.toggle_filtered_search_ranges(self.selection_search_enabled, window, cx);
            drop(self.update_matches(false, window, cx));
            cx.notify();
        }
    }

    fn toggle_regex(&mut self, _: &ToggleRegex, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_search_option(SearchOptions::REGEX, window, cx)
    }

    fn clear_active_searchable_item_matches(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(active_searchable_item) = self.active_searchable_item.as_ref() {
            self.active_match_index = None;
            self.searchable_items_with_matches
                .remove(&active_searchable_item.downgrade());
            active_searchable_item.clear_matches(window, cx);
        }
    }

    pub fn has_active_match(&self) -> bool {
        self.active_match_index.is_some()
    }

    fn clear_matches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut active_item_matches = None;
        for (searchable_item, matches) in self.searchable_items_with_matches.drain() {
            if let Some(searchable_item) =
                WeakSearchableItemHandle::upgrade(searchable_item.as_ref(), cx)
            {
                if Some(&searchable_item) == self.active_searchable_item.as_ref() {
                    active_item_matches = Some((searchable_item.downgrade(), matches));
                } else {
                    searchable_item.clear_matches(window, cx);
                }
            }
        }

        self.searchable_items_with_matches
            .extend(active_item_matches);
    }

    fn update_matches(
        &mut self,
        reuse_existing_query: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<()> {
        let (done_tx, done_rx) = oneshot::channel();
        let query = self.query(cx);
        self.pending_search.take();

        if let Some(active_searchable_item) = self.active_searchable_item.as_ref() {
            self.query_contains_error = false;
            if query.is_empty() {
                self.clear_active_searchable_item_matches(window, cx);
                let _ = done_tx.send(());
                cx.notify();
            } else {
                let query: Arc<_> = if let Some(search) =
                    self.active_search.take().filter(|_| reuse_existing_query)
                {
                    search
                } else {
                    if self.search_options.contains(SearchOptions::REGEX) {
                        match SearchQuery::regex(
                            query,
                            self.search_options.contains(SearchOptions::WHOLE_WORD),
                            self.search_options.contains(SearchOptions::CASE_SENSITIVE),
                            false,
                            self.search_options
                                .contains(SearchOptions::ONE_MATCH_PER_LINE),
                            Default::default(),
                            Default::default(),
                            false,
                            None,
                        ) {
                            Ok(query) => query.with_replacement(self.replacement(cx)),
                            Err(_) => {
                                self.query_contains_error = true;
                                self.clear_active_searchable_item_matches(window, cx);
                                cx.notify();
                                return done_rx;
                            }
                        }
                    } else {
                        match SearchQuery::text(
                            query,
                            self.search_options.contains(SearchOptions::WHOLE_WORD),
                            self.search_options.contains(SearchOptions::CASE_SENSITIVE),
                            false,
                            Default::default(),
                            Default::default(),
                            false,
                            None,
                        ) {
                            Ok(query) => query.with_replacement(self.replacement(cx)),
                            Err(_) => {
                                self.query_contains_error = true;
                                self.clear_active_searchable_item_matches(window, cx);
                                cx.notify();
                                return done_rx;
                            }
                        }
                    }
                    .into()
                };

                self.active_search = Some(query.clone());
                let query_text = query.as_str().to_string();

                let matches = active_searchable_item.find_matches(query, window, cx);

                let active_searchable_item = active_searchable_item.downgrade();
                self.pending_search = Some(cx.spawn_in(window, async move |this, cx| {
                    let matches = matches.await;

                    this.update_in(cx, |this, window, cx| {
                        if let Some(active_searchable_item) =
                            WeakSearchableItemHandle::upgrade(active_searchable_item.as_ref(), cx)
                        {
                            this.searchable_items_with_matches
                                .insert(active_searchable_item.downgrade(), matches);

                            this.update_match_index(window, cx);
                            this.search_history
                                .add(&mut this.search_history_cursor, query_text);
                            if !this.dismissed {
                                let matches = this
                                    .searchable_items_with_matches
                                    .get(&active_searchable_item.downgrade())
                                    .unwrap();
                                if matches.is_empty() {
                                    active_searchable_item.clear_matches(window, cx);
                                } else {
                                    active_searchable_item.update_matches(matches, window, cx);
                                }
                                let _ = done_tx.send(());
                            }
                            cx.notify();
                        }
                    })
                    .log_err();
                }));
            }
        }
        done_rx
    }

    fn reverse_direction_if_backwards(&self, direction: Direction) -> Direction {
        if self.search_options.contains(SearchOptions::BACKWARDS) {
            direction.opposite()
        } else {
            direction
        }
    }

    pub fn update_match_index(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let direction = self.reverse_direction_if_backwards(Direction::Next);
        let new_index = self
            .active_searchable_item
            .as_ref()
            .and_then(|searchable_item| {
                let matches = self
                    .searchable_items_with_matches
                    .get(&searchable_item.downgrade())?;
                searchable_item.active_match_index(direction, matches, window, cx)
            });
        if new_index != self.active_match_index {
            self.active_match_index = new_index;
            cx.notify();
        }
    }

    fn tab(&mut self, _: &Tab, window: &mut Window, cx: &mut Context<Self>) {
        // Search -> Replace -> Editor
        let focus_handle = if self.replace_enabled && self.query_editor_focused {
            self.replacement_editor.focus_handle(cx)
        } else if let Some(item) = self.active_searchable_item.as_ref() {
            item.item_focus_handle(cx)
        } else {
            return;
        };
        self.focus(&focus_handle, window, cx);
        cx.stop_propagation();
    }

    fn backtab(&mut self, _: &Backtab, window: &mut Window, cx: &mut Context<Self>) {
        // Search -> Replace -> Search
        let focus_handle = if self.replace_enabled && self.query_editor_focused {
            self.replacement_editor.focus_handle(cx)
        } else if self.replacement_editor_focused {
            self.query_editor.focus_handle(cx)
        } else {
            return;
        };
        self.focus(&focus_handle, window, cx);
        cx.stop_propagation();
    }

    fn next_history_query(
        &mut self,
        _: &NextHistoryQuery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(new_query) = self
            .search_history
            .next(&mut self.search_history_cursor)
            .map(str::to_string)
        {
            drop(self.search(&new_query, Some(self.search_options), window, cx));
        } else {
            self.search_history_cursor.reset();
            drop(self.search("", Some(self.search_options), window, cx));
        }
    }

    fn previous_history_query(
        &mut self,
        _: &PreviousHistoryQuery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.query(cx).is_empty() {
            if let Some(new_query) = self
                .search_history
                .current(&mut self.search_history_cursor)
                .map(str::to_string)
            {
                drop(self.search(&new_query, Some(self.search_options), window, cx));
                return;
            }
        }

        if let Some(new_query) = self
            .search_history
            .previous(&mut self.search_history_cursor)
            .map(str::to_string)
        {
            drop(self.search(&new_query, Some(self.search_options), window, cx));
        }
    }

    fn focus(&self, handle: &gpui::FocusHandle, window: &mut Window, cx: &mut Context<Self>) {
        cx.on_next_frame(window, |_, window, _| {
            window.invalidate_character_coordinates();
        });
        window.focus(handle);
    }

    fn toggle_replace(&mut self, _: &ToggleReplace, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_searchable_item.is_some() {
            self.replace_enabled = !self.replace_enabled;
            let handle = if self.replace_enabled {
                self.replacement_editor.focus_handle(cx)
            } else {
                self.query_editor.focus_handle(cx)
            };
            self.focus(&handle, window, cx);
            cx.notify();
        }
    }

    fn replace_next(&mut self, _: &ReplaceNext, window: &mut Window, cx: &mut Context<Self>) {
        let mut should_propagate = true;
        if !self.dismissed && self.active_search.is_some() {
            if let Some(searchable_item) = self.active_searchable_item.as_ref() {
                if let Some(query) = self.active_search.as_ref() {
                    if let Some(matches) = self
                        .searchable_items_with_matches
                        .get(&searchable_item.downgrade())
                    {
                        if let Some(active_index) = self.active_match_index {
                            let query = query
                                .as_ref()
                                .clone()
                                .with_replacement(self.replacement(cx));
                            searchable_item.replace(matches.at(active_index), &query, window, cx);
                            self.select_next_match(&SelectNextMatch, window, cx);
                        }
                        should_propagate = false;
                    }
                }
            }
        }
        if !should_propagate {
            cx.stop_propagation();
        }
    }

    pub fn replace_all(&mut self, _: &ReplaceAll, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dismissed && self.active_search.is_some() {
            if let Some(searchable_item) = self.active_searchable_item.as_ref() {
                if let Some(query) = self.active_search.as_ref() {
                    if let Some(matches) = self
                        .searchable_items_with_matches
                        .get(&searchable_item.downgrade())
                    {
                        let query = query
                            .as_ref()
                            .clone()
                            .with_replacement(self.replacement(cx));
                        searchable_item.replace_all(&mut matches.iter(), &query, window, cx);
                    }
                }
            }
        }
    }

    pub fn match_exists(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.update_match_index(window, cx);
        self.active_match_index.is_some()
    }

    pub fn should_use_smartcase_search(&mut self, cx: &mut Context<Self>) -> bool {
        EditorSettings::get_global(cx).use_smartcase_search
    }

    pub fn is_contains_uppercase(&mut self, str: &String) -> bool {
        str.chars().any(|c| c.is_uppercase())
    }

    fn smartcase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.should_use_smartcase_search(cx) {
            let query = self.query(cx);
            if !query.is_empty() {
                let is_case = self.is_contains_uppercase(&query);
                if self.has_search_option(SearchOptions::CASE_SENSITIVE) != is_case {
                    self.toggle_search_option(SearchOptions::CASE_SENSITIVE, window, cx);
                }
            }
        }
    }

    fn adjust_query_regex_language(&self, cx: &mut App) {
        let enable = self.search_options.contains(SearchOptions::REGEX);
        let query_buffer = self
            .query_editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
            .expect("query editor should be backed by a singleton buffer");
        if enable {
            if let Some(regex_language) = self.regex_language.clone() {
                query_buffer.update(cx, |query_buffer, cx| {
                    query_buffer.set_language(Some(regex_language), cx);
                })
            }
        } else {
            query_buffer.update(cx, |query_buffer, cx| {
                query_buffer.set_language(None, cx);
            })
        }
    }
}

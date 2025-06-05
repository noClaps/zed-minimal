mod app_menus;
pub mod component_preview;
pub mod inline_completion_registry;
pub(crate) mod mac_only_instance;
mod migrate;
mod open_listener;
mod quick_action_bar;

use anyhow::Context as _;
pub use app_menus::*;
use assets::Assets;
use breadcrumbs::Breadcrumbs;
use client::zed_urls;
use collections::VecDeque;
use editor::ProposedChangesEditorToolbar;
use editor::{Editor, MultiBuffer, scroll::Autoscroll};
use futures::future::Either;
use futures::{StreamExt, channel::mpsc, select_biased};
use git_ui::git_panel::GitPanel;
use git_ui::project_diff::ProjectDiffToolbar;
use gpui::{
    Action, App, AppContext as _, Context, DismissEvent, Element, Entity, Focusable, KeyBinding,
    ParentElement, PathPromptOptions, PromptLevel, ReadGlobal, SharedString, Styled, Task,
    TitlebarOptions, UpdateGlobal, Window, WindowKind, WindowOptions, actions, image_cache, point,
    px, retain_all,
};
use image_viewer::ImageInfo;
use migrate::{MigrationBanner, MigrationEvent, MigrationNotification, MigrationType};
use migrator::{migrate_keymap, migrate_settings};
pub use open_listener::*;
use outline_panel::OutlinePanel;
use paths::{local_settings_file_relative_path, local_tasks_file_relative_path};
use project::{DirectoryLister, ProjectItem};
use project_panel::ProjectPanel;
use quick_action_bar::QuickActionBar;
use recent_projects::open_ssh_project;
use release_channel::{AppCommitSha, ReleaseChannel};
use rope::Rope;
use search::project_search::ProjectSearchBar;
use settings::{
    InvalidSettingsError, KeymapFile, KeymapFileLoadResult, Settings, SettingsStore,
    initial_project_settings_content, initial_tasks_content, update_settings_file,
};
use std::path::PathBuf;
use std::sync::atomic::{self, AtomicBool};
use std::{borrow::Cow, path::Path, sync::Arc};
use terminal_view::terminal_panel::{self, TerminalPanel};
use theme::{ActiveTheme, ThemeSettings};
use ui::{PopoverMenuHandle, prelude::*};
use util::markdown::MarkdownString;
use util::{ResultExt, asset_str};
use uuid::Uuid;
use welcome::{DOCS_URL, MultibufferHint};
use workspace::CloseIntent;
use workspace::notifications::{NotificationId, dismiss_app_notification, show_app_notification};
use workspace::{
    AppState, NewFile, NewWindow, OpenLog, Toast, Workspace, WorkspaceSettings,
    create_and_open_local_file, notifications::simple_message_notification::MessageNotification,
    open_new,
};
use workspace::{CloseWindow, with_active_or_new_workspace};
use workspace::{Pane, notifications::DetachAndPromptErr};
use zed_actions::{
    OpenAccountSettings, OpenBrowser, OpenDocs, OpenServerSettings, OpenSettings, OpenZedUrl, Quit,
};

actions!(
    zed,
    [
        DebugElements,
        Hide,
        HideOthers,
        Minimize,
        OpenDefaultSettings,
        OpenProjectSettings,
        OpenProjectTasks,
        OpenTasks,
        ResetDatabase,
        ShowAll,
        ToggleFullScreen,
        Zoom,
        TestPanic,
    ]
);

pub fn init(cx: &mut App) {
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(quit);

    if ReleaseChannel::global(cx) == ReleaseChannel::Dev {
        cx.on_action(test_panic);
    }

    cx.on_action(|_: &OpenLog, cx| {
        with_active_or_new_workspace(cx, |workspace, window, cx| {
            open_log_file(workspace, window, cx);
        });
    });
    cx.on_action(|_: &zed_actions::OpenLicenses, cx| {
        with_active_or_new_workspace(cx, |workspace, window, cx| {
            open_bundled_file(
                workspace,
                asset_str::<Assets>("licenses.md"),
                "Open Source License Attribution",
                "Markdown",
                window,
                cx,
            );
        });
    });
    cx.on_action(|_: &zed_actions::OpenTelemetryLog, cx| {
        with_active_or_new_workspace(cx, |workspace, window, cx| {
            open_telemetry_log_file(workspace, window, cx);
        });
    });
    cx.on_action(|&zed_actions::OpenKeymap, cx| {
        with_active_or_new_workspace(cx, |_, window, cx| {
            open_settings_file(
                paths::keymap_file(),
                || settings::initial_keymap_content().as_ref().into(),
                window,
                cx,
            );
        });
    });
    cx.on_action(|_: &OpenSettings, cx| {
        with_active_or_new_workspace(cx, |_, window, cx| {
            open_settings_file(
                paths::settings_file(),
                || settings::initial_user_settings_content().as_ref().into(),
                window,
                cx,
            );
        });
    });
    cx.on_action(|_: &OpenAccountSettings, cx| {
        with_active_or_new_workspace(cx, |_, _, cx| {
            cx.open_url(&zed_urls::account_url(cx));
        });
    });
    cx.on_action(|_: &OpenTasks, cx| {
        with_active_or_new_workspace(cx, |_, window, cx| {
            open_settings_file(
                paths::tasks_file(),
                || settings::initial_tasks_content().as_ref().into(),
                window,
                cx,
            );
        });
    });
    cx.on_action(|_: &OpenDefaultSettings, cx| {
        with_active_or_new_workspace(cx, |workspace, window, cx| {
            open_bundled_file(
                workspace,
                settings::default_settings(),
                "Default Settings",
                "JSON",
                window,
                cx,
            );
        });
    });
    cx.on_action(|_: &zed_actions::OpenDefaultKeymap, cx| {
        with_active_or_new_workspace(cx, |workspace, window, cx| {
            open_bundled_file(
                workspace,
                settings::default_keymap(),
                "Default Key Bindings",
                "JSON",
                window,
                cx,
            );
        });
    });
}

fn bind_on_window_closed(cx: &mut App) -> Option<gpui::Subscription> {
    WorkspaceSettings::get_global(cx)
        .on_last_window_closed
        .is_quit_app()
        .then(|| {
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
        })
}

pub fn build_window_options(display_uuid: Option<Uuid>, cx: &mut App) -> WindowOptions {
    let display = display_uuid.and_then(|uuid| {
        cx.displays()
            .into_iter()
            .find(|display| display.uuid().ok() == Some(uuid))
    });
    let app_id = ReleaseChannel::global(cx).app_id();
    let window_decorations = match std::env::var("ZED_WINDOW_DECORATIONS") {
        Ok(val) if val == "server" => gpui::WindowDecorations::Server,
        Ok(val) if val == "client" => gpui::WindowDecorations::Client,
        _ => gpui::WindowDecorations::Client,
    };

    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: Some(point(px(9.0), px(9.0))),
        }),
        window_bounds: None,
        focus: false,
        show: false,
        kind: WindowKind::Normal,
        is_movable: true,
        display_id: display.map(|display| display.id()),
        window_background: cx.theme().window_background_appearance(),
        app_id: Some(app_id.to_owned()),
        window_decorations: Some(window_decorations),
        window_min_size: Some(gpui::Size {
            width: px(360.0),
            height: px(240.0),
        }),
    }
}

pub fn initialize_workspace(app_state: Arc<AppState>, cx: &mut App) {
    let mut _on_close_subscription = bind_on_window_closed(cx);
    cx.observe_global::<SettingsStore>(move |cx| {
        _on_close_subscription = bind_on_window_closed(cx);
    })
    .detach();

    cx.observe_new(move |workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        let workspace_handle = cx.entity().clone();
        let center_pane = workspace.active_pane().clone();
        initialize_pane(workspace, &center_pane, window, cx);

        cx.subscribe_in(&workspace_handle, window, {
            move |workspace, _, event, window, cx| match event {
                workspace::Event::PaneAdded(pane) => {
                    initialize_pane(workspace, &pane, window, cx);
                }
                workspace::Event::OpenBundledFile {
                    text,
                    title,
                    language,
                } => open_bundled_file(workspace, text.clone(), title, language, window, cx),
                _ => {}
            }
        })
        .detach();

        if let Some(specs) = window.gpu_specs() {
            log::info!("Using GPU: {:?}", specs);
            show_software_emulation_warning_if_needed(specs, window, cx);
        }

        let popover_menu_handle = PopoverMenuHandle::default();

        let inline_completion_button = cx.new(|cx| {
            inline_completion_button::InlineCompletionButton::new(
                app_state.fs.clone(),
                popover_menu_handle.clone(),
                cx,
            )
        });

        workspace.register_action({
            move |_, _: &inline_completion_button::ToggleMenu, window, cx| {
                popover_menu_handle.toggle(window, cx);
            }
        });

        let search_button = cx.new(|_| search::search_status_button::SearchButton::new());
        let diagnostic_summary =
            cx.new(|cx| diagnostics::items::DiagnosticIndicator::new(workspace, cx));
        let activity_indicator = activity_indicator::ActivityIndicator::new(
            workspace,
            app_state.languages.clone(),
            window,
            cx,
        );
        let active_buffer_language =
            cx.new(|_| language_selector::ActiveBufferLanguage::new(workspace));
        let active_toolchain_language =
            cx.new(|cx| toolchain_selector::ActiveToolchain::new(workspace, window, cx));
        let image_info = cx.new(|_cx| ImageInfo::new(workspace));
        let cursor_position =
            cx.new(|_| go_to_line::cursor_position::CursorPosition::new(workspace));
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_left_item(search_button, window, cx);
            status_bar.add_left_item(diagnostic_summary, window, cx);
            status_bar.add_left_item(activity_indicator, window, cx);
            status_bar.add_right_item(inline_completion_button, window, cx);
            status_bar.add_right_item(active_buffer_language, window, cx);
            status_bar.add_right_item(active_toolchain_language, window, cx);
            status_bar.add_right_item(cursor_position, window, cx);
            status_bar.add_right_item(image_info, window, cx);
        });

        let handle = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            handle
                .update(cx, |workspace, cx| {
                    // We'll handle closing asynchronously
                    workspace.close_window(&CloseWindow, window, cx);
                    false
                })
                .unwrap_or(true)
        });

        initialize_panels(window, cx);
        register_actions(app_state.clone(), workspace, window, cx);

        workspace.focus_handle(cx).focus(window);
    })
    .detach();
}

fn show_software_emulation_warning_if_needed(
    specs: gpui::GpuSpecs,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if specs.is_software_emulated && std::env::var("ZED_ALLOW_EMULATED_GPU").is_err() {
        let message = format!(
            db::indoc! {r#"
            Zed uses Vulkan for rendering and requires a compatible GPU.

            Currently you are using a software emulated GPU ({}) which
            will result in awful performance.

            For troubleshooting see: https://zed.dev/docs/linux
            Set ZED_ALLOW_EMULATED_GPU=1 env var to permanently override.
            "#},
            specs.device_name
        );
        let prompt = window.prompt(
            PromptLevel::Critical,
            "Unsupported GPU",
            Some(&message),
            &["Skip", "Troubleshoot and Quit"],
            cx,
        );
        cx.spawn(async move |_, cx| {
            if prompt.await == Ok(1) {
                cx.update(|cx| {
                    cx.open_url("https://zed.dev/docs/linux#zed-fails-to-open-windows");
                    cx.quit();
                })
                .ok();
            }
        })
        .detach()
    }
}

fn initialize_panels(window: &mut Window, cx: &mut Context<Workspace>) {
    cx.spawn_in(window, async move |workspace_handle, cx| {
        let project_panel = ProjectPanel::load(workspace_handle.clone(), cx.clone());
        let outline_panel = OutlinePanel::load(workspace_handle.clone(), cx.clone());
        let terminal_panel = TerminalPanel::load(workspace_handle.clone(), cx.clone());

        let (project_panel, outline_panel, terminal_panel) =
            futures::try_join!(project_panel, outline_panel, terminal_panel,)?;

        workspace_handle.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(project_panel, window, cx);
            workspace.add_panel(outline_panel, window, cx);
            workspace.add_panel(terminal_panel, window, cx);

            let entity = cx.entity();
            let project = workspace.project().clone();
            let app_state = workspace.app_state().clone();
            let git_panel = cx.new(|cx| GitPanel::new(entity, project, app_state, window, cx));
            workspace.add_panel(git_panel, window, cx);
        })?;

        anyhow::Ok(())
    })
    .detach();
}

fn register_actions(
    app_state: Arc<AppState>,
    workspace: &mut Workspace,
    _: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace
        .register_action(about)
        .register_action(|_, _: &OpenDocs, _, cx| cx.open_url(DOCS_URL))
        .register_action(|_, _: &Minimize, window, _| {
            window.minimize_window();
        })
        .register_action(|_, _: &Zoom, window, _| {
            window.zoom_window();
        })
        .register_action(|_, _: &ToggleFullScreen, window, _| {
            window.toggle_fullscreen();
        })
        .register_action(|_, action: &OpenZedUrl, _, cx| {
            OpenListener::global(cx).open_urls(vec![action.url.clone()])
        })
        .register_action(|_, action: &OpenBrowser, _window, cx| cx.open_url(&action.url))
        .register_action(|workspace, _: &workspace::Open, window, cx| {
            telemetry::event!("Project Opened");
            let paths = workspace.prompt_for_open_path(
                PathPromptOptions {
                    files: true,
                    directories: true,
                    multiple: true,
                },
                DirectoryLister::Local(
                    workspace.project().clone(),
                    workspace.app_state().fs.clone(),
                ),
                window,
                cx,
            );

            cx.spawn_in(window, async move |this, cx| {
                let Some(paths) = paths.await.log_err().flatten() else {
                    return;
                };

                if let Some(task) = this
                    .update_in(cx, |this, window, cx| {
                        this.open_workspace_for_paths(false, paths, window, cx)
                    })
                    .log_err()
                {
                    task.await.log_err();
                }
            })
            .detach()
        })
        .register_action(|workspace, action: &zed_actions::OpenRemote, window, cx| {
            if !action.from_existing_connection {
                cx.propagate();
                return;
            }
            // You need existing remote connection to open it this way
            if workspace.project().read(cx).is_local() {
                return;
            }
            telemetry::event!("Project Opened");
            let paths = workspace.prompt_for_open_path(
                PathPromptOptions {
                    files: true,
                    directories: true,
                    multiple: true,
                },
                DirectoryLister::Project(workspace.project().clone()),
                window,
                cx,
            );
            cx.spawn_in(window, async move |this, cx| {
                let Some(paths) = paths.await.log_err().flatten() else {
                    return;
                };
                if let Some(task) = this
                    .update_in(cx, |this, window, cx| {
                        open_new_ssh_project_from_project(this, paths, window, cx)
                    })
                    .log_err()
                {
                    task.await.log_err();
                }
            })
            .detach()
        })
        .register_action({
            let fs = app_state.fs.clone();
            move |_, action: &zed_actions::IncreaseUiFontSize, _window, cx| {
                if action.persist {
                    update_settings_file::<ThemeSettings>(fs.clone(), cx, move |settings, cx| {
                        let ui_font_size = ThemeSettings::get_global(cx).ui_font_size(cx) + px(1.0);
                        let _ = settings
                            .ui_font_size
                            .insert(theme::clamp_font_size(ui_font_size).0);
                    });
                } else {
                    theme::adjust_ui_font_size(cx, |size| {
                        *size += px(1.0);
                    });
                }
            }
        })
        .register_action({
            let fs = app_state.fs.clone();
            move |_, action: &zed_actions::DecreaseUiFontSize, _window, cx| {
                if action.persist {
                    update_settings_file::<ThemeSettings>(fs.clone(), cx, move |settings, cx| {
                        let ui_font_size = ThemeSettings::get_global(cx).ui_font_size(cx) - px(1.0);
                        let _ = settings
                            .ui_font_size
                            .insert(theme::clamp_font_size(ui_font_size).0);
                    });
                } else {
                    theme::adjust_ui_font_size(cx, |size| {
                        *size -= px(1.0);
                    });
                }
            }
        })
        .register_action({
            let fs = app_state.fs.clone();
            move |_, action: &zed_actions::ResetUiFontSize, _window, cx| {
                if action.persist {
                    update_settings_file::<ThemeSettings>(fs.clone(), cx, move |settings, _| {
                        settings.ui_font_size = None;
                    });
                } else {
                    theme::reset_ui_font_size(cx);
                }
            }
        })
        .register_action({
            let fs = app_state.fs.clone();
            move |_, action: &zed_actions::IncreaseBufferFontSize, _window, cx| {
                if action.persist {
                    update_settings_file::<ThemeSettings>(fs.clone(), cx, move |settings, cx| {
                        let buffer_font_size =
                            ThemeSettings::get_global(cx).buffer_font_size(cx) + px(1.0);
                        let _ = settings
                            .buffer_font_size
                            .insert(theme::clamp_font_size(buffer_font_size).0);
                    });
                } else {
                    theme::adjust_buffer_font_size(cx, |size| {
                        *size += px(1.0);
                    });
                }
            }
        })
        .register_action({
            let fs = app_state.fs.clone();
            move |_, action: &zed_actions::DecreaseBufferFontSize, _window, cx| {
                if action.persist {
                    update_settings_file::<ThemeSettings>(fs.clone(), cx, move |settings, cx| {
                        let buffer_font_size =
                            ThemeSettings::get_global(cx).buffer_font_size(cx) - px(1.0);
                        let _ = settings
                            .buffer_font_size
                            .insert(theme::clamp_font_size(buffer_font_size).0);
                    });
                } else {
                    theme::adjust_buffer_font_size(cx, |size| {
                        *size -= px(1.0);
                    });
                }
            }
        })
        .register_action({
            let fs = app_state.fs.clone();
            move |_, action: &zed_actions::ResetBufferFontSize, _window, cx| {
                if action.persist {
                    update_settings_file::<ThemeSettings>(fs.clone(), cx, move |settings, _| {
                        settings.buffer_font_size = None;
                    });
                } else {
                    theme::reset_buffer_font_size(cx);
                }
            }
        })
        .register_action(install_cli)
        .register_action(|_, _: &install_cli::RegisterZedScheme, window, cx| {
            cx.spawn_in(window, async move |workspace, cx| {
                install_cli::register_zed_scheme(&cx).await?;
                workspace.update_in(cx, |workspace, _, cx| {
                    struct RegisterZedScheme;

                    workspace.show_toast(
                        Toast::new(
                            NotificationId::unique::<RegisterZedScheme>(),
                            format!(
                                "zed:// links will now open in {}.",
                                ReleaseChannel::global(cx).display_name()
                            ),
                        ),
                        cx,
                    )
                })?;
                Ok(())
            })
            .detach_and_prompt_err(
                "Error registering zed:// scheme",
                window,
                cx,
                |_, _, _| None,
            );
        })
        .register_action(open_project_settings_file)
        .register_action(open_project_tasks_file)
        .register_action(
            |workspace: &mut Workspace,
             _: &project_panel::ToggleFocus,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                workspace.toggle_panel_focus::<ProjectPanel>(window, cx);
            },
        )
        .register_action(
            |workspace: &mut Workspace,
             _: &outline_panel::ToggleFocus,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                workspace.toggle_panel_focus::<OutlinePanel>(window, cx);
            },
        )
        .register_action(
            |workspace: &mut Workspace,
             _: &terminal_panel::ToggleFocus,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                workspace.toggle_panel_focus::<TerminalPanel>(window, cx);
            },
        )
        .register_action({
            let app_state = Arc::downgrade(&app_state);
            move |_, _: &NewWindow, _, cx| {
                if let Some(app_state) = app_state.upgrade() {
                    open_new(
                        Default::default(),
                        app_state,
                        cx,
                        |workspace, window, cx| {
                            cx.activate(true);
                            Editor::new_file(workspace, &Default::default(), window, cx)
                        },
                    )
                    .detach();
                }
            }
        })
        .register_action({
            let app_state = Arc::downgrade(&app_state);
            move |_, _: &NewFile, _, cx| {
                if let Some(app_state) = app_state.upgrade() {
                    open_new(
                        Default::default(),
                        app_state,
                        cx,
                        |workspace, window, cx| {
                            Editor::new_file(workspace, &Default::default(), window, cx)
                        },
                    )
                    .detach();
                }
            }
        });
    if workspace.project().read(cx).is_via_ssh() {
        workspace.register_action({
            move |workspace, _: &OpenServerSettings, window, cx| {
                let open_server_settings = workspace
                    .project()
                    .update(cx, |project, cx| project.open_server_settings(cx));

                cx.spawn_in(window, async move |workspace, cx| {
                    let buffer = open_server_settings.await?;

                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace.open_path(
                                buffer
                                    .read(cx)
                                    .project_path(cx)
                                    .expect("Settings file must have a location"),
                                None,
                                true,
                                window,
                                cx,
                            )
                        })?
                        .await?;

                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            }
        });
    }
}

fn initialize_pane(
    workspace: &Workspace,
    pane: &Entity<Pane>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    pane.update(cx, |pane, cx| {
        pane.toolbar().update(cx, |toolbar, cx| {
            let multibuffer_hint = cx.new(|_| MultibufferHint::new());
            toolbar.add_item(multibuffer_hint, window, cx);
            let breadcrumbs = cx.new(|_| Breadcrumbs::new());
            toolbar.add_item(breadcrumbs, window, cx);
            let buffer_search_bar = cx.new(|cx| {
                search::BufferSearchBar::new(
                    Some(workspace.project().read(cx).languages().clone()),
                    window,
                    cx,
                )
            });
            toolbar.add_item(buffer_search_bar.clone(), window, cx);
            let proposed_change_bar = cx.new(|_| ProposedChangesEditorToolbar::new());
            toolbar.add_item(proposed_change_bar, window, cx);
            let quick_action_bar =
                cx.new(|cx| QuickActionBar::new(buffer_search_bar, workspace, cx));
            toolbar.add_item(quick_action_bar, window, cx);
            let diagnostic_editor_controls = cx.new(|_| diagnostics::ToolbarControls::new());
            toolbar.add_item(diagnostic_editor_controls, window, cx);
            let project_search_bar = cx.new(|_| ProjectSearchBar::new());
            toolbar.add_item(project_search_bar, window, cx);
            let lsp_log_item = cx.new(|_| language_tools::LspLogToolbarItemView::new());
            toolbar.add_item(lsp_log_item, window, cx);
            let syntax_tree_item = cx.new(|_| language_tools::SyntaxTreeToolbarItemView::new());
            toolbar.add_item(syntax_tree_item, window, cx);
            let migration_banner = cx.new(|cx| MigrationBanner::new(workspace, cx));
            toolbar.add_item(migration_banner, window, cx);
            let project_diff_toolbar = cx.new(|cx| ProjectDiffToolbar::new(workspace, cx));
            toolbar.add_item(project_diff_toolbar, window, cx);
        })
    });
}

fn about(
    _: &mut Workspace,
    _: &zed_actions::About,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let release_channel = ReleaseChannel::global(cx).display_name();
    let version = env!("CARGO_PKG_VERSION");
    let debug = "";
    let message = format!("{release_channel} {version} {debug}");
    let detail = AppCommitSha::try_global(cx).map(|sha| sha.full());

    let prompt = window.prompt(PromptLevel::Info, &message, detail.as_deref(), &["OK"], cx);
    cx.foreground_executor()
        .spawn(async {
            prompt.await.ok();
        })
        .detach();
}

fn test_panic(_: &TestPanic, _: &mut App) {
    panic!("Ran the TestPanic action")
}

fn install_cli(
    _: &mut Workspace,
    _: &install_cli::Install,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    install_cli::install_cli(window, cx);
}

static WAITING_QUIT_CONFIRMATION: AtomicBool = AtomicBool::new(false);
fn quit(_: &Quit, cx: &mut App) {
    if WAITING_QUIT_CONFIRMATION.load(atomic::Ordering::Acquire) {
        return;
    }

    let should_confirm = WorkspaceSettings::get_global(cx).confirm_quit;
    cx.spawn(async move |cx| {
        let mut workspace_windows = cx.update(|cx| {
            cx.windows()
                .into_iter()
                .filter_map(|window| window.downcast::<Workspace>())
                .collect::<Vec<_>>()
        })?;

        // If multiple windows have unsaved changes, and need a save prompt,
        // prompt in the active window before switching to a different window.
        cx.update(|cx| {
            workspace_windows.sort_by_key(|window| window.is_active(cx) == Some(false));
        })
        .log_err();

        if should_confirm {
            if let Some(workspace) = workspace_windows.first() {
                let answer = workspace
                    .update(cx, |_, window, cx| {
                        window.prompt(
                            PromptLevel::Info,
                            "Are you sure you want to quit?",
                            None,
                            &["Quit", "Cancel"],
                            cx,
                        )
                    })
                    .log_err();

                if let Some(answer) = answer {
                    WAITING_QUIT_CONFIRMATION.store(true, atomic::Ordering::Release);
                    let answer = answer.await.ok();
                    WAITING_QUIT_CONFIRMATION.store(false, atomic::Ordering::Release);
                    if answer != Some(0) {
                        return Ok(());
                    }
                }
            }
        }

        // If the user cancels any save prompt, then keep the app open.
        for window in workspace_windows {
            if let Some(should_close) = window
                .update(cx, |workspace, window, cx| {
                    workspace.prepare_to_close(CloseIntent::Quit, window, cx)
                })
                .log_err()
            {
                if !should_close.await? {
                    return Ok(());
                }
            }
        }
        cx.update(|cx| cx.quit())?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn open_log_file(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    const MAX_LINES: usize = 1000;
    workspace
        .with_local_workspace(window, cx, move |workspace, window, cx| {
            let fs = workspace.app_state().fs.clone();
            cx.spawn_in(window, async move |workspace, cx| {
                let (old_log, new_log) =
                    futures::join!(fs.load(paths::old_log_file()), fs.load(paths::log_file()));
                let log = match (old_log, new_log) {
                    (Err(_), Err(_)) => None,
                    (old_log, new_log) => {
                        let mut lines = VecDeque::with_capacity(MAX_LINES);
                        for line in old_log
                            .iter()
                            .flat_map(|log| log.lines())
                            .chain(new_log.iter().flat_map(|log| log.lines()))
                        {
                            if lines.len() == MAX_LINES {
                                lines.pop_front();
                            }
                            lines.push_back(line);
                        }
                        Some(
                            lines
                                .into_iter()
                                .flat_map(|line| [line, "\n"])
                                .collect::<String>(),
                        )
                    }
                };

                workspace
                    .update_in(cx, |workspace, window, cx| {
                        let Some(log) = log else {
                            struct OpenLogError;

                            workspace.show_notification(
                                NotificationId::unique::<OpenLogError>(),
                                cx,
                                |cx| {
                                    cx.new(|cx| {
                                        MessageNotification::new(
                                            format!(
                                                "Unable to access/open log file at path {:?}",
                                                paths::log_file().as_path()
                                            ),
                                            cx,
                                        )
                                    })
                                },
                            );
                            return;
                        };
                        let project = workspace.project().clone();
                        let buffer = project.update(cx, |project, cx| {
                            project.create_local_buffer(&log, None, cx)
                        });

                        let buffer = cx
                            .new(|cx| MultiBuffer::singleton(buffer, cx).with_title("Log".into()));
                        let editor = cx.new(|cx| {
                            let mut editor =
                                Editor::for_multibuffer(buffer, Some(project), window, cx);
                            editor.set_read_only(true);
                            editor.set_breadcrumb_header(format!(
                                "Last {} lines in {}",
                                MAX_LINES,
                                paths::log_file().display()
                            ));
                            editor
                        });

                        editor.update(cx, |editor, cx| {
                            let last_multi_buffer_offset = editor.buffer().read(cx).len(cx);
                            editor.change_selections(Some(Autoscroll::fit()), window, cx, |s| {
                                s.select_ranges(Some(
                                    last_multi_buffer_offset..last_multi_buffer_offset,
                                ));
                            })
                        });

                        workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
                    })
                    .log_err();
            })
            .detach();
        })
        .detach();
}

pub fn handle_settings_file_changes(
    mut user_settings_file_rx: mpsc::UnboundedReceiver<String>,
    mut global_settings_file_rx: mpsc::UnboundedReceiver<String>,
    cx: &mut App,
    settings_changed: impl Fn(Option<anyhow::Error>, &mut App) + 'static,
) {
    MigrationNotification::set_global(cx.new(|_| MigrationNotification), cx);

    // Helper function to process settings content
    let process_settings =
        move |content: String, is_user: bool, store: &mut SettingsStore, cx: &mut App| -> bool {
            // Apply migrations to both user and global settings
            let (processed_content, content_migrated) =
                if let Ok(Some(migrated_content)) = migrate_settings(&content) {
                    (migrated_content, true)
                } else {
                    (content, false)
                };

            let result = if is_user {
                store.set_user_settings(&processed_content, cx)
            } else {
                store.set_global_settings(&processed_content, cx)
            };

            if let Err(err) = &result {
                let settings_type = if is_user { "user" } else { "global" };
                log::error!("Failed to load {} settings: {err}", settings_type);
            }

            settings_changed(result.err(), cx);

            content_migrated
        };

    // Initial load of both settings files
    let global_content = cx
        .background_executor()
        .block(global_settings_file_rx.next())
        .unwrap();
    let user_content = cx
        .background_executor()
        .block(user_settings_file_rx.next())
        .unwrap();

    SettingsStore::update_global(cx, |store, cx| {
        process_settings(global_content, false, store, cx);
        process_settings(user_content, true, store, cx);
    });

    // Watch for changes in both files
    cx.spawn(async move |cx| {
        let mut settings_streams = futures::stream::select(
            global_settings_file_rx.map(Either::Left),
            user_settings_file_rx.map(Either::Right),
        );

        while let Some(content) = settings_streams.next().await {
            let (content, is_user) = match content {
                Either::Left(content) => (content, false),
                Either::Right(content) => (content, true),
            };

            let result = cx.update_global(|store: &mut SettingsStore, cx| {
                let migrating_in_memory = process_settings(content, is_user, store, cx);
                if let Some(notifier) = MigrationNotification::try_global(cx) {
                    notifier.update(cx, |_, cx| {
                        cx.emit(MigrationEvent::ContentChanged {
                            migration_type: MigrationType::Settings,
                            migrating_in_memory,
                        });
                    });
                }
                cx.refresh_windows();
            });

            if result.is_err() {
                break; // App dropped
            }
        }
    })
    .detach();
}

pub fn handle_keymap_file_changes(
    mut user_keymap_file_rx: mpsc::UnboundedReceiver<String>,
    cx: &mut App,
) {
    let (keyboard_layout_tx, mut keyboard_layout_rx) = mpsc::unbounded();

    let mut current_mapping = settings::get_key_equivalents(cx.keyboard_layout().id());
    cx.on_keyboard_layout_change(move |cx| {
        let next_mapping = settings::get_key_equivalents(cx.keyboard_layout().id());
        if next_mapping != current_mapping {
            current_mapping = next_mapping;
            keyboard_layout_tx.unbounded_send(()).ok();
        }
    })
    .detach();

    struct KeymapParseErrorNotification;
    let notification_id = NotificationId::unique::<KeymapParseErrorNotification>();

    cx.spawn(async move |cx| {
        let mut user_keymap_content = String::new();
        let mut migrating_in_memory = false;
        loop {
            select_biased! {
                _ = keyboard_layout_rx.next() => {},
                content = user_keymap_file_rx.next() => {
                    if let Some(content) = content {
                        if let Ok(Some(migrated_content)) = migrate_keymap(&content) {
                            user_keymap_content = migrated_content;
                            migrating_in_memory = true;
                        } else {
                            user_keymap_content = content;
                            migrating_in_memory = false;
                        }
                    }
                }
            };
            cx.update(|cx| {
                if let Some(notifier) = MigrationNotification::try_global(cx) {
                    notifier.update(cx, |_, cx| {
                        cx.emit(MigrationEvent::ContentChanged {
                            migration_type: MigrationType::Keymap,
                            migrating_in_memory,
                        });
                    });
                }
                let load_result = KeymapFile::load(&user_keymap_content, cx);
                match load_result {
                    KeymapFileLoadResult::Success { key_bindings } => {
                        reload_keymaps(cx, key_bindings);
                        dismiss_app_notification(&notification_id.clone(), cx);
                    }
                    KeymapFileLoadResult::SomeFailedToLoad {
                        key_bindings,
                        error_message,
                    } => {
                        if !key_bindings.is_empty() {
                            reload_keymaps(cx, key_bindings);
                        }
                        show_keymap_file_load_error(notification_id.clone(), error_message, cx);
                    }
                    KeymapFileLoadResult::JsonParseFailure { error } => {
                        show_keymap_file_json_error(notification_id.clone(), &error, cx)
                    }
                }
            })
            .ok();
        }
    })
    .detach();
}

fn show_keymap_file_json_error(
    notification_id: NotificationId,
    error: &anyhow::Error,
    cx: &mut App,
) {
    let message: SharedString =
        format!("JSON parse error in keymap file. Bindings not reloaded.\n\n{error}").into();
    show_app_notification(notification_id, cx, move |cx| {
        cx.new(|cx| {
            MessageNotification::new(message.clone(), cx)
                .primary_message("Open Keymap File")
                .primary_on_click(|window, cx| {
                    window.dispatch_action(zed_actions::OpenKeymap.boxed_clone(), cx);
                    cx.emit(DismissEvent);
                })
        })
    });
}

fn show_keymap_file_load_error(
    notification_id: NotificationId,
    error_message: MarkdownString,
    cx: &mut App,
) {
    show_markdown_app_notification(
        notification_id.clone(),
        error_message,
        "Open Keymap File".into(),
        |window, cx| {
            window.dispatch_action(zed_actions::OpenKeymap.boxed_clone(), cx);
            cx.emit(DismissEvent);
        },
        cx,
    )
}

fn show_markdown_app_notification<F>(
    notification_id: NotificationId,
    message: MarkdownString,
    primary_button_message: SharedString,
    primary_button_on_click: F,
    cx: &mut App,
) where
    F: 'static + Send + Sync + Fn(&mut Window, &mut Context<MessageNotification>),
{
    let parsed_markdown = cx.background_spawn(async move {
        let file_location_directory = None;
        let language_registry = None;
        markdown_preview::markdown_parser::parse_markdown(
            &message.0,
            file_location_directory,
            language_registry,
        )
        .await
    });

    cx.spawn(async move |cx| {
        let parsed_markdown = Arc::new(parsed_markdown.await);
        let primary_button_message = primary_button_message.clone();
        let primary_button_on_click = Arc::new(primary_button_on_click);
        cx.update(|cx| {
            show_app_notification(notification_id, cx, move |cx| {
                let workspace_handle = cx.entity().downgrade();
                let parsed_markdown = parsed_markdown.clone();
                let primary_button_message = primary_button_message.clone();
                let primary_button_on_click = primary_button_on_click.clone();
                cx.new(move |cx| {
                    MessageNotification::new_from_builder(cx, move |window, cx| {
                        image_cache(retain_all("notification-cache"))
                            .text_xs()
                            .child(markdown_preview::markdown_renderer::render_parsed_markdown(
                                &parsed_markdown.clone(),
                                Some(workspace_handle.clone()),
                                window,
                                cx,
                            ))
                            .into_any()
                    })
                    .primary_message(primary_button_message)
                    .primary_on_click_arc(primary_button_on_click)
                })
            })
        })
        .ok();
    })
    .detach();
}

fn reload_keymaps(cx: &mut App, user_key_bindings: Vec<KeyBinding>) {
    cx.clear_key_bindings();
    cx.bind_keys(user_key_bindings);
    cx.set_menus(app_menus());
    cx.set_dock_menu(vec![gpui::MenuItem::action(
        "New Window",
        workspace::NewWindow,
    )]);
}

pub fn handle_settings_changed(error: Option<anyhow::Error>, cx: &mut App) {
    struct SettingsParseErrorNotification;
    let id = NotificationId::unique::<SettingsParseErrorNotification>();

    match error {
        Some(error) => {
            if let Some(InvalidSettingsError::LocalSettings { .. }) =
                error.downcast_ref::<InvalidSettingsError>()
            {
                // Local settings errors are displayed by the projects
                return;
            }
            show_app_notification(id, cx, move |cx| {
                cx.new(|cx| {
                    MessageNotification::new(format!("Invalid user settings file\n{error}"), cx)
                        .primary_message("Open Settings File")
                        .primary_icon(IconName::Settings)
                        .primary_on_click(|window, cx| {
                            window.dispatch_action(zed_actions::OpenSettings.boxed_clone(), cx);
                            cx.emit(DismissEvent);
                        })
                })
            });
        }
        None => {
            dismiss_app_notification(&id, cx);
        }
    }
}

pub fn open_new_ssh_project_from_project(
    workspace: &mut Workspace,
    paths: Vec<PathBuf>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<anyhow::Result<()>> {
    let app_state = workspace.app_state().clone();
    let Some(ssh_client) = workspace.project().read(cx).ssh_client() else {
        return Task::ready(Err(anyhow::anyhow!("Not an ssh project")));
    };
    let connection_options = ssh_client.read(cx).connection_options();
    cx.spawn_in(window, async move |_, cx| {
        open_ssh_project(
            connection_options,
            paths,
            app_state,
            workspace::OpenOptions {
                open_new_workspace: Some(true),
                ..Default::default()
            },
            cx,
        )
        .await
    })
}

fn open_project_settings_file(
    workspace: &mut Workspace,
    _: &OpenProjectSettings,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open_local_file(
        workspace,
        local_settings_file_relative_path(),
        initial_project_settings_content(),
        window,
        cx,
    )
}

fn open_project_tasks_file(
    workspace: &mut Workspace,
    _: &OpenProjectTasks,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open_local_file(
        workspace,
        local_tasks_file_relative_path(),
        initial_tasks_content(),
        window,
        cx,
    )
}

fn open_local_file(
    workspace: &mut Workspace,
    settings_relative_path: &'static Path,
    initial_contents: Cow<'static, str>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let worktree = project
        .read(cx)
        .visible_worktrees(cx)
        .find_map(|tree| tree.read(cx).root_entry()?.is_dir().then_some(tree));
    if let Some(worktree) = worktree {
        let tree_id = worktree.read(cx).id();
        cx.spawn_in(window, async move |workspace, cx| {
            // Check if the file actually exists on disk (even if it's excluded from worktree)
            let file_exists = {
                let full_path = worktree
                    .read_with(cx, |tree, _| tree.abs_path().join(settings_relative_path))?;

                let fs = project.read_with(cx, |project, _| project.fs().clone())?;
                let file_exists = fs
                    .metadata(&full_path)
                    .await
                    .ok()
                    .flatten()
                    .map_or(false, |metadata| !metadata.is_dir && !metadata.is_fifo);
                file_exists
            };

            if !file_exists {
                if let Some(dir_path) = settings_relative_path.parent() {
                    if worktree.read_with(cx, |tree, _| tree.entry_for_path(dir_path).is_none())? {
                        project
                            .update(cx, |project, cx| {
                                project.create_entry((tree_id, dir_path), true, cx)
                            })?
                            .await
                            .context("worktree was removed")?;
                    }
                }

                if worktree.read_with(cx, |tree, _| {
                    tree.entry_for_path(settings_relative_path).is_none()
                })? {
                    project
                        .update(cx, |project, cx| {
                            project.create_entry((tree_id, settings_relative_path), false, cx)
                        })?
                        .await
                        .context("worktree was removed")?;
                }
            }

            let editor = workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_path((tree_id, settings_relative_path), None, true, window, cx)
                })?
                .await?
                .downcast::<Editor>()
                .context("unexpected item type: expected editor item")?;

            editor
                .downgrade()
                .update(cx, |editor, cx| {
                    if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
                        if buffer.read(cx).is_empty() {
                            buffer.update(cx, |buffer, cx| {
                                buffer.edit([(0..0, initial_contents)], None, cx)
                            });
                        }
                    }
                })
                .ok();

            anyhow::Ok(())
        })
        .detach();
    } else {
        struct NoOpenFolders;

        workspace.show_notification(NotificationId::unique::<NoOpenFolders>(), cx, |cx| {
            cx.new(|cx| MessageNotification::new("This project has no folders open.", cx))
        })
    }
}

fn open_telemetry_log_file(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.with_local_workspace(window, cx, move |workspace, window, cx| {
        let app_state = workspace.app_state().clone();
        cx.spawn_in(window, async move |workspace, cx| {
            async fn fetch_log_string(app_state: &Arc<AppState>) -> Option<String> {
                let path = client::telemetry::Telemetry::log_file_path();
                app_state.fs.load(&path).await.log_err()
            }

            let log = fetch_log_string(&app_state).await.unwrap_or_else(|| "// No data has been collected yet".to_string());

            const MAX_TELEMETRY_LOG_LEN: usize = 5 * 1024 * 1024;
            let mut start_offset = log.len().saturating_sub(MAX_TELEMETRY_LOG_LEN);
            if let Some(newline_offset) = log[start_offset..].find('\n') {
                start_offset += newline_offset + 1;
            }
            let log_suffix = &log[start_offset..];
            let header = concat!(
                "// Zed collects anonymous usage data to help us understand how people are using the app.\n",
                "// Telemetry can be disabled via the `settings.json` file.\n",
                "// Here is the data that has been reported for the current session:\n",
            );
            let content = format!("{}\n{}", header, log_suffix);
            let json = app_state.languages.language_for_name("JSON").await.log_err();

            workspace.update_in( cx, |workspace, window, cx| {
                let project = workspace.project().clone();
                let buffer = project.update(cx, |project, cx| project.create_local_buffer(&content, json, cx));
                let buffer = cx.new(|cx| {
                    MultiBuffer::singleton(buffer, cx).with_title("Telemetry Log".into())
                });
                workspace.add_item_to_active_pane(
                    Box::new(cx.new(|cx| {
                        let mut editor = Editor::for_multibuffer(buffer, Some(project), window, cx);
                        editor.set_read_only(true);
                        editor.set_breadcrumb_header("Telemetry Log".into());
                        editor
                    })),
                    None,
                    true,
                    window, cx,
                );
            }).log_err()?;

            Some(())
        })
        .detach();
    }).detach();
}

fn open_bundled_file(
    workspace: &Workspace,
    text: Cow<'static, str>,
    title: &'static str,
    language: &'static str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let language = workspace.app_state().languages.language_for_name(language);
    cx.spawn_in(window, async move |workspace, cx| {
        let language = language.await.log_err();
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.with_local_workspace(window, cx, |workspace, window, cx| {
                    let project = workspace.project();
                    let buffer = project.update(cx, move |project, cx| {
                        project.create_local_buffer(text.as_ref(), language, cx)
                    });
                    let buffer =
                        cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title.into()));
                    workspace.add_item_to_active_pane(
                        Box::new(cx.new(|cx| {
                            let mut editor =
                                Editor::for_multibuffer(buffer, Some(project.clone()), window, cx);
                            editor.set_read_only(true);
                            editor.set_breadcrumb_header(title.into());
                            editor
                        })),
                        None,
                        true,
                        window,
                        cx,
                    );
                })
            })?
            .await
    })
    .detach_and_log_err(cx);
}

fn open_settings_file(
    abs_path: &'static Path,
    default_content: impl FnOnce() -> Rope + Send + 'static,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    cx.spawn_in(window, async move |workspace, cx| {
        let (worktree_creation_task, settings_open_task) = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.with_local_workspace(window, cx, move |workspace, window, cx| {
                    let worktree_creation_task = workspace.project().update(cx, |project, cx| {
                        // Set up a dedicated worktree for settings, since
                        // otherwise we're dropping and re-starting LSP servers
                        // for each file inside on every settings file
                        // close/open

                        // TODO: Do note that all other external files (e.g.
                        // drag and drop from OS) still have their worktrees
                        // released on file close, causing LSP servers'
                        // restarts.
                        project.find_or_create_worktree(paths::config_dir().as_path(), false, cx)
                    });
                    let settings_open_task =
                        create_and_open_local_file(abs_path, window, cx, default_content);
                    (worktree_creation_task, settings_open_task)
                })
            })?
            .await?;
        let _ = worktree_creation_task.await?;
        let _ = settings_open_task.await?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

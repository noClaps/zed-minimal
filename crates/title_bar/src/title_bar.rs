mod application_menu;
mod platforms;
mod title_bar_settings;

#[cfg(feature = "stories")]
mod stories;

use crate::application_menu::ApplicationMenu;

use crate::platforms::platform_mac;
use auto_update::AutoUpdateStatus;
use client::{Client, UserStore};
use gpui::{
    Action, AnyElement, App, Context, Corner, Decorations, Element, Entity, InteractiveElement,
    Interactivity, IntoElement, MouseButton, ParentElement, Render, Stateful,
    StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window, actions, div, px,
};
use project::Project;
use rpc::proto;
use settings::Settings as _;
use smallvec::SmallVec;
use std::sync::Arc;
use theme::ActiveTheme;
use title_bar_settings::TitleBarSettings;
use ui::{
    Avatar, Button, ButtonLike, ButtonStyle, ContextMenu, Icon, IconName, IconSize, PopoverMenu,
    Tooltip, h_flex, prelude::*,
};
use workspace::Workspace;
use zed_actions::OpenRecent;

#[cfg(feature = "stories")]
pub use stories::*;

const MAX_PROJECT_NAME_LENGTH: usize = 40;
const MAX_BRANCH_NAME_LENGTH: usize = 40;
const MAX_SHORT_SHA_LENGTH: usize = 8;

actions!(collab, [ToggleUserMenu, ToggleProjectMenu, SwitchBranch]);

pub fn init(cx: &mut App) {
    TitleBarSettings::register(cx);

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let item = cx.new(|cx| TitleBar::new("title-bar", workspace, window, cx));
        workspace.set_titlebar_item(item.into(), window, cx);
    })
    .detach();
}

pub struct TitleBar {
    platform_style: PlatformStyle,
    content: Stateful<Div>,
    children: SmallVec<[AnyElement; 2]>,
    project: Entity<Project>,
    user_store: Entity<UserStore>,
    client: Arc<Client>,
    workspace: WeakEntity<Workspace>,
    application_menu: Option<Entity<ApplicationMenu>>,
    _subscriptions: Vec<Subscription>,
}

impl Render for TitleBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title_bar_settings = *TitleBarSettings::get_global(cx);
        let height = Self::height(window);
        let decorations = window.window_decorations();
        let titlebar_color = cx.theme().colors().title_bar_background;

        h_flex()
            .id("titlebar")
            .w_full()
            .h(height)
            .map(|this| {
                if window.is_fullscreen() {
                    this.pl_2()
                } else if self.platform_style == PlatformStyle::Mac {
                    this.pl(px(platform_mac::TRAFFIC_LIGHT_PADDING))
                } else {
                    this.pl_2()
                }
            })
            .map(|el| match decorations {
                Decorations::Server => el,
                Decorations::Client { tiling, .. } => el
                    .when(!(tiling.top || tiling.right), |el| {
                        el.rounded_tr(theme::CLIENT_SIDE_DECORATION_ROUNDING)
                    })
                    .when(!(tiling.top || tiling.left), |el| {
                        el.rounded_tl(theme::CLIENT_SIDE_DECORATION_ROUNDING)
                    })
                    // this border is to avoid a transparent gap in the rounded corners
                    .mt(px(-1.))
                    .border(px(1.))
                    .border_color(titlebar_color),
            })
            .bg(titlebar_color)
            .content_stretch()
            .child(
                div()
                    .id("titlebar-content")
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .when(self.platform_style == PlatformStyle::Mac, |this| {
                        this.on_click(|event, window, _| {
                            if event.up.click_count == 2 {
                                window.titlebar_double_click();
                            }
                        })
                    })
                    .child(
                        h_flex()
                            .gap_1()
                            .map(|title_bar| {
                                let mut render_project_items = title_bar_settings.show_branch_name
                                    || title_bar_settings.show_project_items;
                                title_bar
                                    .when_some(self.application_menu.clone(), |title_bar, menu| {
                                        render_project_items &= !menu.read(cx).all_menus_shown();
                                        title_bar.child(menu)
                                    })
                                    .when(render_project_items, |title_bar| {
                                        title_bar
                                            .when(
                                                title_bar_settings.show_project_items,
                                                |title_bar| {
                                                    title_bar.child(self.render_project_name(cx))
                                                },
                                            )
                                            .when(
                                                title_bar_settings.show_branch_name,
                                                |title_bar| {
                                                    title_bar
                                                        .children(self.render_project_branch(cx))
                                                },
                                            )
                                    })
                            })
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .pr_1()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .map(|el| {
                                let status = self.client.status();
                                let status = &*status.borrow();
                                if matches!(status, client::Status::Connected { .. }) {
                                    el.child(self.render_user_menu_button(cx))
                                } else {
                                    el.children(self.render_connection_status(status, cx))
                                        .child(self.render_user_menu_button(cx))
                                }
                            }),
                    ),
            )
            .when(!window.is_fullscreen(), |title_bar| {
                match self.platform_style {
                    PlatformStyle::Mac => title_bar,
                }
            })
    }
}

impl TitleBar {
    pub fn new(
        id: impl Into<ElementId>,
        workspace: &Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = workspace.project().clone();
        let user_store = workspace.app_state().user_store.clone();
        let client = workspace.app_state().client.clone();

        let platform_style = PlatformStyle::platform();
        let application_menu = match platform_style {
            PlatformStyle::Mac => {
                if option_env!("ZED_USE_CROSS_PLATFORM_MENU").is_some() {
                    Some(cx.new(|cx| ApplicationMenu::new(window, cx)))
                } else {
                    None
                }
            }
        };

        let mut subscriptions = Vec::new();
        subscriptions.push(
            cx.observe(&workspace.weak_handle().upgrade().unwrap(), |_, _, cx| {
                cx.notify()
            }),
        );
        subscriptions.push(cx.subscribe(&project, |_, _, _: &project::Event, cx| cx.notify()));
        subscriptions.push(cx.observe_window_activation(window, Self::window_activation_changed));
        subscriptions.push(cx.observe(&user_store, |_, _, cx| cx.notify()));

        Self {
            platform_style,
            content: div().id(id.into()),
            children: SmallVec::new(),
            application_menu,
            workspace: workspace.weak_handle(),
            project,
            user_store,
            client,
            _subscriptions: subscriptions,
        }
    }

    pub fn height(window: &mut Window) -> Pixels {
        (1.75 * window.rem_size()).max(px(34.))
    }

    /// Sets the platform style.
    pub fn platform_style(mut self, style: PlatformStyle) -> Self {
        self.platform_style = style;
        self
    }

    pub fn render_project_name(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let name = {
            let mut names = self.project.read(cx).visible_worktrees(cx).map(|worktree| {
                let worktree = worktree.read(cx);
                worktree.root_name()
            });

            names.next()
        };
        let is_project_selected = name.is_some();
        let name = if let Some(name) = name {
            util::truncate_and_trailoff(name, MAX_PROJECT_NAME_LENGTH)
        } else {
            "Open recent project".to_string()
        };

        Button::new("project_name_trigger", name)
            .when(!is_project_selected, |b| b.color(Color::Muted))
            .style(ButtonStyle::Subtle)
            .label_size(LabelSize::Small)
            .tooltip(move |window, cx| {
                Tooltip::for_action(
                    "Recent Projects",
                    &zed_actions::OpenRecent {
                        create_new_window: false,
                    },
                    window,
                    cx,
                )
            })
            .on_click(cx.listener(move |_, _, window, cx| {
                window.dispatch_action(
                    OpenRecent {
                        create_new_window: false,
                    }
                    .boxed_clone(),
                    cx,
                );
            }))
    }

    pub fn render_project_branch(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let repository = self.project.read(cx).active_repository(cx)?;
        let workspace = self.workspace.upgrade()?;
        let branch_name = {
            let repo = repository.read(cx);
            repo.branch
                .as_ref()
                .map(|branch| branch.name())
                .map(|name| util::truncate_and_trailoff(&name, MAX_BRANCH_NAME_LENGTH))
                .or_else(|| {
                    repo.head_commit.as_ref().map(|commit| {
                        commit
                            .sha
                            .chars()
                            .take(MAX_SHORT_SHA_LENGTH)
                            .collect::<String>()
                    })
                })
        }?;

        Some(
            Button::new("project_branch_trigger", branch_name)
                .color(Color::Muted)
                .style(ButtonStyle::Subtle)
                .label_size(LabelSize::Small)
                .tooltip(move |window, cx| {
                    Tooltip::with_meta(
                        "Recent Branches",
                        Some(&zed_actions::git::Branch),
                        "Local branches only",
                        window,
                        cx,
                    )
                })
                .on_click(move |_, window, cx| {
                    let _ = workspace.update(cx, |_this, cx| {
                        window.dispatch_action(zed_actions::git::Branch.boxed_clone(), cx);
                    });
                })
                .when(
                    TitleBarSettings::get_global(cx).show_branch_icon,
                    |branch_button| {
                        branch_button
                            .icon(IconName::GitBranch)
                            .icon_position(IconPosition::Start)
                            .icon_color(Color::Muted)
                    },
                ),
        )
    }

    fn window_activation_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.update_active_view_for_followers(window, cx);
            })
            .ok();
    }

    fn render_connection_status(
        &self,
        status: &client::Status,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        match status {
            client::Status::ConnectionError
            | client::Status::ConnectionLost
            | client::Status::Reauthenticating { .. }
            | client::Status::Reconnecting { .. }
            | client::Status::ReconnectionError { .. } => Some(
                div()
                    .id("disconnected")
                    .child(Icon::new(IconName::Disconnected).size(IconSize::Small))
                    .tooltip(Tooltip::text("Disconnected"))
                    .into_any_element(),
            ),
            client::Status::UpgradeRequired => {
                let auto_updater = auto_update::AutoUpdater::get(cx);
                let label = match auto_updater.map(|auto_update| auto_update.read(cx).status()) {
                    Some(AutoUpdateStatus::Updated { .. }) => "Please restart Zed to Collaborate",
                    Some(AutoUpdateStatus::Installing { .. })
                    | Some(AutoUpdateStatus::Downloading { .. })
                    | Some(AutoUpdateStatus::Checking) => "Updating...",
                    Some(AutoUpdateStatus::Idle) | Some(AutoUpdateStatus::Errored) | None => {
                        "Please update Zed to Collaborate"
                    }
                };

                Some(
                    Button::new("connection-status", label)
                        .label_size(LabelSize::Small)
                        .on_click(|_, window, cx| {
                            if let Some(auto_updater) = auto_update::AutoUpdater::get(cx) {
                                if auto_updater.read(cx).status().is_updated() {
                                    workspace::reload(&Default::default(), cx);
                                    return;
                                }
                            }
                            auto_update::check(&Default::default(), window, cx);
                        })
                        .into_any_element(),
                )
            }
            _ => None,
        }
    }

    pub fn render_user_menu_button(&mut self, cx: &mut Context<Self>) -> impl Element {
        let user_store = self.user_store.read(cx);
        if let Some(user) = user_store.current_user() {
            let has_subscription_period = self.user_store.read(cx).subscription_period().is_some();
            let plan = self.user_store.read(cx).current_plan().filter(|_| {
                // Since the user might be on the legacy free plan we filter based on whether we have a subscription period.
                has_subscription_period
            });
            PopoverMenu::new("user-menu")
                .anchor(Corner::TopRight)
                .menu(move |window, cx| {
                    ContextMenu::build(window, cx, |menu, _, _cx| {
                        menu.link(
                            format!(
                                "Current Plan: {}",
                                match plan {
                                    None => "None",
                                    Some(proto::Plan::Free) => "Zed Free",
                                    Some(proto::Plan::ZedPro) => "Zed Pro",
                                    Some(proto::Plan::ZedProTrial) => "Zed Pro (Trial)",
                                }
                            ),
                            zed_actions::OpenAccountSettings.boxed_clone(),
                        )
                        .separator()
                        .action("Settings", zed_actions::OpenSettings.boxed_clone())
                        .action("Key Bindings", Box::new(zed_actions::OpenKeymap))
                        .action(
                            "Themes…",
                            zed_actions::theme_selector::Toggle::default().boxed_clone(),
                        )
                        .action(
                            "Icon Themes…",
                            zed_actions::icon_theme_selector::Toggle::default().boxed_clone(),
                        )
                        .action(
                            "Extensions",
                            zed_actions::Extensions::default().boxed_clone(),
                        )
                        .separator()
                        .action("Sign Out", client::SignOut.boxed_clone())
                    })
                    .into()
                })
                .trigger_with_tooltip(
                    ButtonLike::new("user-menu")
                        .child(
                            h_flex()
                                .gap_0p5()
                                .children(
                                    TitleBarSettings::get_global(cx)
                                        .show_user_picture
                                        .then(|| Avatar::new(user.avatar_uri.clone())),
                                )
                                .child(
                                    Icon::new(IconName::ChevronDown)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                        .style(ButtonStyle::Subtle),
                    Tooltip::text("Toggle User Menu"),
                )
                .anchor(gpui::Corner::TopRight)
        } else {
            PopoverMenu::new("user-menu")
                .anchor(Corner::TopRight)
                .menu(|window, cx| {
                    ContextMenu::build(window, cx, |menu, _, _| {
                        menu.action("Settings", zed_actions::OpenSettings.boxed_clone())
                            .action("Key Bindings", Box::new(zed_actions::OpenKeymap))
                            .action(
                                "Themes…",
                                zed_actions::theme_selector::Toggle::default().boxed_clone(),
                            )
                            .action(
                                "Icon Themes…",
                                zed_actions::icon_theme_selector::Toggle::default().boxed_clone(),
                            )
                            .action(
                                "Extensions",
                                zed_actions::Extensions::default().boxed_clone(),
                            )
                    })
                    .into()
                })
                .trigger_with_tooltip(
                    IconButton::new("user-menu", IconName::ChevronDown).icon_size(IconSize::Small),
                    Tooltip::text("Toggle User Menu"),
                )
        }
    }
}

impl InteractiveElement for TitleBar {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.content.interactivity()
    }
}

impl StatefulInteractiveElement for TitleBar {}

impl ParentElement for TitleBar {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

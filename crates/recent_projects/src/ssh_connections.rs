use std::collections::BTreeSet;
use std::{path::PathBuf, time::Duration};

use anyhow::Result;
use editor::Editor;
use futures::channel::oneshot;
use gpui::{
    Animation, AnimationExt, App, DismissEvent, Entity, EventEmitter, Focusable, FontFeatures,
    ParentElement as _, Render, SharedString, TextStyleRefinement, Transformation, percentage,
};

use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsSources};
use theme::ThemeSettings;
use ui::{
    ActiveTheme, Color, Context, Icon, IconName, IconSize, InteractiveElement, IntoElement, Label,
    LabelCommon, Styled, Window, prelude::*,
};
use util::serde::default_true;
use workspace::{ModalView, Workspace};

#[derive(Deserialize)]
pub struct SshSettings {
    pub ssh_connections: Option<Vec<SshConnection>>,
    /// Whether to read ~/.ssh/config for ssh connection sources.
    #[serde(default = "default_true")]
    pub read_ssh_config: bool,
}

impl SshSettings {
    pub fn ssh_connections(&self) -> impl Iterator<Item = SshConnection> + use<> {
        self.ssh_connections.clone().into_iter().flatten()
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct SshConnection {
    pub host: SharedString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub projects: BTreeSet<SshProject>,
    /// Name to use for this server in UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    // By default Zed will download the binary to the host directly.
    // If this is set to true, Zed will download the binary to your local machine,
    // and then upload it over the SSH connection. Useful if your SSH server has
    // limited outbound internet access.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_binary_over_ssh: Option<bool>,
}

#[derive(Clone, Default, Serialize, PartialEq, Eq, PartialOrd, Ord, Deserialize, JsonSchema)]
pub struct SshProject {
    pub paths: Vec<String>,
}

#[derive(Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RemoteSettingsContent {
    pub ssh_connections: Option<Vec<SshConnection>>,
    pub read_ssh_config: Option<bool>,
}

impl Settings for SshSettings {
    const KEY: Option<&'static str> = None;

    type FileContent = RemoteSettingsContent;

    fn load(sources: SettingsSources<Self::FileContent>, _: &mut App) -> Result<Self> {
        sources.json_merge()
    }
}

pub struct SshPrompt {
    connection_string: SharedString,
    nickname: Option<SharedString>,
    status_message: Option<SharedString>,
    prompt: Option<(Entity<Markdown>, oneshot::Sender<String>)>,
    cancellation: Option<oneshot::Sender<()>>,
    editor: Entity<Editor>,
}

impl Drop for SshPrompt {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancellation.take() {
            cancel.send(()).ok();
        }
    }
}

pub struct SshConnectionModal {
    pub(crate) prompt: Entity<SshPrompt>,
    paths: Vec<PathBuf>,
    finished: bool,
}

impl SshPrompt {
    pub fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, tx)) = self.prompt.take() {
            self.status_message = Some("Connecting".into());
            self.editor.update(cx, |editor, cx| {
                tx.send(editor.text(cx)).ok();
                editor.clear(window, cx);
            });
        }
    }
}

impl Render for SshPrompt {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeSettings::get_global(cx);

        let mut text_style = window.text_style();
        let refinement = TextStyleRefinement {
            font_family: Some(theme.buffer_font.family.clone()),
            font_features: Some(FontFeatures::disable_ligatures()),
            font_size: Some(theme.buffer_font_size(cx).into()),
            color: Some(cx.theme().colors().editor_foreground),
            background_color: Some(gpui::transparent_black()),
            ..Default::default()
        };

        text_style.refine(&refinement);
        let markdown_style = MarkdownStyle {
            base_text_style: text_style,
            selection_background_color: cx.theme().players().local().selection,
            ..Default::default()
        };

        v_flex()
            .key_context("PasswordPrompt")
            .py_2()
            .px_3()
            .size_full()
            .text_buffer(cx)
            .when_some(self.status_message.clone(), |el, status_message| {
                el.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::ArrowCircle)
                                .size(IconSize::Medium)
                                .with_animation(
                                    "arrow-circle",
                                    Animation::new(Duration::from_secs(2)).repeat(),
                                    |icon, delta| {
                                        icon.transform(Transformation::rotate(percentage(delta)))
                                    },
                                ),
                        )
                        .child(
                            div()
                                .text_ellipsis()
                                .overflow_x_hidden()
                                .child(format!("{}…", status_message)),
                        ),
                )
            })
            .when_some(self.prompt.as_ref(), |el, prompt| {
                el.child(
                    div()
                        .size_full()
                        .overflow_hidden()
                        .child(MarkdownElement::new(prompt.0.clone(), markdown_style))
                        .child(self.editor.clone()),
                )
            })
    }
}

impl SshConnectionModal {
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt
            .update(cx, |prompt, cx| prompt.confirm(window, cx))
    }

    pub fn finished(&mut self, cx: &mut Context<Self>) {
        self.finished = true;
        cx.emit(DismissEvent);
    }

    fn dismiss(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(tx) = self
            .prompt
            .update(cx, |prompt, _cx| prompt.cancellation.take())
        {
            tx.send(()).ok();
        }
        self.finished(cx);
    }
}

pub(crate) struct SshConnectionHeader {
    pub(crate) connection_string: SharedString,
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) nickname: Option<SharedString>,
}

impl RenderOnce for SshConnectionHeader {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();

        let mut header_color = theme.colors().text;
        header_color.fade_out(0.96);

        let (main_label, meta_label) = if let Some(nickname) = self.nickname {
            (nickname, Some(format!("({})", self.connection_string)))
        } else {
            (self.connection_string, None)
        };

        h_flex()
            .px(DynamicSpacing::Base12.rems(cx))
            .pt(DynamicSpacing::Base08.rems(cx))
            .pb(DynamicSpacing::Base04.rems(cx))
            .rounded_t_sm()
            .w_full()
            .gap_1p5()
            .child(Icon::new(IconName::Server).size(IconSize::XSmall))
            .child(
                h_flex()
                    .gap_1()
                    .overflow_x_hidden()
                    .child(
                        div()
                            .max_w_96()
                            .overflow_x_hidden()
                            .text_ellipsis()
                            .child(Headline::new(main_label).size(HeadlineSize::XSmall)),
                    )
                    .children(
                        meta_label.map(|label| {
                            Label::new(label).color(Color::Muted).size(LabelSize::Small)
                        }),
                    )
                    .child(div().overflow_x_hidden().text_ellipsis().children(
                        self.paths.into_iter().map(|path| {
                            Label::new(path.to_string_lossy().to_string())
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                        }),
                    )),
            )
    }
}

impl Render for SshConnectionModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl ui::IntoElement {
        let nickname = self.prompt.read(cx).nickname.clone();
        let connection_string = self.prompt.read(cx).connection_string.clone();

        let theme = cx.theme().clone();
        let body_color = theme.colors().editor_background;

        v_flex()
            .elevation_3(cx)
            .w(rems(34.))
            .border_1()
            .border_color(theme.colors().border)
            .key_context("SshConnectionModal")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::confirm))
            .child(
                SshConnectionHeader {
                    paths: self.paths.clone(),
                    connection_string,
                    nickname,
                }
                .render(window, cx),
            )
            .child(
                div()
                    .w_full()
                    .rounded_b_lg()
                    .bg(body_color)
                    .border_t_1()
                    .border_color(theme.colors().border_variant)
                    .child(self.prompt.clone()),
            )
    }
}

impl Focusable for SshConnectionModal {
    fn focus_handle(&self, cx: &gpui::App) -> gpui::FocusHandle {
        self.prompt.read(cx).editor.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for SshConnectionModal {}

impl ModalView for SshConnectionModal {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> workspace::DismissDecision {
        return workspace::DismissDecision::Dismiss(self.finished);
    }

    fn fade_out_background(&self) -> bool {
        true
    }
}

pub fn is_connecting_over_ssh(workspace: &Workspace, cx: &App) -> bool {
    workspace.active_modal::<SshConnectionModal>(cx).is_some()
}

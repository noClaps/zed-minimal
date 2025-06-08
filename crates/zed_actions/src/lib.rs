use gpui::{actions, impl_actions};
use schemars::JsonSchema;
use serde::Deserialize;

// If the zed binary doesn't use anything in this crate, it will be optimized away
// and the actions won't initialize. So we just provide an empty initialization function
// to be called from main.
//
// These may provide relevant context:
// https://github.com/rust-lang/rust/issues/47384
// https://github.com/mmastrac/rust-ctor/issues/280
pub fn init() {}

#[derive(Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenBrowser {
    pub url: String,
}

#[derive(Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenZedUrl {
    pub url: String,
}

impl_actions!(zed, [OpenBrowser, OpenZedUrl]);

actions!(
    zed,
    [
        OpenSettings,
        OpenDefaultKeymap,
        OpenAccountSettings,
        OpenServerSettings,
        Quit,
        OpenKeymap,
        About,
        OpenDocs,
        OpenLicenses,
        OpenTelemetryLog,
    ]
);

#[derive(PartialEq, Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionCategoryFilter {
    Themes,
    IconThemes,
    Languages,
    Grammars,
    LanguageServers,
    IndexedDocsProviders,
    Snippets,
}

#[derive(PartialEq, Clone, Default, Debug, Deserialize, JsonSchema)]
pub struct Extensions {
    /// Filters the extensions page down to extensions that are in the specified category.
    #[serde(default)]
    pub category_filter: Option<ExtensionCategoryFilter>,
}

#[derive(PartialEq, Clone, Default, Debug, Deserialize, JsonSchema)]
pub struct DecreaseBufferFontSize {
    #[serde(default)]
    pub persist: bool,
}

#[derive(PartialEq, Clone, Default, Debug, Deserialize, JsonSchema)]
pub struct IncreaseBufferFontSize {
    #[serde(default)]
    pub persist: bool,
}

#[derive(PartialEq, Clone, Default, Debug, Deserialize, JsonSchema)]
pub struct ResetBufferFontSize {
    #[serde(default)]
    pub persist: bool,
}

#[derive(PartialEq, Clone, Default, Debug, Deserialize, JsonSchema)]
pub struct DecreaseUiFontSize {
    #[serde(default)]
    pub persist: bool,
}

#[derive(PartialEq, Clone, Default, Debug, Deserialize, JsonSchema)]
pub struct IncreaseUiFontSize {
    #[serde(default)]
    pub persist: bool,
}

#[derive(PartialEq, Clone, Default, Debug, Deserialize, JsonSchema)]
pub struct ResetUiFontSize {
    #[serde(default)]
    pub persist: bool,
}

impl_actions!(
    zed,
    [
        Extensions,
        DecreaseBufferFontSize,
        IncreaseBufferFontSize,
        ResetBufferFontSize,
        DecreaseUiFontSize,
        IncreaseUiFontSize,
        ResetUiFontSize,
    ]
);

pub mod dev {
    use gpui::actions;

    actions!(dev, [ToggleInspector]);
}

pub mod workspace {
    use gpui::action_with_deprecated_aliases;

    action_with_deprecated_aliases!(
        workspace,
        CopyPath,
        [
            "editor::CopyPath",
            "outline_panel::CopyPath",
            "project_panel::CopyPath"
        ]
    );

    action_with_deprecated_aliases!(
        workspace,
        CopyRelativePath,
        [
            "editor::CopyRelativePath",
            "outline_panel::CopyRelativePath",
            "project_panel::CopyRelativePath"
        ]
    );
}

pub mod git {
    use gpui::{action_with_deprecated_aliases, actions};

    actions!(git, [CheckoutBranch, Switch, SelectRepo]);
    action_with_deprecated_aliases!(git, Branch, ["branches::OpenRecent"]);
}

pub mod jj {
    use gpui::actions;

    actions!(jj, [BookmarkList]);
}

pub mod toast {
    use gpui::actions;

    actions!(toast, [RunAction]);
}

pub mod command_palette {
    use gpui::actions;

    actions!(command_palette, [Toggle]);
}

pub mod feedback {
    use gpui::actions;

    actions!(feedback, [FileBugReport, GiveFeedback]);
}

pub mod theme_selector {
    use gpui::impl_actions;
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(PartialEq, Clone, Default, Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Toggle {
        /// A list of theme names to filter the theme selector down to.
        pub themes_filter: Option<Vec<String>>,
    }

    impl_actions!(theme_selector, [Toggle]);
}

pub mod icon_theme_selector {
    use gpui::impl_actions;
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(PartialEq, Clone, Default, Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Toggle {
        /// A list of icon theme names to filter the theme selector down to.
        pub themes_filter: Option<Vec<String>>,
    }

    impl_actions!(icon_theme_selector, [Toggle]);
}

#[derive(PartialEq, Clone, Deserialize, Default, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenRecent {
    #[serde(default)]
    pub create_new_window: bool,
}

impl_actions!(projects, [OpenRecent]);

pub mod outline {
    use std::sync::OnceLock;

    use gpui::{AnyView, App, Window, action_as};

    action_as!(outline, ToggleOutline as Toggle);
    /// A pointer to outline::toggle function, exposed here to sewer the breadcrumbs <-> outline dependency.
    pub static TOGGLE_OUTLINE: OnceLock<fn(AnyView, &mut Window, &mut App)> = OnceLock::new();
}

actions!(git_onboarding, [OpenGitIntegrationOnboarding]);

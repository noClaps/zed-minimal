use crate::PlatformStyle;
use crate::{Icon, IconName, IconSize, h_flex, prelude::*};
use gpui::{
    Action, AnyElement, App, FocusHandle, IntoElement, Keystroke, Modifiers, Window, relative,
};
use itertools::Itertools;

#[derive(Debug, IntoElement, Clone, RegisterComponent)]
pub struct KeyBinding {
    /// A keybinding consists of a key and a set of modifier keys.
    /// More then one keybinding produces a chord.
    ///
    /// This should always contain at least one element.
    key_binding: gpui::KeyBinding,

    /// The [`PlatformStyle`] to use when displaying this keybinding.
    platform_style: PlatformStyle,
    size: Option<AbsoluteLength>,

    /// Indicates whether the keybinding is currently disabled.
    disabled: bool,
}

impl KeyBinding {
    /// Returns the highest precedence keybinding for an action. This is the last binding added to
    /// the keymap. User bindings are added after built-in bindings so that they take precedence.
    pub fn for_action(action: &dyn Action, window: &mut Window, cx: &App) -> Option<Self> {
        if let Some(focused) = window.focused(cx) {
            return Self::for_action_in(action, &focused, window, cx);
        }
        let key_binding = window.highest_precedence_binding_for_action(action)?;
        Some(Self::new(key_binding, cx))
    }

    /// Like `for_action`, but lets you specify the context from which keybindings are matched.
    pub fn for_action_in(
        action: &dyn Action,
        focus: &FocusHandle,
        window: &mut Window,
        cx: &App,
    ) -> Option<Self> {
        let key_binding = window.highest_precedence_binding_for_action_in(action, focus)?;
        Some(Self::new(key_binding, cx))
    }

    pub fn new(key_binding: gpui::KeyBinding, _: &App) -> Self {
        Self {
            key_binding,
            platform_style: PlatformStyle::platform(),
            size: None,
            disabled: false,
        }
    }

    /// Sets the [`PlatformStyle`] for this [`KeyBinding`].
    pub fn platform_style(mut self, platform_style: PlatformStyle) -> Self {
        self.platform_style = platform_style;
        self
    }

    /// Sets the size for this [`KeyBinding`].
    pub fn size(mut self, size: impl Into<AbsoluteLength>) -> Self {
        self.size = Some(size.into());
        self
    }

    /// Sets whether this keybinding is currently disabled.
    /// Disabled keybinds will be rendered in a dimmed state.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    fn render_key(&self, keystroke: &Keystroke, color: Option<Color>) -> AnyElement {
        let key_icon = icon_for_key(keystroke, self.platform_style);
        match key_icon {
            Some(icon) => KeyIcon::new(icon, color).size(self.size).into_any_element(),
            None => {
                let key = util::capitalize(&keystroke.key);
                Key::new(&key, color).size(self.size).into_any_element()
            }
        }
    }
}

impl RenderOnce for KeyBinding {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let color = self.disabled.then_some(Color::Disabled);
        let use_text = false;
        h_flex()
            .debug_selector(|| {
                format!(
                    "KEY_BINDING-{}",
                    self.key_binding
                        .keystrokes()
                        .iter()
                        .map(|k| k.key.to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            })
            .gap(DynamicSpacing::Base04.rems(cx))
            .flex_none()
            .children(self.key_binding.keystrokes().iter().map(|keystroke| {
                h_flex()
                    .flex_none()
                    .py_0p5()
                    .rounded_xs()
                    .text_color(cx.theme().colors().text_muted)
                    .when(use_text, |el| {
                        el.child(Key::new(keystroke_text(&keystroke), color).size(self.size))
                    })
                    .when(!use_text, |el| {
                        el.children(render_modifiers(
                            &keystroke.modifiers,
                            color,
                            self.size,
                            true,
                        ))
                        .map(|el| el.child(self.render_key(&keystroke, color)))
                    })
            }))
    }
}

fn icon_for_key(keystroke: &Keystroke, platform_style: PlatformStyle) -> Option<IconName> {
    match keystroke.key.as_str() {
        "left" => Some(IconName::ArrowLeft),
        "right" => Some(IconName::ArrowRight),
        "up" => Some(IconName::ArrowUp),
        "down" => Some(IconName::ArrowDown),
        "backspace" => Some(IconName::Backspace),
        "delete" => Some(IconName::Delete),
        "return" => Some(IconName::Return),
        "enter" => Some(IconName::Return),
        "tab" => Some(IconName::Tab),
        "space" => Some(IconName::Space),
        "escape" => Some(IconName::Escape),
        "pagedown" => Some(IconName::PageDown),
        "pageup" => Some(IconName::PageUp),
        "shift" if platform_style == PlatformStyle::Mac => Some(IconName::Shift),
        "control" if platform_style == PlatformStyle::Mac => Some(IconName::Control),
        "platform" if platform_style == PlatformStyle::Mac => Some(IconName::Command),
        "function" if platform_style == PlatformStyle::Mac => Some(IconName::Control),
        "alt" if platform_style == PlatformStyle::Mac => Some(IconName::Option),
        _ => None,
    }
}

pub fn render_modifiers(
    modifiers: &Modifiers,
    color: Option<Color>,
    size: Option<AbsoluteLength>,
    trailing_separator: bool,
) -> impl Iterator<Item = AnyElement> {
    #[derive(Clone)]
    enum KeyOrIcon {
        Icon(IconName),
    }

    struct Modifier {
        enabled: bool,
        mac: KeyOrIcon,
    }

    let table = {
        use KeyOrIcon::*;

        [
            Modifier {
                enabled: modifiers.function,
                mac: Icon(IconName::Control),
            },
            Modifier {
                enabled: modifiers.control,
                mac: Icon(IconName::Control),
            },
            Modifier {
                enabled: modifiers.alt,
                mac: Icon(IconName::Option),
            },
            Modifier {
                enabled: modifiers.platform,
                mac: Icon(IconName::Command),
            },
            Modifier {
                enabled: modifiers.shift,
                mac: Icon(IconName::Shift),
            },
        ]
    };

    let filtered = table
        .into_iter()
        .filter(|modifier| modifier.enabled)
        .collect::<Vec<_>>();

    let platform_keys = filtered.into_iter().map(move |modifier| Some(modifier.mac));

    let separator = None;

    let platform_keys = itertools::intersperse(platform_keys, separator.clone());

    platform_keys
        .chain(if modifiers.modified() && trailing_separator {
            Some(separator)
        } else {
            None
        })
        .flatten()
        .map(move |key_or_icon| match key_or_icon {
            KeyOrIcon::Icon(icon) => KeyIcon::new(icon, color).size(size).into_any_element(),
        })
}

#[derive(IntoElement)]
pub struct Key {
    key: SharedString,
    color: Option<Color>,
    size: Option<AbsoluteLength>,
}

impl RenderOnce for Key {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let single_char = self.key.len() == 1;
        let size = self
            .size
            .unwrap_or_else(|| TextSize::default().rems(cx).into());

        div()
            .py_0()
            .map(|this| {
                if single_char {
                    this.w(size).flex().flex_none().justify_center()
                } else {
                    this.px_0p5()
                }
            })
            .h(size)
            .text_size(size)
            .line_height(relative(1.))
            .text_color(self.color.unwrap_or(Color::Muted).color(cx))
            .child(self.key.clone())
    }
}

impl Key {
    pub fn new(key: impl Into<SharedString>, color: Option<Color>) -> Self {
        Self {
            key: key.into(),
            color,
            size: None,
        }
    }

    pub fn size(mut self, size: impl Into<Option<AbsoluteLength>>) -> Self {
        self.size = size.into();
        self
    }
}

#[derive(IntoElement)]
pub struct KeyIcon {
    icon: IconName,
    color: Option<Color>,
    size: Option<AbsoluteLength>,
}

impl RenderOnce for KeyIcon {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let size = self.size.unwrap_or(IconSize::Small.rems().into());

        Icon::new(self.icon)
            .size(IconSize::Custom(size.to_rems(window.rem_size())))
            .color(self.color.unwrap_or(Color::Muted))
    }
}

impl KeyIcon {
    pub fn new(icon: IconName, color: Option<Color>) -> Self {
        Self {
            icon,
            color,
            size: None,
        }
    }

    pub fn size(mut self, size: impl Into<Option<AbsoluteLength>>) -> Self {
        self.size = size.into();
        self
    }
}

/// Returns a textual representation of the key binding for the given [`Action`].
pub fn text_for_action(action: &dyn Action, window: &Window, cx: &App) -> Option<String> {
    let key_binding = window.highest_precedence_binding_for_action(action)?;
    Some(text_for_keystrokes(key_binding.keystrokes(), cx))
}

pub fn text_for_keystrokes(keystrokes: &[Keystroke], _: &App) -> String {
    keystrokes
        .iter()
        .map(|keystroke| keystroke_text(keystroke))
        .join(" ")
}

pub fn text_for_keystroke(keystroke: &Keystroke, _: &App) -> String {
    keystroke_text(keystroke)
}

/// Returns a textual representation of the given [`Keystroke`].
fn keystroke_text(keystroke: &Keystroke) -> String {
    let mut text = String::new();
    let delimiter = '-';

    if keystroke.modifiers.function {
        text.push(delimiter);
    }

    if keystroke.modifiers.control {
        text.push_str("Control");
        text.push(delimiter);
    }

    if keystroke.modifiers.platform {
        text.push_str("Command");
        text.push(delimiter);
    }

    if keystroke.modifiers.alt {
        text.push_str("Option");
        text.push(delimiter);
    }

    if keystroke.modifiers.shift {
        text.push_str("Shift");
        text.push(delimiter);
    }

    let key = match keystroke.key.as_str() {
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        key => &util::capitalize(key),
    };
    text.push_str(key);

    text
}

impl Component for KeyBinding {
    fn scope() -> ComponentScope {
        ComponentScope::Typography
    }

    fn name() -> &'static str {
        "KeyBinding"
    }

    fn description() -> Option<&'static str> {
        Some("A component that displays a key binding, supporting different platform styles.")
    }

    fn preview(_window: &mut Window, cx: &mut App) -> Option<AnyElement> {
        Some(
            v_flex()
                .gap_6()
                .children(vec![
                    example_group_with_title(
                        "Basic Usage",
                        vec![
                            single_example(
                                "Default",
                                KeyBinding::new(
                                    gpui::KeyBinding::new("ctrl-s", gpui::NoAction, None),
                                    cx,
                                )
                                .into_any_element(),
                            ),
                            single_example(
                                "Mac Style",
                                KeyBinding::new(
                                    gpui::KeyBinding::new("cmd-s", gpui::NoAction, None),
                                    cx,
                                )
                                .platform_style(PlatformStyle::Mac)
                                .into_any_element(),
                            ),
                        ],
                    ),
                    example_group_with_title(
                        "Complex Bindings",
                        vec![
                            single_example(
                                "Multiple Keys",
                                KeyBinding::new(
                                    gpui::KeyBinding::new("ctrl-k ctrl-b", gpui::NoAction, None),
                                    cx,
                                )
                                .into_any_element(),
                            ),
                            single_example(
                                "With Shift",
                                KeyBinding::new(
                                    gpui::KeyBinding::new("shift-cmd-p", gpui::NoAction, None),
                                    cx,
                                )
                                .into_any_element(),
                            ),
                        ],
                    ),
                ])
                .into_any_element(),
        )
    }
}

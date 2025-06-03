use gpui::Render;
use story::{Story, StoryItem, StorySection};

use crate::{IconButton, IconName};
use crate::{IconButtonShape, Tooltip, prelude::*};

pub struct IconButtonStory;

impl Render for IconButtonStory {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let default_button = StoryItem::new(
            "Default",
            IconButton::new("default_icon_button", IconName::Hash),
        )
        .description("Displays an icon button.")
        .usage(
            r#"
            IconButton::new("default_icon_button", Icon::Hash)
        "#,
        );

        let selected_button = StoryItem::new(
            "Selected",
            IconButton::new("selected_icon_button", IconName::Hash).toggle_state(true),
        )
        .description("Displays an icon button that is selected.")
        .usage(
            r#"
            IconButton::new("selected_icon_button", Icon::Hash).selected(true)
        "#,
        );

        let disabled_button = StoryItem::new(
            "Disabled",
            IconButton::new("disabled_icon_button", IconName::Hash).disabled(true),
        )
        .description("Displays an icon button that is disabled.")
        .usage(
            r#"
            IconButton::new("disabled_icon_button", Icon::Hash).disabled(true)
        "#,
        );

        let with_tooltip_button = StoryItem::new(
            "With `tooltip`",
            IconButton::new("with_tooltip_button", IconName::MessageBubbles)
                .tooltip(Tooltip::text("Open messages")),
        )
        .description("Displays an icon button that has a tooltip when hovered.")
        .usage(
            r#"
            IconButton::new("with_tooltip_button", Icon::MessageBubbles)
                .tooltip(Tooltip::text_f("Open messages"))
        "#,
        );

        let selected_with_tooltip_button = StoryItem::new(
            "Selected with `tooltip`",
            IconButton::new("selected_with_tooltip_button", IconName::InlayHint)
                .toggle_state(true)
                .tooltip(Tooltip::text("Toggle inlay hints")),
        )
        .description("Displays a selected icon button with tooltip.")
        .usage(
            r#"
            IconButton::new("selected_with_tooltip_button", Icon::InlayHint)
                .selected(true)
                .tooltip(Tooltip::text_f("Toggle inlay hints"))
        "#,
        );

        let buttons = vec![
            default_button,
            selected_button,
            disabled_button,
            with_tooltip_button,
            selected_with_tooltip_button,
        ];

        Story::container(cx)
            .child(Story::title_for::<IconButton>(cx))
            .child(StorySection::new().children(buttons))
            .child(
                StorySection::new().child(StoryItem::new(
                    "Square",
                    h_flex()
                        .gap_2()
                        .child(
                            IconButton::new("square-medium", IconName::Close)
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::Medium),
                        )
                        .child(
                            IconButton::new("square-small", IconName::Close)
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::Small),
                        )
                        .child(
                            IconButton::new("square-xsmall", IconName::Close)
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::XSmall),
                        )
                        .child(
                            IconButton::new("square-indicator", IconName::Close)
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::Indicator),
                        ),
                )),
            )
            .into_element()
    }
}

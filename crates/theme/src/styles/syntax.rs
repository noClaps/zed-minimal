#![allow(missing_docs)]

use std::sync::Arc;

use gpui::{HighlightStyle, Hsla};

#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct SyntaxTheme {
    pub highlights: Vec<(String, HighlightStyle)>,
}

impl SyntaxTheme {
    pub fn get(&self, name: &str) -> HighlightStyle {
        self.highlights
            .iter()
            .find_map(|entry| if entry.0 == name { Some(entry.1) } else { None })
            .unwrap_or_default()
    }

    pub fn color(&self, name: &str) -> Hsla {
        self.get(name).color.unwrap_or_default()
    }

    pub fn highlight_id(&self, name: &str) -> Option<u32> {
        let ix = self.highlights.iter().position(|entry| entry.0 == name)?;
        Some(ix as u32)
    }

    /// Returns a new [`Arc<SyntaxTheme>`] with the given syntax styles merged in.
    pub fn merge(base: Arc<Self>, user_syntax_styles: Vec<(String, HighlightStyle)>) -> Arc<Self> {
        if user_syntax_styles.is_empty() {
            return base;
        }

        let mut merged_highlights = base.highlights.clone();

        for (name, highlight) in user_syntax_styles {
            if let Some((_, existing_highlight)) = merged_highlights
                .iter_mut()
                .find(|(existing_name, _)| existing_name == &name)
            {
                existing_highlight.color = highlight.color.or(existing_highlight.color);
                existing_highlight.font_weight =
                    highlight.font_weight.or(existing_highlight.font_weight);
                existing_highlight.font_style =
                    highlight.font_style.or(existing_highlight.font_style);
                existing_highlight.background_color = highlight
                    .background_color
                    .or(existing_highlight.background_color);
                existing_highlight.underline = highlight.underline.or(existing_highlight.underline);
                existing_highlight.strikethrough =
                    highlight.strikethrough.or(existing_highlight.strikethrough);
                existing_highlight.fade_out = highlight.fade_out.or(existing_highlight.fade_out);
            } else {
                merged_highlights.push((name, highlight));
            }
        }

        Arc::new(Self {
            highlights: merged_highlights,
        })
    }
}

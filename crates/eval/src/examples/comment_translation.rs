use crate::example::{Example, ExampleMetadata};
use async_trait::async_trait;

pub struct CommentTranslation;

#[async_trait(?Send)]
impl Example for CommentTranslation {
    fn meta(&self) -> ExampleMetadata {
        ExampleMetadata {
            name: "comment_translation".to_string(),
            url: "https://github.com/servo/font-kit.git".to_string(),
            revision: "504d084e29bce4f60614bc702e91af7f7d9e60ad".to_string(),
            language_server: None,
        }
    }
}

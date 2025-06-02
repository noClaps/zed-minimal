use async_trait::async_trait;

use crate::example::{Example, ExampleMetadata};

pub struct Planets;

#[async_trait(?Send)]
impl Example for Planets {
    fn meta(&self) -> ExampleMetadata {
        ExampleMetadata {
            name: "planets".to_string(),
            url: "https://github.com/roc-lang/roc".to_string(), // This commit in this repo is just the Apache2 license,
            revision: "59e49c75214f60b4dc4a45092292061c8c26ce27".to_string(), // so effectively a blank project.
            language_server: None,
        }
    }
}

use async_trait::async_trait;

use crate::example::{Example, ExampleMetadata};

pub struct FileOverwriteExample;

/*
This eval tests a fix for a destructive behavior of the `edit_file` tool.
Previously, it would rewrite existing files too aggressively, which often
resulted in content loss.
*/

#[async_trait(?Send)]
impl Example for FileOverwriteExample {
    fn meta(&self) -> ExampleMetadata {
        ExampleMetadata {
            name: "file_overwrite".to_string(),
            url: "https://github.com/zed-industries/zed.git".to_string(),
            revision: "023a60806a8cc82e73bd8d88e63b4b07fc7a0040".to_string(),
            language_server: None,
        }
    }
}

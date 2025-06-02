use async_trait::async_trait;

use crate::example::{Example, ExampleMetadata, LanguageServer};

pub struct CodeBlockCitations;

#[async_trait(?Send)]
impl Example for CodeBlockCitations {
    fn meta(&self) -> ExampleMetadata {
        ExampleMetadata {
            name: "code_block_citations".to_string(),
            url: "https://github.com/zed-industries/zed.git".to_string(),
            revision: "f69aeb6311dde3c0b8979c293d019d66498d54f2".to_string(),
            language_server: Some(LanguageServer {
                file_extension: "rs".to_string(),
            }),
        }
    }
}

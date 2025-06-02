use async_trait::async_trait;

use crate::example::{Example, ExampleMetadata};

pub struct FileSearchExample;

#[async_trait(?Send)]
impl Example for FileSearchExample {
    fn meta(&self) -> ExampleMetadata {
        ExampleMetadata {
            name: "file_search".to_string(),
            url: "https://github.com/zed-industries/zed.git".to_string(),
            revision: "03ecb88fe30794873f191ddb728f597935b3101c".to_string(),
            language_server: None,
        }
    }
}

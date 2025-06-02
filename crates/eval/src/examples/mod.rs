use serde::Deserialize;
use std::collections::BTreeMap;
use std::rc::Rc;
use util::serde::default_true;

use crate::example::Example;

mod add_arg_to_trait_method;
mod code_block_citations;
mod comment_translation;
mod file_search;
mod overwrite_file;
mod planets;

pub fn all() -> Vec<Rc<dyn Example>> {
    let threads: Vec<Rc<dyn Example>> = vec![
        Rc::new(file_search::FileSearchExample),
        Rc::new(add_arg_to_trait_method::AddArgToTraitMethod),
        Rc::new(code_block_citations::CodeBlockCitations),
        Rc::new(planets::Planets),
        Rc::new(comment_translation::CommentTranslation),
        Rc::new(overwrite_file::FileOverwriteExample),
    ];

    threads
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExampleToml {
    pub url: String,
    pub revision: String,
    pub language_extension: Option<String>,
    pub insert_id: Option<String>,
    #[serde(default = "default_true")]
    pub require_lsp: bool,
    #[serde(default)]
    pub allow_preexisting_diagnostics: bool,
    pub prompt: String,
    #[serde(default)]
    pub profile_name: Option<String>,
    #[serde(default)]
    pub diff_assertions: BTreeMap<String, String>,
    #[serde(default)]
    pub thread_assertions: BTreeMap<String, String>,
    #[serde(default)]
    pub existing_thread_path: Option<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
}

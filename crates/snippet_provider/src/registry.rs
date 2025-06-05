use std::sync::Arc;

use collections::HashMap;
use gpui::{App, Global, ReadGlobal, UpdateGlobal};
use parking_lot::RwLock;

use crate::{Snippet, SnippetKind};

struct GlobalSnippetRegistry(Arc<SnippetRegistry>);

impl Global for GlobalSnippetRegistry {}

#[derive(Default)]
pub struct SnippetRegistry {
    snippets: RwLock<HashMap<SnippetKind, Vec<Arc<Snippet>>>>,
}

impl SnippetRegistry {
    pub fn global(cx: &App) -> Arc<Self> {
        GlobalSnippetRegistry::global(cx).0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Arc<Self>> {
        cx.try_global::<GlobalSnippetRegistry>()
            .map(|registry| registry.0.clone())
    }

    pub fn init_global(cx: &mut App) {
        GlobalSnippetRegistry::set_global(cx, GlobalSnippetRegistry(Arc::new(Self::new())))
    }

    pub fn new() -> Self {
        Self {
            snippets: RwLock::new(HashMap::default()),
        }
    }

    pub fn get_snippets(&self, kind: &SnippetKind) -> Vec<Arc<Snippet>> {
        self.snippets.read().get(kind).cloned().unwrap_or_default()
    }
}

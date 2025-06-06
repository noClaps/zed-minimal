pub mod format;
mod registry;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use collections::{BTreeMap, BTreeSet, HashMap};
use fs::Fs;
use futures::stream::StreamExt;
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, Task, WeakEntity};
pub use registry::*;

pub fn init(cx: &mut App) {
    SnippetRegistry::init_global(cx);
}

// Is `None` if the snippet file is global.
type SnippetKind = Option<String>;
fn file_stem_to_key(stem: &str) -> SnippetKind {
    if stem == "snippets" {
        None
    } else {
        Some(stem.to_owned())
    }
}

// Snippet with all of the metadata
#[derive(Debug)]
pub struct Snippet {
    pub prefix: Vec<String>,
    pub body: String,
    pub description: Option<String>,
    pub name: String,
}

async fn process_updates(
    this: WeakEntity<SnippetProvider>,
    entries: Vec<PathBuf>,
    mut cx: AsyncApp,
) -> Result<()> {
    let fs = this.read_with(&mut cx, |this, _| this.fs.clone())?;
    for entry_path in entries {
        if !entry_path
            .extension()
            .map_or(false, |extension| extension == "json")
        {
            continue;
        }
        let entry_metadata = fs.metadata(&entry_path).await;
        if entry_metadata.map_or(false, |entry| entry.map_or(false, |e| e.is_dir)) {
            // Don't process dirs.
            continue;
        }
        let Some(stem) = entry_path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let key = file_stem_to_key(stem);

        this.update(&mut cx, move |this, _| {
            let snippets_of_kind = this.snippets.entry(key).or_default();
            snippets_of_kind.remove(&entry_path);
        })?;
    }
    Ok(())
}

async fn initial_scan(
    this: WeakEntity<SnippetProvider>,
    path: Arc<Path>,
    mut cx: AsyncApp,
) -> Result<()> {
    let fs = this.read_with(&mut cx, |this, _| this.fs.clone())?;
    let entries = fs.read_dir(&path).await;
    if let Ok(entries) = entries {
        let entries = entries
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        process_updates(this, entries, cx).await?;
    }
    Ok(())
}

pub struct SnippetProvider {
    fs: Arc<dyn Fs>,
    snippets: HashMap<SnippetKind, BTreeMap<PathBuf, Vec<Arc<Snippet>>>>,
    watch_tasks: Vec<Task<Result<()>>>,
}

// Watches global snippet directory, is created just once and reused across multiple projects
struct GlobalSnippetWatcher(Entity<SnippetProvider>);

impl GlobalSnippetWatcher {
    fn new(fs: Arc<dyn Fs>, cx: &mut App) -> Self {
        let global_snippets_dir = paths::snippets_dir();
        let provider = cx.new(|_cx| SnippetProvider {
            fs,
            snippets: Default::default(),
            watch_tasks: vec![],
        });
        provider.update(cx, |this, cx| this.watch_directory(global_snippets_dir, cx));
        Self(provider)
    }
}

impl gpui::Global for GlobalSnippetWatcher {}

impl SnippetProvider {
    pub fn new(fs: Arc<dyn Fs>, dirs_to_watch: BTreeSet<PathBuf>, cx: &mut App) -> Entity<Self> {
        cx.new(move |cx| {
            if !cx.has_global::<GlobalSnippetWatcher>() {
                let global_watcher = GlobalSnippetWatcher::new(fs.clone(), cx);
                cx.set_global(global_watcher);
            }
            let mut this = Self {
                fs,
                watch_tasks: Vec::new(),
                snippets: Default::default(),
            };

            for dir in dirs_to_watch {
                this.watch_directory(&dir, cx);
            }

            this
        })
    }

    /// Add directory to be watched for content changes
    fn watch_directory(&mut self, path: &Path, cx: &Context<Self>) {
        let path: Arc<Path> = Arc::from(path);

        self.watch_tasks.push(cx.spawn(async move |this, cx| {
            let fs = this.read_with(cx, |this, _| this.fs.clone())?;
            let watched_path = path.clone();
            let watcher = fs.watch(&watched_path, Duration::from_secs(1));
            initial_scan(this.clone(), path, cx.clone()).await?;

            let (mut entries, _) = watcher.await;
            while let Some(entries) = entries.next().await {
                process_updates(
                    this.clone(),
                    entries.into_iter().map(|event| event.path).collect(),
                    cx.clone(),
                )
                .await?;
            }
            Ok(())
        }));
    }

    fn lookup_snippets<'a, const LOOKUP_GLOBALS: bool>(
        &'a self,
        language: &'a SnippetKind,
        cx: &App,
    ) -> Vec<Arc<Snippet>> {
        let mut user_snippets: Vec<_> = self
            .snippets
            .get(language)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .flat_map(|(_, snippets)| snippets.into_iter())
            .collect();
        if LOOKUP_GLOBALS {
            if let Some(global_watcher) = cx.try_global::<GlobalSnippetWatcher>() {
                user_snippets.extend(
                    global_watcher
                        .0
                        .read(cx)
                        .lookup_snippets::<false>(language, cx),
                );
            }

            let Some(registry) = SnippetRegistry::try_global(cx) else {
                return user_snippets;
            };

            let registry_snippets = registry.get_snippets(language);
            user_snippets.extend(registry_snippets);
        }

        user_snippets
    }

    pub fn snippets_for(&self, language: SnippetKind, cx: &App) -> Vec<Arc<Snippet>> {
        let mut requested_snippets = self.lookup_snippets::<true>(&language, cx);

        if language.is_some() {
            // Look up global snippets as well.
            requested_snippets.extend(self.lookup_snippets::<true>(&None, cx));
        }
        requested_snippets
    }
}

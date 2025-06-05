use crate::{Settings, settings_store::SettingsStore};
use collections::HashSet;
use fs::{Fs, PathEventKind};
use futures::{StreamExt, channel::mpsc};
use gpui::{App, BackgroundExecutor, ReadGlobal};
use std::{path::PathBuf, sync::Arc, time::Duration};

pub const EMPTY_THEME_NAME: &str = "empty-theme";

pub fn watch_config_file(
    executor: &BackgroundExecutor,
    fs: Arc<dyn Fs>,
    path: PathBuf,
) -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded();
    executor
        .spawn(async move {
            let (events, _) = fs.watch(&path, Duration::from_millis(100)).await;
            futures::pin_mut!(events);

            let contents = fs.load(&path).await.unwrap_or_default();
            if tx.unbounded_send(contents).is_err() {
                return;
            }

            loop {
                if events.next().await.is_none() {
                    break;
                }

                if let Ok(contents) = fs.load(&path).await {
                    if tx.unbounded_send(contents).is_err() {
                        break;
                    }
                }
            }
        })
        .detach();
    rx
}

pub fn watch_config_dir(
    executor: &BackgroundExecutor,
    fs: Arc<dyn Fs>,
    dir_path: PathBuf,
    config_paths: HashSet<PathBuf>,
) -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded();
    executor
        .spawn(async move {
            for file_path in &config_paths {
                if fs.metadata(file_path).await.is_ok_and(|v| v.is_some()) {
                    if let Ok(contents) = fs.load(file_path).await {
                        if tx.unbounded_send(contents).is_err() {
                            return;
                        }
                    }
                }
            }

            let (events, _) = fs.watch(&dir_path, Duration::from_millis(100)).await;
            futures::pin_mut!(events);

            while let Some(event_batch) = events.next().await {
                for event in event_batch {
                    if config_paths.contains(&event.path) {
                        match event.kind {
                            Some(PathEventKind::Removed) => {
                                if tx.unbounded_send(String::new()).is_err() {
                                    return;
                                }
                            }
                            Some(PathEventKind::Created) | Some(PathEventKind::Changed) => {
                                if let Ok(contents) = fs.load(&event.path).await {
                                    if tx.unbounded_send(contents).is_err() {
                                        return;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        })
        .detach();

    rx
}

pub fn update_settings_file<T: Settings>(
    fs: Arc<dyn Fs>,
    cx: &App,
    update: impl 'static + Send + FnOnce(&mut T::FileContent, &App),
) {
    SettingsStore::global(cx).update_settings_file::<T>(fs, update);
}

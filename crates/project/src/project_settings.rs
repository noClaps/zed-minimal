use anyhow::Context as _;
use collections::HashMap;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, AsyncApp, BorrowAppContext, Context, Entity, EventEmitter, Task};
use lsp::LanguageServerName;
use paths::{local_settings_file_relative_path, task_file_name};
use rpc::{
    AnyProtoClient, TypedEnvelope,
    proto::{self, FromProto, ToProto},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{
    InvalidSettingsError, LocalSettingsKind, Settings, SettingsLocation, SettingsSources,
    SettingsStore, watch_config_file,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use util::{ResultExt, serde::default_true};
use worktree::{Worktree, WorktreeId};

use crate::{
    task_store::{TaskSettingsLocation, TaskStore},
    worktree_store::WorktreeStore,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ProjectSettings {
    /// Configuration for language servers.
    ///
    /// The following settings can be overridden for specific language servers:
    /// - initialization_options
    ///
    /// To override settings for a language, add an entry for that language server's
    /// name to the lsp value.
    /// Default: null
    #[serde(default)]
    pub lsp: HashMap<LanguageServerName, LspSettings>,

    /// Configuration for Diagnostics-related features.
    #[serde(default)]
    pub diagnostics: DiagnosticsSettings,

    /// Configuration for Git-related features
    #[serde(default)]
    pub git: GitSettings,

    /// Configuration for Node-related features
    #[serde(default)]
    pub node: NodeBinarySettings,

    /// Configuration for how direnv configuration should be loaded
    #[serde(default)]
    pub load_direnv: DirenvSettings,

    /// Configuration for session-related features
    #[serde(default)]
    pub session: SessionSettings,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct DapSettings {
    pub binary: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NodeBinarySettings {
    /// The path to the Node binary.
    pub path: Option<String>,
    /// The path to the npm binary Zed should use (defaults to `.path/../npm`).
    pub npm_path: Option<String>,
    /// If enabled, Zed will download its own copy of Node.
    #[serde(default)]
    pub ignore_system_version: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DirenvSettings {
    /// Load direnv configuration through a shell hook
    ShellHook,
    /// Load direnv configuration directly using `direnv export json`
    #[default]
    Direct,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct DiagnosticsSettings {
    /// Whether to show the project diagnostics button in the status bar.
    pub button: bool,

    /// Whether or not to include warning diagnostics.
    pub include_warnings: bool,

    /// Settings for showing inline diagnostics.
    pub inline: InlineDiagnosticsSettings,

    /// Configuration, related to Rust language diagnostics.
    pub cargo: Option<CargoDiagnosticsSettings>,
}

impl DiagnosticsSettings {
    pub fn fetch_cargo_diagnostics(&self) -> bool {
        self.cargo.as_ref().map_or(false, |cargo_diagnostics| {
            cargo_diagnostics.fetch_cargo_diagnostics
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct InlineDiagnosticsSettings {
    /// Whether or not to show inline diagnostics
    ///
    /// Default: false
    pub enabled: bool,
    /// Whether to only show the inline diagnostics after a delay after the
    /// last editor event.
    ///
    /// Default: 150
    pub update_debounce_ms: u64,
    /// The amount of padding between the end of the source line and the start
    /// of the inline diagnostic in units of columns.
    ///
    /// Default: 4
    pub padding: u32,
    /// The minimum column to display inline diagnostics. This setting can be
    /// used to horizontally align inline diagnostics at some position. Lines
    /// longer than this value will still push diagnostics further to the right.
    ///
    /// Default: 0
    pub min_column: u32,

    pub max_severity: Option<DiagnosticSeverity>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct CargoDiagnosticsSettings {
    /// When enabled, Zed disables rust-analyzer's check on save and starts to query
    /// Cargo diagnostics separately.
    ///
    /// Default: false
    #[serde(default)]
    pub fetch_cargo_diagnostics: bool,
}

#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    // No diagnostics are shown.
    Off,
    Error,
    Warning,
    Info,
    Hint,
}

impl DiagnosticSeverity {
    pub fn into_lsp(self) -> Option<lsp::DiagnosticSeverity> {
        match self {
            DiagnosticSeverity::Off => None,
            DiagnosticSeverity::Error => Some(lsp::DiagnosticSeverity::ERROR),
            DiagnosticSeverity::Warning => Some(lsp::DiagnosticSeverity::WARNING),
            DiagnosticSeverity::Info => Some(lsp::DiagnosticSeverity::INFORMATION),
            DiagnosticSeverity::Hint => Some(lsp::DiagnosticSeverity::HINT),
        }
    }
}

impl Default for DiagnosticsSettings {
    fn default() -> Self {
        Self {
            button: true,
            include_warnings: true,
            inline: Default::default(),
            cargo: Default::default(),
        }
    }
}

impl Default for InlineDiagnosticsSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            update_debounce_ms: 150,
            padding: 4,
            min_column: 0,
            max_severity: None,
        }
    }
}

#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct GitSettings {
    /// Whether or not to show the git gutter.
    ///
    /// Default: tracked_files
    pub git_gutter: Option<GitGutterSetting>,
    /// Sets the debounce threshold (in milliseconds) after which changes are reflected in the git gutter.
    ///
    /// Default: null
    pub gutter_debounce: Option<u64>,
    /// Whether or not to show git blame data inline in
    /// the currently focused line.
    ///
    /// Default: on
    pub inline_blame: Option<InlineBlameSettings>,
    /// How hunks are displayed visually in the editor.
    ///
    /// Default: staged_hollow
    pub hunk_style: Option<GitHunkStyleSetting>,
}

impl GitSettings {
    pub fn inline_blame_enabled(&self) -> bool {
        #[allow(unknown_lints, clippy::manual_unwrap_or_default)]
        match self.inline_blame {
            Some(InlineBlameSettings { enabled, .. }) => enabled,
            _ => false,
        }
    }

    pub fn inline_blame_delay(&self) -> Option<Duration> {
        match self.inline_blame {
            Some(InlineBlameSettings {
                delay_ms: Some(delay_ms),
                ..
            }) if delay_ms > 0 => Some(Duration::from_millis(delay_ms)),
            _ => None,
        }
    }

    pub fn show_inline_commit_summary(&self) -> bool {
        match self.inline_blame {
            Some(InlineBlameSettings {
                show_commit_summary,
                ..
            }) => show_commit_summary,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitHunkStyleSetting {
    /// Show unstaged hunks with a filled background and staged hunks hollow.
    #[default]
    StagedHollow,
    /// Show unstaged hunks hollow and staged hunks with a filled background.
    UnstagedHollow,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitGutterSetting {
    /// Show git gutter in tracked files.
    #[default]
    TrackedFiles,
    /// Hide git gutter
    Hide,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct InlineBlameSettings {
    /// Whether or not to show git blame data inline in
    /// the currently focused line.
    ///
    /// Default: true
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Whether to only show the inline blame information
    /// after a delay once the cursor stops moving.
    ///
    /// Default: 0
    pub delay_ms: Option<u64>,
    /// The minimum column number to show the inline blame information at
    ///
    /// Default: 0
    pub min_column: Option<u32>,
    /// Whether to show commit summary as part of the inline blame.
    ///
    /// Default: false
    #[serde(default)]
    pub show_commit_summary: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct BinarySettings {
    pub path: Option<String>,
    pub arguments: Option<Vec<String>>,
    // this can't be an FxHashMap because the extension APIs require the default SipHash
    pub env: Option<std::collections::HashMap<String, String>>,
    pub ignore_system_version: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct LspSettings {
    pub binary: Option<BinarySettings>,
    pub initialization_options: Option<serde_json::Value>,
    pub settings: Option<serde_json::Value>,
    /// If the server supports sending tasks over LSP extensions,
    /// this setting can be used to enable or disable them in Zed.
    /// Default: true
    #[serde(default = "default_true")]
    pub enable_lsp_tasks: bool,
}

impl Default for LspSettings {
    fn default() -> Self {
        Self {
            binary: None,
            initialization_options: None,
            settings: None,
            enable_lsp_tasks: true,
        }
    }
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionSettings {
    /// Whether or not to restore unsaved buffers on restart.
    ///
    /// If this is true, user won't be prompted whether to save/discard
    /// dirty files when closing the application.
    ///
    /// Default: true
    pub restore_unsaved_buffers: bool,
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            restore_unsaved_buffers: true,
        }
    }
}

impl Settings for ProjectSettings {
    const KEY: Option<&'static str> = None;

    type FileContent = Self;

    fn load(sources: SettingsSources<Self::FileContent>, _: &mut App) -> anyhow::Result<Self> {
        sources.json_merge()
    }
}

pub enum SettingsObserverMode {
    Local(Arc<dyn Fs>),
    Remote,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SettingsObserverEvent {
    LocalSettingsUpdated(Result<PathBuf, InvalidSettingsError>),
    LocalTasksUpdated(Result<PathBuf, InvalidSettingsError>),
}

impl EventEmitter<SettingsObserverEvent> for SettingsObserver {}

pub struct SettingsObserver {
    downstream_client: Option<AnyProtoClient>,
    worktree_store: Entity<WorktreeStore>,
    project_id: u64,
    task_store: Entity<TaskStore>,
    _global_task_config_watcher: Task<()>,
}

/// SettingsObserver observers changes to .zed/{settings, task}.json files in local worktrees
/// (or the equivalent protobuf messages from upstream) and updates local settings
/// and sends notifications downstream.
/// In ssh mode it also monitors ~/.config/zed/{settings, task}.json and sends the content
/// upstream.
impl SettingsObserver {
    pub fn init(client: &AnyProtoClient) {
        client.add_entity_message_handler(Self::handle_update_worktree_settings);
    }

    pub fn new_local(
        fs: Arc<dyn Fs>,
        worktree_store: Entity<WorktreeStore>,
        task_store: Entity<TaskStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            worktree_store,
            task_store,
            downstream_client: None,
            project_id: 0,
            _global_task_config_watcher: Self::subscribe_to_global_task_file_changes(
                fs.clone(),
                paths::tasks_file().clone(),
                cx,
            ),
        }
    }

    pub fn new_remote(
        fs: Arc<dyn Fs>,
        worktree_store: Entity<WorktreeStore>,
        task_store: Entity<TaskStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            worktree_store,
            task_store,
            downstream_client: None,
            project_id: 0,
            _global_task_config_watcher: Self::subscribe_to_global_task_file_changes(
                fs.clone(),
                paths::tasks_file().clone(),
                cx,
            ),
        }
    }

    pub fn shared(
        &mut self,
        project_id: u64,
        downstream_client: AnyProtoClient,
        cx: &mut Context<Self>,
    ) {
        self.project_id = project_id;
        self.downstream_client = Some(downstream_client.clone());

        let store = cx.global::<SettingsStore>();
        for worktree in self.worktree_store.read(cx).worktrees() {
            let worktree_id = worktree.read(cx).id().to_proto();
            for (path, content) in store.local_settings(worktree.read(cx).id()) {
                downstream_client
                    .send(proto::UpdateWorktreeSettings {
                        project_id,
                        worktree_id,
                        path: path.to_proto(),
                        content: Some(content),
                        kind: Some(
                            local_settings_kind_to_proto(LocalSettingsKind::Settings).into(),
                        ),
                    })
                    .log_err();
            }
            for (path, content, _) in store.local_editorconfig_settings(worktree.read(cx).id()) {
                downstream_client
                    .send(proto::UpdateWorktreeSettings {
                        project_id,
                        worktree_id,
                        path: path.to_proto(),
                        content: Some(content),
                        kind: Some(
                            local_settings_kind_to_proto(LocalSettingsKind::Editorconfig).into(),
                        ),
                    })
                    .log_err();
            }
        }
    }

    pub fn unshared(&mut self, _: &mut Context<Self>) {
        self.downstream_client = None;
    }

    async fn handle_update_worktree_settings(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::UpdateWorktreeSettings>,
        mut cx: AsyncApp,
    ) -> anyhow::Result<()> {
        let kind = match envelope.payload.kind {
            Some(kind) => proto::LocalSettingsKind::from_i32(kind)
                .with_context(|| format!("unknown kind {kind}"))?,
            None => proto::LocalSettingsKind::Settings,
        };
        this.update(&mut cx, |this, cx| {
            let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
            let Some(worktree) = this
                .worktree_store
                .read(cx)
                .worktree_for_id(worktree_id, cx)
            else {
                return;
            };

            this.update_settings(
                worktree,
                [(
                    Arc::<Path>::from_proto(envelope.payload.path.clone()),
                    local_settings_kind_from_proto(kind),
                    envelope.payload.content,
                )],
                cx,
            );
        })?;
        Ok(())
    }

    fn update_settings(
        &mut self,
        worktree: Entity<Worktree>,
        settings_contents: impl IntoIterator<Item = (Arc<Path>, LocalSettingsKind, Option<String>)>,
        cx: &mut Context<Self>,
    ) {
        let worktree_id = worktree.read(cx).id();
        let remote_worktree_id = worktree.read(cx).id();
        let task_store = self.task_store.clone();

        for (directory, kind, file_content) in settings_contents {
            match kind {
                LocalSettingsKind::Settings | LocalSettingsKind::Editorconfig => cx
                    .update_global::<SettingsStore, _>(|store, cx| {
                        let result = store.set_local_settings(
                            worktree_id,
                            directory.clone(),
                            kind,
                            file_content.as_deref(),
                            cx,
                        );

                        match result {
                            Err(InvalidSettingsError::LocalSettings { path, message }) => {
                                log::error!("Failed to set local settings in {path:?}: {message}");
                                cx.emit(SettingsObserverEvent::LocalSettingsUpdated(Err(
                                    InvalidSettingsError::LocalSettings { path, message },
                                )));
                            }
                            Err(e) => {
                                log::error!("Failed to set local settings: {e}");
                            }
                            Ok(()) => {
                                cx.emit(SettingsObserverEvent::LocalSettingsUpdated(Ok(
                                    directory.join(local_settings_file_relative_path())
                                )));
                            }
                        }
                    }),
                LocalSettingsKind::Tasks => {
                    let result = task_store.update(cx, |task_store, cx| {
                        task_store.update_user_tasks(
                            TaskSettingsLocation::Worktree(SettingsLocation {
                                worktree_id,
                                path: directory.as_ref(),
                            }),
                            file_content.as_deref(),
                            cx,
                        )
                    });

                    match result {
                        Err(InvalidSettingsError::Tasks { path, message }) => {
                            log::error!("Failed to set local tasks in {path:?}: {message:?}");
                            cx.emit(SettingsObserverEvent::LocalTasksUpdated(Err(
                                InvalidSettingsError::Tasks { path, message },
                            )));
                        }
                        Err(e) => {
                            log::error!("Failed to set local tasks: {e}");
                        }
                        Ok(()) => {
                            cx.emit(SettingsObserverEvent::LocalTasksUpdated(Ok(
                                directory.join(task_file_name())
                            )));
                        }
                    }
                }
            };

            if let Some(downstream_client) = &self.downstream_client {
                downstream_client
                    .send(proto::UpdateWorktreeSettings {
                        project_id: self.project_id,
                        worktree_id: remote_worktree_id.to_proto(),
                        path: directory.to_proto(),
                        content: file_content,
                        kind: Some(local_settings_kind_to_proto(kind).into()),
                    })
                    .log_err();
            }
        }
    }

    fn subscribe_to_global_task_file_changes(
        fs: Arc<dyn Fs>,
        file_path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let mut user_tasks_file_rx =
            watch_config_file(&cx.background_executor(), fs, file_path.clone());
        let user_tasks_content = cx.background_executor().block(user_tasks_file_rx.next());
        let weak_entry = cx.weak_entity();
        cx.spawn(async move |settings_observer, cx| {
            let Ok(task_store) = settings_observer.read_with(cx, |settings_observer, _| {
                settings_observer.task_store.clone()
            }) else {
                return;
            };
            if let Some(user_tasks_content) = user_tasks_content {
                let Ok(()) = task_store.update(cx, |task_store, cx| {
                    task_store
                        .update_user_tasks(
                            TaskSettingsLocation::Global(&file_path),
                            Some(&user_tasks_content),
                            cx,
                        )
                        .log_err();
                }) else {
                    return;
                };
            }
            while let Some(user_tasks_content) = user_tasks_file_rx.next().await {
                let Ok(result) = task_store.update(cx, |task_store, cx| {
                    task_store.update_user_tasks(
                        TaskSettingsLocation::Global(&file_path),
                        Some(&user_tasks_content),
                        cx,
                    )
                }) else {
                    break;
                };

                weak_entry
                    .update(cx, |_, cx| match result {
                        Ok(()) => cx.emit(SettingsObserverEvent::LocalTasksUpdated(Ok(
                            file_path.clone()
                        ))),
                        Err(err) => cx.emit(SettingsObserverEvent::LocalTasksUpdated(Err(
                            InvalidSettingsError::Tasks {
                                path: file_path.clone(),
                                message: err.to_string(),
                            },
                        ))),
                    })
                    .ok();
            }
        })
    }
}

pub fn local_settings_kind_from_proto(kind: proto::LocalSettingsKind) -> LocalSettingsKind {
    match kind {
        proto::LocalSettingsKind::Settings => LocalSettingsKind::Settings,
        proto::LocalSettingsKind::Tasks => LocalSettingsKind::Tasks,
        proto::LocalSettingsKind::Editorconfig => LocalSettingsKind::Editorconfig,
        _ => unreachable!(),
    }
}

pub fn local_settings_kind_to_proto(kind: LocalSettingsKind) -> proto::LocalSettingsKind {
    match kind {
        LocalSettingsKind::Settings => proto::LocalSettingsKind::Settings,
        LocalSettingsKind::Tasks => proto::LocalSettingsKind::Tasks,
        LocalSettingsKind::Editorconfig => proto::LocalSettingsKind::Editorconfig,
    }
}

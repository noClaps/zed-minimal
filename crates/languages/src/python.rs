use anyhow::{Context as _, ensure};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use collections::HashMap;
use gpui::{AsyncApp, SharedString};
use language::LanguageToolchainStore;
use language::Toolchain;
use language::ToolchainList;
use language::ToolchainLister;
use language::{LanguageName, ManifestName, ManifestProvider, ManifestQuery};
use language::{LspAdapter, LspAdapterDelegate};
use lsp::LanguageServerBinary;
use lsp::LanguageServerName;
use node_runtime::NodeRuntime;
use pet_core::Configuration;
use pet_core::os_environment::Environment;
use pet_core::python_environment::PythonEnvironmentKind;
use project::Fs;
use project::lsp_store::language_server_settings;
use serde_json::{Value, json};
use smol::lock::OnceCell;
use std::cmp::Ordering;

use parking_lot::Mutex;
use std::str::FromStr;
use std::{
    any::Any,
    ffi::OsString,
    fmt::Write,
    fs,
    io::{self, BufRead},
    path::{Path, PathBuf},
    sync::Arc,
};
use util::ResultExt;

pub(crate) struct PyprojectTomlManifestProvider;

impl ManifestProvider for PyprojectTomlManifestProvider {
    fn name(&self) -> ManifestName {
        SharedString::new_static("pyproject.toml").into()
    }

    fn search(
        &self,
        ManifestQuery {
            path,
            depth,
            delegate,
        }: ManifestQuery,
    ) -> Option<Arc<Path>> {
        for path in path.ancestors().take(depth) {
            let p = path.join("pyproject.toml");
            if delegate.exists(&p, Some(false)) {
                return Some(path.into());
            }
        }

        None
    }
}

const SERVER_PATH: &str = "node_modules/pyright/langserver.index.js";
const NODE_MODULE_RELATIVE_SERVER_PATH: &str = "pyright/langserver.index.js";

enum TestRunner {
    UNITTEST,
    PYTEST,
}

impl FromStr for TestRunner {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "unittest" => Ok(Self::UNITTEST),
            "pytest" => Ok(Self::PYTEST),
            _ => Err(()),
        }
    }
}

fn server_binary_arguments(server_path: &Path) -> Vec<OsString> {
    vec![server_path.into(), "--stdio".into()]
}

pub struct PythonLspAdapter {
    node: NodeRuntime,
}

impl PythonLspAdapter {
    const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("pyright");

    pub fn new(node: NodeRuntime) -> Self {
        PythonLspAdapter { node }
    }
}

#[async_trait(?Send)]
impl LspAdapter for PythonLspAdapter {
    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME.clone()
    }

    async fn initialization_options(
        self: Arc<Self>,
        _: &dyn Fs,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> Result<Option<Value>> {
        // Provide minimal initialization options
        // Virtual environment configuration will be handled through workspace configuration
        Ok(Some(json!({
            "python": {
                "analysis": {
                    "autoSearchPaths": true,
                    "useLibraryCodeForTypes": true,
                    "autoImportCompletions": true
                }
            }
        })))
    }

    async fn check_if_user_installed(
        &self,
        delegate: &dyn LspAdapterDelegate,
        _: Arc<dyn LanguageToolchainStore>,
        _: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        if let Some(pyright_bin) = delegate.which("pyright-langserver".as_ref()).await {
            let env = delegate.shell_env().await;
            Some(LanguageServerBinary {
                path: pyright_bin,
                env: Some(env),
                arguments: vec!["--stdio".into()],
            })
        } else {
            let node = delegate.which("node".as_ref()).await?;
            let (node_modules_path, _) = delegate
                .npm_package_installed_version(Self::SERVER_NAME.as_ref())
                .await
                .log_err()??;

            let path = node_modules_path.join(NODE_MODULE_RELATIVE_SERVER_PATH);

            let env = delegate.shell_env().await;
            Some(LanguageServerBinary {
                path: node,
                env: Some(env),
                arguments: server_binary_arguments(&path),
            })
        }
    }

    async fn fetch_latest_server_version(
        &self,
        _: &dyn LspAdapterDelegate,
    ) -> Result<Box<dyn 'static + Any + Send>> {
        Ok(Box::new(
            self.node
                .npm_package_latest_version(Self::SERVER_NAME.as_ref())
                .await?,
        ) as Box<_>)
    }

    async fn fetch_server_binary(
        &self,
        latest_version: Box<dyn 'static + Send + Any>,
        container_dir: PathBuf,
        delegate: &dyn LspAdapterDelegate,
    ) -> Result<LanguageServerBinary> {
        let latest_version = latest_version.downcast::<String>().unwrap();
        let server_path = container_dir.join(SERVER_PATH);

        self.node
            .npm_install_packages(
                &container_dir,
                &[(Self::SERVER_NAME.as_ref(), latest_version.as_str())],
            )
            .await?;

        let env = delegate.shell_env().await;
        Ok(LanguageServerBinary {
            path: self.node.binary_path().await?,
            env: Some(env),
            arguments: server_binary_arguments(&server_path),
        })
    }

    async fn check_if_version_installed(
        &self,
        version: &(dyn 'static + Send + Any),
        container_dir: &PathBuf,
        delegate: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        let version = version.downcast_ref::<String>().unwrap();
        let server_path = container_dir.join(SERVER_PATH);

        let should_install_language_server = self
            .node
            .should_install_npm_package(
                Self::SERVER_NAME.as_ref(),
                &server_path,
                &container_dir,
                &version,
            )
            .await;

        if should_install_language_server {
            None
        } else {
            let env = delegate.shell_env().await;
            Some(LanguageServerBinary {
                path: self.node.binary_path().await.ok()?,
                env: Some(env),
                arguments: server_binary_arguments(&server_path),
            })
        }
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        delegate: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        let mut binary = get_cached_server_binary(container_dir, &self.node).await?;
        binary.env = Some(delegate.shell_env().await);
        Some(binary)
    }

    async fn process_completions(&self, items: &mut [lsp::CompletionItem]) {
        // Pyright assigns each completion item a `sortText` of the form `XX.YYYY.name`.
        // Where `XX` is the sorting category, `YYYY` is based on most recent usage,
        // and `name` is the symbol name itself.
        //
        // Because the symbol name is included, there generally are not ties when
        // sorting by the `sortText`, so the symbol's fuzzy match score is not taken
        // into account. Here, we remove the symbol name from the sortText in order
        // to allow our own fuzzy score to be used to break ties.
        //
        // see https://github.com/microsoft/pyright/blob/95ef4e103b9b2f129c9320427e51b73ea7cf78bd/packages/pyright-internal/src/languageService/completionProvider.ts#LL2873
        for item in items {
            let Some(sort_text) = &mut item.sort_text else {
                continue;
            };
            let mut parts = sort_text.split('.');
            let Some(first) = parts.next() else { continue };
            let Some(second) = parts.next() else { continue };
            let Some(_) = parts.next() else { continue };
            sort_text.replace_range(first.len() + second.len() + 1.., "");
        }
    }

    async fn label_for_completion(
        &self,
        item: &lsp::CompletionItem,
        language: &Arc<language::Language>,
    ) -> Option<language::CodeLabel> {
        let label = &item.label;
        let grammar = language.grammar()?;
        let highlight_id = match item.kind? {
            lsp::CompletionItemKind::METHOD => grammar.highlight_id_for_name("function.method")?,
            lsp::CompletionItemKind::FUNCTION => grammar.highlight_id_for_name("function")?,
            lsp::CompletionItemKind::CLASS => grammar.highlight_id_for_name("type")?,
            lsp::CompletionItemKind::CONSTANT => grammar.highlight_id_for_name("constant")?,
            _ => return None,
        };
        Some(language::CodeLabel {
            text: label.clone(),
            runs: vec![(0..label.len(), highlight_id)],
            filter_range: 0..label.len(),
        })
    }

    async fn label_for_symbol(
        &self,
        name: &str,
        kind: lsp::SymbolKind,
        language: &Arc<language::Language>,
    ) -> Option<language::CodeLabel> {
        let (text, filter_range, display_range) = match kind {
            lsp::SymbolKind::METHOD | lsp::SymbolKind::FUNCTION => {
                let text = format!("def {}():\n", name);
                let filter_range = 4..4 + name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            lsp::SymbolKind::CLASS => {
                let text = format!("class {}:", name);
                let filter_range = 6..6 + name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            lsp::SymbolKind::CONSTANT => {
                let text = format!("{} = 0", name);
                let filter_range = 0..name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            _ => return None,
        };

        Some(language::CodeLabel {
            runs: language.highlight_text(&text.as_str().into(), display_range.clone()),
            text: text[display_range].to_string(),
            filter_range,
        })
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        _: &dyn Fs,
        adapter: &Arc<dyn LspAdapterDelegate>,
        toolchains: Arc<dyn LanguageToolchainStore>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let toolchain = toolchains
            .active_toolchain(
                adapter.worktree_id(),
                Arc::from("".as_ref()),
                LanguageName::new("Python"),
                cx,
            )
            .await;
        cx.update(move |cx| {
            let mut user_settings =
                language_server_settings(adapter.as_ref(), &Self::SERVER_NAME, cx)
                    .and_then(|s| s.settings.clone())
                    .unwrap_or_default();

            // If we have a detected toolchain, configure Pyright to use it
            if let Some(toolchain) = toolchain {
                if user_settings.is_null() {
                    user_settings = Value::Object(serde_json::Map::default());
                }
                let object = user_settings.as_object_mut().unwrap();

                let interpreter_path = toolchain.path.to_string();

                // Detect if this is a virtual environment
                if let Some(interpreter_dir) = Path::new(&interpreter_path).parent() {
                    if let Some(venv_dir) = interpreter_dir.parent() {
                        // Check if this looks like a virtual environment
                        if venv_dir.join("pyvenv.cfg").exists()
                            || venv_dir.join("bin/activate").exists()
                            || venv_dir.join("Scripts/activate.bat").exists()
                        {
                            // Set venvPath and venv at the root level
                            // This matches the format of a pyrightconfig.json file
                            if let Some(parent) = venv_dir.parent() {
                                // Use relative path if the venv is inside the workspace
                                let venv_path = if parent == adapter.worktree_root_path() {
                                    ".".to_string()
                                } else {
                                    parent.to_string_lossy().into_owned()
                                };
                                object.insert("venvPath".to_string(), Value::String(venv_path));
                            }

                            if let Some(venv_name) = venv_dir.file_name() {
                                object.insert(
                                    "venv".to_owned(),
                                    Value::String(venv_name.to_string_lossy().into_owned()),
                                );
                            }
                        }
                    }
                }

                // Always set the python interpreter path
                // Get or create the python section
                let python = object
                    .entry("python")
                    .or_insert(Value::Object(serde_json::Map::default()))
                    .as_object_mut()
                    .unwrap();

                // Set both pythonPath and defaultInterpreterPath for compatibility
                python.insert(
                    "pythonPath".to_owned(),
                    Value::String(interpreter_path.clone()),
                );
                python.insert(
                    "defaultInterpreterPath".to_owned(),
                    Value::String(interpreter_path),
                );
            }

            user_settings
        })
    }
    fn manifest_name(&self) -> Option<ManifestName> {
        Some(SharedString::new_static("pyproject.toml").into())
    }
}

async fn get_cached_server_binary(
    container_dir: PathBuf,
    node: &NodeRuntime,
) -> Option<LanguageServerBinary> {
    let server_path = container_dir.join(SERVER_PATH);
    if server_path.exists() {
        Some(LanguageServerBinary {
            path: node.binary_path().await.log_err()?,
            env: None,
            arguments: server_binary_arguments(&server_path),
        })
    } else {
        log::error!("missing executable in directory {:?}", server_path);
        None
    }
}

fn python_env_kind_display(k: &PythonEnvironmentKind) -> &'static str {
    match k {
        PythonEnvironmentKind::Conda => "Conda",
        PythonEnvironmentKind::Pixi => "pixi",
        PythonEnvironmentKind::Homebrew => "Homebrew",
        PythonEnvironmentKind::Pyenv => "global (Pyenv)",
        PythonEnvironmentKind::GlobalPaths => "global",
        PythonEnvironmentKind::PyenvVirtualEnv => "Pyenv",
        PythonEnvironmentKind::Pipenv => "Pipenv",
        PythonEnvironmentKind::Poetry => "Poetry",
        PythonEnvironmentKind::MacPythonOrg => "global (Python.org)",
        PythonEnvironmentKind::MacCommandLineTools => "global (Command Line Tools for Xcode)",
        PythonEnvironmentKind::MacXCode => "global (Xcode)",
        PythonEnvironmentKind::Venv => "venv",
        PythonEnvironmentKind::VirtualEnv => "virtualenv",
        PythonEnvironmentKind::VirtualEnvWrapper => "virtualenvwrapper",
        _ => unreachable!(),
    }
}

pub(crate) struct PythonToolchainProvider {
    term: SharedString,
}

impl Default for PythonToolchainProvider {
    fn default() -> Self {
        Self {
            term: SharedString::new_static("Virtual Environment"),
        }
    }
}

static ENV_PRIORITY_LIST: &'static [PythonEnvironmentKind] = &[
    // Prioritize non-Conda environments.
    PythonEnvironmentKind::Poetry,
    PythonEnvironmentKind::Pipenv,
    PythonEnvironmentKind::VirtualEnvWrapper,
    PythonEnvironmentKind::Venv,
    PythonEnvironmentKind::VirtualEnv,
    PythonEnvironmentKind::PyenvVirtualEnv,
    PythonEnvironmentKind::Pixi,
    PythonEnvironmentKind::Conda,
    PythonEnvironmentKind::Pyenv,
    PythonEnvironmentKind::GlobalPaths,
    PythonEnvironmentKind::Homebrew,
];

fn env_priority(kind: Option<PythonEnvironmentKind>) -> usize {
    if let Some(kind) = kind {
        ENV_PRIORITY_LIST
            .iter()
            .position(|blessed_env| blessed_env == &kind)
            .unwrap_or(ENV_PRIORITY_LIST.len())
    } else {
        // Unknown toolchains are less useful than non-blessed ones.
        ENV_PRIORITY_LIST.len() + 1
    }
}

/// Return the name of environment declared in <worktree-root/.venv.
///
/// https://virtualfish.readthedocs.io/en/latest/plugins.html#auto-activation-auto-activation
fn get_worktree_venv_declaration(worktree_root: &Path) -> Option<String> {
    fs::File::open(worktree_root.join(".venv"))
        .and_then(|file| {
            let mut venv_name = String::new();
            io::BufReader::new(file).read_line(&mut venv_name)?;
            Ok(venv_name.trim().to_string())
        })
        .ok()
}

#[async_trait]
impl ToolchainLister for PythonToolchainProvider {
    fn manifest_name(&self) -> language::ManifestName {
        ManifestName::from(SharedString::new_static("pyproject.toml"))
    }
    async fn list(
        &self,
        worktree_root: PathBuf,
        subroot_relative_path: Option<Arc<Path>>,
        project_env: Option<HashMap<String, String>>,
    ) -> ToolchainList {
        let env = project_env.unwrap_or_default();
        let environment = EnvironmentApi::from_env(&env);
        let locators = pet::locators::create_locators(
            Arc::new(pet_conda::Conda::from(&environment)),
            Arc::new(pet_poetry::Poetry::from(&environment)),
            &environment,
        );
        let mut config = Configuration::default();

        let mut directories = vec![worktree_root.clone()];
        if let Some(subroot_relative_path) = subroot_relative_path {
            debug_assert!(subroot_relative_path.is_relative());
            directories.push(worktree_root.join(subroot_relative_path));
        }

        config.workspace_directories = Some(directories);
        for locator in locators.iter() {
            locator.configure(&config);
        }

        let reporter = pet_reporter::collect::create_reporter();
        pet::find::find_and_report_envs(&reporter, config, &locators, &environment, None);

        let mut toolchains = reporter
            .environments
            .lock()
            .map_or(Vec::new(), |mut guard| std::mem::take(&mut guard));

        let wr = worktree_root;
        let wr_venv = get_worktree_venv_declaration(&wr);
        // Sort detected environments by:
        //     environment name matching activation file (<workdir>/.venv)
        //     environment project dir matching worktree_root
        //     general env priority
        //     environment path matching the CONDA_PREFIX env var
        //     executable path
        toolchains.sort_by(|lhs, rhs| {
            // Compare venv names against worktree .venv file
            let venv_ordering =
                wr_venv
                    .as_ref()
                    .map_or(Ordering::Equal, |venv| match (&lhs.name, &rhs.name) {
                        (Some(l), Some(r)) => (r == venv).cmp(&(l == venv)),
                        (Some(l), None) if l == venv => Ordering::Less,
                        (None, Some(r)) if r == venv => Ordering::Greater,
                        _ => Ordering::Equal,
                    });

            // Compare project paths against worktree root
            let proj_ordering = || match (&lhs.project, &rhs.project) {
                (Some(l), Some(r)) => (r == &wr).cmp(&(l == &wr)),
                (Some(l), None) if l == &wr => Ordering::Less,
                (None, Some(r)) if r == &wr => Ordering::Greater,
                _ => Ordering::Equal,
            };

            // Compare environment priorities
            let priority_ordering = || env_priority(lhs.kind).cmp(&env_priority(rhs.kind));

            // Compare conda prefixes
            let conda_ordering = || {
                if lhs.kind == Some(PythonEnvironmentKind::Conda) {
                    environment
                        .get_env_var("CONDA_PREFIX".to_string())
                        .map(|conda_prefix| {
                            let is_match = |exe: &Option<PathBuf>| {
                                exe.as_ref().map_or(false, |e| e.starts_with(&conda_prefix))
                            };
                            match (is_match(&lhs.executable), is_match(&rhs.executable)) {
                                (true, false) => Ordering::Less,
                                (false, true) => Ordering::Greater,
                                _ => Ordering::Equal,
                            }
                        })
                        .unwrap_or(Ordering::Equal)
                } else {
                    Ordering::Equal
                }
            };

            // Compare Python executables
            let exe_ordering = || lhs.executable.cmp(&rhs.executable);

            venv_ordering
                .then_with(proj_ordering)
                .then_with(priority_ordering)
                .then_with(conda_ordering)
                .then_with(exe_ordering)
        });

        let mut toolchains: Vec<_> = toolchains
            .into_iter()
            .filter_map(|toolchain| {
                let mut name = String::from("Python");
                if let Some(ref version) = toolchain.version {
                    _ = write!(name, " {version}");
                }

                let name_and_kind = match (&toolchain.name, &toolchain.kind) {
                    (Some(name), Some(kind)) => {
                        Some(format!("({name}; {})", python_env_kind_display(kind)))
                    }
                    (Some(name), None) => Some(format!("({name})")),
                    (None, Some(kind)) => Some(format!("({})", python_env_kind_display(kind))),
                    (None, None) => None,
                };

                if let Some(nk) = name_and_kind {
                    _ = write!(name, " {nk}");
                }

                Some(Toolchain {
                    name: name.into(),
                    path: toolchain.executable.as_ref()?.to_str()?.to_owned().into(),
                    language_name: LanguageName::new("Python"),
                    as_json: serde_json::to_value(toolchain).ok()?,
                })
            })
            .collect();
        toolchains.dedup();
        ToolchainList {
            toolchains,
            default: None,
            groups: Default::default(),
        }
    }
    fn term(&self) -> SharedString {
        self.term.clone()
    }
}

pub struct EnvironmentApi<'a> {
    global_search_locations: Arc<Mutex<Vec<PathBuf>>>,
    project_env: &'a HashMap<String, String>,
    pet_env: pet_core::os_environment::EnvironmentApi,
}

impl<'a> EnvironmentApi<'a> {
    pub fn from_env(project_env: &'a HashMap<String, String>) -> Self {
        let paths = project_env
            .get("PATH")
            .map(|p| std::env::split_paths(p).collect())
            .unwrap_or_default();

        EnvironmentApi {
            global_search_locations: Arc::new(Mutex::new(paths)),
            project_env,
            pet_env: pet_core::os_environment::EnvironmentApi::new(),
        }
    }

    fn user_home(&self) -> Option<PathBuf> {
        self.project_env
            .get("HOME")
            .or_else(|| self.project_env.get("USERPROFILE"))
            .map(|home| pet_fs::path::norm_case(PathBuf::from(home)))
            .or_else(|| self.pet_env.get_user_home())
    }
}

impl pet_core::os_environment::Environment for EnvironmentApi<'_> {
    fn get_user_home(&self) -> Option<PathBuf> {
        self.user_home()
    }

    fn get_root(&self) -> Option<PathBuf> {
        None
    }

    fn get_env_var(&self, key: String) -> Option<String> {
        self.project_env
            .get(&key)
            .cloned()
            .or_else(|| self.pet_env.get_env_var(key))
    }

    fn get_know_global_search_locations(&self) -> Vec<PathBuf> {
        if self.global_search_locations.lock().is_empty() {
            let mut paths =
                std::env::split_paths(&self.get_env_var("PATH".to_string()).unwrap_or_default())
                    .collect::<Vec<PathBuf>>();

            log::trace!("Env PATH: {:?}", paths);
            for p in self.pet_env.get_know_global_search_locations() {
                if !paths.contains(&p) {
                    paths.push(p);
                }
            }

            let mut paths = paths
                .into_iter()
                .filter(|p| p.exists())
                .collect::<Vec<PathBuf>>();

            self.global_search_locations.lock().append(&mut paths);
        }
        self.global_search_locations.lock().clone()
    }
}

pub(crate) struct PyLspAdapter {
    python_venv_base: OnceCell<Result<Arc<Path>, String>>,
}
impl PyLspAdapter {
    const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("pylsp");
    pub(crate) fn new() -> Self {
        Self {
            python_venv_base: OnceCell::new(),
        }
    }
    async fn ensure_venv(delegate: &dyn LspAdapterDelegate) -> Result<Arc<Path>> {
        let python_path = Self::find_base_python(delegate)
            .await
            .context("Could not find Python installation for PyLSP")?;
        let work_dir = delegate
            .language_server_download_dir(&Self::SERVER_NAME)
            .await
            .context("Could not get working directory for PyLSP")?;
        let mut path = PathBuf::from(work_dir.as_ref());
        path.push("pylsp-venv");
        if !path.exists() {
            util::command::new_smol_command(python_path)
                .arg("-m")
                .arg("venv")
                .arg("pylsp-venv")
                .current_dir(work_dir)
                .spawn()?
                .output()
                .await?;
        }

        Ok(path.into())
    }
    // Find "baseline", user python version from which we'll create our own venv.
    async fn find_base_python(delegate: &dyn LspAdapterDelegate) -> Option<PathBuf> {
        for path in ["python3", "python"] {
            if let Some(path) = delegate.which(path.as_ref()).await {
                return Some(path);
            }
        }
        None
    }

    async fn base_venv(&self, delegate: &dyn LspAdapterDelegate) -> Result<Arc<Path>, String> {
        self.python_venv_base
            .get_or_init(move || async move {
                Self::ensure_venv(delegate)
                    .await
                    .map_err(|e| format!("{e}"))
            })
            .await
            .clone()
    }
}

const BINARY_DIR: &str = "bin";

#[async_trait(?Send)]
impl LspAdapter for PyLspAdapter {
    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME.clone()
    }

    async fn check_if_user_installed(
        &self,
        delegate: &dyn LspAdapterDelegate,
        toolchains: Arc<dyn LanguageToolchainStore>,
        cx: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        if let Some(pylsp_bin) = delegate.which(Self::SERVER_NAME.as_ref()).await {
            let env = delegate.shell_env().await;
            Some(LanguageServerBinary {
                path: pylsp_bin,
                env: Some(env),
                arguments: vec![],
            })
        } else {
            let venv = toolchains
                .active_toolchain(
                    delegate.worktree_id(),
                    Arc::from("".as_ref()),
                    LanguageName::new("Python"),
                    &mut cx.clone(),
                )
                .await?;
            let pylsp_path = Path::new(venv.path.as_ref()).parent()?.join("pylsp");
            pylsp_path.exists().then(|| LanguageServerBinary {
                path: venv.path.to_string().into(),
                arguments: vec![pylsp_path.into()],
                env: None,
            })
        }
    }

    async fn fetch_latest_server_version(
        &self,
        _: &dyn LspAdapterDelegate,
    ) -> Result<Box<dyn 'static + Any + Send>> {
        Ok(Box::new(()) as Box<_>)
    }

    async fn fetch_server_binary(
        &self,
        _: Box<dyn 'static + Send + Any>,
        _: PathBuf,
        delegate: &dyn LspAdapterDelegate,
    ) -> Result<LanguageServerBinary> {
        let venv = self.base_venv(delegate).await.map_err(|e| anyhow!(e))?;
        let pip_path = venv.join(BINARY_DIR).join("pip3");
        ensure!(
            util::command::new_smol_command(pip_path.as_path())
                .arg("install")
                .arg("python-lsp-server")
                .arg("-U")
                .output()
                .await?
                .status
                .success(),
            "python-lsp-server installation failed"
        );
        ensure!(
            util::command::new_smol_command(pip_path.as_path())
                .arg("install")
                .arg("python-lsp-server[all]")
                .arg("-U")
                .output()
                .await?
                .status
                .success(),
            "python-lsp-server[all] installation failed"
        );
        ensure!(
            util::command::new_smol_command(pip_path)
                .arg("install")
                .arg("pylsp-mypy")
                .arg("-U")
                .output()
                .await?
                .status
                .success(),
            "pylsp-mypy installation failed"
        );
        let pylsp = venv.join(BINARY_DIR).join("pylsp");
        Ok(LanguageServerBinary {
            path: pylsp,
            env: None,
            arguments: vec![],
        })
    }

    async fn cached_server_binary(
        &self,
        _: PathBuf,
        delegate: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        let venv = self.base_venv(delegate).await.ok()?;
        let pylsp = venv.join(BINARY_DIR).join("pylsp");
        Some(LanguageServerBinary {
            path: pylsp,
            env: None,
            arguments: vec![],
        })
    }

    async fn process_completions(&self, _items: &mut [lsp::CompletionItem]) {}

    async fn label_for_completion(
        &self,
        item: &lsp::CompletionItem,
        language: &Arc<language::Language>,
    ) -> Option<language::CodeLabel> {
        let label = &item.label;
        let grammar = language.grammar()?;
        let highlight_id = match item.kind? {
            lsp::CompletionItemKind::METHOD => grammar.highlight_id_for_name("function.method")?,
            lsp::CompletionItemKind::FUNCTION => grammar.highlight_id_for_name("function")?,
            lsp::CompletionItemKind::CLASS => grammar.highlight_id_for_name("type")?,
            lsp::CompletionItemKind::CONSTANT => grammar.highlight_id_for_name("constant")?,
            _ => return None,
        };
        Some(language::CodeLabel {
            text: label.clone(),
            runs: vec![(0..label.len(), highlight_id)],
            filter_range: 0..label.len(),
        })
    }

    async fn label_for_symbol(
        &self,
        name: &str,
        kind: lsp::SymbolKind,
        language: &Arc<language::Language>,
    ) -> Option<language::CodeLabel> {
        let (text, filter_range, display_range) = match kind {
            lsp::SymbolKind::METHOD | lsp::SymbolKind::FUNCTION => {
                let text = format!("def {}():\n", name);
                let filter_range = 4..4 + name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            lsp::SymbolKind::CLASS => {
                let text = format!("class {}:", name);
                let filter_range = 6..6 + name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            lsp::SymbolKind::CONSTANT => {
                let text = format!("{} = 0", name);
                let filter_range = 0..name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            _ => return None,
        };

        Some(language::CodeLabel {
            runs: language.highlight_text(&text.as_str().into(), display_range.clone()),
            text: text[display_range].to_string(),
            filter_range,
        })
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        _: &dyn Fs,
        adapter: &Arc<dyn LspAdapterDelegate>,
        toolchains: Arc<dyn LanguageToolchainStore>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let toolchain = toolchains
            .active_toolchain(
                adapter.worktree_id(),
                Arc::from("".as_ref()),
                LanguageName::new("Python"),
                cx,
            )
            .await;
        cx.update(move |cx| {
            let mut user_settings =
                language_server_settings(adapter.as_ref(), &Self::SERVER_NAME, cx)
                    .and_then(|s| s.settings.clone())
                    .unwrap_or_else(|| {
                        json!({
                            "plugins": {
                                "pycodestyle": {"enabled": false},
                                "rope_autoimport": {"enabled": true, "memory": true},
                                "pylsp_mypy": {"enabled": false}
                            },
                            "rope": {
                                "ropeFolder": null
                            },
                        })
                    });

            // If user did not explicitly modify their python venv, use one from picker.
            if let Some(toolchain) = toolchain {
                if user_settings.is_null() {
                    user_settings = Value::Object(serde_json::Map::default());
                }
                let object = user_settings.as_object_mut().unwrap();
                if let Some(python) = object
                    .entry("plugins")
                    .or_insert(Value::Object(serde_json::Map::default()))
                    .as_object_mut()
                {
                    if let Some(jedi) = python
                        .entry("jedi")
                        .or_insert(Value::Object(serde_json::Map::default()))
                        .as_object_mut()
                    {
                        jedi.entry("environment".to_string())
                            .or_insert_with(|| Value::String(toolchain.path.clone().into()));
                    }
                    if let Some(pylint) = python
                        .entry("pylsp_mypy")
                        .or_insert(Value::Object(serde_json::Map::default()))
                        .as_object_mut()
                    {
                        pylint.entry("overrides".to_string()).or_insert_with(|| {
                            Value::Array(vec![
                                Value::String("--python-executable".into()),
                                Value::String(toolchain.path.into()),
                                Value::String("--cache-dir=/dev/null".into()),
                                Value::Bool(true),
                            ])
                        });
                    }
                }
            }
            user_settings = Value::Object(serde_json::Map::from_iter([(
                "pylsp".to_string(),
                user_settings,
            )]));

            user_settings
        })
    }
    fn manifest_name(&self) -> Option<ManifestName> {
        Some(SharedString::new_static("pyproject.toml").into())
    }
}

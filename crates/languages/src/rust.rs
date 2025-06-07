use anyhow::{Context as _, Result};
use async_compression::futures::bufread::GzipDecoder;
use async_trait::async_trait;
use futures::{StreamExt, io::BufReader};
use gpui::{App, AsyncApp, SharedString};
use http_client::github::AssetKind;
use http_client::github::{GitHubLspBinaryVersion, latest_github_release};
pub use language::*;
use lsp::{InitializeParams, LanguageServerBinary};
use project::lsp_store::rust_analyzer_ext::CARGO_DIAGNOSTICS_SOURCE_NAME;
use project::project_settings::ProjectSettings;
use regex::Regex;
use serde_json::json;
use settings::Settings as _;
use smol::fs;
use std::fmt::Display;
use std::{
    any::Any,
    borrow::Cow,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};
use util::archive::extract_zip;
use util::merge_json_value_into;
use util::{ResultExt, fs::remove_matching, maybe};

pub struct RustLspAdapter;

impl RustLspAdapter {
    const GITHUB_ASSET_KIND: AssetKind = AssetKind::Gz;
    const ARCH_SERVER_NAME: &str = "apple-darwin";
}

const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("rust-analyzer");

impl RustLspAdapter {
    fn build_asset_name() -> String {
        let extension = match Self::GITHUB_ASSET_KIND {
            AssetKind::TarGz => "tar.gz",
            AssetKind::Gz => "gz",
            AssetKind::Zip => "zip",
        };

        format!(
            "{}-{}-{}.{}",
            SERVER_NAME,
            std::env::consts::ARCH,
            Self::ARCH_SERVER_NAME,
            extension
        )
    }
}

pub(crate) struct CargoManifestProvider;

impl ManifestProvider for CargoManifestProvider {
    fn name(&self) -> ManifestName {
        SharedString::new_static("Cargo.toml").into()
    }

    fn search(
        &self,
        ManifestQuery {
            path,
            depth,
            delegate,
        }: ManifestQuery,
    ) -> Option<Arc<Path>> {
        let mut outermost_cargo_toml = None;
        for path in path.ancestors().take(depth) {
            let p = path.join("Cargo.toml");
            if delegate.exists(&p, Some(false)) {
                outermost_cargo_toml = Some(Arc::from(path));
            }
        }

        outermost_cargo_toml
    }
}

#[async_trait(?Send)]
impl LspAdapter for RustLspAdapter {
    fn name(&self) -> LanguageServerName {
        SERVER_NAME.clone()
    }

    fn manifest_name(&self) -> Option<ManifestName> {
        Some(SharedString::new_static("Cargo.toml").into())
    }

    async fn check_if_user_installed(
        &self,
        delegate: &dyn LspAdapterDelegate,
        _: Arc<dyn LanguageToolchainStore>,
        _: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        let path = delegate.which("rust-analyzer".as_ref()).await?;
        let env = delegate.shell_env().await;

        // It is surprisingly common for ~/.cargo/bin/rust-analyzer to be a symlink to
        // /usr/bin/rust-analyzer that fails when you run it; so we need to test it.
        log::info!("found rust-analyzer in PATH. trying to run `rust-analyzer --help`");
        let result = delegate
            .try_exec(LanguageServerBinary {
                path: path.clone(),
                arguments: vec!["--help".into()],
                env: Some(env.clone()),
            })
            .await;
        if let Err(err) = result {
            log::debug!(
                "failed to run rust-analyzer after detecting it in PATH: binary: {:?}: {}",
                path,
                err
            );
            return None;
        }

        Some(LanguageServerBinary {
            path,
            env: Some(env),
            arguments: vec![],
        })
    }

    async fn fetch_latest_server_version(
        &self,
        delegate: &dyn LspAdapterDelegate,
    ) -> Result<Box<dyn 'static + Send + Any>> {
        let release = latest_github_release(
            "rust-lang/rust-analyzer",
            true,
            false,
            delegate.http_client(),
        )
        .await?;
        let asset_name = Self::build_asset_name();

        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .with_context(|| format!("no asset found matching `{asset_name:?}`"))?;
        Ok(Box::new(GitHubLspBinaryVersion {
            name: release.tag_name,
            url: asset.browser_download_url.clone(),
        }))
    }

    async fn fetch_server_binary(
        &self,
        version: Box<dyn 'static + Send + Any>,
        container_dir: PathBuf,
        delegate: &dyn LspAdapterDelegate,
    ) -> Result<LanguageServerBinary> {
        let version = version.downcast::<GitHubLspBinaryVersion>().unwrap();
        let destination_path = container_dir.join(format!("rust-analyzer-{}", version.name));
        let server_path = match Self::GITHUB_ASSET_KIND {
            AssetKind::TarGz | AssetKind::Gz => destination_path.clone(), // Tar and gzip extract in place.
            AssetKind::Zip => destination_path.clone().join("rust-analyzer.exe"), // zip contains a .exe
        };

        if fs::metadata(&server_path).await.is_err() {
            remove_matching(&container_dir, |entry| entry != destination_path).await;

            let mut response = delegate
                .http_client()
                .get(&version.url, Default::default(), true)
                .await
                .with_context(|| format!("downloading release from {}", version.url))?;
            match Self::GITHUB_ASSET_KIND {
                AssetKind::TarGz => {
                    let decompressed_bytes = GzipDecoder::new(BufReader::new(response.body_mut()));
                    let archive = async_tar::Archive::new(decompressed_bytes);
                    archive.unpack(&destination_path).await.with_context(|| {
                        format!("extracting {} to {:?}", version.url, destination_path)
                    })?;
                }
                AssetKind::Gz => {
                    let mut decompressed_bytes =
                        GzipDecoder::new(BufReader::new(response.body_mut()));
                    let mut file =
                        fs::File::create(&destination_path).await.with_context(|| {
                            format!(
                                "creating a file {:?} for a download from {}",
                                destination_path, version.url,
                            )
                        })?;
                    futures::io::copy(&mut decompressed_bytes, &mut file)
                        .await
                        .with_context(|| {
                            format!("extracting {} to {:?}", version.url, destination_path)
                        })?;
                }
                AssetKind::Zip => {
                    extract_zip(&destination_path, response.body_mut())
                        .await
                        .with_context(|| {
                            format!("unzipping {} to {:?}", version.url, destination_path)
                        })?;
                }
            };

            fs::set_permissions(
                &server_path,
                <fs::Permissions as fs::unix::PermissionsExt>::from_mode(0o755),
            )
            .await?;
        }

        Ok(LanguageServerBinary {
            path: server_path,
            env: None,
            arguments: Default::default(),
        })
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        get_cached_server_binary(container_dir).await
    }

    fn disk_based_diagnostic_sources(&self) -> Vec<String> {
        vec![CARGO_DIAGNOSTICS_SOURCE_NAME.to_owned()]
    }

    fn disk_based_diagnostics_progress_token(&self) -> Option<String> {
        Some("rust-analyzer/flycheck".into())
    }

    fn process_diagnostics(
        &self,
        params: &mut lsp::PublishDiagnosticsParams,
        _: LanguageServerId,
        _: Option<&'_ Buffer>,
    ) {
        static REGEX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?m)`([^`]+)\n`$").expect("Failed to create REGEX"));

        for diagnostic in &mut params.diagnostics {
            for message in diagnostic
                .related_information
                .iter_mut()
                .flatten()
                .map(|info| &mut info.message)
                .chain([&mut diagnostic.message])
            {
                if let Cow::Owned(sanitized) = REGEX.replace_all(message, "`$1`") {
                    *message = sanitized;
                }
            }
        }
    }

    fn diagnostic_message_to_markdown(&self, message: &str) -> Option<String> {
        static REGEX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?m)\n *").expect("Failed to create REGEX"));
        Some(REGEX.replace_all(message, "\n\n").to_string())
    }

    async fn label_for_completion(
        &self,
        completion: &lsp::CompletionItem,
        language: &Arc<Language>,
    ) -> Option<CodeLabel> {
        let detail = completion
            .label_details
            .as_ref()
            .and_then(|detail| detail.detail.as_ref())
            .or(completion.detail.as_ref())
            .map(|detail| detail.trim());
        let function_signature = completion
            .label_details
            .as_ref()
            .and_then(|detail| detail.description.as_deref())
            .or(completion.detail.as_deref());
        match (detail, completion.kind) {
            (Some(detail), Some(lsp::CompletionItemKind::FIELD)) => {
                let name = &completion.label;
                let text = format!("{name}: {detail}");
                let prefix = "struct S { ";
                let source = Rope::from(format!("{prefix}{text} }}"));
                let runs =
                    language.highlight_text(&source, prefix.len()..prefix.len() + text.len());
                return Some(CodeLabel {
                    text,
                    runs,
                    filter_range: 0..name.len(),
                });
            }
            (
                Some(detail),
                Some(lsp::CompletionItemKind::CONSTANT | lsp::CompletionItemKind::VARIABLE),
            ) if completion.insert_text_format != Some(lsp::InsertTextFormat::SNIPPET) => {
                let name = &completion.label;
                let text = format!(
                    "{}: {}",
                    name,
                    completion.detail.as_deref().unwrap_or(detail)
                );
                let prefix = "let ";
                let source = Rope::from(format!("{prefix}{text} = ();"));
                let runs =
                    language.highlight_text(&source, prefix.len()..prefix.len() + text.len());
                return Some(CodeLabel {
                    text,
                    runs,
                    filter_range: 0..name.len(),
                });
            }
            (
                Some(detail),
                Some(lsp::CompletionItemKind::FUNCTION | lsp::CompletionItemKind::METHOD),
            ) => {
                static REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new("\\(…?\\)").unwrap());
                const FUNCTION_PREFIXES: [&str; 6] = [
                    "async fn",
                    "async unsafe fn",
                    "const fn",
                    "const unsafe fn",
                    "unsafe fn",
                    "fn",
                ];
                // Is it function `async`?
                let fn_keyword = FUNCTION_PREFIXES.iter().find_map(|prefix| {
                    function_signature.as_ref().and_then(|signature| {
                        signature
                            .strip_prefix(*prefix)
                            .map(|suffix| (*prefix, suffix))
                    })
                });
                // fn keyword should be followed by opening parenthesis.
                if let Some((prefix, suffix)) = fn_keyword {
                    let mut text = REGEX.replace(&completion.label, suffix).to_string();
                    let source = Rope::from(format!("{prefix} {text} {{}}"));
                    let run_start = prefix.len() + 1;
                    let runs = language.highlight_text(&source, run_start..run_start + text.len());
                    if detail.starts_with("(") {
                        text.push(' ');
                        text.push_str(&detail);
                    }

                    return Some(CodeLabel {
                        filter_range: 0..completion.label.find('(').unwrap_or(text.len()),
                        text,
                        runs,
                    });
                } else if completion
                    .detail
                    .as_ref()
                    .map_or(false, |detail| detail.starts_with("macro_rules! "))
                {
                    let source = Rope::from(completion.label.as_str());
                    let runs = language.highlight_text(&source, 0..completion.label.len());

                    return Some(CodeLabel {
                        filter_range: 0..completion.label.len(),
                        text: completion.label.clone(),
                        runs,
                    });
                }
            }
            (_, Some(kind)) => {
                let highlight_name = match kind {
                    lsp::CompletionItemKind::STRUCT
                    | lsp::CompletionItemKind::INTERFACE
                    | lsp::CompletionItemKind::ENUM => Some("type"),
                    lsp::CompletionItemKind::ENUM_MEMBER => Some("variant"),
                    lsp::CompletionItemKind::KEYWORD => Some("keyword"),
                    lsp::CompletionItemKind::VALUE | lsp::CompletionItemKind::CONSTANT => {
                        Some("constant")
                    }
                    _ => None,
                };

                let mut label = completion.label.clone();
                if let Some(detail) = detail.filter(|detail| detail.starts_with("(")) {
                    label.push(' ');
                    label.push_str(detail);
                }
                let mut label = CodeLabel::plain(label, None);
                if let Some(highlight_name) = highlight_name {
                    let highlight_id = language.grammar()?.highlight_id_for_name(highlight_name)?;
                    label.runs.push((
                        0..label.text.rfind('(').unwrap_or(completion.label.len()),
                        highlight_id,
                    ));
                }

                return Some(label);
            }
            _ => {}
        }
        None
    }

    async fn label_for_symbol(
        &self,
        name: &str,
        kind: lsp::SymbolKind,
        language: &Arc<Language>,
    ) -> Option<CodeLabel> {
        let (text, filter_range, display_range) = match kind {
            lsp::SymbolKind::METHOD | lsp::SymbolKind::FUNCTION => {
                let text = format!("fn {} () {{}}", name);
                let filter_range = 3..3 + name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            lsp::SymbolKind::STRUCT => {
                let text = format!("struct {} {{}}", name);
                let filter_range = 7..7 + name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            lsp::SymbolKind::ENUM => {
                let text = format!("enum {} {{}}", name);
                let filter_range = 5..5 + name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            lsp::SymbolKind::INTERFACE => {
                let text = format!("trait {} {{}}", name);
                let filter_range = 6..6 + name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            lsp::SymbolKind::CONSTANT => {
                let text = format!("const {}: () = ();", name);
                let filter_range = 6..6 + name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            lsp::SymbolKind::MODULE => {
                let text = format!("mod {} {{}}", name);
                let filter_range = 4..4 + name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            lsp::SymbolKind::TYPE_PARAMETER => {
                let text = format!("type {} {{}}", name);
                let filter_range = 5..5 + name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            _ => return None,
        };

        Some(CodeLabel {
            runs: language.highlight_text(&text.as_str().into(), display_range.clone()),
            text: text[display_range].to_string(),
            filter_range,
        })
    }

    fn prepare_initialize_params(
        &self,
        mut original: InitializeParams,
        cx: &App,
    ) -> Result<InitializeParams> {
        let enable_lsp_tasks = ProjectSettings::get_global(cx)
            .lsp
            .get(&SERVER_NAME)
            .map_or(false, |s| s.enable_lsp_tasks);
        if enable_lsp_tasks {
            let experimental = json!({
                "runnables": {
                    "kinds": [ "cargo", "shell" ],
                },
            });
            if let Some(original_experimental) = &mut original.capabilities.experimental {
                merge_json_value_into(experimental, original_experimental);
            } else {
                original.capabilities.experimental = Some(experimental);
            }
        }

        let cargo_diagnostics_fetched_separately = ProjectSettings::get_global(cx)
            .diagnostics
            .fetch_cargo_diagnostics();
        if cargo_diagnostics_fetched_separately {
            let disable_check_on_save = json!({
                "checkOnSave": false,
            });
            if let Some(initialization_options) = &mut original.initialization_options {
                merge_json_value_into(disable_check_on_save, initialization_options);
            } else {
                original.initialization_options = Some(disable_check_on_save);
            }
        }

        Ok(original)
    }
}

/// Part of the data structure of Cargo metadata
#[derive(serde::Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
}

#[derive(serde::Deserialize)]
struct CargoPackage {
    id: String,
    targets: Vec<CargoTarget>,
}

#[derive(serde::Deserialize)]
struct CargoTarget {
    name: String,
    kind: Vec<String>,
    src_path: String,
    #[serde(rename = "required-features", default)]
    required_features: Vec<String>,
}

#[derive(Debug, PartialEq)]
enum TargetKind {
    Bin,
    Example,
}

impl Display for TargetKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TargetKind::Bin => write!(f, "bin"),
            TargetKind::Example => write!(f, "example"),
        }
    }
}

impl TryFrom<&str> for TargetKind {
    type Error = ();
    fn try_from(value: &str) -> Result<Self, ()> {
        match value {
            "bin" => Ok(Self::Bin),
            "example" => Ok(Self::Example),
            _ => Err(()),
        }
    }
}

async fn get_cached_server_binary(container_dir: PathBuf) -> Option<LanguageServerBinary> {
    maybe!(async {
        let mut last = None;
        let mut entries = fs::read_dir(&container_dir).await?;
        while let Some(entry) = entries.next().await {
            last = Some(entry?.path());
        }

        anyhow::Ok(LanguageServerBinary {
            path: last.context("no cached binary")?,
            env: None,
            arguments: Default::default(),
        })
    })
    .await
    .log_err()
}

mod assertions;
mod example;
mod examples;
mod explorer;
mod ids;
mod instance;
mod tool_metrics;

use instance::{ExampleInstance, run_git};
pub(crate) use tool_metrics::*;

use ::fs::RealFs;
use clap::Parser;
use client::{Client, ProxySettings, UserStore};
use collections::{HashMap, HashSet};
use extension::ExtensionHostProxy;
use futures::future;
use gpui::http_client::read_proxy_from_env;
use gpui::{App, AppContext, Application, Entity, SemanticVersion, UpdateGlobal};
use gpui_tokio::Tokio;
use language::LanguageRegistry;
use node_runtime::{NodeBinaryOptions, NodeRuntime};
use project::Project;
use project::project_settings::ProjectSettings;
use release_channel::AppVersion;
use reqwest_client::ReqwestClient;
use settings::{Settings, SettingsStore};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::env;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, LazyLock};
use util::ResultExt as _;

static CARGO_MANIFEST_DIR: LazyLock<PathBuf> =
    LazyLock::new(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));

#[derive(Parser, Debug)]
#[command(name = "eval", disable_version_flag = true)]
struct Args {
    /// Runs all examples and threads that contain these substrings. If unspecified, all examples and threads are run.
    #[arg(value_name = "EXAMPLE_SUBSTRING")]
    filter: Vec<String>,
    /// provider/model to use for agent
    #[arg(long)]
    model: String,
    /// provider/model to use for judges
    #[arg(long)]
    judge_model: String,
    #[arg(long, value_delimiter = ',', default_value = "rs,ts,py")]
    languages: Vec<String>,
    /// How many times to run each example.
    #[arg(long, default_value = "8")]
    repetitions: usize,
    /// Maximum number of examples to run concurrently.
    #[arg(long, default_value = "4")]
    concurrency: usize,
}

fn main() {
    dotenv::from_filename(CARGO_MANIFEST_DIR.join(".env")).ok();

    env_logger::init();

    let system_id = ids::get_or_create_id(&ids::eval_system_id_path()).ok();
    let installation_id = ids::get_or_create_id(&ids::eval_installation_id_path()).ok();
    let session_id = uuid::Uuid::new_v4().to_string();
    let run_timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let run_id = match env::var("GITHUB_RUN_ID") {
        Ok(run_id) => format!("github/{}", run_id),
        Err(_) => format!("local/{}", run_timestamp),
    };

    let root_dir = Path::new(std::env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    let eval_crate_dir = root_dir.join("crates").join("eval");
    let repos_dir = eval_crate_dir.join("repos");
    let worktrees_dir = eval_crate_dir.join("worktrees");
    let examples_dir = eval_crate_dir.join("src").join("examples");
    let run_dir = eval_crate_dir
        .join("runs")
        .join(format!("{}", run_timestamp));
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::create_dir_all(&repos_dir).unwrap();
    std::fs::create_dir_all(&worktrees_dir).unwrap();
    std::fs::create_dir_all(&examples_dir).unwrap();
    std::fs::create_dir_all(&paths::config_dir()).unwrap();

    let zed_commit_sha = commit_sha_for_path(&root_dir);
    let zed_branch_name = git_branch_for_path(&root_dir);
    let args = Args::parse();
    let languages: HashSet<String> = args.languages.into_iter().collect();

    let http_client = Arc::new(ReqwestClient::new());
    let app = Application::headless().with_http_client(http_client.clone());
    let all_threads = examples::all();

    app.run(move |cx| {
        let app_state = init(cx);

        let telemetry = app_state.client.telemetry();
        telemetry.start(system_id, installation_id, session_id, cx);

        let enable_telemetry = env::var("ZED_EVAL_TELEMETRY").map_or(false, |value| value == "1")
            && telemetry.has_checksum_seed();
        if enable_telemetry {
            println!("Telemetry enabled");
            telemetry::event!(
                "Agent Eval Started",
                zed_commit_sha = zed_commit_sha,
                zed_branch_name = zed_branch_name,
                run_id = run_id,
            );
        }

        let agent_model = load_model(&args.model, cx).unwrap();
        let judge_model = load_model(&args.judge_model, cx).unwrap();

        LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
            registry.set_default_model(Some(agent_model.clone()), cx);
        });

        let auth1 = agent_model.provider.authenticate(cx);
        let auth2 = judge_model.provider.authenticate(cx);

        cx.spawn(async move |cx| {
            auth1.await?;
            auth2.await?;

            let mut examples = Vec::new();

            const COLORS: [&str; 12] = [
                "\x1b[31m", // Red
                "\x1b[32m", // Green
                "\x1b[33m", // Yellow
                "\x1b[34m", // Blue
                "\x1b[35m", // Magenta
                "\x1b[36m", // Cyan
                "\x1b[91m", // Bright Red
                "\x1b[92m", // Bright Green
                "\x1b[93m", // Bright Yellow
                "\x1b[94m", // Bright Blue
                "\x1b[95m", // Bright Magenta
                "\x1b[96m", // Bright Cyan
            ];

            let mut skipped = Vec::new();

            for thread in all_threads {
                let meta = thread.meta();
                if !args.filter.is_empty() && !args.filter.iter().any(|sub| meta.name.contains(sub))
                {
                    skipped.push(meta.name);
                    continue;
                }

                if let Some(language) = meta.language_server {
                    if !languages.contains(&language.file_extension) {
                        panic!(
                            "Eval for {:?} could not be run because no language server was found for extension {:?}",
                            meta.name,
                            language.file_extension
                        );
                    }
                }

                // TODO: This creates a worktree per repetition. Ideally these examples should
                // either be run sequentially on the same worktree, or reuse worktrees when there
                // are more examples to run than the concurrency limit.
                for repetition_number in 0..args.repetitions {
                    let example_instance = ExampleInstance::new(
                        thread.clone(),
                        &repos_dir,
                        &run_dir,
                        &worktrees_dir,
                        repetition_number,
                    );

                    examples.push(example_instance);
                }
            }

            if !skipped.is_empty() {
                println!("Skipped threads: {}", skipped.join(", "));
            }

            if examples.is_empty() {
                eprintln!("Filter matched no examples");
                return cx.update(|cx| cx.quit());
            }

            let mut repo_urls = HashSet::default();
            let mut clone_tasks = Vec::new();

            let max_name_width = examples
                .iter()
                .map(|e| e.worktree_name().len())
                .max()
                .unwrap_or(0);

            for (i, example_instance) in examples.iter_mut().enumerate() {
                let color = COLORS[i % COLORS.len()].to_string();
                example_instance.set_log_prefix_style(&color, max_name_width);

                println!(
                    "{}Logging to: {}",
                    example_instance.log_prefix,
                    example_instance.run_directory.display()
                );

                let repo_url = example_instance.repo_url();
                if repo_urls.insert(repo_url.clone()) {
                    let repo_path = example_instance.repo_path.clone();

                    if !repo_path.join(".git").is_dir() {
                        println!(
                            "{:<width$} < {}",
                            "↓ Cloning",
                            repo_url,
                            width = max_name_width
                        );

                        let git_task = cx.spawn(async move |_cx| {
                            std::fs::create_dir_all(&repo_path)?;
                            run_git(&repo_path, &["init"]).await?;
                            run_git(&repo_path, &["remote", "add", "origin", &repo_url]).await
                        });

                        clone_tasks.push(git_task);
                    } else {
                        println!(
                            "{:<width$}  < {}",
                            "✔︎ Already cloned",
                            repo_url,
                            width = max_name_width
                        );

                        let actual_origin =
                            run_git(&repo_path, &["remote", "get-url", "origin"]).await?;
                        anyhow::ensure!(
                            actual_origin == repo_url,
                            "remote origin {actual_origin} does not match expected origin {repo_url}"
                        );
                    }
                }
            }

            future::join_all(clone_tasks).await;

            for example_instance in examples.iter_mut() {
                example_instance.fetch().await?;
            }

            let examples = Rc::new(RefCell::new(VecDeque::from(examples)));
            let results_by_example_name = Rc::new(RefCell::new(HashMap::default()));

            future::join_all((0..args.concurrency).map(|_| {
                let examples = examples.clone();
                let results = results_by_example_name.clone();
                cx.spawn(async move |_| {
                    loop {
                        let Some(mut example) = examples.borrow_mut().pop_front() else {
                            break;
                        };
                        let result = async {
                            example.setup().await?;
                            anyhow::Ok(())
                        }
                        .await;
                        results
                            .borrow_mut()
                            .entry(example.name.clone())
                            .or_insert(Vec::new())
                            .push((example.clone(), result));
                    }
                })
            }))
            .await;

            app_state.client.telemetry().flush_events().await;

            cx.update(|cx| cx.quit())
        })
        .detach_and_log_err(cx);
    });
}

/// Subset of `workspace::AppState` needed by `HeadlessAssistant`, with additional fields.
pub struct AgentAppState {
    pub languages: Arc<LanguageRegistry>,
    pub client: Arc<Client>,
    pub user_store: Entity<UserStore>,
    pub fs: Arc<dyn fs::Fs>,
    pub node_runtime: NodeRuntime,
}

pub fn init(cx: &mut App) -> Arc<AgentAppState> {
    release_channel::init(SemanticVersion::default(), cx);
    gpui_tokio::init(cx);

    let mut settings_store = SettingsStore::new(cx);
    settings_store
        .set_default_settings(settings::default_settings().as_ref(), cx)
        .unwrap();
    cx.set_global(settings_store);
    client::init_settings(cx);

    // Set User-Agent so we can download language servers from GitHub
    let user_agent = format!(
        "Zed/{} ({}; {})",
        AppVersion::global(cx),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let proxy_str = ProxySettings::get_global(cx).proxy.to_owned();
    let proxy_url = proxy_str
        .as_ref()
        .and_then(|input| input.parse().ok())
        .or_else(read_proxy_from_env);
    let http = {
        let _guard = Tokio::handle(cx).enter();

        ReqwestClient::proxy_and_user_agent(proxy_url, &user_agent)
            .expect("could not start HTTP client")
    };
    cx.set_http_client(Arc::new(http));

    Project::init_settings(cx);

    let client = Client::production(cx);
    cx.set_http_client(client.http_client());

    let git_binary_path = None;
    let fs = Arc::new(RealFs::new(
        git_binary_path,
        cx.background_executor().clone(),
    ));

    let mut languages = LanguageRegistry::new(cx.background_executor().clone());
    languages.set_language_server_download_dir(paths::languages_dir().clone());
    let languages = Arc::new(languages);

    let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));

    extension::init(cx);

    let (tx, rx) = async_watch::channel(None);
    cx.observe_global::<SettingsStore>(move |cx| {
        let settings = &ProjectSettings::get_global(cx).node;
        let options = NodeBinaryOptions {
            allow_path_lookup: !settings.ignore_system_version,
            allow_binary_download: true,
            use_paths: settings.path.as_ref().map(|node_path| {
                let node_path = PathBuf::from(shellexpand::tilde(node_path).as_ref());
                let npm_path = settings
                    .npm_path
                    .as_ref()
                    .map(|path| PathBuf::from(shellexpand::tilde(&path).as_ref()));
                (
                    node_path.clone(),
                    npm_path.unwrap_or_else(|| {
                        let base_path = PathBuf::new();
                        node_path.parent().unwrap_or(&base_path).join("npm")
                    }),
                )
            }),
        };
        tx.send(Some(options)).log_err();
    })
    .detach();
    let node_runtime = NodeRuntime::new(client.http_client(), None, rx);

    let extension_host_proxy = ExtensionHostProxy::global(cx);

    language::init(cx);
    debug_adapter_extension::init(extension_host_proxy.clone(), cx);
    language_extension::init(extension_host_proxy.clone(), languages.clone());
    languages::init(languages.clone(), node_runtime.clone(), cx);
    terminal_view::init(cx);

    SettingsStore::update_global(cx, |store, cx| {
        store.set_user_settings(include_str!("../runner_settings.json"), cx)
    })
    .unwrap();

    Arc::new(AgentAppState {
        languages,
        client,
        user_store,
        fs,
        node_runtime,
    })
}

pub fn find_model(
    model_name: &str,
    model_registry: &LanguageModelRegistry,
    cx: &App,
) -> anyhow::Result<Arc<dyn LanguageModel>> {
    let selected = SelectedModel::from_str(model_name).map_err(|e| anyhow::anyhow!(e))?;
    model_registry
        .available_models(cx)
        .find(|model| model.id() == selected.model && model.provider_id() == selected.provider)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No language model with ID {}/{} was available. Available models: {}",
                selected.model.0,
                selected.provider.0,
                model_registry
                    .available_models(cx)
                    .map(|model| format!("{}/{}", model.provider_id().0, model.id().0))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

pub fn load_model(model_name: &str, cx: &mut App) -> anyhow::Result<ConfiguredModel> {
    let model = {
        let model_registry = LanguageModelRegistry::read_global(cx);
        find_model(model_name, model_registry, cx)?
    };

    let provider = {
        let model_registry = LanguageModelRegistry::read_global(cx);
        model_registry
            .provider(&model.provider_id())
            .ok_or_else(|| anyhow::anyhow!("Provider not found: {}", model.provider_id()))?
    };

    Ok(ConfiguredModel {
        provider: provider.clone(),
        model: model.clone(),
    })
}

pub fn commit_sha_for_path(repo_path: &Path) -> String {
    futures::executor::block_on(run_git(repo_path, &["rev-parse", "HEAD"])).unwrap()
}

pub fn git_branch_for_path(repo_path: &Path) -> String {
    match std::env::var("GITHUB_REF_NAME") {
        Ok(branch) => branch,
        Err(_) => {
            futures::executor::block_on(run_git(repo_path, &["rev-parse", "--abbrev-ref", "HEAD"]))
                .unwrap_or_else(|_| "unknown".to_string())
        }
    }
}

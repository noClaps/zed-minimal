use client::Client;
use gpui::Application;
use http_client::HttpClientWithUrl;
use language::language_settings::AllLanguageSettings;
use project::Project;
use settings::SettingsStore;
use std::{path::Path, sync::Arc};

fn main() {
    zlog::init();

    use clock::FakeSystemClock;

    Application::new().run(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        language::init(cx);
        Project::init_settings(cx);
        SettingsStore::update(cx, |store, cx| {
            store.update_user_settings::<AllLanguageSettings>(cx, |_| {});
        });

        let clock = Arc::new(FakeSystemClock::new());

        let http = Arc::new(HttpClientWithUrl::new(
            Arc::new(
                reqwest_client::ReqwestClient::user_agent("Zed semantic index example").unwrap(),
            ),
            "http://localhost:11434",
            None,
        ));
        let client = client::Client::new(clock, http.clone(), cx);
        Client::set_global(client.clone(), cx);

        let args: Vec<String> = std::env::args().collect();
        if args.len() < 2 {
            eprintln!("Usage: cargo run --example index -p semantic_index -- <project_path>");
            cx.quit();
            return;
        }

        // let embedding_provider = semantic_index::FakeEmbeddingProvider;

        cx.spawn(async move |cx| {
            let project_path = Path::new(&args[1]);

            let project = Project::example([project_path], cx).await;

            cx.update(|cx| {
                let language_registry = project.read(cx).languages().clone();
                let node_runtime = project.read(cx).node_runtime().unwrap().clone();
                languages::init(language_registry, node_runtime, cx);
            })
            .unwrap();

            let index_start = std::time::Instant::now();
            println!("Index time: {:?}", index_start.elapsed());

            cx.background_executor()
                .timer(std::time::Duration::from_secs(100000))
                .await;

            cx.update(|cx| cx.quit()).unwrap();
        })
        .detach();
    });
}

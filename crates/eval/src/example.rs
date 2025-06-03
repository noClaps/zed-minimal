use std::{
    error::Error,
    fmt::{self, Debug},
};

use anyhow::Result;
use async_trait::async_trait;
use buffer_diff::DiffHunkStatus;
use gpui::{App, AppContext, AsyncApp, Entity};

#[async_trait(?Send)]
pub trait Example {
    fn meta(&self) -> ExampleMetadata;
}

#[derive(Clone, Debug)]
pub struct ExampleMetadata {
    pub name: String,
    pub url: String,
    pub revision: String,
    pub language_server: Option<LanguageServer>,
}

#[derive(Clone, Debug)]
pub struct LanguageServer {
    pub file_extension: String,
}

impl ExampleMetadata {
    pub fn repo_name(&self) -> String {
        self.url
            .split('/')
            .next_back()
            .unwrap_or(&"")
            .trim_end_matches(".git")
            .into()
    }
}

pub struct FailedAssertion(pub String);

impl fmt::Debug for FailedAssertion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Assertion failure: {}", self.0)
    }
}

impl fmt::Display for FailedAssertion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for FailedAssertion {}

pub struct ExampleContext {
    log_prefix: String,
    app: AsyncApp,
}

impl ExampleContext {
    #[allow(dead_code)]
    pub fn assert_eq<T: PartialEq + Debug>(
        &mut self,
        left: T,
        right: T,
        message: impl ToString,
    ) -> Result<()> {
        let message = message.to_string();
        self.log_assertion(
            if left == right {
                Ok(())
            } else {
                println!(
                    "{}{}",
                    self.log_prefix,
                    pretty_assertions::Comparison::new(&left, &right)
                );
                Err(anyhow::Error::from(FailedAssertion(message.clone())))
            },
            message,
        )
    }

    fn log_assertion<T>(&mut self, result: Result<T>, message: String) -> Result<T> {
        if result.is_ok() {
            println!("{}✅ {}", self.log_prefix, message);
        } else {
            println!("{}❌ {}", self.log_prefix, message);
        }

        result
    }

    pub async fn run_to_end(&mut self) -> Result<Response> {
        self.run_turns(u32::MAX).await
    }

    pub async fn run_turn(&mut self) -> Result<Response> {
        self.run_turns(1).await
    }

    pub async fn run_turns(&mut self, iterations: u32) -> Result<Response> {
        let (mut tx, mut rx) = mpsc::channel(1);

        let tool_metrics = self.tool_metrics.clone();
        let log_prefix = self.log_prefix.clone();
        let _subscription = self.app.subscribe(
            &self.agent_thread,
            move |thread, event: &ThreadEvent, cx| match event {
                ThreadEvent::ShowError(thread_error) => {
                    tx.try_send(Err(anyhow!(thread_error.clone()))).ok();
                }
                ThreadEvent::Stopped(reason) => match reason {
                    Ok(StopReason::EndTurn) => {
                        tx.close_channel();
                    }
                    Ok(StopReason::ToolUse) => {
                        if thread.read(cx).remaining_turns() == 0 {
                            tx.close_channel();
                        }
                    }
                    Ok(StopReason::MaxTokens) => {
                        tx.try_send(Err(anyhow!("Exceeded maximum tokens"))).ok();
                    }
                    Ok(StopReason::Refusal) => {
                        tx.try_send(Err(anyhow!("Model refused to generate content")))
                            .ok();
                    }
                    Err(err) => {
                        tx.try_send(Err(anyhow!(err.clone()))).ok();
                    }
                },
                ThreadEvent::NewRequest
                | ThreadEvent::StreamedAssistantText(_, _)
                | ThreadEvent::StreamedAssistantThinking(_, _)
                | ThreadEvent::UsePendingTools { .. }
                | ThreadEvent::CompletionCanceled => {}
                ThreadEvent::ToolUseLimitReached => {}
                ThreadEvent::ToolFinished {
                    tool_use_id,
                    pending_tool_use,
                    ..
                } => {
                    thread.update(cx, |thread, _cx| {
                        if let Some(tool_use) = pending_tool_use {
                            let mut tool_metrics = tool_metrics.lock().unwrap();
                            if let Some(tool_result) = thread.tool_result(&tool_use_id) {
                                let message = if tool_result.is_error {
                                    format!("✖︎ {}", tool_use.name)
                                } else {
                                    format!("✔︎ {}", tool_use.name)
                                };
                                println!("{log_prefix}{message}");
                                tool_metrics
                                    .insert(tool_result.tool_name.clone(), !tool_result.is_error);
                            } else {
                                let message =
                                    format!("TOOL FINISHED WITHOUT RESULT: {}", tool_use.name);
                                println!("{log_prefix}{message}");
                                tool_metrics.insert(tool_use.name.clone(), true);
                            }
                        }
                    });
                }
                ThreadEvent::InvalidToolInput { .. } => {
                    println!("{log_prefix} invalid tool input");
                }
                ThreadEvent::MissingToolUse {
                    tool_use_id: _,
                    ui_text,
                } => {
                    println!("{log_prefix} {ui_text}");
                }
                ThreadEvent::ToolConfirmationNeeded => {
                    panic!(
                        "{}Bug: Tool confirmation should not be required in eval",
                        log_prefix
                    );
                }
                ThreadEvent::StreamedCompletion
                | ThreadEvent::MessageAdded(_)
                | ThreadEvent::MessageEdited(_)
                | ThreadEvent::MessageDeleted(_)
                | ThreadEvent::SummaryChanged
                | ThreadEvent::SummaryGenerated
                | ThreadEvent::ReceivedTextChunk
                | ThreadEvent::StreamedToolUse { .. }
                | ThreadEvent::CheckpointChanged
                | ThreadEvent::CancelEditing => {
                    tx.try_send(Ok(())).ok();
                    if std::env::var("ZED_EVAL_DEBUG").is_ok() {
                        println!("{}Event: {:#?}", log_prefix, event);
                    }
                }
            },
        );

        let model = self.model.clone();

        let message_count_before = self.app.update_entity(&self.agent_thread, |thread, cx| {
            thread.set_remaining_turns(iterations);
            thread.send_to_model(model, CompletionIntent::UserPrompt, None, cx);
            thread.messages().len()
        })?;

        loop {
            select_biased! {
                result = rx.next() => {
                    if let Some(result) = result {
                        result?;
                    } else {
                        break;
                    }
                }
                _ = self.app.background_executor().timer(THREAD_EVENT_TIMEOUT).fuse() => {
                    anyhow::bail!("Agentic loop stalled - waited {THREAD_EVENT_TIMEOUT:?} without any events");
                }
            }
        }

        let messages = self.app.read_entity(&self.agent_thread, |thread, cx| {
            let mut messages = Vec::new();
            for message in thread.messages().skip(message_count_before) {
                messages.push(Message {
                    _role: message.role,
                    text: message.to_string(),
                    tool_use: thread
                        .tool_uses_for_message(message.id, cx)
                        .into_iter()
                        .map(|tool_use| ToolUse {
                            name: tool_use.name.to_string(),
                            value: tool_use.input,
                        })
                        .collect(),
                });
            }
            messages
        })?;

        let response = Response::new(messages);

        Ok(response)
    }

    pub fn edits(&self) -> HashMap<Arc<Path>, FileEdits> {
        self.agent_thread
            .read_with(&self.app, |thread, cx| {
                let action_log = thread.action_log().read(cx);
                HashMap::from_iter(action_log.changed_buffers(cx).into_iter().map(
                    |(buffer, diff)| {
                        let snapshot = buffer.read(cx).snapshot();

                        let file = snapshot.file().unwrap();
                        let diff = diff.read(cx);
                        let base_text = diff.base_text().text();

                        let hunks = diff
                            .hunks(&snapshot, cx)
                            .map(|hunk| FileEditHunk {
                                base_text: base_text[hunk.diff_base_byte_range.clone()].to_string(),
                                text: snapshot
                                    .text_for_range(hunk.range.clone())
                                    .collect::<String>(),
                                status: hunk.status(),
                            })
                            .collect();

                        (file.path().clone(), FileEdits { hunks })
                    },
                ))
            })
            .unwrap()
    }

    pub fn agent_thread(&self) -> Entity<Thread> {
        self.agent_thread.clone()
    }
}

impl AppContext for ExampleContext {
    type Result<T> = anyhow::Result<T>;

    fn new<T: 'static>(
        &mut self,
        build_entity: impl FnOnce(&mut gpui::Context<T>) -> T,
    ) -> Self::Result<Entity<T>> {
        self.app.new(build_entity)
    }

    fn reserve_entity<T: 'static>(&mut self) -> Self::Result<gpui::Reservation<T>> {
        self.app.reserve_entity()
    }

    fn insert_entity<T: 'static>(
        &mut self,
        reservation: gpui::Reservation<T>,
        build_entity: impl FnOnce(&mut gpui::Context<T>) -> T,
    ) -> Self::Result<Entity<T>> {
        self.app.insert_entity(reservation, build_entity)
    }

    fn update_entity<T, R>(
        &mut self,
        handle: &Entity<T>,
        update: impl FnOnce(&mut T, &mut gpui::Context<T>) -> R,
    ) -> Self::Result<R>
    where
        T: 'static,
    {
        self.app.update_entity(handle, update)
    }

    fn read_entity<T, R>(
        &self,
        handle: &Entity<T>,
        read: impl FnOnce(&T, &App) -> R,
    ) -> Self::Result<R>
    where
        T: 'static,
    {
        self.app.read_entity(handle, read)
    }

    fn update_window<T, F>(&mut self, window: gpui::AnyWindowHandle, f: F) -> Result<T>
    where
        F: FnOnce(gpui::AnyView, &mut gpui::Window, &mut App) -> T,
    {
        self.app.update_window(window, f)
    }

    fn read_window<T, R>(
        &self,
        window: &gpui::WindowHandle<T>,
        read: impl FnOnce(Entity<T>, &App) -> R,
    ) -> Result<R>
    where
        T: 'static,
    {
        self.app.read_window(window, read)
    }

    fn background_spawn<R>(
        &self,
        future: impl std::future::Future<Output = R> + Send + 'static,
    ) -> gpui::Task<R>
    where
        R: Send + 'static,
    {
        self.app.background_spawn(future)
    }

    fn read_global<G, R>(&self, callback: impl FnOnce(&G, &App) -> R) -> Self::Result<R>
    where
        G: gpui::Global,
    {
        self.app.read_global(callback)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct FileEdits {
    pub hunks: Vec<FileEditHunk>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct FileEditHunk {
    pub base_text: String,
    pub text: String,
    pub status: DiffHunkStatus,
}

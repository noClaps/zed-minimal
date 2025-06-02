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

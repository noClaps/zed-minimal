use crate::{App, PlatformDispatcher};
use async_task::Runnable;
use futures::channel::mpsc;
use smol::prelude::*;
use std::mem::ManuallyDrop;
use std::panic::Location;
use std::thread::{self, ThreadId};
use std::{
    fmt::Debug,
    marker::PhantomData,
    mem,
    num::NonZeroUsize,
    pin::Pin,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use util::TryFutureExt;
use waker_fn::waker_fn;

/// A pointer to the executor that is currently running,
/// for spawning background tasks.
#[derive(Clone)]
pub struct BackgroundExecutor {
    #[doc(hidden)]
    pub dispatcher: Arc<dyn PlatformDispatcher>,
}

/// A pointer to the executor that is currently running,
/// for spawning tasks on the main thread.
///
/// This is intentionally `!Send` via the `not_send` marker field. This is because
/// `ForegroundExecutor::spawn` does not require `Send` but checks at runtime that the future is
/// only polled from the same thread it was spawned from. These checks would fail when spawning
/// foreground tasks from from background threads.
#[derive(Clone)]
pub struct ForegroundExecutor {
    #[doc(hidden)]
    pub dispatcher: Arc<dyn PlatformDispatcher>,
    not_send: PhantomData<Rc<()>>,
}

/// Task is a primitive that allows work to happen in the background.
///
/// It implements [`Future`] so you can `.await` on it.
///
/// If you drop a task it will be cancelled immediately. Calling [`Task::detach`] allows
/// the task to continue running, but with no way to return a value.
#[must_use]
#[derive(Debug)]
pub struct Task<T>(TaskState<T>);

#[derive(Debug)]
enum TaskState<T> {
    /// A task that is ready to return a value
    Ready(Option<T>),

    /// A task that is currently running.
    Spawned(async_task::Task<T>),
}

impl<T> Task<T> {
    /// Creates a new task that will resolve with the value
    pub fn ready(val: T) -> Self {
        Task(TaskState::Ready(Some(val)))
    }

    /// Detaching a task runs it to completion in the background
    pub fn detach(self) {
        match self {
            Task(TaskState::Ready(_)) => {}
            Task(TaskState::Spawned(task)) => task.detach(),
        }
    }
}

impl<E, T> Task<Result<T, E>>
where
    T: 'static,
    E: 'static + Debug,
{
    /// Run the task to completion in the background and log any
    /// errors that occur.
    #[track_caller]
    pub fn detach_and_log_err(self, cx: &App) {
        let location = core::panic::Location::caller();
        cx.foreground_executor()
            .spawn(self.log_tracked_err(*location))
            .detach();
    }
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        match unsafe { self.get_unchecked_mut() } {
            Task(TaskState::Ready(val)) => Poll::Ready(val.take().unwrap()),
            Task(TaskState::Spawned(task)) => task.poll(cx),
        }
    }
}

/// A task label is an opaque identifier that you can use to
/// refer to a task in tests.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TaskLabel(NonZeroUsize);

impl Default for TaskLabel {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskLabel {
    /// Construct a new task label.
    pub fn new() -> Self {
        static NEXT_TASK_LABEL: AtomicUsize = AtomicUsize::new(1);
        Self(NEXT_TASK_LABEL.fetch_add(1, SeqCst).try_into().unwrap())
    }
}

type AnyLocalFuture<R> = Pin<Box<dyn 'static + Future<Output = R>>>;

type AnyFuture<R> = Pin<Box<dyn 'static + Send + Future<Output = R>>>;

/// BackgroundExecutor lets you run things on background threads.
/// In production this is a thread pool with no ordering guarantees.
/// In tests this is simulated by running tasks one by one in a deterministic
/// (but arbitrary) order controlled by the `SEED` environment variable.
impl BackgroundExecutor {
    #[doc(hidden)]
    pub fn new(dispatcher: Arc<dyn PlatformDispatcher>) -> Self {
        Self { dispatcher }
    }

    /// Enqueues the given future to be run to completion on a background thread.
    pub fn spawn<R>(&self, future: impl Future<Output = R> + Send + 'static) -> Task<R>
    where
        R: Send + 'static,
    {
        self.spawn_internal::<R>(Box::pin(future), None)
    }

    /// Enqueues the given future to be run to completion on a background thread.
    /// The given label can be used to control the priority of the task in tests.
    pub fn spawn_labeled<R>(
        &self,
        label: TaskLabel,
        future: impl Future<Output = R> + Send + 'static,
    ) -> Task<R>
    where
        R: Send + 'static,
    {
        self.spawn_internal::<R>(Box::pin(future), Some(label))
    }

    fn spawn_internal<R: Send + 'static>(
        &self,
        future: AnyFuture<R>,
        label: Option<TaskLabel>,
    ) -> Task<R> {
        let dispatcher = self.dispatcher.clone();
        let (runnable, task) =
            async_task::spawn(future, move |runnable| dispatcher.dispatch(runnable, label));
        runnable.schedule();
        Task(TaskState::Spawned(task))
    }

    /// Block the current thread until the given future resolves.
    /// Consider using `block_with_timeout` instead.
    pub fn block<R>(&self, future: impl Future<Output = R>) -> R {
        if let Ok(value) = self.block_internal(true, future, None) {
            value
        } else {
            unreachable!()
        }
    }

    pub(crate) fn block_internal<Fut: Future>(
        &self,
        _background_only: bool,
        future: Fut,
        timeout: Option<Duration>,
    ) -> Result<Fut::Output, impl Future<Output = Fut::Output> + use<Fut>> {
        use std::time::Instant;

        let mut future = Box::pin(future);
        if timeout == Some(Duration::ZERO) {
            return Err(future);
        }
        let deadline = timeout.map(|timeout| Instant::now() + timeout);

        let unparker = self.dispatcher.unparker();
        let waker = waker_fn(move || {
            unparker.unpark();
        });
        let mut cx = std::task::Context::from_waker(&waker);

        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(result) => return Ok(result),
                Poll::Pending => {
                    let timeout =
                        deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
                    if !self.dispatcher.park(timeout)
                        && deadline.is_some_and(|deadline| deadline < Instant::now())
                    {
                        return Err(future);
                    }
                }
            }
        }
    }

    /// Block the current thread until the given future resolves
    /// or `duration` has elapsed.
    pub fn block_with_timeout<Fut: Future>(
        &self,
        duration: Duration,
        future: Fut,
    ) -> Result<Fut::Output, impl Future<Output = Fut::Output> + use<Fut>> {
        self.block_internal(true, future, Some(duration))
    }

    /// Scoped lets you start a number of tasks and waits
    /// for all of them to complete before returning.
    pub async fn scoped<'scope, F>(&self, scheduler: F)
    where
        F: FnOnce(&mut Scope<'scope>),
    {
        let mut scope = Scope::new(self.clone());
        (scheduler)(&mut scope);
        let spawned = mem::take(&mut scope.futures)
            .into_iter()
            .map(|f| self.spawn(f))
            .collect::<Vec<_>>();
        for task in spawned {
            task.await;
        }
    }

    /// Get the current time.
    ///
    /// Calling this instead of `std::time::Instant::now` allows the use
    /// of fake timers in tests.
    pub fn now(&self) -> Instant {
        self.dispatcher.now()
    }

    /// Returns a task that will complete after the given duration.
    /// Depending on other concurrent tasks the elapsed duration may be longer
    /// than requested.
    pub fn timer(&self, duration: Duration) -> Task<()> {
        if duration.is_zero() {
            return Task::ready(());
        }
        let (runnable, task) = async_task::spawn(async move {}, {
            let dispatcher = self.dispatcher.clone();
            move |runnable| dispatcher.dispatch_after(duration, runnable)
        });
        runnable.schedule();
        Task(TaskState::Spawned(task))
    }

    /// How many CPUs are available to the dispatcher.
    pub fn num_cpus(&self) -> usize {
        return num_cpus::get();
    }

    /// Whether we're on the main thread.
    pub fn is_main_thread(&self) -> bool {
        self.dispatcher.is_main_thread()
    }
}

/// ForegroundExecutor runs things on the main thread.
impl ForegroundExecutor {
    /// Creates a new ForegroundExecutor from the given PlatformDispatcher.
    pub fn new(dispatcher: Arc<dyn PlatformDispatcher>) -> Self {
        Self {
            dispatcher,
            not_send: PhantomData,
        }
    }

    /// Enqueues the given Task to run on the main thread at some point in the future.
    #[track_caller]
    pub fn spawn<R>(&self, future: impl Future<Output = R> + 'static) -> Task<R>
    where
        R: 'static,
    {
        let dispatcher = self.dispatcher.clone();

        #[track_caller]
        fn inner<R: 'static>(
            dispatcher: Arc<dyn PlatformDispatcher>,
            future: AnyLocalFuture<R>,
        ) -> Task<R> {
            let (runnable, task) = spawn_local_with_source_location(future, move |runnable| {
                dispatcher.dispatch_on_main_thread(runnable)
            });
            runnable.schedule();
            Task(TaskState::Spawned(task))
        }
        inner::<R>(dispatcher, Box::pin(future))
    }
}

/// Variant of `async_task::spawn_local` that includes the source location of the spawn in panics.
///
/// Copy-modified from:
/// https://github.com/smol-rs/async-task/blob/ca9dbe1db9c422fd765847fa91306e30a6bb58a9/src/runnable.rs#L405
#[track_caller]
fn spawn_local_with_source_location<Fut, S>(
    future: Fut,
    schedule: S,
) -> (Runnable<()>, async_task::Task<Fut::Output, ()>)
where
    Fut: Future + 'static,
    Fut::Output: 'static,
    S: async_task::Schedule<()> + Send + Sync + 'static,
{
    #[inline]
    fn thread_id() -> ThreadId {
        std::thread_local! {
            static ID: ThreadId = thread::current().id();
        }
        ID.try_with(|id| *id)
            .unwrap_or_else(|_| thread::current().id())
    }

    struct Checked<F> {
        id: ThreadId,
        inner: ManuallyDrop<F>,
        location: &'static Location<'static>,
    }

    impl<F> Drop for Checked<F> {
        fn drop(&mut self) {
            assert!(
                self.id == thread_id(),
                "local task dropped by a thread that didn't spawn it. Task spawned at {}",
                self.location
            );
            unsafe {
                ManuallyDrop::drop(&mut self.inner);
            }
        }
    }

    impl<F: Future> Future for Checked<F> {
        type Output = F::Output;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            assert!(
                self.id == thread_id(),
                "local task polled by a thread that didn't spawn it. Task spawned at {}",
                self.location
            );
            unsafe { self.map_unchecked_mut(|c| &mut *c.inner).poll(cx) }
        }
    }

    // Wrap the future into one that checks which thread it's on.
    let future = Checked {
        id: thread_id(),
        inner: ManuallyDrop::new(future),
        location: Location::caller(),
    };

    unsafe { async_task::spawn_unchecked(future, schedule) }
}

/// Scope manages a set of tasks that are enqueued and waited on together. See [`BackgroundExecutor::scoped`].
pub struct Scope<'a> {
    executor: BackgroundExecutor,
    futures: Vec<Pin<Box<dyn Future<Output = ()> + Send + 'static>>>,
    tx: Option<mpsc::Sender<()>>,
    rx: mpsc::Receiver<()>,
    lifetime: PhantomData<&'a ()>,
}

impl<'a> Scope<'a> {
    fn new(executor: BackgroundExecutor) -> Self {
        let (tx, rx) = mpsc::channel(1);
        Self {
            executor,
            tx: Some(tx),
            rx,
            futures: Default::default(),
            lifetime: PhantomData,
        }
    }

    /// How many CPUs are available to the dispatcher.
    pub fn num_cpus(&self) -> usize {
        self.executor.num_cpus()
    }

    /// Spawn a future into this scope.
    pub fn spawn<F>(&mut self, f: F)
    where
        F: Future<Output = ()> + Send + 'a,
    {
        let tx = self.tx.clone().unwrap();

        // SAFETY: The 'a lifetime is guaranteed to outlive any of these futures because
        // dropping this `Scope` blocks until all of the futures have resolved.
        let f = unsafe {
            mem::transmute::<
                Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
                Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
            >(Box::pin(async move {
                f.await;
                drop(tx);
            }))
        };
        self.futures.push(f);
    }
}

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        self.tx.take().unwrap();

        // Wait until the channel is closed, which means that all of the spawned
        // futures have resolved.
        self.executor.block(self.rx.next());
    }
}

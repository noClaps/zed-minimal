mod error;

pub use error::*;
use parking_lot::{RwLock, RwLockReadGuard, RwLockUpgradableReadGuard};
use std::{
    collections::BTreeMap,
    mem,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};

pub fn channel<T>(value: T) -> (Sender<T>, Receiver<T>) {
    let state = Arc::new(RwLock::new(State {
        value,
        wakers: BTreeMap::new(),
        next_waker_id: WakerId::default(),
        version: 0,
        closed: false,
    }));

    (
        Sender {
            state: state.clone(),
        },
        Receiver { state, version: 0 },
    )
}

#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct WakerId(usize);

impl WakerId {
    fn post_inc(&mut self) -> Self {
        let id = *self;
        self.0 = id.0.wrapping_add(1);
        *self
    }
}

struct State<T> {
    value: T,
    wakers: BTreeMap<WakerId, Waker>,
    next_waker_id: WakerId,
    version: usize,
    closed: bool,
}

pub struct Sender<T> {
    state: Arc<RwLock<State<T>>>,
}

impl<T> Sender<T> {
    pub fn receiver(&self) -> Receiver<T> {
        let version = self.state.read().version;
        Receiver {
            state: self.state.clone(),
            version,
        }
    }

    pub fn send(&mut self, value: T) -> Result<(), NoReceiverError> {
        if let Some(state) = Arc::get_mut(&mut self.state) {
            let state = state.get_mut();
            state.value = value;
            debug_assert_eq!(state.wakers.len(), 0);
            Err(NoReceiverError)
        } else {
            let mut state = self.state.write();
            state.value = value;
            state.version = state.version.wrapping_add(1);
            let wakers = mem::take(&mut state.wakers);
            drop(state);

            for (_, waker) in wakers {
                waker.wake();
            }

            Ok(())
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut state = self.state.write();
        state.closed = true;
        for (_, waker) in mem::take(&mut state.wakers) {
            waker.wake();
        }
    }
}

#[derive(Clone)]
pub struct Receiver<T> {
    state: Arc<RwLock<State<T>>>,
    version: usize,
}

struct Changed<'a, T> {
    receiver: &'a mut Receiver<T>,
    pending_waker_id: Option<WakerId>,
}

impl<T> Future for Changed<'_, T> {
    type Output = Result<(), NoSenderError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = &mut *self;

        let state = this.receiver.state.upgradable_read();
        if state.version != this.receiver.version {
            // The sender produced a new value. Avoid unregistering the pending
            // waker, because the sender has already done so.
            this.pending_waker_id = None;
            this.receiver.version = state.version;
            Poll::Ready(Ok(()))
        } else if state.closed {
            Poll::Ready(Err(NoSenderError))
        } else {
            let mut state = RwLockUpgradableReadGuard::upgrade(state);

            // Unregister the pending waker. This should happen automatically
            // when the waker gets awoken by the sender, but if this future was
            // polled again without an explicit call to `wake` (e.g., a spurious
            // wake by the executor), we need to remove it manually.
            if let Some(pending_waker_id) = this.pending_waker_id.take() {
                state.wakers.remove(&pending_waker_id);
            }

            // Register the waker for this future.
            let waker_id = state.next_waker_id.post_inc();
            state.wakers.insert(waker_id, cx.waker().clone());
            this.pending_waker_id = Some(waker_id);

            Poll::Pending
        }
    }
}

impl<T> Drop for Changed<'_, T> {
    fn drop(&mut self) {
        // If this future gets dropped before the waker has a chance of being
        // awoken, we need to clear it to avoid a memory leak.
        if let Some(waker_id) = self.pending_waker_id {
            let mut state = self.receiver.state.write();
            state.wakers.remove(&waker_id);
        }
    }
}

impl<T> Receiver<T> {
    pub fn borrow(&mut self) -> parking_lot::MappedRwLockReadGuard<T> {
        let state = self.state.read();
        self.version = state.version;
        RwLockReadGuard::map(state, |state| &state.value)
    }

    pub fn changed(&mut self) -> impl Future<Output = Result<(), NoSenderError>> {
        Changed {
            receiver: self,
            pending_waker_id: None,
        }
    }
}

impl<T: Clone> Receiver<T> {
    pub async fn recv(&mut self) -> Result<T, NoSenderError> {
        self.changed().await?;
        Ok(self.borrow().clone())
    }
}

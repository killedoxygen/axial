//! Cooperative cancellation shared by transfers and runtime materialization.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::watch;

struct Shared {
    cancelled: AtomicBool,
    changed: watch::Sender<bool>,
}

/// Cancels every clone of its paired [`Cancellation`]. Dropping it does not cancel.
#[derive(Clone)]
pub(crate) struct Canceller {
    shared: Arc<Shared>,
}

#[derive(Clone)]
pub struct Cancellation {
    shared: Arc<Shared>,
    changed: watch::Receiver<bool>,
}

/// Synchronous view of one or more cancellations for blocking work.
#[derive(Clone)]
pub(crate) struct ThreadCancellation {
    sources: Vec<Arc<Shared>>,
}

pub(crate) fn channel() -> (Canceller, Cancellation) {
    let (changed, receiver) = watch::channel(false);
    let shared = Arc::new(Shared {
        cancelled: AtomicBool::new(false),
        changed,
    });
    (
        Canceller {
            shared: Arc::clone(&shared),
        },
        Cancellation {
            shared,
            changed: receiver,
        },
    )
}

impl Canceller {
    pub(crate) fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::Release);
        let _ = self.shared.changed.send(true);
    }
}

impl Cancellation {
    pub fn is_cancelled(&self) -> bool {
        self.shared.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn canceller(&self) -> Canceller {
        Canceller {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Resolves once cancelled; never resolves otherwise.
    pub(crate) async fn cancelled(&mut self) {
        while !self.is_cancelled() {
            // The shared state owns the sender, so the channel cannot close.
            let _ = self.changed.changed().await;
        }
    }

    pub(crate) async fn wait<T>(&mut self, future: impl Future<Output = T>) -> Option<T> {
        tokio::select! {
            biased;
            () = self.cancelled() => None,
            result = future => Some(result),
        }
    }

    pub(crate) fn thread(&self) -> ThreadCancellation {
        ThreadCancellation {
            sources: vec![Arc::clone(&self.shared)],
        }
    }
}

impl fmt::Debug for Cancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Cancellation")
            .finish_non_exhaustive()
    }
}

impl ThreadCancellation {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.sources
            .iter()
            .any(|shared| shared.cancelled.load(Ordering::Acquire))
    }

    /// Also observe `other`.
    pub(crate) fn or(mut self, other: &Cancellation) -> Self {
        self.sources.push(Arc::clone(&other.shared));
        self
    }
}

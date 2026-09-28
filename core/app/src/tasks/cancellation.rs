use std::sync::Arc;
use tokio::sync::watch;

/// A cooperative cancellation request, not a terminal operation outcome.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    requested: Arc<watch::Sender<bool>>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            requested: Arc::new(watch::channel(false).0),
        }
    }

    /// Returns true only for the first cancellation request.
    pub fn cancel(&self) -> bool {
        self.requested.send_if_modified(|requested| {
            if *requested {
                false
            } else {
                *requested = true;
                true
            }
        })
    }

    pub fn is_cancelled(&self) -> bool {
        *self.requested.borrow()
    }

    /// Observes cancellation even when it happened before this waiter existed.
    pub async fn cancelled(&self) {
        let mut receiver = self.requested.subscribe();
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_before_subscription_is_observed() {
        let token = CancellationToken::new();
        assert!(token.cancel());
        assert!(!token.clone().cancel());
        token.cancelled().await;
        assert!(token.is_cancelled());
    }
}

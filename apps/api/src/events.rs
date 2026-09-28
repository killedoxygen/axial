//! Full public projections only. Feature owners retain their domain state machines.
use serde::Serialize;
use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;

/// A JSON-safe, monotonically increasing revision and its complete projection.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct EventSnapshot<T> {
    pub revision: u64,
    pub value: T,
}

const MAX_REVISION: u64 = (1_u64 << 53) - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventError {
    Unavailable,
    Closed,
    RevisionExhausted,
    StaleRevision,
}

struct Current<T> {
    snapshot: EventSnapshot<T>,
    sender: Option<broadcast::Sender<EventSnapshot<T>>>,
}

/// Snapshot replacement and receiver registration use the same short lock.
pub struct RevisionedEvents<T> {
    current: Arc<Mutex<Current<T>>>,
}

impl<T> Clone for RevisionedEvents<T> {
    fn clone(&self) -> Self {
        Self {
            current: self.current.clone(),
        }
    }
}

impl<T: Clone> RevisionedEvents<T> {
    pub fn new(value: T, capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.clamp(1, 1024));
        Self {
            current: Arc::new(Mutex::new(Current {
                snapshot: EventSnapshot { revision: 1, value },
                sender: Some(sender),
            })),
        }
    }

    pub fn snapshot(&self) -> Result<EventSnapshot<T>, EventError> {
        Ok(self
            .current
            .lock()
            .map_err(|_| EventError::Unavailable)?
            .snapshot
            .clone())
    }

    pub fn publish(&self, value: T) -> Result<EventSnapshot<T>, EventError> {
        let mut current = self.current.lock().map_err(|_| EventError::Unavailable)?;
        let revision = current
            .snapshot
            .revision
            .checked_add(1)
            .filter(|revision| *revision <= MAX_REVISION)
            .ok_or(EventError::RevisionExhausted)?;
        Self::replace(&mut current, revision, value)
    }

    /// Use when a feature already owns the revision used by its status reads.
    pub fn publish_at(&self, revision: u64, value: T) -> Result<EventSnapshot<T>, EventError> {
        let mut current = self.current.lock().map_err(|_| EventError::Unavailable)?;
        if revision <= current.snapshot.revision {
            return Err(EventError::StaleRevision);
        }
        if revision > MAX_REVISION {
            return Err(EventError::RevisionExhausted);
        }
        Self::replace(&mut current, revision, value)
    }

    fn replace(
        current: &mut Current<T>,
        revision: u64,
        value: T,
    ) -> Result<EventSnapshot<T>, EventError> {
        let sender = current.sender.as_ref().ok_or(EventError::Closed)?;
        let snapshot = EventSnapshot { revision, value };
        current.snapshot = snapshot.clone();
        // No receivers is normal: a later subscriber obtains current atomically.
        let _ = sender.send(snapshot.clone());
        Ok(snapshot)
    }

    pub fn subscribe(&self) -> Result<EventSubscription<T>, EventError> {
        let current = self.current.lock().map_err(|_| EventError::Unavailable)?;
        Ok(EventSubscription {
            source: self.clone(),
            receiver: current
                .sender
                .as_ref()
                .ok_or(EventError::Closed)?
                .subscribe(),
            pending: Some(current.snapshot.clone()),
            last_revision: 0,
        })
    }

    /// Stop admitting subscriptions/updates and let existing streams drain and close.
    /// The last projection remains available for authoritative status reads.
    pub fn close(&self) -> Result<(), EventError> {
        self.current
            .lock()
            .map_err(|_| EventError::Unavailable)?
            .sender
            .take();
        Ok(())
    }
}

pub struct EventSubscription<T> {
    source: RevisionedEvents<T>,
    receiver: broadcast::Receiver<EventSnapshot<T>>,
    pending: Option<EventSnapshot<T>>,
    last_revision: u64,
}

impl<T: Clone> EventSubscription<T> {
    pub async fn recv(&mut self) -> Result<EventSnapshot<T>, EventError> {
        loop {
            let next = if let Some(snapshot) = self.pending.take() {
                snapshot
            } else {
                match self.receiver.recv().await {
                    Ok(snapshot) => snapshot,
                    Err(broadcast::error::RecvError::Closed) => return Err(EventError::Closed),
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let current = self
                            .source
                            .current
                            .lock()
                            .map_err(|_| EventError::Unavailable)?;
                        if let Some(sender) = &current.sender {
                            self.receiver = sender.subscribe();
                        }
                        current.snapshot.clone()
                    }
                }
            };
            if next.revision > self.last_revision {
                self.last_revision = next.revision;
                return Ok(next);
            }
        }
    }
}

/// Routes can use this for default message events or build named status/log events.
/// Authentication is applied by the outer transport router before subscribing.
pub fn snapshot_sse<T: Clone + Serialize + Send + 'static>(
    mut subscription: EventSubscription<T>,
) -> axum::response::Sse<
    impl futures_util::Stream<Item = Result<axum::response::sse::Event, Infallible>> + Send,
> {
    use axum::response::{
        Sse,
        sse::{Event, KeepAlive},
    };
    let stream = async_stream::stream! {
        while let Ok(snapshot) = subscription.recv().await {
            match Event::default().id(snapshot.revision.to_string()).json_data(&snapshot) {
                Ok(event) => yield Ok(event),
                // Never disguise a serialization failure as a current snapshot.
                Err(_) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscription_couples_snapshot_to_later_updates() {
        let events = RevisionedEvents::new("before", 8);
        let mut subscription = events.subscribe().unwrap();
        events.publish("after").unwrap();
        assert_eq!(
            subscription.recv().await.unwrap(),
            EventSnapshot {
                revision: 1,
                value: "before"
            }
        );
        assert_eq!(
            subscription.recv().await.unwrap(),
            EventSnapshot {
                revision: 2,
                value: "after"
            }
        );
    }

    #[tokio::test]
    async fn lag_rebases_to_current_and_remains_subscribed() {
        let events = RevisionedEvents::new(0, 1);
        let mut subscription = events.subscribe().unwrap();
        assert_eq!(subscription.recv().await.unwrap().value, 0);
        for value in 1..=8 {
            events.publish(value).unwrap();
        }
        assert_eq!(
            subscription.recv().await.unwrap(),
            EventSnapshot {
                revision: 9,
                value: 8
            }
        );
        events.publish(9).unwrap();
        assert_eq!(subscription.recv().await.unwrap().value, 9);
    }

    #[test]
    fn stale_or_unrepresentable_revision_cannot_replace_projection() {
        let events = RevisionedEvents::new("before", 1);
        assert_eq!(
            events.publish_at(1, "stale"),
            Err(EventError::StaleRevision)
        );
        assert_eq!(
            events.publish_at(MAX_REVISION + 1, "unsafe"),
            Err(EventError::RevisionExhausted)
        );
        assert_eq!(events.snapshot().unwrap().value, "before");
    }

    #[tokio::test]
    async fn close_terminates_streams_and_prevents_further_admission() {
        let events = RevisionedEvents::new("last", 1);
        let mut subscription = events.subscribe().unwrap();
        events.close().unwrap();
        assert_eq!(subscription.recv().await.unwrap().value, "last");
        assert_eq!(subscription.recv().await, Err(EventError::Closed));
        assert!(matches!(events.subscribe(), Err(EventError::Closed)));
        assert_eq!(events.publish("later"), Err(EventError::Closed));
        assert_eq!(events.snapshot().unwrap().value, "last");
    }

    #[tokio::test]
    async fn lagged_stream_delivers_final_projection_before_closing() {
        let events = RevisionedEvents::new(0, 1);
        let mut subscription = events.subscribe().unwrap();
        assert_eq!(subscription.recv().await.unwrap().value, 0);
        events.publish(1).unwrap();
        events.publish(2).unwrap();
        events.close().unwrap();
        assert_eq!(subscription.recv().await.unwrap().value, 2);
        assert_eq!(subscription.recv().await, Err(EventError::Closed));
    }
}

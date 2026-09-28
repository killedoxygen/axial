use std::{
    fmt,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

/// An in-memory latest projection. `value` and its terminal meaning belong to
/// the feature. Recovery must read the feature's durable state where applicable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Revisioned<T> {
    pub revision: u64,
    pub value: T,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionError {
    StaleRevision { current: u64 },
    Finished,
    RevisionExhausted,
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ProjectionError {}

struct State<T> {
    current: Revisioned<T>,
    finished: bool,
}

struct Inner<T> {
    state: Mutex<State<T>>,
    latest: watch::Sender<Revisioned<T>>,
}

/// Create a fresh projection for every domain operation incarnation. An old
/// publisher then has no authority over the replacement operation's snapshot.
#[derive(Clone)]
pub struct Projection<T> {
    inner: Arc<Inner<T>>,
}

impl<T: Clone> Projection<T> {
    pub fn new(value: T) -> Self {
        let current = Revisioned { revision: 1, value };
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    current: current.clone(),
                    finished: false,
                }),
                latest: watch::channel(current).0,
            }),
        }
    }

    pub fn snapshot(&self) -> Revisioned<T> {
        self.inner
            .state
            .lock()
            .expect("projection lock poisoned")
            .current
            .clone()
    }

    /// Snapshot and subscription are captured under the same publication lock.
    /// Every following notification contains a complete replacement snapshot.
    pub fn subscribe(&self) -> (Revisioned<T>, ProjectionSubscription<T>) {
        let state = self.inner.state.lock().expect("projection lock poisoned");
        (
            state.current.clone(),
            ProjectionSubscription {
                receiver: self.inner.latest.subscribe(),
            },
        )
    }

    pub fn publish(&self, expected: u64, value: T) -> Result<Revisioned<T>, ProjectionError> {
        self.replace(expected, value, false)
    }

    /// Publish a feature-decided terminal projection exactly once. Cancellation
    /// requests must not call this until that feature's effects have settled.
    pub fn finish(&self, expected: u64, value: T) -> Result<Revisioned<T>, ProjectionError> {
        self.replace(expected, value, true)
    }

    fn replace(
        &self,
        expected: u64,
        value: T,
        finished: bool,
    ) -> Result<Revisioned<T>, ProjectionError> {
        let mut state = self.inner.state.lock().expect("projection lock poisoned");
        if state.finished {
            return Err(ProjectionError::Finished);
        }
        if state.current.revision != expected {
            return Err(ProjectionError::StaleRevision {
                current: state.current.revision,
            });
        }
        let revision = expected
            .checked_add(1)
            .ok_or(ProjectionError::RevisionExhausted)?;
        state.current = Revisioned { revision, value };
        state.finished = finished;
        self.inner.latest.send_replace(state.current.clone());
        Ok(state.current.clone())
    }
}

pub struct ProjectionSubscription<T> {
    receiver: watch::Receiver<Revisioned<T>>,
}

impl<T: Clone> ProjectionSubscription<T> {
    /// Slow subscribers rebase directly to the latest complete snapshot instead
    /// of accumulating an unbounded event backlog or replaying mutations.
    pub async fn changed(&mut self) -> Option<Revisioned<T>> {
        self.receiver.changed().await.ok()?;
        Some(self.receiver.borrow_and_update().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Barrier, thread};

    #[tokio::test]
    async fn subscribed_snapshot_has_no_gap_and_slow_reader_rebases() {
        let projection = Projection::new("queued");
        let (initial, mut subscription) = projection.subscribe();
        assert_eq!(initial.revision, 1);
        projection.publish(1, "running").unwrap();
        projection.finish(2, "completed").unwrap();
        assert_eq!(
            subscription.changed().await,
            Some(Revisioned {
                revision: 3,
                value: "completed"
            })
        );
    }

    #[test]
    fn stale_or_second_terminal_completion_cannot_overwrite_state() {
        let projection = Projection::new("queued");
        let old_publisher = projection.clone();
        projection.publish(1, "running").unwrap();
        assert_eq!(
            old_publisher.finish(1, "cancelled"),
            Err(ProjectionError::StaleRevision { current: 2 })
        );
        projection.finish(2, "completed").unwrap();
        assert_eq!(
            projection.finish(3, "failed"),
            Err(ProjectionError::Finished)
        );
        assert_eq!(projection.snapshot().value, "completed");
        let new_incarnation = Projection::new("queued");
        assert!(old_publisher.publish(3, "stale").is_err());
        assert_eq!(new_incarnation.snapshot().value, "queued");
    }

    #[test]
    fn simultaneous_terminal_publications_choose_one_revision_and_outcome() {
        for _ in 0..16 {
            let projection = Projection::new("running");
            let start = Arc::new(Barrier::new(3));
            let publishers = ["completed", "cancelled"].map(|outcome| {
                let projection = projection.clone();
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    start.wait();
                    projection.finish(1, outcome)
                })
            });
            start.wait();
            let outcomes = publishers.map(|publisher| publisher.join().unwrap());
            assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
            assert_eq!(
                outcomes
                    .iter()
                    .filter(|outcome| matches!(outcome, Err(ProjectionError::Finished)))
                    .count(),
                1
            );
            let winner = outcomes.into_iter().find_map(Result::ok).unwrap();
            assert_eq!(winner.revision, 2);
            assert_eq!(projection.snapshot(), winner);
        }
    }
}

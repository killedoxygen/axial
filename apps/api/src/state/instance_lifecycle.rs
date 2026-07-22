use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Clone, Default)]
pub(super) struct InstanceLifecycleGates {
    gates: Arc<Mutex<HashMap<String, Weak<InstanceLifecycleGate>>>>,
}

struct InstanceLifecycleGate {
    lock: Arc<Mutex<()>>,
    retired: AtomicBool,
}

#[derive(Clone)]
pub(super) struct InstanceLifecycleIncarnation {
    gate: Arc<InstanceLifecycleGate>,
}

pub(super) struct InstanceLifecycleGuard {
    guard: OwnedMutexGuard<()>,
    incarnation: InstanceLifecycleIncarnation,
}

impl InstanceLifecycleIncarnation {
    pub(super) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.gate, &other.gate)
    }

    pub(super) fn retire(&self) {
        self.gate.retired.store(true, Ordering::Release);
    }

    fn is_retired(&self) -> bool {
        self.gate.retired.load(Ordering::Acquire)
    }
}

impl InstanceLifecycleGuard {
    pub(super) fn into_parts(
        self,
    ) -> (OwnedMutexGuard<()>, InstanceLifecycleIncarnation) {
        (self.guard, self.incarnation)
    }

    #[cfg(test)]
    pub(super) fn incarnation(&self) -> &InstanceLifecycleIncarnation {
        &self.incarnation
    }
}

impl InstanceLifecycleGates {
    pub(super) fn owns(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.gates, &other.gates)
    }

    pub(super) async fn acquire(&self, instance_id: &str) -> InstanceLifecycleGuard {
        self.acquire_from(instance_id, None).await
    }

    async fn acquire_from(
        &self,
        instance_id: &str,
        mut candidate: Option<Arc<InstanceLifecycleGate>>,
    ) -> InstanceLifecycleGuard {
        loop {
            let gate = match candidate.take() {
                Some(gate) => gate,
                None => self.gate(instance_id).await,
            };
            let guard = Arc::clone(&gate.lock).lock_owned().await;
            let incarnation = InstanceLifecycleIncarnation { gate };
            if incarnation.is_retired() {
                drop(guard);
                self.remove_retired(instance_id, &incarnation.gate).await;
                continue;
            }
            return InstanceLifecycleGuard {
                guard,
                incarnation,
            };
        }
    }

    pub(super) async fn try_acquire(
        &self,
        instance_id: &str,
    ) -> Option<InstanceLifecycleGuard> {
        loop {
            let gate = self.gate(instance_id).await;
            let guard = Arc::clone(&gate.lock).try_lock_owned().ok()?;
            let incarnation = InstanceLifecycleIncarnation { gate };
            if incarnation.is_retired() {
                drop(guard);
                self.remove_retired(instance_id, &incarnation.gate).await;
                continue;
            }
            return Some(InstanceLifecycleGuard {
                guard,
                incarnation,
            });
        }
    }

    async fn gate(&self, instance_id: &str) -> Arc<InstanceLifecycleGate> {
        let mut gates = self.gates.lock().await;
        gates.retain(|_, gate| gate.strong_count() > 0);
        match gates.get(instance_id).and_then(Weak::upgrade) {
            Some(gate) => gate,
            _ => {
                let gate = Arc::new(InstanceLifecycleGate {
                    lock: Arc::new(Mutex::new(())),
                    retired: AtomicBool::new(false),
                });
                gates.insert(instance_id.to_string(), Arc::downgrade(&gate));
                gate
            }
        }
    }

    async fn remove_retired(&self, instance_id: &str, retired: &Arc<InstanceLifecycleGate>) {
        let mut gates = self.gates.lock().await;
        if gates
            .get(instance_id)
            .and_then(Weak::upgrade)
            .is_some_and(|current| Arc::ptr_eq(&current, retired))
        {
            gates.remove(instance_id);
        }
    }

    #[cfg(test)]
    pub(super) async fn is_held(&self, instance_id: &str) -> bool {
        let gate = self
            .gates
            .lock()
            .await
            .get(instance_id)
            .and_then(Weak::upgrade);
        gate.is_some_and(|gate| Arc::clone(&gate.lock).try_lock_owned().is_err())
    }
}

#[cfg(test)]
mod tests {
    use super::InstanceLifecycleGates;
    use std::sync::Arc;

    #[tokio::test]
    async fn try_acquire_reuses_the_exact_instance_gate_without_waiting() {
        let gates = InstanceLifecycleGates::default();
        let held = gates.acquire("instance").await;

        assert!(gates.try_acquire("instance").await.is_none());

        drop(held);
        assert!(gates.try_acquire("instance").await.is_some());
    }

    #[tokio::test]
    async fn queued_waiter_rejects_the_retired_incarnation_after_locking() {
        use std::time::Duration;

        let gates = InstanceLifecycleGates::default();
        let retired = gates.acquire("instance").await;
        let retired_incarnation = retired.incarnation().clone();
        let queued_candidate = gates.gate("instance").await;
        assert!(Arc::ptr_eq(
            &queued_candidate,
            &retired_incarnation.gate
        ));
        let queued_gates = gates.clone();
        let mut waiter = tokio::spawn(async move {
            queued_gates
                .acquire_from("instance", Some(queued_candidate))
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut waiter)
                .await
                .is_err(),
            "waiter must queue on the held old gate"
        );

        retired_incarnation.retire();
        assert!(
            gates.try_acquire("instance").await.is_none(),
            "retirement must not publish a fresh gate before the old holder releases"
        );
        drop(retired);

        let replacement = waiter.await.expect("queued lifecycle waiter");
        assert!(!retired_incarnation.same(replacement.incarnation()));
    }
}

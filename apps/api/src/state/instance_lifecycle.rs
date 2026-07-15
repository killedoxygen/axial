use std::collections::HashMap;
use std::sync::{Arc, Weak};
use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Clone, Default)]
pub(super) struct InstanceLifecycleGates {
    gates: Arc<Mutex<HashMap<String, Weak<Mutex<()>>>>>,
}

impl InstanceLifecycleGates {
    pub(super) fn owns(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.gates, &other.gates)
    }

    pub(super) async fn acquire(&self, instance_id: &str) -> OwnedMutexGuard<()> {
        self.gate(instance_id).await.lock_owned().await
    }

    pub(super) async fn try_acquire(&self, instance_id: &str) -> Option<OwnedMutexGuard<()>> {
        self.gate(instance_id).await.try_lock_owned().ok()
    }

    async fn gate(&self, instance_id: &str) -> Arc<Mutex<()>> {
        let mut gates = self.gates.lock().await;
        gates.retain(|_, gate| gate.strong_count() > 0);
        match gates.get(instance_id).and_then(Weak::upgrade) {
            Some(gate) => gate,
            None => {
                let gate = Arc::new(Mutex::new(()));
                gates.insert(instance_id.to_string(), Arc::downgrade(&gate));
                gate
            }
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
        gate.is_some_and(|gate| gate.try_lock_owned().is_err())
    }
}

#[cfg(test)]
mod tests {
    use super::InstanceLifecycleGates;

    #[tokio::test]
    async fn try_acquire_reuses_the_exact_instance_gate_without_waiting() {
        let gates = InstanceLifecycleGates::default();
        let held = gates.acquire("instance").await;

        assert!(gates.try_acquire("instance").await.is_none());

        drop(held);
        assert!(gates.try_acquire("instance").await.is_some());
    }
}

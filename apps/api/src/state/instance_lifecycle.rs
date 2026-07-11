use std::collections::HashMap;
use std::sync::{Arc, Weak};
use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Clone, Default)]
pub(super) struct InstanceLifecycleGates {
    gates: Arc<Mutex<HashMap<String, Weak<Mutex<()>>>>>,
}

impl InstanceLifecycleGates {
    pub(super) async fn acquire(&self, instance_id: &str) -> OwnedMutexGuard<()> {
        let gate = {
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
        };
        gate.lock_owned().await
    }
}

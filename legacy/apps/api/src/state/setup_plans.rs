use std::any::Any;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) const SETUP_PLAN_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_SETUP_PLANS: usize = 64;
const SETUP_PLAN_STORE_LOCK_INVARIANT: &str =
    "instance setup plan store lock poisoned; plan ownership may be inconsistent";

pub(crate) struct SetupPlanStore {
    ttl: Duration,
    max_entries: usize,
    inner: Mutex<SetupPlanStoreInner>,
}

struct SetupPlanStoreInner {
    closed: bool,
    plans: HashMap<String, SetupPlanEntry>,
}

struct SetupPlanEntry {
    expires_at: Instant,
    payload: Box<dyn Any + Send>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SetupPlanInsertError {
    Closed,
    Full,
}

#[derive(Debug)]
pub(crate) enum SetupPlanTake<T> {
    Found(T),
    Missing,
    Expired,
}

impl SetupPlanStore {
    pub(crate) fn new() -> Self {
        Self::with_limits(SETUP_PLAN_TTL, MAX_SETUP_PLANS)
    }

    fn with_limits(ttl: Duration, max_entries: usize) -> Self {
        Self {
            ttl,
            max_entries,
            inner: Mutex::new(SetupPlanStoreInner {
                closed: false,
                plans: HashMap::new(),
            }),
        }
    }

    pub(crate) fn insert<T>(&self, payload: T) -> Result<String, SetupPlanInsertError>
    where
        T: Send + 'static,
    {
        let now = Instant::now();
        let mut inner = self.inner.lock().expect(SETUP_PLAN_STORE_LOCK_INVARIANT);
        if inner.closed {
            return Err(SetupPlanInsertError::Closed);
        }
        inner.plans.retain(|_, plan| plan.expires_at > now);
        if inner.plans.len() >= self.max_entries {
            return Err(SetupPlanInsertError::Full);
        }

        let plan_id = loop {
            let candidate = format!("setup-{}", uuid::Uuid::new_v4().simple());
            if !inner.plans.contains_key(&candidate) {
                break candidate;
            }
        };
        inner.plans.insert(
            plan_id.clone(),
            SetupPlanEntry {
                expires_at: now + self.ttl,
                payload: Box::new(payload),
            },
        );
        Ok(plan_id)
    }

    pub(crate) fn take<T>(&self, plan_id: &str) -> SetupPlanTake<T>
    where
        T: Send + 'static,
    {
        let entry = self
            .inner
            .lock()
            .expect(SETUP_PLAN_STORE_LOCK_INVARIANT)
            .plans
            .remove(plan_id);
        let Some(entry) = entry else {
            return SetupPlanTake::Missing;
        };
        if entry.expires_at <= Instant::now() {
            return SetupPlanTake::Expired;
        }
        let payload = entry
            .payload
            .downcast::<T>()
            .expect("instance setup plan payload type must match its owning workflow");
        SetupPlanTake::Found(*payload)
    }

    pub(crate) fn close(&self) {
        let mut inner = self.inner.lock().expect(SETUP_PLAN_STORE_LOCK_INVARIANT);
        inner.closed = true;
        inner.plans.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner
            .lock()
            .expect(SETUP_PLAN_STORE_LOCK_INVARIANT)
            .plans
            .len()
    }
}

impl Default for SetupPlanStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_are_isolated_and_plan_ids_are_opaque_random_uuids() {
        let first = SetupPlanStore::new();
        let second = SetupPlanStore::new();
        let first_id = first.insert("first").expect("insert first plan");
        let second_id = second.insert("second").expect("insert second plan");

        assert_ne!(first_id, second_id);
        let first_uuid = first_id
            .strip_prefix("setup-")
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
            .expect("opaque setup UUID");
        assert_eq!(first_uuid.get_version(), Some(uuid::Version::Random));
        assert!(matches!(
            second.take::<&'static str>(&first_id),
            SetupPlanTake::Missing
        ));
        assert!(matches!(
            first.take::<&'static str>(&first_id),
            SetupPlanTake::Found("first")
        ));
    }

    #[test]
    fn insert_prunes_expired_plans_before_enforcing_the_bound() {
        let store = SetupPlanStore::with_limits(Duration::from_secs(60), 1);
        store.insert("expired").expect("insert expiring plan");
        for entry in store
            .inner
            .lock()
            .expect(SETUP_PLAN_STORE_LOCK_INVARIANT)
            .plans
            .values_mut()
        {
            entry.expires_at = Instant::now();
        }

        let live_id = store
            .insert("live")
            .expect("expired plan does not consume capacity");

        assert_eq!(store.len(), 1);
        assert!(matches!(
            store.take::<&'static str>(&live_id),
            SetupPlanTake::Found("live")
        ));
    }

    #[test]
    fn full_and_closed_stores_reject_new_ownership() {
        let store = SetupPlanStore::with_limits(Duration::from_secs(60), 1);
        store.insert("first").expect("insert first");
        assert_eq!(store.insert("second"), Err(SetupPlanInsertError::Full));

        store.close();

        assert_eq!(store.len(), 0);
        assert_eq!(
            store.insert("after-close"),
            Err(SetupPlanInsertError::Closed)
        );
    }

    #[test]
    fn expired_plan_is_consumed_with_an_explicit_result() {
        let store = SetupPlanStore::with_limits(Duration::ZERO, 1);
        let plan_id = store.insert("expired").expect("insert expired plan");

        assert!(matches!(
            store.take::<&'static str>(&plan_id),
            SetupPlanTake::Expired
        ));
        assert!(matches!(
            store.take::<&'static str>(&plan_id),
            SetupPlanTake::Missing
        ));
    }
}

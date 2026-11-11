//! Retained asynchronous work and admission leases.
//!
//! These primitives do not decide domain outcomes or persist recovery records.
//! An accepted future must settle its effects, or transfer its retained guards
//! into a domain-owned receipt, before returning. Dropping a waiter never drops
//! that future. Shutdown requests cancellation; it never aborts unsettled work.

mod cancellation;
mod exclusion;
mod owner;

pub use cancellation::CancellationToken;
pub use exclusion::{ArtifactKey, ExclusionError, ExclusionLease, Exclusions};
pub use owner::{
    OwnerBusy, OwnerSnapshot, ShutdownError, ShutdownReceipt, SpawnError, TaskHandle, TaskId,
    TaskJoinError, TaskOwner,
};

const SETTLE_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// Native effects a feature retained because they did not settle. `lock` reads
/// through a poisoned lock so retention and settlement never drop an effect,
/// but a panic mid-update may have lost effects, so a poisoned holder is never
/// intact or settled.
pub struct RetainedEffects<T> {
    effects: std::sync::Mutex<T>,
}

impl<T> RetainedEffects<T> {
    pub fn new(effects: T) -> Self {
        Self {
            effects: std::sync::Mutex::new(effects),
        }
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
        self.effects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The retained effects, unless a panic may have lost some.
    pub fn intact(&self) -> Option<std::sync::MutexGuard<'_, T>> {
        self.effects.lock().ok()
    }

    pub fn is_settled(&self, settled: impl FnOnce(&T) -> bool) -> bool {
        self.intact().is_some_and(|effects| settled(&effects))
    }
}

impl<T: Default> Default for RetainedEffects<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// Retry one owned settlement until it succeeds. A failed attempt leaves the
/// caller's owners retained; nothing is released as if it had settled.
pub async fn retry_until_settled<T, E, F>(what: &'static str, mut attempt: impl FnMut() -> F) -> T
where
    E: std::fmt::Display,
    F: std::future::Future<Output = Result<T, E>>,
{
    loop {
        match attempt().await {
            Ok(value) => return value,
            Err(error) => tracing::warn!(%error, "{what} remains unsettled; retrying"),
        }
        tokio::time::sleep(SETTLE_RETRY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::RetainedEffects;
    use std::sync::Arc;

    #[test]
    fn poisoned_effects_stay_retained_and_never_report_settled() {
        let retained = Arc::new(RetainedEffects::new(Vec::<u8>::new()));
        let poisoner = retained.clone();
        std::thread::spawn(move || {
            let _effects = poisoner.lock();
            panic!("poison the retained effects");
        })
        .join()
        .unwrap_err();

        retained.lock().push(1);
        assert_eq!(*retained.lock(), [1]);
        retained.lock().clear();
        assert!(retained.intact().is_none());
        assert!(!retained.is_settled(Vec::is_empty));
    }
}

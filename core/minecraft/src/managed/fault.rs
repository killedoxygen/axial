//! Test-only fault injection for the managed effect engines.
//!
//! A test arms a fault for its own thread or for a root it owns. The first engine probe for
//! that fault fires it once: a thread-scoped fault on the arming thread, a root-scoped fault
//! wherever the probed path lies under the root, even on a blocking worker. Without `test` or
//! `test-support`, every probe is a constant `false`.

use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Fault {
    /// A guarded file move applies, then reports itself unsettled.
    GuardedMoveUnsettled,
    /// A guarded file removal applies, then reports itself unsettled.
    GuardedRemovalUnsettled,
    /// The native file at `index` is promoted without acknowledgement.
    NativeFileUnacknowledged {
        index: usize,
    },
    /// A content transaction reaches its final manifest revalidation.
    BeforeManifestRevalidation,
    /// Staging checkpoint cleanup removed a recorded parent directory.
    AfterStagingParentRemoval,
    /// Staging checkpoint cleanup removed a guarded public file.
    AfterPublishedFileRemoval,
    /// An exact component projection reaches its final guarded validation.
    ExactBeforeFinalValidation,
    /// The component intent marker fails before promotion.
    IntentBeforePromotion,
    /// Intent publication reports an attempted promotion without writing the marker.
    IntentWithoutMarker,
    /// The component intent marker is promoted without acknowledgement.
    IntentUnacknowledged,
    /// Intent publication fails after the lease revalidates.
    IntentAfterLeaseRevalidated,
    /// Component execution fails after a row and rolls back live.
    RowFailure,
    /// Component execution stops unsettled after a row.
    RowUnsettled,
    /// Component execution stops unsettled after quarantining a replaced file.
    ReplacementQuarantineUnsettled,
    /// Component execution stops unsettled before journaling an ancestor shard.
    AncestorJournalUnsettled,
    /// Component execution stops unsettled after promoting an ancestor.
    AncestorUnsettled,
    /// Component execution stops unsettled before publishing its outcome.
    OutcomeUnsettled,
    /// The component outcome is promoted without acknowledgement.
    OutcomeUnacknowledged,
    /// The version bundle intent marker is promoted without acknowledgement.
    VersionIntentUnacknowledged,
    /// The version bundle settlement marker is promoted without acknowledgement.
    VersionSettlementUnacknowledged,
    /// Component settlement stops after each durable frontier.
    AfterSettlementPromotion,
    AfterAncestorBucket,
    AfterAncestorRecord,
    AfterStagingBucket,
    AfterQuarantineBucket,
    AfterTableShard,
    AfterOutcomeRemoval,
    AfterIntentRemoval,
    AfterSettlementRemoval,
}

/// Fire `fault` if it is armed for this thread or for a root containing `path`.
pub(crate) fn hit(path: &Path, fault: Fault) -> bool {
    take(fault, |root| path.starts_with(root))
}

#[cfg(not(any(test, feature = "test-support")))]
#[inline(always)]
pub(crate) fn take(_fault: Fault, _under_root: impl Fn(&Path) -> bool) -> bool {
    false
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) use armed::*;

#[cfg(any(test, feature = "test-support"))]
mod armed {
    use super::Fault;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard, PoisonError};

    enum Scope {
        #[cfg(test)]
        Thread(std::thread::ThreadId),
        Root(PathBuf),
    }

    struct Armed {
        scope: Scope,
        fault: Fault,
        action: Option<Box<dyn FnOnce() + Send>>,
    }

    static ARMED: Mutex<Vec<Armed>> = Mutex::new(Vec::new());

    fn armed() -> MutexGuard<'static, Vec<Armed>> {
        ARMED.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Arm `fault` for probes on the current thread.
    #[cfg(test)]
    pub(crate) fn arm_here(fault: Fault, action: Option<Box<dyn FnOnce() + Send>>) {
        armed().push(Armed {
            scope: Scope::Thread(std::thread::current().id()),
            fault,
            action,
        });
    }

    /// Arm `fault` for probes under `root`, on any thread.
    pub(crate) fn arm(root: &Path, fault: Fault, action: Option<Box<dyn FnOnce() + Send>>) {
        armed().push(Armed {
            scope: Scope::Root(root.to_path_buf()),
            fault,
            action,
        });
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn is_armed(root: &Path, fault: Fault) -> bool {
        armed().iter().any(|armed| {
            armed.fault == fault && matches!(&armed.scope, Scope::Root(armed) if armed == root)
        })
    }

    /// Whether `fault` armed for the current thread has not fired yet.
    #[cfg(test)]
    pub(crate) fn armed_here(fault: Fault) -> bool {
        let thread = std::thread::current().id();
        armed().iter().any(|armed| {
            armed.fault == fault && matches!(armed.scope, Scope::Thread(id) if id == thread)
        })
    }

    /// Whether any fault armed for `root` has not fired yet.
    #[cfg(test)]
    pub(crate) fn pending(root: &Path) -> bool {
        armed()
            .iter()
            .any(|armed| matches!(&armed.scope, Scope::Root(armed) if armed == root))
    }

    /// Fire `fault` if it is armed for this thread or for a root accepted by `under_root`.
    pub(crate) fn take(fault: Fault, under_root: impl Fn(&Path) -> bool) -> bool {
        let mut armed = armed();
        let Some(index) = armed.iter().position(|armed| {
            armed.fault == fault
                && match &armed.scope {
                    #[cfg(test)]
                    Scope::Thread(id) => *id == std::thread::current().id(),
                    Scope::Root(root) => under_root(root),
                }
        }) else {
            return false;
        };
        let action = armed.remove(index).action;
        drop(armed);
        if let Some(action) = action {
            action();
        }
        true
    }
}

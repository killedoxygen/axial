use crate::state::{
    RegisteredWholeInstanceDurableOutcome, RegisteredWholeInstancePreparation,
    RegisteredWholeInstanceRematerializationAdmission,
};
use axial_minecraft::{ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRebuildError};
use std::future::Future;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GuardianWholeInstanceRematerializationStatus {
    Rematerialized,
    Failed,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct GuardianWholeInstanceRematerializationOutcome {
    status: GuardianWholeInstanceRematerializationStatus,
}

impl GuardianWholeInstanceRematerializationOutcome {
    pub(crate) fn status(&self) -> GuardianWholeInstanceRematerializationStatus {
        self.status
    }
}

#[derive(Debug, thiserror::Error)]
#[error("whole-instance rematerialization State settlement failed: {class}")]
pub(crate) struct GuardianWholeInstanceRematerializationError {
    class: &'static str,
}

impl GuardianWholeInstanceRematerializationError {
    pub(crate) fn class(&self) -> &'static str {
        self.class
    }
}

pub(crate) async fn execute_whole_instance_rematerialization(
    admission: RegisteredWholeInstanceRematerializationAdmission,
) -> Result<
    GuardianWholeInstanceRematerializationOutcome,
    GuardianWholeInstanceRematerializationError,
> {
    execute_whole_instance_rematerialization_with(
        admission,
        |root, runtime_cache, version_id| async move {
            axial_minecraft::rematerialize_managed_instance(root, &runtime_cache, &version_id).await
        },
    )
    .await
}

pub(crate) async fn execute_whole_instance_rematerialization_with<Driver, DriverFuture>(
    admission: RegisteredWholeInstanceRematerializationAdmission,
    driver: Driver,
) -> Result<
    GuardianWholeInstanceRematerializationOutcome,
    GuardianWholeInstanceRematerializationError,
>
where
    Driver: FnOnce(PathBuf, axial_minecraft::runtime::ManagedRuntimeCache, String) -> DriverFuture
        + Send,
    DriverFuture: Future<Output = Result<ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRebuildError>>
        + Send,
{
    let preparation = admission.into_effect().await.map_err(|error| {
        GuardianWholeInstanceRematerializationError {
            class: error.class(),
        }
    })?;
    let (request, completion) = match preparation {
        RegisteredWholeInstancePreparation::Admitted {
            request,
            completion,
        } => (request, completion),
        RegisteredWholeInstancePreparation::Closed(outcome) => {
            return Ok(guardian_outcome(outcome));
        }
    };
    let (root, runtime_cache, version_id) = {
        let (root, runtime_cache, version_id) = request.core_request();
        (
            root.to_path_buf(),
            runtime_cache.clone(),
            version_id.to_string(),
        )
    };
    let settlement = match driver(root, runtime_cache, version_id).await {
        Ok(receipt) => completion.settle_commit(receipt).await,
        Err(ManagedWholeInstanceRebuildError::RolledBack(receipt)) => {
            completion.settle_rollback(receipt).await
        }
        Err(
            ManagedWholeInstanceRebuildError::Reconstruction(_)
            | ManagedWholeInstanceRebuildError::Preparation
            | ManagedWholeInstanceRebuildError::RuntimePreparation,
        ) => completion.into_failed_settlement(),
    };
    let outcome =
        settlement
            .settle()
            .await
            .map_err(|error| GuardianWholeInstanceRematerializationError {
                class: error.class(),
            })?;
    Ok(guardian_outcome(outcome))
}

fn guardian_outcome(
    outcome: RegisteredWholeInstanceDurableOutcome,
) -> GuardianWholeInstanceRematerializationOutcome {
    GuardianWholeInstanceRematerializationOutcome {
        status: if outcome.succeeded() {
            GuardianWholeInstanceRematerializationStatus::Rematerialized
        } else {
            GuardianWholeInstanceRematerializationStatus::Failed
        },
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn production_executor_has_one_core_rematerialization_call_site() {
        let source = include_str!("whole_instance_rematerialization.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source precedes tests");
        assert_eq!(
            source
                .matches("axial_minecraft::rematerialize_managed_instance(")
                .count(),
            1
        );
        assert!(!source.contains("OperationJournal"));
        assert!(!source.contains("ReconciliationQuarantineCheckpoint"));
    }
}

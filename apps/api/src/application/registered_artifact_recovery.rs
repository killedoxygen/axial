use crate::execution::integrity::sense_current_integrity_tier1;
use crate::guardian::{
    DiagnosisId, GuardianArtifactRepairSettlement, GuardianArtifactRepairStatus,
    GuardianComponentRebuildStatus, execute_managed_libraries_component_rebuild,
    execute_registered_guardian_artifact_repair,
};
use crate::state::contracts::OperationId;
use crate::state::{
    AppState, OperationJournalStoreError, RegisteredArtifactFailedRepair,
    RegisteredArtifactRepairAdmission,
};

pub(super) const REGISTERED_ARTIFACT_REPAIR_SUPPRESSION_MINUTES: i64 = 15;

pub(super) fn new_registered_artifact_repair_operation_id() -> OperationId {
    OperationId::new(format!(
        "guardian-registered-artifact-repair:{}",
        uuid::Uuid::new_v4()
    ))
}

fn new_libraries_component_rebuild_operation_id() -> OperationId {
    OperationId::new(format!(
        "guardian-libraries-component-rebuild:{}",
        uuid::Uuid::new_v4()
    ))
}

#[derive(Clone, Copy)]
pub(super) enum LibrariesComponentRebuildSource {
    Production,
    #[cfg(test)]
    Fixture,
}

pub(super) struct RegisteredArtifactRecoverySequenceOutcome {
    pub(super) diagnosis_id: DiagnosisId,
    pub(super) effective_status: GuardianArtifactRepairStatus,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_registered_artifact_recovery_sequence(
    state: &AppState,
    admission: RegisteredArtifactRepairAdmission,
    client: &reqwest::Client,
    foreground: &crate::state::IntegrityForegroundLease,
    instance_id: &str,
    rebuild_source: LibrariesComponentRebuildSource,
) -> Result<RegisteredArtifactRecoverySequenceOutcome, OperationJournalStoreError> {
    match execute_registered_guardian_artifact_repair(admission, client).await? {
        GuardianArtifactRepairSettlement::Completed(outcome) => {
            Ok(RegisteredArtifactRecoverySequenceOutcome {
                diagnosis_id: outcome.diagnosis_id(),
                effective_status: outcome.status(),
            })
        }
        GuardianArtifactRepairSettlement::Failed(failure) => {
            let diagnosis_id = failure.outcome().diagnosis_id();
            execute_failed_registered_artifact_recovery(
                state,
                diagnosis_id,
                failure.into_continuation(),
                foreground,
                instance_id,
                rebuild_source,
            )
            .await
        }
    }
}

async fn execute_failed_registered_artifact_recovery(
    state: &AppState,
    diagnosis_id: DiagnosisId,
    continuation: RegisteredArtifactFailedRepair,
    foreground: &crate::state::IntegrityForegroundLease,
    instance_id: &str,
    rebuild_source: LibrariesComponentRebuildSource,
) -> Result<RegisteredArtifactRecoverySequenceOutcome, OperationJournalStoreError> {
    let component_admission = state
        .admit_registered_artifact_component_rebuild(
            continuation,
            new_libraries_component_rebuild_operation_id(),
            chrono::Duration::minutes(REGISTERED_ARTIFACT_REPAIR_SUPPRESSION_MINUTES),
        )
        .await
        .map_err(|_| {
            registered_artifact_recovery_error("Libraries component rebuild admission was refused")
        })?;

    let rebuild = execute_managed_libraries_component_rebuild(
        component_admission,
        move |effect| async move {
            let (root, version_id) = effect.core_request();
            let root = root.to_path_buf();
            let version_id = version_id.to_string();
            let rebuilt = match rebuild_source {
                LibrariesComponentRebuildSource::Production => {
                    axial_minecraft::rebuild_managed_libraries(root, &version_id).await
                }
                #[cfg(test)]
                LibrariesComponentRebuildSource::Fixture => {
                    axial_minecraft::rebuild_managed_libraries_fixture_for_test(root, &version_id)
                        .await
                }
            };
            match rebuilt {
                Ok(receipt) => effect.committed(receipt, Vec::new()),
                Err(
                    axial_minecraft::ManagedLibrariesRebuildError::Reconstruction(_)
                    | axial_minecraft::ManagedLibrariesRebuildError::Preparation,
                ) => effect.failed_before_effect(["libraries_component_preparation_failed".into()]),
                Err(axial_minecraft::ManagedLibrariesRebuildError::RolledBack(receipt)) => {
                    effect.rolled_back(receipt, ["libraries_component_rebuild_rolled_back".into()])
                }
            }
        },
    )
    .await?;
    if rebuild.status != GuardianComponentRebuildStatus::Rebuilt {
        return Ok(RegisteredArtifactRecoverySequenceOutcome {
            diagnosis_id,
            effective_status: GuardianArtifactRepairStatus::Failed,
        });
    }

    let lifecycle = state
        .acquire_integrity_instance_lifecycle(foreground, instance_id)
        .await
        .map_err(|_| {
            registered_artifact_recovery_error(
                "Libraries rebuild postcheck could not reacquire the instance lifecycle",
            )
        })?;
    let postcheck = sense_current_integrity_tier1(state, foreground, &lifecycle)
        .await
        .map_err(|_| {
            registered_artifact_recovery_error("Libraries rebuild Tier1 postcheck was unavailable")
        })?;
    Ok(RegisteredArtifactRecoverySequenceOutcome {
        diagnosis_id,
        effective_status: if postcheck.facts.is_empty() {
            GuardianArtifactRepairStatus::Repaired
        } else {
            GuardianArtifactRepairStatus::Failed
        },
    })
}

fn registered_artifact_recovery_error(message: &'static str) -> OperationJournalStoreError {
    OperationJournalStoreError::Persistence(std::io::Error::other(message))
}

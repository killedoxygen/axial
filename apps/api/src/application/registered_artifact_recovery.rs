use crate::execution::integrity::IntegrityTier2Report;
#[cfg(test)]
use crate::guardian::execute_managed_assets_component_rebuild_fixture_for_test;
use crate::guardian::{
    DiagnosisId, GuardianArtifactRepairSettlement, GuardianArtifactRepairStatus,
    GuardianComponentRebuildStatus, GuardianMode, Tier2RegisteredArtifactAssessment,
    assess_tier2_registered_artifact_repair, execute_managed_assets_component_rebuild,
    execute_managed_libraries_component_rebuild, execute_managed_version_bundle_component_rebuild,
    execute_registered_guardian_artifact_repair,
};
use crate::state::contracts::{OperationId, ReconciliationComponent};
use crate::state::{
    AppState, OperationJournalStoreError, ProducerLease, RegisteredArtifactFailedRepair,
    RegisteredArtifactFindings,
    RegisteredArtifactRecoveryEntry as StateRegisteredArtifactRecoveryEntry,
    RegisteredArtifactRepairAdmission,
};

pub(super) const REGISTERED_ARTIFACT_REPAIR_SUPPRESSION_MINUTES: i64 = 15;

pub(super) fn new_registered_artifact_repair_operation_id() -> OperationId {
    OperationId::mint()
}

fn new_registered_component_rebuild_operation_id() -> OperationId {
    OperationId::mint()
}

#[derive(Clone, Copy)]
pub(super) enum RegisteredArtifactComponentRebuildSource {
    Production,
    #[cfg(test)]
    Fixture,
}

#[must_use]
pub(super) enum RegisteredArtifactRecoveryEntry {
    Fresh(Box<RegisteredArtifactRepairAdmission>),
    Resume(Box<RegisteredArtifactFailedRepair>),
}

pub(super) struct RegisteredArtifactRecoverySequenceOutcome {
    pub(super) diagnosis_id: DiagnosisId,
    pub(super) effective_status: GuardianArtifactRepairStatus,
}

#[must_use = "a prepared Tier 2 recovery must execute before its sweep can settle"]
pub(super) struct Tier2RegisteredArtifactRecovery {
    execution: Option<Box<Tier2RegisteredArtifactRecoveryExecution>>,
}

struct Tier2RegisteredArtifactRecoveryExecution {
    state: AppState,
    producer: ProducerLease,
    entry: StateRegisteredArtifactRecoveryEntry,
    client: reqwest::Client,
    rebuild_source: RegisteredArtifactComponentRebuildSource,
}

pub(super) fn prepare_tier2_registered_artifact_recovery(
    state: AppState,
    producer: ProducerLease,
    sweep_operation_id: &OperationId,
    report: &IntegrityTier2Report,
    findings: RegisteredArtifactFindings,
    client: reqwest::Client,
    rebuild_source: RegisteredArtifactComponentRebuildSource,
) -> Tier2RegisteredArtifactRecovery {
    let assessment: Tier2RegisteredArtifactAssessment = {
        let Some(candidate) = findings.repair_candidate() else {
            return Tier2RegisteredArtifactRecovery { execution: None };
        };
        let mut matching_facts = report
            .facts
            .iter()
            .filter(|fact| fact.target.as_ref() == Some(candidate.target()));
        let Some(fact) = matching_facts.next() else {
            return Tier2RegisteredArtifactRecovery { execution: None };
        };
        if matching_facts.next().is_some() {
            return Tier2RegisteredArtifactRecovery { execution: None };
        }
        let mode = GuardianMode::from_config(&state.config().current().guardian_mode);
        let Some(assessment) = assess_tier2_registered_artifact_repair(
            sweep_operation_id.clone(),
            mode,
            fact,
            candidate,
        ) else {
            return Tier2RegisteredArtifactRecovery { execution: None };
        };
        assessment
    };
    let Ok(authorization) = findings.authorize_repair(assessment.decision()) else {
        return Tier2RegisteredArtifactRecovery { execution: None };
    };
    let Ok(entry) = state.registered_artifact_recovery_entry(authorization) else {
        return Tier2RegisteredArtifactRecovery { execution: None };
    };
    Tier2RegisteredArtifactRecovery {
        execution: Some(Box::new(Tier2RegisteredArtifactRecoveryExecution {
            state,
            producer,
            entry,
            client,
            rebuild_source,
        })),
    }
}

impl Tier2RegisteredArtifactRecovery {
    pub(super) async fn execute(
        self,
    ) -> Result<Option<RegisteredArtifactRecoverySequenceOutcome>, OperationJournalStoreError> {
        let Some(execution) = self.execution else {
            return Ok(None);
        };
        let Tier2RegisteredArtifactRecoveryExecution {
            state,
            producer,
            entry,
            client,
            rebuild_source,
        } = *execution;
        let entry = match entry {
            StateRegisteredArtifactRecoveryEntry::Fresh(authorization) => {
                let Ok(admission) = state
                    .admit_registered_artifact_repair(
                        authorization,
                        new_registered_artifact_repair_operation_id(),
                        chrono::Duration::minutes(REGISTERED_ARTIFACT_REPAIR_SUPPRESSION_MINUTES),
                    )
                    .await
                else {
                    return Ok(None);
                };
                RegisteredArtifactRecoveryEntry::Fresh(Box::new(admission))
            }
            StateRegisteredArtifactRecoveryEntry::Resume(continuation) => {
                RegisteredArtifactRecoveryEntry::Resume(Box::new(continuation))
            }
        };
        execute_registered_artifact_recovery_sequence(
            &state,
            producer,
            entry,
            &client,
            rebuild_source,
        )
        .await
        .map(Some)
    }
}

pub(super) async fn execute_registered_artifact_recovery_sequence(
    state: &AppState,
    producer: ProducerLease,
    entry: RegisteredArtifactRecoveryEntry,
    client: &reqwest::Client,
    rebuild_source: RegisteredArtifactComponentRebuildSource,
) -> Result<RegisteredArtifactRecoverySequenceOutcome, OperationJournalStoreError> {
    let continuation = match entry {
        RegisteredArtifactRecoveryEntry::Fresh(admission) => {
            match execute_registered_guardian_artifact_repair(*admission, client).await? {
                GuardianArtifactRepairSettlement::Completed(outcome) => {
                    return Ok(RegisteredArtifactRecoverySequenceOutcome {
                        diagnosis_id: outcome.diagnosis_id(),
                        effective_status: outcome.status(),
                    });
                }
                GuardianArtifactRepairSettlement::Failed(failure) => (*failure).into_continuation(),
            }
        }
        RegisteredArtifactRecoveryEntry::Resume(continuation) => *continuation,
    };

    let component_admission = state
        .admit_registered_artifact_component_rebuild(
            continuation,
            new_registered_component_rebuild_operation_id(),
            chrono::Duration::minutes(REGISTERED_ARTIFACT_REPAIR_SUPPRESSION_MINUTES),
        )
        .await
        .map_err(|_| {
            registered_artifact_recovery_error("component rebuild admission was refused")
        })?;
    let diagnosis_id = component_admission.attempt().diagnosis_id();
    let component = component_admission.attempt().component();
    let rebuild = match component {
        ReconciliationComponent::VersionBundle => {
            execute_managed_version_bundle_component_rebuild(
                producer,
                component_admission,
                move |effect| async move {
                    let (root, version_id, inventory) = effect.core_request();
                    let root = root.to_path_buf();
                    let version_id = version_id.to_string();
                    let inventory = inventory.clone();
                    let rebuilt = match rebuild_source {
                        RegisteredArtifactComponentRebuildSource::Production => {
                            axial_minecraft::rebuild_managed_version_bundle(
                                root,
                                &version_id,
                                inventory,
                            )
                            .await
                        }
                        #[cfg(test)]
                        RegisteredArtifactComponentRebuildSource::Fixture => {
                            axial_minecraft::rebuild_managed_version_bundle_fixture_for_test(
                                root,
                                &version_id,
                            )
                            .await
                        }
                    };
                    match rebuilt {
                        Ok(receipt) => effect.committed(receipt, Vec::new()),
                        Err(
                            axial_minecraft::ManagedVersionBundleRebuildError::Reconstruction(
                                axial_minecraft::KnownGoodReconstructionError::Vanilla
                                | axial_minecraft::KnownGoodReconstructionError::Loader,
                            )
                            | axial_minecraft::ManagedVersionBundleRebuildError::Source,
                        ) => effect.failed_before_effect([
                            "version_bundle_component_source_failed".into(),
                        ]),
                        Err(axial_minecraft::ManagedVersionBundleRebuildError::Authority) => effect
                            .failed_before_effect([
                                "version_bundle_component_authority_rejected".into()
                            ]),
                        Err(
                            axial_minecraft::ManagedVersionBundleRebuildError::Reconstruction(
                                axial_minecraft::KnownGoodReconstructionError::ManagedRoot,
                            )
                            | axial_minecraft::ManagedVersionBundleRebuildError::LocalPreparation
                            | axial_minecraft::ManagedVersionBundleRebuildError::Preparation,
                        ) => effect.failed_before_effect([
                            "version_bundle_component_local_preparation_failed".into(),
                        ]),
                        Err(axial_minecraft::ManagedVersionBundleRebuildError::RolledBack(
                            receipt,
                        )) => effect.rolled_back(
                            receipt,
                            ["version_bundle_component_rebuild_rolled_back".into()],
                        ),
                    }
                },
            )
            .await?
        }
        ReconciliationComponent::Libraries => {
            execute_managed_libraries_component_rebuild(
                producer,
                component_admission,
                move |effect| async move {
                    let (root, version_id) = effect.core_request();
                    let root = root.to_path_buf();
                    let version_id = version_id.to_string();
                    let rebuilt = match rebuild_source {
                        RegisteredArtifactComponentRebuildSource::Production => {
                            axial_minecraft::rebuild_managed_libraries(root, &version_id).await
                        }
                        #[cfg(test)]
                        RegisteredArtifactComponentRebuildSource::Fixture => {
                            axial_minecraft::rebuild_managed_libraries_fixture_for_test(
                                root,
                                &version_id,
                            )
                            .await
                        }
                    };
                    match rebuilt {
                        Ok(receipt) => effect.committed(receipt, Vec::new()),
                        Err(
                            axial_minecraft::ManagedLibrariesRebuildError::Reconstruction(_)
                            | axial_minecraft::ManagedLibrariesRebuildError::Preparation,
                        ) => effect.failed_before_effect([
                            "libraries_component_preparation_failed".into(),
                        ]),
                        Err(axial_minecraft::ManagedLibrariesRebuildError::RolledBack(receipt)) => {
                            effect.rolled_back(
                                receipt,
                                ["libraries_component_rebuild_rolled_back".into()],
                            )
                        }
                    }
                },
            )
            .await?
        }
        ReconciliationComponent::Assets => match rebuild_source {
            RegisteredArtifactComponentRebuildSource::Production => {
                execute_managed_assets_component_rebuild(producer, component_admission).await?
            }
            #[cfg(test)]
            RegisteredArtifactComponentRebuildSource::Fixture => {
                execute_managed_assets_component_rebuild_fixture_for_test(
                    producer,
                    component_admission,
                )
                .await?
            }
        },
        _ => {
            return Err(registered_artifact_recovery_error(
                "registered artifact recovery selected an unsupported component",
            ));
        }
    };

    Ok(RegisteredArtifactRecoverySequenceOutcome {
        diagnosis_id,
        effective_status: if rebuild.status == GuardianComponentRebuildStatus::Rebuilt {
            GuardianArtifactRepairStatus::Repaired
        } else {
            GuardianArtifactRepairStatus::Failed
        },
    })
}

fn registered_artifact_recovery_error(message: &'static str) -> OperationJournalStoreError {
    OperationJournalStoreError::Persistence(std::io::Error::other(message))
}

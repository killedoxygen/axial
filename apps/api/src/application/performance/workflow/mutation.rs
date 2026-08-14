use super::managed_plan::{ManagedPlanResolutionError, resolve_managed_install_plan};
use super::operations::{
    PerformanceApplicationError, PerformanceInstallAction, PerformanceOperationExecutionError,
    PerformanceOperationResultRequest, begin_performance_operation_journal,
    record_performance_applied_unverified, record_performance_effect_started,
    record_performance_guardian_supervision, record_performance_operation_result,
    record_performance_plan_resolved, record_performance_prepared,
};
use super::plan_health::{
    PerformanceManagedArtifactSummary, managed_artifact_summary, performance_composition_target,
    resolve_instance_mode, resolve_instance_version_target, response_warnings, tier_name,
};
use super::{
    PerformanceInstallResponse, PerformanceOperation, PerformanceRollbackListRequest,
    optional_value, required_value,
};
use crate::application::transfer::{managed_transfer_retry_policy, pinned_public_transfer_client};
use crate::guardian::{
    GuardianCopyRequest, GuardianFact, GuardianMode, GuardianPerformanceOperationKind,
    GuardianPerformanceSupervisionPlan, GuardianPerformanceSupervisionRejection,
    GuardianPerformanceSupervisionRequest, GuardianPolicyContext, MAX_OPERATION_EVIDENCE_FACTS,
    OperationEvidenceBatch, author_guardian_copy, performance_plan_guardian_facts,
    plan_performance_supervision,
};
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::{
    OperationId, OperationPhase, PerformanceOperationPhase, PerformanceOperationPrepared,
    PerformancePreparedProof, PerformanceRollbackTarget, RollbackState,
};
use crate::state::{AppManagedCompositionAdmission, AppState, IntegrityForegroundLease};
use axial_minecraft::download::{TransferClient, TransferOrigin};
use axial_performance::{
    BundleHealth, CompositionPlan, CompositionState, InstallError, ManagedArtifactTransferResolver,
    ManagedCompositionInspection, ManagedCompositionInstallPlan, ManagedInstallExecutionError,
    ManagedMutationError, ManagedRollbackOutcome, PerformanceMode, ResolutionRequest,
    RollbackSnapshotSummary as CoreRollbackSnapshotSummary, RollbackSnapshotTarget, StateError,
};
use axum::{Json, http::StatusCode};
use serde::Serialize;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;

pub(super) async fn resolve_performance_install_plan(
    state: AppState,
    declarative: CompositionPlan,
    game_version: String,
    loader: String,
) -> Result<ManagedCompositionInstallPlan, ManagedPlanResolutionError> {
    resolve_managed_install_plan(&state, declarative, &game_version, &loader).await
}

pub(super) const PERFORMANCE_INSTALL_INTERNAL_ERROR: &str =
    "Could not update managed performance files. Check instance folder permissions and try again.";

struct PerformanceInstallExecutionRequest<'a> {
    state: &'a AppState,
    admitted: &'a AppManagedCompositionAdmission,
    operation: &'a PerformanceOperation,
    mode: PerformanceMode,
    game_version: String,
    loader: String,
}

#[derive(Clone, Debug)]
struct PerformanceRollbackPreflight {
    target_id: String,
    rollback_state: RollbackState,
    prepared: Option<PerformanceOperationPrepared>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PerformanceJournalIdentity {
    pub(super) action: PerformanceInstallAction,
    pub(super) target_id: String,
    pub(super) rollback: RollbackState,
}

#[derive(Debug, Serialize)]
pub struct PerformanceRollbackListResponse {
    pub snapshots: Vec<PerformanceRollbackSnapshotSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PerformanceRollbackSnapshotSummary {
    pub id: String,
    pub created_at: String,
    pub target: RollbackSnapshotTarget,
    pub composition_id: Option<String>,
    pub tier: Option<axial_performance::CompositionTier>,
    pub installed_count: usize,
    pub artifact_count: usize,
    pub ownership_class: axial_performance::OwnershipClass,
    pub rollback_available: bool,
    pub latest: bool,
}

pub async fn performance_rollback_list(
    state: &AppState,
    query: PerformanceRollbackListRequest,
) -> Result<PerformanceRollbackListResponse, (StatusCode, Json<serde_json::Value>)> {
    let instance_id = required_value(
        query.instance_id.as_deref(),
        "instance_id query parameter is required",
    )?;
    let snapshots = state
        .inspect_managed_instance(&instance_id, None)
        .await
        .map_err(internal_install_error)?
        .rollback_snapshots
        .into_iter()
        .map(performance_rollback_snapshot_summary)
        .collect();

    Ok(PerformanceRollbackListResponse { snapshots })
}

fn performance_rollback_snapshot_summary(
    snapshot: CoreRollbackSnapshotSummary,
) -> PerformanceRollbackSnapshotSummary {
    PerformanceRollbackSnapshotSummary {
        id: super::super::public_performance_descriptor(&snapshot.id, "rollback_snapshot"),
        created_at: public_performance_timestamp(&snapshot.created_at),
        target: snapshot.target,
        composition_id: snapshot.composition_id.as_deref().map(|composition_id| {
            super::super::public_performance_descriptor(composition_id, "composition")
        }),
        tier: snapshot.tier,
        installed_count: snapshot.installed_count,
        artifact_count: snapshot.artifact_count,
        ownership_class: snapshot.ownership_class,
        rollback_available: snapshot.rollback_available,
        latest: snapshot.latest,
    }
}

fn public_performance_timestamp(value: &str) -> String {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 64)
        .unwrap_or_else(|| "created_at".to_string())
}

#[cfg(test)]
pub(super) async fn execute_performance_operation(
    state: &AppState,
    operation: &PerformanceOperation,
    foreground: &IntegrityForegroundLease,
) -> Result<PerformanceInstallResponse, PerformanceOperationExecutionError> {
    execute_performance_operation_with_resolver_and_progress(
        state,
        operation,
        foreground,
        resolve_performance_install_plan,
        |_| async {},
    )
    .await
}

pub(super) async fn execute_performance_operation_with_resolver_and_progress<
    Resolver,
    ResolutionFuture,
    Progress,
    ProgressFuture,
>(
    state: &AppState,
    operation: &PerformanceOperation,
    foreground: &IntegrityForegroundLease,
    resolver: Resolver,
    progress: Progress,
) -> Result<PerformanceInstallResponse, PerformanceOperationExecutionError>
where
    Resolver: FnOnce(AppState, CompositionPlan, String, String) -> ResolutionFuture,
    ResolutionFuture: std::future::Future<
            Output = Result<ManagedCompositionInstallPlan, ManagedPlanResolutionError>,
        >,
    Progress: FnOnce(PerformanceInstallAction) -> ProgressFuture + Send + 'static,
    ProgressFuture: std::future::Future<Output = ()> + Send + 'static,
{
    let instance = state
        .instances()
        .get(&operation.instance_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "instance not found" })),
            )
        })?;
    let admitted = state
        .admit_managed_instance_with_foreground(foreground, &instance.id, true)
        .await
        .map_err(managed_admission_error)?;

    if matches!(operation.action, PerformanceInstallAction::Rollback) {
        return execute_performance_rollback(state, &admitted, operation, progress).await;
    }

    let mode = resolve_instance_mode(state, &instance, operation.mode.as_deref())?;
    if matches!(operation.action, PerformanceInstallAction::Remove)
        || !matches!(mode, PerformanceMode::Managed)
    {
        return execute_performance_remove(state, &admitted, operation, progress).await;
    }

    let (game_version, loader) = resolve_instance_version_target(
        operation.installed_versions.as_ref(),
        &instance,
        operation.game_version.as_deref(),
        operation.loader.as_deref(),
    )?;
    execute_performance_install(
        PerformanceInstallExecutionRequest {
            state,
            admitted: &admitted,
            operation,
            mode,
            game_version,
            loader,
        },
        resolver,
        progress,
    )
    .await
}

pub(super) async fn performance_operation_journal_identity(
    state: &AppState,
    operation: &PerformanceOperation,
    foreground: &IntegrityForegroundLease,
) -> Result<PerformanceJournalIdentity, PerformanceApplicationError> {
    let instance = state
        .instances()
        .get(&operation.instance_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "instance not found" })),
            )
        })?;
    let admitted = state
        .admit_managed_instance_with_foreground(foreground, &instance.id, true)
        .await
        .map_err(managed_admission_error)?;

    if matches!(operation.action, PerformanceInstallAction::Rollback) {
        let preflight = rollback_preflight(&admitted, operation.rollback_id.as_deref()).await?;
        return Ok(PerformanceJournalIdentity {
            action: PerformanceInstallAction::Rollback,
            target_id: preflight.target_id,
            rollback: preflight.rollback_state,
        });
    }

    let mode = resolve_instance_mode(state, &instance, operation.mode.as_deref())?;
    if matches!(operation.action, PerformanceInstallAction::Remove)
        || !matches!(mode, PerformanceMode::Managed)
    {
        let current = preflight_current_performance_state(&admitted).await?;
        return Ok(PerformanceJournalIdentity {
            action: PerformanceInstallAction::Remove,
            target_id: current
                .as_ref()
                .map(|state| state.composition_id.clone())
                .unwrap_or_else(|| "performance_composition_lock".to_string()),
            rollback: rollback_state_for_current_state(current.as_ref()),
        });
    }

    let (game_version, loader) = resolve_instance_version_target(
        operation.installed_versions.as_ref(),
        &instance,
        operation.game_version.as_deref(),
        operation.loader.as_deref(),
    )?;
    let inspection = admitted
        .inspect(None)
        .await
        .map_err(managed_mutation_error)?;
    let plan = state.performance().get_plan(ResolutionRequest {
        game_version,
        loader,
        mode,
        hardware: state.performance().hardware(),
        installed_mods: inspection.installed_mod_evidence.clone(),
    });
    let rollback = install_rollback_state_for_inspection(&inspection);
    Ok(PerformanceJournalIdentity {
        action: PerformanceInstallAction::Install,
        target_id: plan.composition_id,
        rollback,
    })
}

async fn execute_performance_rollback<Progress, ProgressFuture>(
    state: &AppState,
    admitted: &AppManagedCompositionAdmission,
    operation: &PerformanceOperation,
    progress: Progress,
) -> Result<PerformanceInstallResponse, PerformanceOperationExecutionError>
where
    Progress: FnOnce(PerformanceInstallAction) -> ProgressFuture + Send + 'static,
    ProgressFuture: std::future::Future<Output = ()> + Send + 'static,
{
    let preflight = rollback_preflight(admitted, operation.rollback_id.as_deref()).await;
    let (target_id, rollback_state) = match &preflight {
        Ok(preflight) => (preflight.target_id.clone(), preflight.rollback_state),
        Err(_) => (
            "performance_rollback_snapshot".to_string(),
            RollbackState::Unavailable,
        ),
    };
    let operation_id = begin_performance_operation_journal(
        state,
        operation.action,
        &target_id,
        rollback_state,
        operation.status_operation_id.as_ref(),
        operation.resume_existing_journal,
    )
    .await
    .map_err(|error| {
        PerformanceOperationExecutionError::journal_transition(
            operation.status_operation_id.clone(),
            error,
        )
    })?;
    let preflight = match preflight {
        Ok(preflight) => preflight,
        Err(error) => {
            let result = Err(error);
            record_performance_operation_result(
                state,
                PerformanceOperationResultRequest {
                    operation_id: &operation_id,
                    terminal_rollback: rollback_state,
                    changed_target: false,
                    result: &result,
                    failure_signal: operation.persistence_failure.as_ref(),
                },
            )
            .await?;
            return result.map_err(Into::into);
        }
    };
    let supervision = match supervise_performance_operation(
        state,
        &operation_id,
        GuardianPerformanceOperationKind::RollbackManagedComposition,
        &target_id,
        OperationPhase::RollingBack,
        rollback_state,
        &[],
    ) {
        Ok(supervision) => supervision,
        Err(error) => {
            let result = Err(performance_supervision_error(
                error,
                OperationPhase::RollingBack,
            ));
            record_performance_operation_result(
                state,
                PerformanceOperationResultRequest {
                    operation_id: &operation_id,
                    terminal_rollback: rollback_state,
                    changed_target: false,
                    result: &result,
                    failure_signal: operation.persistence_failure.as_ref(),
                },
            )
            .await?;
            return result.map_err(Into::into);
        }
    };
    record_performance_guardian_supervision(state, &supervision)
        .await
        .map_err(|error| {
            PerformanceOperationExecutionError::journal_transition(
                Some(operation_id.clone()),
                error,
            )
        })?;
    let Some(prepared) = preflight.prepared else {
        let result = Err(performance_install_error(InstallError::NoRollbackSnapshot));
        record_performance_operation_result(
            state,
            PerformanceOperationResultRequest {
                operation_id: &operation_id,
                terminal_rollback: rollback_state,
                changed_target: false,
                result: &result,
                failure_signal: operation.persistence_failure.as_ref(),
            },
        )
        .await?;
        return result.map_err(Into::into);
    };
    record_performance_prepared(state, &operation_id, prepared)
        .await
        .map_err(|error| {
            PerformanceOperationExecutionError::journal_transition(
                Some(operation_id.clone()),
                error,
            )
        })?;
    record_performance_effect_started(
        state,
        &operation_id,
        operation.action,
        &target_id,
        rollback_state,
    )
    .await
    .map_err(|error| {
        PerformanceOperationExecutionError::journal_transition(Some(operation_id.clone()), error)
    })?;
    progress(operation.action).await;

    let rollback_id = optional_value(operation.rollback_id.as_deref());
    let mutation = admitted.rollback_managed(rollback_id.as_deref()).await;
    let (result, terminal_rollback, changed_target) = match mutation {
        Ok(restored) => {
            let inspection = admitted.inspect(None).await;
            let result = match inspection {
                Ok(inspection) => Ok({
                    let health = inspection.health;
                    let warnings = inspection.warnings;
                    match restored {
                        ManagedRollbackOutcome::ManagedStateAbsent => PerformanceInstallResponse {
                            active: false,
                            status: "rolled_back".to_string(),
                            install_id: None,
                            health,
                            composition_id: String::new(),
                            tier: String::new(),
                            installed_count: 0,
                            managed_artifacts: Vec::new(),
                            warnings,
                        },
                        ManagedRollbackOutcome::ManagedComposition(restored_state) => {
                            PerformanceInstallResponse {
                                active: true,
                                status: "rolled_back".to_string(),
                                install_id: None,
                                health,
                                composition_id: super::super::public_performance_descriptor(
                                    &restored_state.composition_id,
                                    "composition",
                                ),
                                tier: tier_name(restored_state.tier).to_string(),
                                installed_count: restored_state.installed_mods.len(),
                                managed_artifacts: managed_artifact_summary(Some(&restored_state)),
                                warnings,
                            }
                        }
                    }
                }),
                Err(error) if managed_mutation_is_indeterminate(&error) => {
                    record_indeterminate_performance_effect(state, &operation_id).await?;
                    return Err(managed_mutation_error(error).into());
                }
                Err(error) => Err(managed_mutation_error(error)),
            };
            (result, RollbackState::Applied, true)
        }
        Err(error) if managed_mutation_is_indeterminate(&error) => {
            record_indeterminate_performance_effect(state, &operation_id).await?;
            return Err(managed_mutation_error(error).into());
        }
        Err(error) => (Err(managed_mutation_error(error)), rollback_state, false),
    };
    record_performance_operation_result(
        state,
        PerformanceOperationResultRequest {
            operation_id: &operation_id,
            terminal_rollback,
            changed_target,
            result: &result,
            failure_signal: operation.persistence_failure.as_ref(),
        },
    )
    .await?;

    result.map_err(Into::into)
}

async fn execute_performance_remove<Progress, ProgressFuture>(
    state: &AppState,
    admitted: &AppManagedCompositionAdmission,
    operation: &PerformanceOperation,
    progress: Progress,
) -> Result<PerformanceInstallResponse, PerformanceOperationExecutionError>
where
    Progress: FnOnce(PerformanceInstallAction) -> ProgressFuture,
    ProgressFuture: std::future::Future<Output = ()>,
{
    let journal_action = PerformanceInstallAction::Remove;
    let current_state = preflight_current_performance_state(admitted).await;
    let remove_target_present = matches!(&current_state, Ok(Some(_)));
    let (target_id, rollback_state) = match &current_state {
        Ok(state) => (
            state
                .as_ref()
                .map(|state| state.composition_id.clone())
                .unwrap_or_else(|| "performance_composition_lock".to_string()),
            rollback_state_for_current_state(state.as_ref()),
        ),
        Err(_) => (
            "performance_composition_lock".to_string(),
            RollbackState::Unavailable,
        ),
    };
    let operation_id = begin_performance_operation_journal(
        state,
        journal_action,
        &target_id,
        rollback_state,
        operation.status_operation_id.as_ref(),
        operation.resume_existing_journal,
    )
    .await
    .map_err(|error| {
        PerformanceOperationExecutionError::journal_transition(
            operation.status_operation_id.clone(),
            error,
        )
    })?;
    let supervision = match supervise_performance_operation(
        state,
        &operation_id,
        GuardianPerformanceOperationKind::RemoveManagedComposition,
        &target_id,
        OperationPhase::Installing,
        rollback_state,
        &[],
    ) {
        Ok(supervision) => supervision,
        Err(error) => {
            let result = Err(performance_supervision_error(
                error,
                OperationPhase::Installing,
            ));
            record_performance_operation_result(
                state,
                PerformanceOperationResultRequest {
                    operation_id: &operation_id,
                    terminal_rollback: rollback_state,
                    changed_target: false,
                    result: &result,
                    failure_signal: operation.persistence_failure.as_ref(),
                },
            )
            .await?;
            return result.map_err(Into::into);
        }
    };
    record_performance_guardian_supervision(state, &supervision)
        .await
        .map_err(|error| {
            PerformanceOperationExecutionError::journal_transition(
                Some(operation_id.clone()),
                error,
            )
        })?;
    let current_state = match current_state {
        Ok(current_state) => current_state,
        Err(error) => {
            let result = Err(error);
            record_performance_operation_result(
                state,
                PerformanceOperationResultRequest {
                    operation_id: &operation_id,
                    terminal_rollback: rollback_state,
                    changed_target: false,
                    result: &result,
                    failure_signal: operation.persistence_failure.as_ref(),
                },
            )
            .await?;
            return result.map_err(Into::into);
        }
    };
    let artifact_count = current_state
        .as_ref()
        .map(|state| u64::try_from(state.installed_mods.len()))
        .transpose()
        .map_err(|_| {
            PerformanceOperationExecutionError::journal_transition(
                Some(operation_id.clone()),
                crate::state::OperationJournalStoreError::CapacityExhausted,
            )
        })?;
    let prepared = PerformanceOperationPrepared {
        result_target_id: target_id.clone(),
        proof: match &current_state {
            Some(current) => PerformancePreparedProof::RemoveCurrent {
                graph_sha512: current.graph_sha512.clone(),
                artifact_count: artifact_count.expect("present state has an artifact count"),
            },
            None => PerformancePreparedProof::ManagedStateAbsent {},
        },
    };
    record_performance_prepared(state, &operation_id, prepared)
        .await
        .map_err(|error| {
            PerformanceOperationExecutionError::journal_transition(
                Some(operation_id.clone()),
                error,
            )
        })?;
    if current_state.is_none() {
        let result = Ok(removed_install_response());
        record_performance_operation_result(
            state,
            PerformanceOperationResultRequest {
                operation_id: &operation_id,
                terminal_rollback: rollback_state,
                changed_target: false,
                result: &result,
                failure_signal: operation.persistence_failure.as_ref(),
            },
        )
        .await?;
        return result.map_err(Into::into);
    }
    record_performance_effect_started(
        state,
        &operation_id,
        journal_action,
        &target_id,
        rollback_state,
    )
    .await
    .map_err(|error| {
        PerformanceOperationExecutionError::journal_transition(Some(operation_id.clone()), error)
    })?;
    progress(journal_action).await;

    let result = match admitted.remove_managed().await {
        Ok(()) => Ok(removed_install_response()),
        Err(error) if managed_mutation_is_indeterminate(&error) => {
            record_indeterminate_performance_effect(state, &operation_id).await?;
            return Err(managed_mutation_error(error).into());
        }
        Err(error) => Err(managed_mutation_error(error)),
    };
    record_performance_operation_result(
        state,
        PerformanceOperationResultRequest {
            operation_id: &operation_id,
            terminal_rollback: rollback_state,
            changed_target: remove_target_present && result.is_ok(),
            result: &result,
            failure_signal: operation.persistence_failure.as_ref(),
        },
    )
    .await?;

    result.map_err(Into::into)
}

async fn execute_performance_install<Resolver, ResolutionFuture, Progress, ProgressFuture>(
    request: PerformanceInstallExecutionRequest<'_>,
    resolver: Resolver,
    progress: Progress,
) -> Result<PerformanceInstallResponse, PerformanceOperationExecutionError>
where
    Resolver: FnOnce(AppState, CompositionPlan, String, String) -> ResolutionFuture,
    ResolutionFuture: std::future::Future<
            Output = Result<ManagedCompositionInstallPlan, ManagedPlanResolutionError>,
        >,
    Progress: FnOnce(PerformanceInstallAction) -> ProgressFuture + Send + 'static,
    ProgressFuture: std::future::Future<Output = ()> + Send + 'static,
{
    let PerformanceInstallExecutionRequest {
        state,
        admitted,
        operation,
        mode,
        game_version,
        loader,
    } = request;
    let current_inspection = admitted.inspect(None).await.map_err(managed_mutation_error);
    let plan = state.performance().get_plan(ResolutionRequest {
        game_version: game_version.clone(),
        loader: loader.clone(),
        mode,
        hardware: state.performance().hardware(),
        installed_mods: current_inspection
            .as_ref()
            .map(|inspection| inspection.installed_mod_evidence.clone())
            .unwrap_or_default(),
    });
    let pre_effect_rollback_state = match &current_inspection {
        Ok(inspection) => install_rollback_state_for_inspection(inspection),
        Err(_) => RollbackState::Unavailable,
    };
    let rollback_state = pre_effect_rollback_state;
    let operation_id = begin_performance_operation_journal(
        state,
        operation.action,
        &plan.composition_id,
        rollback_state,
        operation.status_operation_id.as_ref(),
        operation.resume_existing_journal,
    )
    .await
    .map_err(|error| {
        PerformanceOperationExecutionError::journal_transition(
            operation.status_operation_id.clone(),
            error,
        )
    })?;
    let guardian_facts = performance_plan_guardian_facts(&plan, OperationPhase::Installing);
    let supervision = match supervise_performance_operation(
        state,
        &operation_id,
        GuardianPerformanceOperationKind::ApplyManagedComposition,
        &plan.composition_id,
        OperationPhase::Installing,
        rollback_state,
        &guardian_facts,
    ) {
        Ok(supervision) => supervision,
        Err(error) => {
            let result = Err(performance_supervision_error(
                error,
                OperationPhase::Installing,
            ));
            record_performance_operation_result(
                state,
                PerformanceOperationResultRequest {
                    operation_id: &operation_id,
                    terminal_rollback: pre_effect_rollback_state,
                    changed_target: false,
                    result: &result,
                    failure_signal: operation.persistence_failure.as_ref(),
                },
            )
            .await?;
            return result.map_err(Into::into);
        }
    };
    record_performance_guardian_supervision(state, &supervision)
        .await
        .map_err(|error| {
            PerformanceOperationExecutionError::journal_transition(
                Some(operation_id.clone()),
                error,
            )
        })?;
    if let Err(error) = current_inspection {
        let result = Err(error);
        record_performance_operation_result(
            state,
            PerformanceOperationResultRequest {
                operation_id: &operation_id,
                terminal_rollback: pre_effect_rollback_state,
                changed_target: false,
                result: &result,
                failure_signal: operation.persistence_failure.as_ref(),
            },
        )
        .await?;
        return result.map_err(Into::into);
    }
    let install_plan = match resolver(state.clone(), plan.clone(), game_version, loader).await {
        Ok(install_plan) => install_plan,
        Err(error) => {
            let result = Err(managed_plan_resolution_error(error));
            record_performance_operation_result(
                state,
                PerformanceOperationResultRequest {
                    operation_id: &operation_id,
                    terminal_rollback: pre_effect_rollback_state,
                    changed_target: false,
                    result: &result,
                    failure_signal: operation.persistence_failure.as_ref(),
                },
            )
            .await?;
            return result.map_err(Into::into);
        }
    };
    record_performance_plan_resolved(
        state,
        &operation_id,
        operation.action,
        &plan.composition_id,
        rollback_state,
        &install_plan,
    )
    .await
    .map_err(|error| {
        PerformanceOperationExecutionError::journal_transition(Some(operation_id.clone()), error)
    })?;
    let effect_state = state.clone();
    let effect_operation_id = operation_id.clone();
    let effect_action = operation.action;
    let effect_target_id = plan.composition_id.clone();
    let execution = admitted
        .ensure_installed(
            &install_plan,
            performance_artifact_transfer_resolver(),
            move || async move {
                let rollback_ready = RollbackState::Available;
                record_performance_effect_started(
                    &effect_state,
                    &effect_operation_id,
                    effect_action,
                    &effect_target_id,
                    rollback_ready,
                )
                .await
                .map_err(|error| {
                    PerformanceOperationExecutionError::journal_transition(
                        Some(effect_operation_id.clone()),
                        error,
                    )
                })?;
                progress(effect_action).await;
                Ok(())
            },
        )
        .await;
    let (result, terminal_rollback, changed_target) = match execution {
        Ok(outcome) => {
            let terminal_rollback =
                install_terminal_rollback(pre_effect_rollback_state, outcome.rollback_ready());
            let changed_target = outcome.target_changed();
            let installed_state = outcome.into_state();
            let result = match admitted.inspect(Some(&plan)).await {
                Ok(inspection) => {
                    let health = inspection.health;
                    let warnings = response_warnings(&plan, inspection.warnings);
                    Ok(PerformanceInstallResponse {
                        active: true,
                        status: "complete".to_string(),
                        install_id: None,
                        health,
                        composition_id: super::super::public_performance_descriptor(
                            &installed_state.composition_id,
                            "composition",
                        ),
                        tier: tier_name(installed_state.tier).to_string(),
                        installed_count: installed_state.installed_mods.len(),
                        managed_artifacts: managed_artifact_summary(Some(&installed_state)),
                        warnings,
                    })
                }
                Err(error) if managed_mutation_is_indeterminate(&error) => {
                    record_indeterminate_performance_effect(state, &operation_id).await?;
                    return Err(managed_mutation_error(error).into());
                }
                Err(error) => Err(managed_mutation_error(error)),
            };
            (result, terminal_rollback, changed_target)
        }
        Err(ManagedInstallExecutionError::Mutation { source, .. })
            if managed_mutation_is_indeterminate(&source) =>
        {
            record_indeterminate_performance_effect(state, &operation_id).await?;
            return Err(managed_mutation_error(source).into());
        }
        Err(ManagedInstallExecutionError::Mutation {
            source,
            rollback_ready,
        }) => (
            Err(managed_mutation_error(source)),
            install_terminal_rollback(pre_effect_rollback_state, rollback_ready),
            false,
        ),
        Err(ManagedInstallExecutionError::BeforeTargetEffect { error, .. }) => return Err(error),
    };
    record_performance_operation_result(
        state,
        PerformanceOperationResultRequest {
            operation_id: &operation_id,
            terminal_rollback,
            changed_target,
            result: &result,
            failure_signal: operation.persistence_failure.as_ref(),
        },
    )
    .await?;

    result.map_err(Into::into)
}

fn performance_artifact_transfer_resolver() -> ManagedArtifactTransferResolver {
    const MAX_CLIENTS: usize = 8;
    let clients = Arc::new(tokio::sync::Mutex::new(HashMap::<
        TransferOrigin,
        TransferClient,
    >::new()));
    ManagedArtifactTransferResolver::new(
        move |url| {
            let clients = Arc::clone(&clients);
            async move {
                let origin = TransferOrigin::from_url(&url).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "managed artifact transfer URL is not admitted",
                    )
                })?;
                let mut clients = clients.lock().await;
                if let Some(client) = clients.get(&origin) {
                    return Ok(client.clone());
                }
                let client = pinned_public_transfer_client(origin.clone(), &url).await?;
                if clients.len() < MAX_CLIENTS {
                    clients.insert(origin, client.clone());
                }
                Ok(client)
            }
        },
        managed_transfer_retry_policy(),
    )
}

fn supervise_performance_operation(
    state: &AppState,
    operation_id: &OperationId,
    operation: GuardianPerformanceOperationKind,
    target_id: &str,
    phase: OperationPhase,
    rollback_state: RollbackState,
    facts: &[GuardianFact],
) -> Result<GuardianPerformanceSupervisionPlan, GuardianPerformanceSupervisionRejection> {
    plan_performance_operation_supervision(
        GuardianMode::from_config(&state.config().current().guardian_mode),
        operation_id,
        operation,
        target_id,
        phase,
        rollback_state,
        facts,
    )
}

pub(super) fn plan_performance_operation_supervision(
    mode: GuardianMode,
    operation_id: &OperationId,
    operation: GuardianPerformanceOperationKind,
    target_id: &str,
    phase: OperationPhase,
    rollback_state: RollbackState,
    facts: &[GuardianFact],
) -> Result<GuardianPerformanceSupervisionPlan, GuardianPerformanceSupervisionRejection> {
    if facts.len() > MAX_OPERATION_EVIDENCE_FACTS {
        return Err(GuardianPerformanceSupervisionRejection::GuardianBlocked);
    }
    let mut bound_facts = facts.to_vec();
    for fact in &mut bound_facts {
        match fact.operation_id.as_ref() {
            Some(actual) if actual != operation_id => {
                return Err(GuardianPerformanceSupervisionRejection::GuardianBlocked);
            }
            Some(_) => {}
            None => fact.operation_id = Some(operation_id.clone()),
        }
    }
    let evidence = OperationEvidenceBatch::try_from_guardian_operation(operation_id, &bound_facts)
        .map_err(|_| GuardianPerformanceSupervisionRejection::GuardianBlocked)?;
    plan_performance_supervision(GuardianPerformanceSupervisionRequest {
        mode,
        phase,
        operation,
        target: performance_composition_target(target_id),
        evidence: &evidence,
        rollback_state,
        context: GuardianPolicyContext::current_operation(),
    })
}

async fn preflight_current_performance_state(
    admitted: &AppManagedCompositionAdmission,
) -> Result<Option<CompositionState>, (StatusCode, Json<serde_json::Value>)> {
    admitted
        .inspect(None)
        .await
        .map(|inspection| inspection.state)
        .map_err(managed_mutation_error)
}

async fn rollback_preflight(
    admitted: &AppManagedCompositionAdmission,
    rollback_id: Option<&str>,
) -> Result<PerformanceRollbackPreflight, (StatusCode, Json<serde_json::Value>)> {
    let rollback_id = optional_value(rollback_id);
    if rollback_id.as_deref().is_some_and(|snapshot_id| {
        snapshot_id.len() > 96
            || !snapshot_id
                .bytes()
                .all(|value| value.is_ascii_alphanumeric() || value == b'-' || value == b'_')
    }) {
        return Err(performance_supervision_error(
            GuardianPerformanceSupervisionRejection::RollbackUnavailable,
            OperationPhase::RollingBack,
        ));
    }
    let inspection = admitted
        .inspect(None)
        .await
        .map_err(managed_mutation_error)?;
    let snapshot = rollback_id.as_deref().map_or_else(
        || {
            inspection
                .rollback_snapshots
                .iter()
                .find(|snapshot| snapshot.latest)
        },
        |rollback_id| {
            inspection
                .rollback_snapshots
                .iter()
                .find(|snapshot| snapshot.id == rollback_id)
        },
    );

    Ok(match snapshot {
        Some(snapshot) => {
            let artifact_count = u64::try_from(snapshot.artifact_count).map_err(|_| {
                internal_install_error("performance rollback snapshot is too large")
            })?;
            let target = match snapshot.target {
                RollbackSnapshotTarget::ManagedStateAbsent => {
                    PerformanceRollbackTarget::ManagedStateAbsent
                }
                RollbackSnapshotTarget::ManagedComposition => {
                    PerformanceRollbackTarget::ManagedComposition
                }
            };
            let target_id = snapshot
                .composition_id
                .clone()
                .unwrap_or_else(|| "performance_managed_state_absent".to_string());
            PerformanceRollbackPreflight {
                target_id: target_id.clone(),
                rollback_state: RollbackState::Available,
                prepared: Some(PerformanceOperationPrepared {
                    result_target_id: target_id,
                    proof: PerformancePreparedProof::RollbackSnapshot {
                        snapshot_id: snapshot.id.clone(),
                        target,
                        artifact_count,
                    },
                }),
            }
        }
        None => PerformanceRollbackPreflight {
            target_id: "performance_rollback_snapshot".to_string(),
            rollback_state: RollbackState::Unavailable,
            prepared: None,
        },
    })
}

fn rollback_state_for_current_state(state: Option<&CompositionState>) -> RollbackState {
    if state.is_some() {
        RollbackState::Available
    } else {
        RollbackState::Unavailable
    }
}

fn install_rollback_state_for_inspection(
    inspection: &ManagedCompositionInspection,
) -> RollbackState {
    if inspection.state.is_some()
        || inspection
            .rollback_snapshots
            .iter()
            .any(|snapshot| snapshot.rollback_available)
    {
        RollbackState::Available
    } else {
        RollbackState::Unavailable
    }
}

fn install_terminal_rollback(
    pre_effect_rollback: RollbackState,
    rollback_ready: bool,
) -> RollbackState {
    if rollback_ready || matches!(pre_effect_rollback, RollbackState::Available) {
        RollbackState::Available
    } else {
        RollbackState::Unavailable
    }
}

fn removed_install_response() -> PerformanceInstallResponse {
    PerformanceInstallResponse {
        active: false,
        status: "removed".to_string(),
        install_id: None,
        health: BundleHealth::Disabled,
        composition_id: String::new(),
        tier: String::new(),
        installed_count: 0,
        managed_artifacts: Vec::<PerformanceManagedArtifactSummary>::new(),
        warnings: Vec::new(),
    }
}

fn internal_install_error(_error: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    tracing::warn!(
        error_class = "performance_internal",
        "managed performance request failed"
    );
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": PERFORMANCE_INSTALL_INTERNAL_ERROR })),
    )
}

fn performance_supervision_error(
    error: GuardianPerformanceSupervisionRejection,
    phase: OperationPhase,
) -> (StatusCode, Json<serde_json::Value>) {
    let status = match &error {
        GuardianPerformanceSupervisionRejection::UnsafeOwnership
        | GuardianPerformanceSupervisionRejection::GuardianBlocked
        | GuardianPerformanceSupervisionRejection::RollbackUnavailable => StatusCode::BAD_REQUEST,
        GuardianPerformanceSupervisionRejection::MissingJournal
        | GuardianPerformanceSupervisionRejection::UnsafePublicBoundary => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    let Some(outcome) =
        author_guardian_copy(GuardianCopyRequest::performance_rejection(error, phase))
    else {
        return internal_install_error("Guardian performance copy rule is missing");
    };
    (
        status,
        Json(serde_json::json!({
            "error": outcome.summary()
        })),
    )
}

pub(super) fn performance_install_error(
    error: InstallError,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        InstallError::NoRollbackSnapshot | InstallError::RollbackSnapshotNotFound => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error.to_string() })),
        ),
        InstallError::State(StateError::InvalidRollbackId) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid performance rollback snapshot id" })),
        ),
        InstallError::State(StateError::InvalidRollback(_)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid performance rollback state" })),
        ),
        InstallError::State(StateError::InvalidFilename(_)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid performance artifact metadata"
            })),
        ),
        InstallError::State(StateError::Parse(_) | StateError::InvalidState(_)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid performance state metadata"
            })),
        ),
        InstallError::State(StateError::InvalidOwnership { .. }) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid performance artifact ownership metadata"
            })),
        ),
        InstallError::State(StateError::InvalidIntegrity { .. }) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid performance artifact integrity metadata"
            })),
        ),
        InstallError::Transfer => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": "Could not download managed performance files. Check the connection and try again."
            })),
        ),
        error => internal_install_error(error),
    }
}

pub(super) fn managed_plan_resolution_error(
    error: ManagedPlanResolutionError,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        ManagedPlanResolutionError::ResolutionFailed => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": "Could not resolve managed performance dependencies. Check the connection and try again."
            })),
        ),
        ManagedPlanResolutionError::ResolutionConflict => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "Managed performance dependencies are unavailable for this Minecraft version and loader."
            })),
        ),
        ManagedPlanResolutionError::InvalidRootSet
        | ManagedPlanResolutionError::InvalidArtifactGraph
        | ManagedPlanResolutionError::InvalidDependencyGraph
        | ManagedPlanResolutionError::SealRejected => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": "Managed performance provider data could not be trusted. Try again later."
            })),
        ),
    }
}

fn managed_admission_error(
    error: crate::state::ManagedInstanceAdmissionError,
) -> (StatusCode, Json<serde_json::Value>) {
    let status = match error {
        crate::state::ManagedInstanceAdmissionError::InstanceNotFound => StatusCode::NOT_FOUND,
        crate::state::ManagedInstanceAdmissionError::InvalidInstanceIdentity => {
            StatusCode::BAD_REQUEST
        }
        crate::state::ManagedInstanceAdmissionError::ActiveSession => StatusCode::CONFLICT,
        crate::state::ManagedInstanceAdmissionError::ForeignForegroundAuthority
        | crate::state::ManagedInstanceAdmissionError::Owner(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    (
        status,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
}

fn managed_mutation_error(
    error: axial_performance::ManagedMutationError,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        axial_performance::ManagedMutationError::Definite(error) => {
            performance_install_error(error)
        }
        axial_performance::ManagedMutationError::Indeterminate(error) => {
            tracing::warn!(
                error_class = "performance_indeterminate",
                operation = error.operation(),
                "managed performance mutation outcome was indeterminate"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": PERFORMANCE_INSTALL_INTERNAL_ERROR })),
            )
        }
    }
}

fn managed_mutation_is_indeterminate(error: &ManagedMutationError) -> bool {
    matches!(error, ManagedMutationError::Indeterminate(_))
}

async fn record_indeterminate_performance_effect(
    state: &AppState,
    operation_id: &OperationId,
) -> Result<(), PerformanceOperationExecutionError> {
    let Some(projection) = state.journals().performance_operation(operation_id) else {
        return Err(PerformanceOperationExecutionError::journal_transition(
            Some(operation_id.clone()),
            crate::state::OperationJournalStoreError::MissingOperation,
        ));
    };
    if matches!(projection.phase, PerformanceOperationPhase::Prepared { .. }) {
        return Ok(());
    }
    record_performance_applied_unverified(
        state,
        operation_id,
        "managed performance effect requires reconciliation",
    )
    .await
    .map_err(|error| {
        PerformanceOperationExecutionError::journal_transition(Some(operation_id.clone()), error)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ManagedPlanResolutionError, PERFORMANCE_INSTALL_INTERNAL_ERROR,
        managed_mutation_is_indeterminate, managed_plan_resolution_error,
        performance_supervision_error, plan_performance_operation_supervision,
    };
    use crate::guardian::{
        GuardianMode, GuardianPerformanceOperationKind, GuardianPerformanceSupervisionRejection,
    };
    use crate::state::contracts::{OperationId, OperationPhase, RollbackState};
    use axum::http::StatusCode;

    #[test]
    fn indeterminate_managed_mutations_require_applied_unverified_reconciliation() {
        assert!(managed_mutation_is_indeterminate(
            &axial_performance::ManagedMutationError::reconciliation_required("install")
        ));
        assert!(managed_mutation_is_indeterminate(
            &axial_performance::ManagedMutationError::owner_stopped("remove")
        ));
        assert!(!managed_mutation_is_indeterminate(
            &axial_performance::ManagedMutationError::Definite(
                axial_performance::InstallError::NoRollbackSnapshot
            )
        ));
    }

    #[test]
    fn behavior_contract_cross_owner_performance_rejections_use_exact_distinct_copy() {
        let cases = [
            (
                GuardianPerformanceSupervisionRejection::UnsafeOwnership,
                StatusCode::BAD_REQUEST,
                "Guardian blocked the performance update to protect files it does not own.",
            ),
            (
                GuardianPerformanceSupervisionRejection::MissingJournal,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Guardian blocked the performance update because recovery journaling is unavailable.",
            ),
            (
                GuardianPerformanceSupervisionRejection::UnsafePublicBoundary,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Guardian blocked the performance update because safe public evidence is unavailable.",
            ),
            (
                GuardianPerformanceSupervisionRejection::GuardianBlocked,
                StatusCode::BAD_REQUEST,
                "Guardian blocked the performance update after diagnosing an unsafe state.",
            ),
            (
                GuardianPerformanceSupervisionRejection::RollbackUnavailable,
                StatusCode::BAD_REQUEST,
                "Guardian blocked the performance rollback because no verified snapshot is available.",
            ),
        ];

        let mut messages = std::collections::HashSet::new();
        for (rejection, expected_status, expected_message) in cases {
            let (status, body) =
                performance_supervision_error(rejection, OperationPhase::RollingBack);
            let message = body.0["error"].as_str().expect("bounded error string");

            assert_eq!(status, expected_status);
            assert_eq!(message, expected_message);
            assert!(messages.insert(message.to_string()));
            assert_ne!(message, PERFORMANCE_INSTALL_INTERNAL_ERROR);
        }
    }

    #[test]
    fn performance_supervision_carries_the_allocated_operation_id() {
        let operation_id = OperationId::deterministic_test("performance-operation-identity");
        let supervision = plan_performance_operation_supervision(
            GuardianMode::Managed,
            &operation_id,
            GuardianPerformanceOperationKind::RemoveManagedComposition,
            "managed-composition",
            OperationPhase::Installing,
            RollbackState::NotApplicable,
            &[],
        )
        .expect("managed removal supervision");

        assert_eq!(
            supervision.decision.operation_id(),
            Some(&operation_id),
            "Performance policy must use the already allocated journal identity"
        );
    }

    #[test]
    fn managed_plan_resolution_errors_are_static_and_provider_safe() {
        let cases = [
            (
                ManagedPlanResolutionError::ResolutionFailed,
                StatusCode::BAD_GATEWAY,
                "Could not resolve managed performance dependencies. Check the connection and try again.",
            ),
            (
                ManagedPlanResolutionError::ResolutionConflict,
                StatusCode::CONFLICT,
                "Managed performance dependencies are unavailable for this Minecraft version and loader.",
            ),
            (
                ManagedPlanResolutionError::InvalidArtifactGraph,
                StatusCode::BAD_GATEWAY,
                "Managed performance provider data could not be trusted. Try again later.",
            ),
        ];

        for (error, expected_status, expected_message) in cases {
            let (status, body) = managed_plan_resolution_error(error);
            let message = body.0["error"].as_str().expect("safe provider error");
            assert_eq!(status, expected_status);
            assert_eq!(message, expected_message);
            for secret in ["api.modrinth.com", "access_token", "/home/", "C:\\Users\\"] {
                assert!(!message.contains(secret));
            }
        }
    }
}

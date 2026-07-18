use super::managed_plan::{ManagedPlanResolutionError, resolve_managed_install_plan};
use super::operations::{
    PerformanceApplicationError, PerformanceInstallAction, PerformanceJournalTransition,
    PerformanceOperationExecutionError, begin_performance_operation_journal,
    record_performance_effect_started, record_performance_effect_started_status,
    record_performance_guardian_supervision, record_performance_operation_result,
    record_performance_plan_resolved,
};
use super::plan_health::{
    PerformanceManagedArtifactSummary, managed_artifact_summary, performance_composition_target,
    resolve_instance_mode, resolve_instance_version_target, response_warnings, tier_name,
};
use super::{
    PerformanceInstallResponse, PerformanceOperation, PerformanceRollbackListRequest,
    optional_value, required_value,
};
use crate::guardian::{
    GuardianCopyRequest, GuardianFact, GuardianMode, GuardianPerformanceOperationKind,
    GuardianPerformanceSupervisionPlan, GuardianPerformanceSupervisionRejection,
    GuardianPerformanceSupervisionRequest, GuardianPolicyContext, author_guardian_copy,
    performance_plan_guardian_facts, plan_performance_supervision,
};
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::{OperationId, OperationPhase, RollbackState};
use crate::state::{AppManagedCompositionAdmission, AppState, IntegrityForegroundLease};
use axial_performance::{
    BundleHealth, CompositionPlan, CompositionState, InstallError, ManagedCompositionInspection,
    ManagedCompositionInstallPlan, ManagedInstallExecutionError, ManagedRollbackOutcome,
    PerformanceMode, ResolutionRequest, RollbackSnapshotSummary as CoreRollbackSnapshotSummary,
    RollbackSnapshotTarget, StateError,
};
use axum::{Json, http::StatusCode};
use serde::Serialize;

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
    Progress: FnOnce(PerformanceInstallAction) -> ProgressFuture,
    ProgressFuture: std::future::Future<Output = ()>,
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
        state,
        &admitted,
        operation,
        mode,
        game_version,
        loader,
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
        .admit_managed_instance_with_foreground(foreground, &instance.id, false)
        .await
        .map_err(managed_admission_error)?;

    if matches!(operation.action, PerformanceInstallAction::Rollback) {
        return Ok(
            match rollback_preflight(&admitted, operation.rollback_id.as_deref()).await {
                Ok((target_id, rollback)) => PerformanceJournalIdentity {
                    action: PerformanceInstallAction::Rollback,
                    target_id,
                    rollback,
                },
                Err(_) => PerformanceJournalIdentity {
                    action: PerformanceInstallAction::Rollback,
                    target_id: "performance_rollback_snapshot".to_string(),
                    rollback: RollbackState::Unavailable,
                },
            },
        );
    }

    let mode = resolve_instance_mode(state, &instance, operation.mode.as_deref())?;
    if matches!(operation.action, PerformanceInstallAction::Remove)
        || !matches!(mode, PerformanceMode::Managed)
    {
        return Ok(match preflight_current_performance_state(&admitted).await {
            Ok(current) => PerformanceJournalIdentity {
                action: PerformanceInstallAction::Remove,
                target_id: current
                    .as_ref()
                    .map(|state| state.composition_id.clone())
                    .unwrap_or_else(|| "performance_composition_lock".to_string()),
                rollback: rollback_state_for_current_state(current.as_ref()),
            },
            Err(_) => PerformanceJournalIdentity {
                action: PerformanceInstallAction::Remove,
                target_id: "performance_composition_lock".to_string(),
                rollback: RollbackState::Unavailable,
            },
        });
    }

    let (game_version, loader) = resolve_instance_version_target(
        operation.installed_versions.as_ref(),
        &instance,
        operation.game_version.as_deref(),
        operation.loader.as_deref(),
    )?;
    let inspection = admitted.inspect(None).await;
    let plan = state.performance().get_plan(ResolutionRequest {
        game_version,
        loader,
        mode,
        hardware: state.performance().hardware(),
        installed_mods: inspection
            .as_ref()
            .map(|inspection| inspection.installed_mod_evidence.clone())
            .unwrap_or_default(),
    });
    let rollback = inspection
        .as_ref()
        .map(install_rollback_state_for_inspection)
        .unwrap_or(RollbackState::Unavailable);
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
    Progress: FnOnce(PerformanceInstallAction) -> ProgressFuture,
    ProgressFuture: std::future::Future<Output = ()>,
{
    let preflight = rollback_preflight(admitted, operation.rollback_id.as_deref()).await;
    let (target_id, rollback_state) = match &preflight {
        Ok((target_id, rollback_state)) => (target_id.clone(), *rollback_state),
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
        operation.status_operation_id.as_deref(),
    )
    .await
    .map_err(|error| {
        PerformanceOperationExecutionError::journal_transition(
            operation.status_operation_id.clone().map(OperationId::new),
            error,
            PerformanceJournalTransition::created(operation.action, &target_id, rollback_state),
        )
    })?;
    if let Err(error) = preflight {
        let result = Err(error);
        record_performance_operation_result(
            state,
            &operation_id,
            operation.action,
            &target_id,
            rollback_state,
            false,
            &result,
            operation.persistence_failure.as_ref(),
        )
        .await?;
        return result.map_err(Into::into);
    }
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
                &operation_id,
                operation.action,
                &target_id,
                rollback_state,
                false,
                &result,
                operation.persistence_failure.as_ref(),
            )
            .await?;
            return result.map_err(Into::into);
        }
    };
    record_performance_guardian_supervision(state, &operation_id, &supervision)
        .await
        .map_err(|error| {
            PerformanceOperationExecutionError::journal_transition(
                Some(operation_id.clone()),
                error,
                PerformanceJournalTransition::guardian(
                    operation.action,
                    &target_id,
                    rollback_state,
                    &supervision,
                ),
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
        PerformanceOperationExecutionError::journal_transition(
            Some(operation_id.clone()),
            error,
            PerformanceJournalTransition::effect_started(
                operation.action,
                &target_id,
                rollback_state,
            ),
        )
    })?;
    record_performance_effect_started_status(
        state,
        &operation_id,
        operation.persistence_failure.as_ref(),
    )
    .await?;
    progress(operation.action).await;

    let result = async {
        let rollback_id = optional_value(operation.rollback_id.as_deref());
        let restored = admitted
            .rollback_managed(rollback_id.as_deref())
            .await
            .map_err(managed_mutation_error)?;
        let inspection = admitted
            .inspect(None)
            .await
            .map_err(managed_mutation_error)?;
        let health = inspection.health;
        let warnings = inspection.warnings;

        Ok(match restored {
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
        })
    }
    .await;
    record_performance_operation_result(
        state,
        &operation_id,
        operation.action,
        &target_id,
        rollback_state,
        result.is_ok(),
        &result,
        operation.persistence_failure.as_ref(),
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
        operation.status_operation_id.as_deref(),
    )
    .await
    .map_err(|error| {
        PerformanceOperationExecutionError::journal_transition(
            operation.status_operation_id.clone().map(OperationId::new),
            error,
            PerformanceJournalTransition::created(journal_action, &target_id, rollback_state),
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
                &operation_id,
                journal_action,
                &target_id,
                rollback_state,
                false,
                &result,
                operation.persistence_failure.as_ref(),
            )
            .await?;
            return result.map_err(Into::into);
        }
    };
    record_performance_guardian_supervision(state, &operation_id, &supervision)
        .await
        .map_err(|error| {
            PerformanceOperationExecutionError::journal_transition(
                Some(operation_id.clone()),
                error,
                PerformanceJournalTransition::guardian(
                    journal_action,
                    &target_id,
                    rollback_state,
                    &supervision,
                ),
            )
        })?;
    if let Err(error) = current_state {
        let result = Err(error);
        record_performance_operation_result(
            state,
            &operation_id,
            journal_action,
            &target_id,
            rollback_state,
            false,
            &result,
            operation.persistence_failure.as_ref(),
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
        PerformanceOperationExecutionError::journal_transition(
            Some(operation_id.clone()),
            error,
            PerformanceJournalTransition::effect_started(
                journal_action,
                &target_id,
                rollback_state,
            ),
        )
    })?;
    record_performance_effect_started_status(
        state,
        &operation_id,
        operation.persistence_failure.as_ref(),
    )
    .await?;
    progress(journal_action).await;

    let result = admitted
        .remove_managed()
        .await
        .map(|_| removed_install_response())
        .map_err(managed_mutation_error);
    record_performance_operation_result(
        state,
        &operation_id,
        journal_action,
        &target_id,
        rollback_state,
        remove_target_present && result.is_ok(),
        &result,
        operation.persistence_failure.as_ref(),
    )
    .await?;

    result.map_err(Into::into)
}

async fn execute_performance_install<Resolver, ResolutionFuture, Progress, ProgressFuture>(
    state: &AppState,
    admitted: &AppManagedCompositionAdmission,
    operation: &PerformanceOperation,
    mode: PerformanceMode,
    game_version: String,
    loader: String,
    resolver: Resolver,
    progress: Progress,
) -> Result<PerformanceInstallResponse, PerformanceOperationExecutionError>
where
    Resolver: FnOnce(AppState, CompositionPlan, String, String) -> ResolutionFuture,
    ResolutionFuture: std::future::Future<
            Output = Result<ManagedCompositionInstallPlan, ManagedPlanResolutionError>,
        >,
    Progress: FnOnce(PerformanceInstallAction) -> ProgressFuture,
    ProgressFuture: std::future::Future<Output = ()>,
{
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
        operation.status_operation_id.as_deref(),
    )
    .await
    .map_err(|error| {
        PerformanceOperationExecutionError::journal_transition(
            operation.status_operation_id.clone().map(OperationId::new),
            error,
            PerformanceJournalTransition::created(
                operation.action,
                &plan.composition_id,
                rollback_state,
            ),
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
                &operation_id,
                operation.action,
                &plan.composition_id,
                pre_effect_rollback_state,
                false,
                &result,
                operation.persistence_failure.as_ref(),
            )
            .await?;
            return result.map_err(Into::into);
        }
    };
    record_performance_guardian_supervision(state, &operation_id, &supervision)
        .await
        .map_err(|error| {
            PerformanceOperationExecutionError::journal_transition(
                Some(operation_id.clone()),
                error,
                PerformanceJournalTransition::guardian(
                    operation.action,
                    &plan.composition_id,
                    rollback_state,
                    &supervision,
                ),
            )
        })?;
    if let Err(error) = current_inspection {
        let result = Err(error);
        record_performance_operation_result(
            state,
            &operation_id,
            operation.action,
            &plan.composition_id,
            pre_effect_rollback_state,
            false,
            &result,
            operation.persistence_failure.as_ref(),
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
                &operation_id,
                operation.action,
                &plan.composition_id,
                pre_effect_rollback_state,
                false,
                &result,
                operation.persistence_failure.as_ref(),
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
        PerformanceOperationExecutionError::journal_transition(
            Some(operation_id.clone()),
            error,
            PerformanceJournalTransition::plan_resolved(
                operation.action,
                &plan.composition_id,
                rollback_state,
                &install_plan,
            ),
        )
    })?;
    let execution = admitted
        .ensure_installed(&install_plan, state.content().client(), || async {
            let rollback_ready = RollbackState::Available;
            record_performance_effect_started(
                state,
                &operation_id,
                operation.action,
                &plan.composition_id,
                rollback_ready,
            )
            .await
            .map_err(|error| {
                PerformanceOperationExecutionError::journal_transition(
                    Some(operation_id.clone()),
                    error,
                    PerformanceJournalTransition::effect_started(
                        operation.action,
                        &plan.composition_id,
                        rollback_ready,
                    ),
                )
            })?;
            record_performance_effect_started_status(
                state,
                &operation_id,
                operation.persistence_failure.as_ref(),
            )
            .await?;
            progress(operation.action).await;
            Ok(())
        })
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
                Err(error) => Err(managed_mutation_error(error)),
            };
            (result, terminal_rollback, changed_target)
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
        &operation_id,
        operation.action,
        &plan.composition_id,
        terminal_rollback,
        changed_target,
        &result,
        operation.persistence_failure.as_ref(),
    )
    .await?;

    result.map_err(Into::into)
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
    plan_performance_supervision(GuardianPerformanceSupervisionRequest {
        operation_id: Some(operation_id.clone()),
        mode,
        phase,
        operation,
        target: performance_composition_target(target_id),
        facts,
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
) -> Result<(String, RollbackState), (StatusCode, Json<serde_json::Value>)> {
    let rollback_id = optional_value(rollback_id);
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
        Some(snapshot) => (
            snapshot
                .composition_id
                .clone()
                .unwrap_or_else(|| "performance_managed_state_absent".to_string()),
            RollbackState::Available,
        ),
        None => (
            "performance_rollback_snapshot".to_string(),
            RollbackState::Unavailable,
        ),
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
        InstallError::Download(error) => {
            tracing::warn!(
                failure_kind = ?error.kind,
                io_error_kind = ?error.io_error_kind(),
                fact_count = error.facts.len(),
                "managed performance artifact staging failed"
            );
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": "Could not download managed performance files. Check the connection and try again."
                })),
            )
        }
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
        axial_performance::ManagedMutationError::Indeterminate(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": PERFORMANCE_INSTALL_INTERNAL_ERROR })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ManagedPlanResolutionError, PERFORMANCE_INSTALL_INTERNAL_ERROR,
        managed_plan_resolution_error, performance_supervision_error,
        plan_performance_operation_supervision,
    };
    use crate::guardian::{
        GuardianMode, GuardianPerformanceOperationKind, GuardianPerformanceSupervisionRejection,
    };
    use crate::state::contracts::{OperationId, OperationPhase, RollbackState};
    use axum::http::StatusCode;

    #[test]
    fn performance_supervision_rejections_use_exact_bounded_copy() {
        let cases = [
            (
                GuardianPerformanceSupervisionRejection::UnsafeOwnership,
                StatusCode::BAD_REQUEST,
            ),
            (
                GuardianPerformanceSupervisionRejection::MissingJournal,
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                GuardianPerformanceSupervisionRejection::UnsafePublicBoundary,
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                GuardianPerformanceSupervisionRejection::GuardianBlocked,
                StatusCode::BAD_REQUEST,
            ),
            (
                GuardianPerformanceSupervisionRejection::RollbackUnavailable,
                StatusCode::BAD_REQUEST,
            ),
        ];

        for (rejection, expected_status) in cases {
            let (status, body) =
                performance_supervision_error(rejection, OperationPhase::RollingBack);
            let message = body.0["error"].as_str().expect("bounded error string");

            assert_eq!(status, expected_status);
            assert_eq!(
                message,
                "performance update was blocked by Guardian safety supervision"
            );
            assert_ne!(message, PERFORMANCE_INSTALL_INTERNAL_ERROR);
        }
    }

    #[test]
    fn performance_supervision_carries_the_allocated_operation_id() {
        let operation_id = OperationId::new("performance-operation-identity");
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

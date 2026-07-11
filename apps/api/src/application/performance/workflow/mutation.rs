use super::operations::{
    PerformanceApplicationError, PerformanceInstallAction, PerformanceJournalTransition,
    PerformanceOperationExecutionError, begin_performance_operation_journal,
    record_performance_effect_started, record_performance_effect_started_status,
    record_performance_guardian_supervision, record_performance_operation_result,
};
use super::plan_health::{
    PerformanceManagedArtifactSummary, installed_mod_evidence_from_mods_dir_async,
    managed_artifact_summary, performance_composition_target, resolve_instance_mode,
    resolve_instance_version_target, response_warnings, tier_name,
};
use super::{
    PerformanceInstallResponse, PerformanceOperation, PerformanceRollbackListRequest,
    optional_value, required_value,
};
use crate::guardian::{
    GuardianFact, GuardianMode, GuardianPerformanceOperationKind,
    GuardianPerformanceSupervisionPlan, GuardianPerformanceSupervisionRejection,
    GuardianPerformanceSupervisionRequest, GuardianPolicyContext,
    performance_failure_memory_guardian_fact, performance_plan_guardian_facts,
    performance_supervision_rejection_user_outcome, plan_performance_supervision,
};
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::AppState;
use crate::state::contracts::{OperationId, OperationPhase, RollbackState};
use axial_performance::{
    BundleHealth, CompositionState, InstallError, PerformanceMode, ResolutionRequest,
    RollbackSnapshotSummary as CoreRollbackSnapshotSummary, StateError, derive_health_async,
    state::load_state_async,
};
use axum::{Json, http::StatusCode};
use serde::Serialize;

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
    pub composition_id: String,
    pub tier: axial_performance::CompositionTier,
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
    let instance = state.instances().get(&instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;
    let mods_dir = state.instances().game_dir(&instance.id).join("mods");
    let snapshots = state
        .performance()
        .list_rollback_snapshots_async(&mods_dir)
        .await
        .map_err(performance_install_error)?
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
        composition_id: super::super::public_performance_descriptor(
            &snapshot.composition_id,
            "composition",
        ),
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

pub(super) async fn execute_performance_operation(
    state: &AppState,
    operation: &PerformanceOperation,
) -> Result<PerformanceInstallResponse, PerformanceOperationExecutionError> {
    let performance = state.performance().clone();
    let instance = state
        .instances()
        .get(&operation.instance_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "instance not found" })),
            )
        })?;
    let mods_dir = state.instances().game_dir(&instance.id).join("mods");

    if matches!(operation.action, PerformanceInstallAction::Rollback) {
        return execute_performance_rollback(state, &performance, &mods_dir, operation).await;
    }

    let mode = resolve_instance_mode(state, &instance, operation.mode.as_deref())?;
    if matches!(operation.action, PerformanceInstallAction::Remove)
        || !matches!(mode, PerformanceMode::Managed)
    {
        return execute_performance_remove(state, &performance, &mods_dir, operation).await;
    }

    let (game_version, loader) = resolve_instance_version_target(
        state,
        &instance,
        operation.game_version.as_deref(),
        operation.loader.as_deref(),
    )?;
    execute_performance_install(
        state,
        &performance,
        &mods_dir,
        operation,
        mode,
        game_version,
        loader,
    )
    .await
}

pub(super) async fn performance_operation_journal_identity(
    state: &AppState,
    operation: &PerformanceOperation,
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
    let mods_dir = state.instances().game_dir(&instance.id).join("mods");

    if matches!(operation.action, PerformanceInstallAction::Rollback) {
        return Ok(
            match rollback_preflight(&mods_dir, operation.rollback_id.as_deref()).await {
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
        return Ok(match preflight_current_performance_state(&mods_dir).await {
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
        state,
        &instance,
        operation.game_version.as_deref(),
        operation.loader.as_deref(),
    )?;
    let plan = state.performance().get_plan(ResolutionRequest {
        game_version,
        loader,
        mode,
        hardware: state.performance().hardware(),
        installed_mods: installed_mod_evidence_from_mods_dir_async(&mods_dir).await,
    });
    let rollback = preflight_current_performance_state(&mods_dir)
        .await
        .map(|current| rollback_state_for_current_state(current.as_ref()))
        .unwrap_or(RollbackState::Unavailable);
    Ok(PerformanceJournalIdentity {
        action: PerformanceInstallAction::Install,
        target_id: plan.composition_id,
        rollback,
    })
}

async fn execute_performance_rollback(
    state: &AppState,
    performance: &crate::state::AppPerformanceStore,
    mods_dir: &std::path::Path,
    operation: &PerformanceOperation,
) -> Result<PerformanceInstallResponse, PerformanceOperationExecutionError> {
    let preflight = rollback_preflight(mods_dir, operation.rollback_id.as_deref()).await;
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
            &result,
            operation.persistence_failure.as_ref(),
        )
        .await?;
        return result.map_err(Into::into);
    }
    let supervision = match supervise_performance_operation(
        state,
        GuardianPerformanceOperationKind::RollbackManagedComposition,
        &target_id,
        OperationPhase::RollingBack,
        rollback_state,
        &[],
        0,
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

    let result = async {
        let restored_state =
            if let Some(rollback_id) = optional_value(operation.rollback_id.as_deref()) {
                performance
                    .rollback_managed_snapshot_async(mods_dir, &rollback_id)
                    .await
                    .map_err(performance_install_error)?
            } else {
                performance
                    .rollback_managed_async(mods_dir)
                    .await
                    .map_err(performance_install_error)?
            };
        let (health, warnings) = derive_health_async(Some(&restored_state), None, mods_dir).await;

        Ok(PerformanceInstallResponse {
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
        })
    }
    .await;
    record_performance_operation_result(
        state,
        &operation_id,
        operation.action,
        &target_id,
        rollback_state,
        &result,
        operation.persistence_failure.as_ref(),
    )
    .await?;

    result.map_err(Into::into)
}

async fn execute_performance_remove(
    state: &AppState,
    performance: &crate::state::AppPerformanceStore,
    mods_dir: &std::path::Path,
    operation: &PerformanceOperation,
) -> Result<PerformanceInstallResponse, PerformanceOperationExecutionError> {
    let journal_action = PerformanceInstallAction::Remove;
    let current_state = preflight_current_performance_state(mods_dir).await;
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
        GuardianPerformanceOperationKind::RemoveManagedComposition,
        &target_id,
        OperationPhase::Installing,
        rollback_state,
        &[],
        0,
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

    let result = performance
        .remove_managed_async(mods_dir)
        .await
        .map(|_| removed_install_response())
        .map_err(performance_install_error);
    record_performance_operation_result(
        state,
        &operation_id,
        journal_action,
        &target_id,
        rollback_state,
        &result,
        operation.persistence_failure.as_ref(),
    )
    .await?;

    result.map_err(Into::into)
}

async fn execute_performance_install(
    state: &AppState,
    performance: &crate::state::AppPerformanceStore,
    mods_dir: &std::path::Path,
    operation: &PerformanceOperation,
    mode: PerformanceMode,
    game_version: String,
    loader: String,
) -> Result<PerformanceInstallResponse, PerformanceOperationExecutionError> {
    let plan = state.performance().get_plan(ResolutionRequest {
        game_version: game_version.clone(),
        loader,
        mode,
        hardware: state.performance().hardware(),
        installed_mods: installed_mod_evidence_from_mods_dir_async(mods_dir).await,
    });
    let current_state = preflight_current_performance_state(mods_dir).await;
    let rollback_state = match &current_state {
        Ok(state) => rollback_state_for_current_state(state.as_ref()),
        Err(_) => RollbackState::Unavailable,
    };
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
    let guardian_facts =
        performance_install_guardian_facts(state, &plan, OperationPhase::Installing);
    let supervision = match supervise_performance_operation(
        state,
        GuardianPerformanceOperationKind::ApplyManagedComposition,
        &plan.composition_id,
        OperationPhase::Installing,
        rollback_state,
        &guardian_facts,
        plan.fallback_chain.len(),
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
                rollback_state,
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
    if let Err(error) = current_state {
        let result = Err(error);
        record_performance_operation_result(
            state,
            &operation_id,
            operation.action,
            &plan.composition_id,
            rollback_state,
            &result,
            operation.persistence_failure.as_ref(),
        )
        .await?;
        return result.map_err(Into::into);
    }
    record_performance_effect_started(
        state,
        &operation_id,
        operation.action,
        &plan.composition_id,
        rollback_state,
    )
    .await
    .map_err(|error| {
        PerformanceOperationExecutionError::journal_transition(
            Some(operation_id.clone()),
            error,
            PerformanceJournalTransition::effect_started(
                operation.action,
                &plan.composition_id,
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

    let result = match performance
        .ensure_installed(&plan, &game_version, mods_dir)
        .await
    {
        Ok(installed_state) => {
            let (health, warnings) =
                derive_health_async(Some(&installed_state), Some(&plan), mods_dir).await;
            let warnings = response_warnings(&plan, warnings);
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
        Err(error) => Err(performance_install_error(error)),
    };
    record_performance_operation_result(
        state,
        &operation_id,
        operation.action,
        &plan.composition_id,
        rollback_state,
        &result,
        operation.persistence_failure.as_ref(),
    )
    .await?;

    result.map_err(Into::into)
}

fn supervise_performance_operation(
    state: &AppState,
    operation: GuardianPerformanceOperationKind,
    target_id: &str,
    phase: OperationPhase,
    rollback_state: RollbackState,
    facts: &[GuardianFact],
    fallback_chain_len: usize,
) -> Result<GuardianPerformanceSupervisionPlan, GuardianPerformanceSupervisionRejection> {
    plan_performance_supervision(GuardianPerformanceSupervisionRequest {
        operation_id: None,
        mode: performance_guardian_mode(state),
        phase,
        operation,
        target: performance_composition_target(target_id),
        facts,
        fallback_chain_len,
        rollback_state,
        context: GuardianPolicyContext::current_operation(),
    })
}

fn performance_install_guardian_facts(
    state: &AppState,
    plan: &axial_performance::CompositionPlan,
    phase: OperationPhase,
) -> Vec<GuardianFact> {
    let mut facts = performance_plan_guardian_facts(plan, phase);
    facts.extend(
        state
            .failure_memory()
            .list()
            .into_iter()
            .filter(|entry| entry.target.id == plan.composition_id)
            .filter_map(|entry| performance_failure_memory_guardian_fact(&entry, phase)),
    );
    facts
}

fn performance_guardian_mode(state: &AppState) -> GuardianMode {
    match state.config().current().guardian_mode.trim() {
        "custom" => GuardianMode::Custom,
        "disabled" => GuardianMode::Disabled,
        _ => GuardianMode::Managed,
    }
}

async fn preflight_current_performance_state(
    mods_dir: &std::path::Path,
) -> Result<Option<CompositionState>, (StatusCode, Json<serde_json::Value>)> {
    load_state_async(mods_dir)
        .await
        .map_err(|error| performance_install_error(InstallError::State(error)))
}

async fn rollback_preflight(
    mods_dir: &std::path::Path,
    rollback_id: Option<&str>,
) -> Result<(String, RollbackState), (StatusCode, Json<serde_json::Value>)> {
    let snapshot = if let Some(rollback_id) = optional_value(rollback_id) {
        axial_performance::state::load_rollback_snapshot_by_id_async(mods_dir, &rollback_id)
            .await
            .map_err(|error| performance_install_error(InstallError::State(error)))?
    } else {
        axial_performance::state::load_rollback_snapshot_async(mods_dir)
            .await
            .map_err(|error| performance_install_error(InstallError::State(error)))?
    };

    Ok(match snapshot {
        Some(snapshot) => (snapshot.state.composition_id, RollbackState::Available),
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
    let status = match error {
        GuardianPerformanceSupervisionRejection::UnsafeOwnership
        | GuardianPerformanceSupervisionRejection::GuardianBlocked
        | GuardianPerformanceSupervisionRejection::FallbackUnavailable
        | GuardianPerformanceSupervisionRejection::RollbackUnavailable => StatusCode::BAD_REQUEST,
        GuardianPerformanceSupervisionRejection::MissingJournal
        | GuardianPerformanceSupervisionRejection::UnsafePublicBoundary => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    let outcome = performance_supervision_rejection_user_outcome(error, phase);
    (
        status,
        Json(serde_json::json!({
            "error": outcome.summary
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
        InstallError::State(StateError::Parse(_)) => (
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
        error => internal_install_error(error),
    }
}

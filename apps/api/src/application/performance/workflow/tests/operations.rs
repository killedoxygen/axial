use super::super::operations::{
    bootstrap_performance_operation, settle_performance_application_error,
    settle_performance_journal_rejection,
};
use super::*;
use crate::state::contracts::{
    OperationId, PerformanceOperationAction, PerformanceOperationIntent, PerformanceOperationPhase,
    PerformanceOperationPrepared, PerformanceOperationTerminal, PerformancePreparedProof,
};
use crate::state::{OperationJournalStoreError, PerformanceOperationTransition};

#[tokio::test]
async fn install_target_staging_refreshes_once_cold_and_zero_times_warm() {
    let fixture = TestFixture::new("performance-target-staging-version-index");
    let version_id = fabric_version_id("1.20.4");
    let instance_id = fixture.add_instance("Managed", &version_id);
    fixture.write_fabric_version(&version_id, "1.20.4");
    let instance = fixture
        .state
        .instances()
        .get(&instance_id)
        .expect("instance exists");
    let operation = test_operation(instance_id, PerformanceInstallAction::Install);
    let request = fixture.state.try_admit_request().expect("admit request");
    let producer = request
        .producer_handoff()
        .try_claim()
        .expect("claim producer");
    let foreground = fixture
        .state
        .register_integrity_foreground()
        .expect("register foreground")
        .wait_for_settlement()
        .await;

    let cold =
        stage_performance_installed_versions(&fixture.state, &operation, &producer, &foreground)
            .await
            .expect("cold snapshot");
    assert_eq!(fixture.state.installed_versions_walk_count(), 1);
    assert_eq!(
        resolve_instance_version_target(Some(&cold), &instance, None, None)
            .expect("resolve cold target"),
        ("1.20.4".to_string(), "fabric".to_string())
    );

    let warm =
        stage_performance_installed_versions(&fixture.state, &operation, &producer, &foreground)
            .await
            .expect("warm snapshot");
    assert_eq!(fixture.state.installed_versions_walk_count(), 1);
    assert!(resolve_instance_version_target(Some(&warm), &instance, None, None).is_ok());
    drop(foreground);
    drop(producer);
    drop(request);
    fixture.close().await;
}

#[tokio::test]
async fn performance_mutation_rejects_foreign_state_foreground_before_effect() {
    let fixture = TestFixture::new("performance-foreign-foreground-owner");
    let foreign = TestFixture::new("performance-foreign-foreground-source");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let lock_path = seed_managed_lock(&fixture.state, &instance_id, "foreign-owner-preserved");
    let foreground = foreign
        .state
        .register_integrity_foreground()
        .expect("register foreign foreground")
        .wait_for_settlement()
        .await;

    let error = execute_performance_operation(
        &fixture.state,
        &test_operation(instance_id, PerformanceInstallAction::Remove),
        &foreground,
    )
    .await
    .expect_err("foreign authority must be rejected")
    .into_application_error();
    assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(lock_path.is_file(), "foreign authority cannot run effects");
    assert!(fixture.state.journals().list().is_empty());
    drop(foreground);
    fixture.close().await;
    foreign.close().await;
}

#[tokio::test]
async fn queued_remove_returns_install_id_and_complete_projection() {
    let fixture = TestFixture::new("queued-remove-journal-projection");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let Json(response) = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id.clone()),
            game_version: None,
            loader: None,
            mode: None,
            action: Some("remove".to_string()),
            rollback_id: None,
            queued: Some(true),
        }),
    )
    .await
    .expect("queue remove");
    let operation_id = response.install_id.expect("queued operation id");
    let events = collect_install_events(&fixture.state, &operation_id).await;
    assert!(events.last().is_some_and(|event| event.done));

    let public = performance_operation_status(&fixture.state, &operation_id)
        .await
        .expect("journal-backed status");
    assert_eq!(public.status.instance_id, instance_id);
    assert_eq!(public.status.action, "remove");
    assert_eq!(public.status.state, "complete");
    let projection = fixture
        .state
        .journals()
        .performance_operation(&strict_operation_id(&operation_id))
        .expect("typed projection");
    assert!(matches!(
        projection.phase,
        PerformanceOperationPhase::Terminal {
            terminal: PerformanceOperationTerminal::Succeeded {
                changed_target: false,
                ..
            }
        }
    ));
    fixture.close().await;
}

#[tokio::test]
async fn requested_install_effective_remove_preserves_public_action() {
    let fixture = TestFixture::new("requested-install-effective-remove");
    let instance_id = fixture
        .add_persisted_instance("Custom", "1.20.4-fabric")
        .await;
    let mut operation = test_operation(instance_id, PerformanceInstallAction::Install);
    operation.mode = Some("custom".to_string());
    let request = fixture.state.try_admit_request().expect("admit request");
    let producer = request
        .producer_handoff()
        .try_claim()
        .expect("claim producer");
    let foreground = fixture
        .state
        .register_integrity_foreground()
        .expect("register foreground")
        .wait_for_settlement()
        .await;
    let identity = performance_operation_journal_identity(&fixture.state, &operation, &foreground)
        .await
        .expect("resolve effective action");
    assert_eq!(identity.action, PerformanceInstallAction::Remove);
    let projection = fixture
        .state
        .journals()
        .create_performance(performance_operation_intent(&operation, identity))
        .await
        .expect("create typed operation");
    let operation_id = projection.operation_id.to_string();
    let projection = fixture
        .state
        .journals()
        .performance_operation(&strict_operation_id(&operation_id))
        .expect("typed projection");
    assert_eq!(
        projection.intent.requested_action,
        PerformanceOperationAction::Install
    );
    assert_eq!(projection.intent.action, PerformanceOperationAction::Remove);
    assert_eq!(
        operation_from_projection(&projection).action,
        PerformanceInstallAction::Remove,
        "restart execution authority comes from effective action"
    );
    assert_eq!(
        performance_operation_status(&fixture.state, &operation_id)
            .await
            .expect("public status")
            .status
            .action,
        "install"
    );
    drop(foreground);
    drop(producer);
    drop(request);
    fixture.close().await;
}

#[tokio::test]
async fn after_admission_bootstrap_retains_same_owner_until_terminal() {
    let root = test_root("after-admission-retained-owner");
    let backend = Arc::new(ScriptedOperationBackend::default());
    let state = build_test_state_with_operation_backends(&root, backend.clone());
    let fixture = TestFixture {
        state,
        root,
        cleanup_root: true,
    };
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let mut operation = test_operation(instance_id, PerformanceInstallAction::Remove);
    let request = fixture.state.try_admit_request().expect("admit request");
    let producer = request
        .producer_handoff()
        .try_claim()
        .expect("claim producer");
    let foreground = fixture
        .state
        .register_integrity_foreground()
        .expect("register foreground")
        .wait_for_settlement()
        .await;
    let identity = PerformanceWorkerIdentity::default();
    backend.fail_next_attempts(5);

    let error = bootstrap_performance_operation(
        &fixture.state,
        &mut operation,
        &identity,
        &producer,
        &foreground,
    )
    .await
    .expect_err("post-admission persistence failure is reported");
    assert_eq!(error.response.0, StatusCode::INTERNAL_SERVER_ERROR);
    let operation_id = identity.get().expect("minted journal identity retained");
    assert_eq!(operation.status_operation_id.as_ref(), Some(&operation_id));

    let projection = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(projection) = fixture
                .state
                .journals()
                .performance_operation(&operation_id)
                .filter(|projection| projection.terminal)
            {
                break projection;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("retained child reconciles and terminalizes");
    assert_eq!(projection.operation_id, operation_id);
    assert!(matches!(
        projection.phase,
        PerformanceOperationPhase::Terminal {
            terminal: PerformanceOperationTerminal::FailedBeforeEffect { .. }
        }
    ));
    assert_eq!(fixture.state.journals().list().len(), 1);
    let (snapshot, _) = fixture
        .state
        .installs()
        .subscribe_records(&operation_id.to_string())
        .await
        .expect("same operation id admitted to progress store");
    assert!(snapshot.done);

    drop(foreground);
    drop(producer);
    drop(request);
    fixture.close().await;
}

#[tokio::test]
async fn interrupted_effect_is_applied_unverified_and_never_replayed() {
    let fixture = TestFixture::new("interrupted-effect-applied-unverified");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let projection = fixture
        .state
        .journals()
        .create_performance(test_intent(&instance_id))
        .await
        .expect("create operation");
    let operation_id = projection.operation_id;
    let prepared = test_prepared();
    transition(
        &fixture.state,
        &operation_id,
        PerformanceOperationTransition::Planning,
    )
    .await;
    transition(
        &fixture.state,
        &operation_id,
        PerformanceOperationTransition::Prepared(prepared),
    )
    .await;
    transition(
        &fixture.state,
        &operation_id,
        PerformanceOperationTransition::EffectStarted,
    )
    .await;

    let request = fixture.state.try_admit_request().expect("admit request");
    let owner = request.producer_handoff().try_claim().expect("claim owner");
    let foreground = fixture
        .state
        .register_integrity_foreground()
        .expect("register foreground")
        .wait_for_settlement()
        .await;
    let identity = PerformanceWorkerIdentity::default();
    identity.set(operation_id.clone());
    supervise_performance_worker(
        fixture.state.clone(),
        PerformanceInstallAction::Install,
        identity,
        owner,
        foreground,
        |_, _| async { panic!("injected worker panic") },
    )
    .await;
    let projection = fixture
        .state
        .journals()
        .performance_operation(&operation_id)
        .expect("settled projection");
    assert!(matches!(
        projection.phase,
        PerformanceOperationPhase::AppliedUnverified { .. }
    ));
    let public = performance_operation_status(&fixture.state, &operation_id.to_string())
        .await
        .expect("public applied-unverified projection");
    assert_eq!(public.status.state, "applied_unverified");
    assert_eq!(
        public.status.error.as_deref(),
        Some("performance operation stopped before its result could be confirmed")
    );
    assert!(public.proof.is_none());
    assert_eq!(public.view_model.tone, "err");
    assert_eq!(public.view_model.title, "Bundle update failed");
    assert_eq!(public.view_model.detail, public.status.error.unwrap());
    assert!(!public.view_model.is_terminal);
    assert!(!public.view_model.is_complete);
    assert!(!public.view_model.progress.done);
    drop(request);
    fixture.close().await;
}

#[tokio::test]
async fn deterministic_prepared_resume_rejection_terminalizes_and_releases_instance() {
    let fixture = TestFixture::new("prepared-resume-rejection-terminal");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let projection = fixture
        .state
        .journals()
        .create_performance(test_intent(&instance_id))
        .await
        .expect("create resumable operation");
    let operation_id = projection.operation_id;
    transition(
        &fixture.state,
        &operation_id,
        PerformanceOperationTransition::Planning,
    )
    .await;
    transition(
        &fixture.state,
        &operation_id,
        PerformanceOperationTransition::Prepared(test_prepared()),
    )
    .await;

    let projection = fixture
        .state
        .journals()
        .performance_operation(&operation_id)
        .expect("Prepared projection");
    settle_performance_journal_rejection(
        &fixture.state,
        &projection,
        &OperationJournalStoreError::AlreadyExists,
    )
    .await
    .expect("deterministic pre-effect rejection settles");

    let settled = fixture
        .state
        .journals()
        .performance_operation(&operation_id)
        .expect("settled projection");
    assert!(matches!(
        settled.phase,
        PerformanceOperationPhase::Terminal {
            terminal: PerformanceOperationTerminal::FailedBeforeEffect { ref error }
        } if error == "performance operation could not be resumed before effect"
    ));
    fixture
        .state
        .journals()
        .create_performance(test_intent(&instance_id))
        .await
        .expect("terminal rejection releases the instance lifecycle");
    fixture.close().await;
}

#[tokio::test]
async fn prepared_application_error_terminalizes_and_preserves_public_error() {
    let fixture = TestFixture::new("prepared-application-error-terminal");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let projection = fixture
        .state
        .journals()
        .create_performance(test_intent(&instance_id))
        .await
        .expect("create resumable operation");
    let operation_id = projection.operation_id;
    transition(
        &fixture.state,
        &operation_id,
        PerformanceOperationTransition::Planning,
    )
    .await;
    transition(
        &fixture.state,
        &operation_id,
        PerformanceOperationTransition::Prepared(test_prepared()),
    )
    .await;
    let original = (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "instance not found" })),
    );

    settle_performance_application_error(&fixture.state, &operation_id, &original)
        .await
        .expect("factual pre-effect Application error settles");

    assert_eq!(original.0, StatusCode::NOT_FOUND);
    assert_eq!(original.1.0["error"], "instance not found");
    let settled = fixture
        .state
        .journals()
        .performance_operation(&operation_id)
        .expect("settled projection");
    assert!(matches!(
        settled.phase,
        PerformanceOperationPhase::Terminal {
            terminal: PerformanceOperationTerminal::FailedBeforeEffect { ref error }
        } if error == "instance not found"
    ));
    fixture
        .state
        .journals()
        .create_performance(test_intent(&instance_id))
        .await
        .expect("terminal Application error releases the instance lifecycle");
    fixture.close().await;
}

#[tokio::test]
async fn behavior_contract_cross_owner() {
    let fixture = TestFixture::new("performance-journal-contract-cross-owner");
    let instance_id = fixture
        .add_persisted_instance("performance-journal", "1.20.4-fabric")
        .await;
    let projection = fixture
        .state
        .journals()
        .create_performance(test_intent(&instance_id))
        .await
        .expect("create typed operation");
    let operation_id = projection.operation_id;
    let prepared = test_prepared();
    transition(
        &fixture.state,
        &operation_id,
        PerformanceOperationTransition::Planning,
    )
    .await;
    transition(
        &fixture.state,
        &operation_id,
        PerformanceOperationTransition::Prepared(prepared.clone()),
    )
    .await;
    transition(
        &fixture.state,
        &operation_id,
        PerformanceOperationTransition::EffectStarted,
    )
    .await;
    transition(
        &fixture.state,
        &operation_id,
        PerformanceOperationTransition::MarkAppliedUnverified(
            "performance effect requires reconciliation".to_string(),
        ),
    )
    .await;
    let public = performance_operation_status(&fixture.state, &operation_id.to_string())
        .await
        .expect("public journal projection");
    assert_eq!(public.status.id, operation_id);
    assert_eq!(public.status.state, "applied_unverified");
    assert!(!public.view_model.is_terminal);
    assert!(public.proof.is_none());
    assert_eq!(
        fixture
            .state
            .journals()
            .current_or_latest_performance_operation(&instance_id)
            .expect("latest projection")
            .operation_id,
        operation_id
    );
    let retired_status_directory = fixture.root.join("performance").join("operations");
    let status_path = retired_status_directory.join(format!("{operation_id}.json"));
    assert!(
        !status_path.exists(),
        "workflow must not recreate status files"
    );

    let root = fixture.close_for_restart().await;
    let reloaded = load_test_state(&root).await;
    let restarted = performance_operation_status(&reloaded, &operation_id.to_string())
        .await
        .expect("restart projection");
    assert_eq!(restarted.status, public.status);
    assert_eq!(
        reloaded
            .journals()
            .current_or_latest_performance_operation(&instance_id)
            .expect("latest restart projection")
            .operation_id,
        operation_id
    );
    assert!(
        !status_path.exists(),
        "restart must not create a status file"
    );
    reloaded.shutdown().await.expect("shutdown reloaded state");
    fs::remove_dir_all(root).expect("remove fixture root");
}

#[tokio::test]
async fn missing_operation_status_route_returns_json_error() {
    let fixture = TestFixture::new("missing-operation-status");
    let error = performance_operation_status(&fixture.state, "missing")
        .await
        .expect_err("missing operation");
    assert_eq!(error.0, StatusCode::NOT_FOUND);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "performance operation not found" })
    );
    fixture.close().await;
}

fn test_operation(instance_id: String, action: PerformanceInstallAction) -> PerformanceOperation {
    PerformanceOperation {
        instance_id,
        game_version: None,
        loader: None,
        mode: None,
        action,
        rollback_id: None,
        status_operation_id: None,
        resume_existing_journal: false,
        persistence_failure: None,
        installed_versions: None,
    }
}

fn test_intent(instance_id: &str) -> PerformanceOperationIntent {
    PerformanceOperationIntent {
        instance_id: instance_id.to_string(),
        requested_action: PerformanceOperationAction::Install,
        action: PerformanceOperationAction::Install,
        base_target_id: "performance-journal-target".to_string(),
        rollback: RollbackState::Unavailable,
        game_version: Some("1.20.4".to_string()),
        loader: Some("fabric".to_string()),
        mode: Some("managed".to_string()),
        rollback_id: None,
    }
}

fn test_prepared() -> PerformanceOperationPrepared {
    PerformanceOperationPrepared {
        result_target_id: "performance-journal-target".to_string(),
        proof: PerformancePreparedProof::InstallPlan {
            graph_sha512: "a".repeat(128),
            artifact_count: 0,
            aggregate_bytes: 0,
        },
    }
}

async fn transition(
    state: &AppState,
    operation_id: &OperationId,
    transition: PerformanceOperationTransition,
) {
    state
        .journals()
        .transition_performance(operation_id, transition)
        .await
        .expect("typed performance transition");
}

fn strict_operation_id(value: &str) -> OperationId {
    OperationId::try_from(value).expect("strict operation id")
}

fn seed_managed_lock(state: &AppState, instance_id: &str, composition_id: &str) -> PathBuf {
    let mods_dir = state.instances().game_dir(instance_id).join("mods");
    let managed = test_composition_state(composition_id, Vec::new());
    write_managed_state_fixture(&mods_dir, &managed)
}

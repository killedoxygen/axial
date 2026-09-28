use crate::state::{
    AppState, IntegrityForegroundLease, KnownGoodRebuildError, LibraryOperation, ProducerLease,
    RegisteredKnownGoodRebuildSelection,
};
use axial_config::INSTANCE_REGISTRY_MAX_ENTRIES;
use axial_minecraft::{
    KnownGoodReconstructionError, KnownGoodReconstructionReceipt,
    ManagedInstallActivationContractId, verify_managed_install_reconstruction_checkpoint,
};
use futures_util::{StreamExt, future::join_all, stream};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

const MAX_STARTUP_REBUILD_GROUPS: usize = 2;

pub(crate) async fn rebuild_registered_known_good(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    producer: &ProducerLease,
    instance_id: &str,
) -> Result<(), KnownGoodRebuildError> {
    rebuild_registered_known_good_with(
        state,
        foreground,
        producer,
        instance_id,
        |version_id| async move { axial_minecraft::reconstruct_known_good(&version_id).await },
    )
    .await
}

pub(super) async fn rebuild_registered_known_good_with<Reconstruct, ReconstructFuture>(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    producer: &ProducerLease,
    instance_id: &str,
    reconstruct: Reconstruct,
) -> Result<(), KnownGoodRebuildError>
where
    Reconstruct: FnOnce(String) -> ReconstructFuture + Send + 'static,
    ReconstructFuture: Future<Output = Result<KnownGoodReconstructionReceipt, KnownGoodReconstructionError>>
        + Send
        + 'static,
{
    if super::install::checkpoint_recovery_blocks_explicit_known_good_rebuild(state.journals()) {
        return Err(KnownGoodRebuildError::InstallRecoveryActive);
    }
    state
        .rebuild_known_good_for_registered_instance(foreground, producer, instance_id, reconstruct)
        .await
}

pub(super) async fn rebuild_registered_known_good_for_version_with<Reconstruct, ReconstructFuture>(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    producer: &ProducerLease,
    library_operation: &LibraryOperation,
    version_id: &str,
    expected_contract: &ManagedInstallActivationContractId,
    reconstruct: Reconstruct,
) -> Result<(), KnownGoodRebuildError>
where
    Reconstruct: FnOnce(String) -> ReconstructFuture + Send + 'static,
    ReconstructFuture: Future<Output = Result<KnownGoodReconstructionReceipt, KnownGoodReconstructionError>>
        + Send
        + 'static,
{
    let selection = state
        .select_registered_known_good_rebuild_incarnation(
            foreground,
            library_operation,
            version_id,
            expected_contract,
        )
        .await?;
    let instance = match selection {
        RegisteredKnownGoodRebuildSelection::Eligible(instance) => instance,
        RegisteredKnownGoodRebuildSelection::NoRegisteredCandidates => {
            let receipt = reconstruct(version_id.to_string())
                .await
                .map_err(|_| KnownGoodRebuildError::ReconstructionFailed)?;
            if receipt.version_id() != version_id {
                return Err(KnownGoodRebuildError::ReceiptIdentityMismatch);
            }
            let _verified =
                verify_managed_install_reconstruction_checkpoint(expected_contract, receipt)
                    .map_err(|_| KnownGoodRebuildError::VerificationFailed)?;
            state
                .validate_managed_library_operation(library_operation)
                .map_err(|_| KnownGoodRebuildError::TargetChanged)?;
            return Ok(());
        }
        RegisteredKnownGoodRebuildSelection::RegisteredButIncompatible => {
            return Err(KnownGoodRebuildError::PersistedAuthorityInvalid);
        }
    };
    let activation_contract = state
        .rebuild_known_good_for_registered_instance_incarnation(
            foreground,
            producer,
            &instance.instance_id,
            &instance.version_id,
            &instance.created_at,
            library_operation,
            instance.persisted_contract.as_ref(),
            expected_contract,
            reconstruct,
        )
        .await?;
    if &activation_contract != expected_contract {
        return Err(KnownGoodRebuildError::VerificationFailed);
    }
    state
        .validate_managed_library_operation(library_operation)
        .map_err(|_| KnownGoodRebuildError::TargetChanged)?;
    Ok(())
}

pub(crate) async fn registered_known_good_is_live(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    instance_id: &str,
) -> bool {
    state
        .registered_instance_has_live_known_good(foreground, instance_id)
        .await
        .unwrap_or(false)
}

pub(crate) async fn settle_startup_version_bundle_publications(state: &AppState) -> bool {
    let instance_ids = match state.pending_startup_version_bundle_publication_instances() {
        Ok(instance_ids) => instance_ids,
        Err(error) => {
            tracing::error!(
                error_kind = ?error.kind(),
                "Guardian VersionBundle startup evidence is invalid"
            );
            return false;
        }
    };
    if instance_ids.is_empty() {
        return true;
    }
    let Ok(producer) = state.try_claim_producer() else {
        return false;
    };
    let Ok(foreground) = state.register_integrity_foreground() else {
        return false;
    };
    let foreground = foreground.wait_for_settlement().await;
    for instance_id in instance_ids {
        if !state.has_live_startup_version_bundle_source(&instance_id)
            && state
                .rehydrate_known_good_for_registered_instance(
                    &foreground,
                    &producer,
                    &instance_id,
                    |version_id| async move {
                        axial_minecraft::reconstruct_known_good(&version_id).await
                    },
                )
                .await
                .is_err()
        {
            tracing::error!(
                instance_id,
                "Guardian VersionBundle startup source could not be rehydrated"
            );
            return false;
        }
    }
    match state.settle_startup_version_bundle_publications().await {
        Ok(()) => true,
        Err(error) => {
            tracing::error!(
                error_kind = ?error.kind(),
                "Guardian VersionBundle startup publication remains unsettled"
            );
            false
        }
    }
}

pub(crate) fn spawn_startup_known_good_rebuilds(state: &AppState, producer: ProducerLease) -> bool {
    spawn_startup_known_good_rebuilds_with(state, producer, |version_id| async move {
        axial_minecraft::reconstruct_known_good(&version_id).await
    })
    .is_some()
}

fn spawn_startup_known_good_rebuilds_with<Reconstruct, ReconstructFuture>(
    state: &AppState,
    producer: ProducerLease,
    reconstruct: Reconstruct,
) -> Option<tokio::task::JoinHandle<()>>
where
    Reconstruct: Fn(String) -> ReconstructFuture + Clone + Send + Sync + 'static,
    ReconstructFuture: Future<Output = Result<KnownGoodReconstructionReceipt, KnownGoodReconstructionError>>
        + Send
        + 'static,
{
    let shutdown = state.subscribe_shutdown();
    if *shutdown.borrow() {
        return None;
    }
    let groups = startup_rebuild_groups(state);
    if groups.is_empty() {
        return None;
    }
    let Ok(foreground) = state.register_integrity_foreground() else {
        return None;
    };
    let state = state.clone();
    let rebuild_owner = producer.claim_child();
    Some(producer.spawn_joinable(async move {
        let foreground = foreground.wait_for_settlement().await;
        stream::iter(groups)
            .for_each_concurrent(MAX_STARTUP_REBUILD_GROUPS, |instance_ids| {
                let state = state.clone();
                let shutdown = shutdown.clone();
                let reconstruct = reconstruct.clone();
                let foreground = &foreground;
                let rebuild_owner = &rebuild_owner;
                async move {
                    if *shutdown.borrow() {
                        return;
                    }
                    let source_failure = Arc::new(Mutex::new(None));
                    let rebuilds = instance_ids.into_iter().map(|instance_id| {
                        let state = state.clone();
                        let reconstruct = reconstruct.clone();
                        let source_failure = source_failure.clone();
                        async move {
                            let _ = state
                                .rehydrate_known_good_for_registered_instance(
                                    foreground,
                                    rebuild_owner,
                                    &instance_id,
                                    move |version_id| async move {
                                        if let Some(error) = *source_failure
                                            .lock()
                                            .expect("startup source failure lock")
                                        {
                                            return Err(error);
                                        }
                                        let result = reconstruct(version_id).await;
                                        if let Err(error) = &result {
                                            *source_failure
                                                .lock()
                                                .expect("startup source failure lock") =
                                                Some(*error);
                                        }
                                        result
                                    },
                                )
                                .await;
                        }
                    });
                    join_all(rebuilds).await;
                }
            })
            .await;
    }))
}

fn startup_rebuild_groups(state: &AppState) -> Vec<Vec<String>> {
    let mut group_indexes = HashMap::<String, usize>::new();
    let mut groups = Vec::<Vec<String>>::new();
    for instance in state
        .instances()
        .list()
        .into_iter()
        .take(INSTANCE_REGISTRY_MAX_ENTRIES)
    {
        let group_index = match group_indexes.get(&instance.version_id) {
            Some(index) => *index,
            None => {
                let index = groups.len();
                group_indexes.insert(instance.version_id, index);
                groups.push(Vec::new());
                index
            }
        };
        groups[group_index].push(instance.id);
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_minecraft::known_good::{
        KnownGoodArtifactKind, KnownGoodInventory, TestKnownGoodEntry, TestKnownGoodIntegrity,
        TestKnownGoodRoot,
    };
    use axial_performance::PerformanceManager;
    use std::{
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::{
        sync::{Notify, Semaphore, mpsc},
        time::{Duration, timeout},
    };

    async fn state_fixture(label: &str) -> (AppState, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "axial-application-known-good-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let paths = AppPaths::from_root(root.to_path_buf()).expect("absolute test app root");
        let root_session = crate::state::test_root_session(&paths);
        let config = Arc::new(
            ConfigStore::load_from(paths.clone(), Arc::clone(&root_session)).expect("load config"),
        );
        let instances = Arc::new(
            InstanceStore::from_snapshot(
                paths.clone(),
                root_session,
                InstanceRegistrySnapshot::default(),
            )
            .expect("load instances"),
        );
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(paths.performance_dir())
                    .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
        });
        let foreground = state
            .register_integrity_foreground()
            .expect("register managed-library setup foreground")
            .wait_for_settlement()
            .await;
        let target = state
            .managed_library_setup_target(&foreground)
            .expect("managed-library setup target");
        state
            .commit_managed_library_setup(&foreground, &target)
            .await
            .expect("configure managed library");
        (state, root)
    }

    async fn close_fixture(state: AppState, root: &Path) {
        state
            .shutdown()
            .await
            .expect("shut down known-good fixture");
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    async fn seed_persisted_startup_authority(state: &AppState, instance_id: &str) {
        let version_id = state
            .instances()
            .get(instance_id)
            .expect("known-good fixture instance")
            .version_id;
        let foreground = state
            .register_integrity_foreground()
            .expect("register test authority foreground")
            .wait_for_settlement()
            .await;
        state
            .persist_known_good_inventory_for_test(
                &foreground,
                instance_id,
                KnownGoodInventory::from_test_entries([TestKnownGoodEntry {
                    root: TestKnownGoodRoot::Versions,
                    path: format!("{version_id}/{version_id}.jar"),
                    kind: KnownGoodArtifactKind::ClientJar,
                    integrity: TestKnownGoodIntegrity::File { size: 1 },
                }])
                .expect("test known-good inventory"),
            )
            .await;
    }

    #[tokio::test]
    async fn checkpoint_rebuild_rejects_classified_library_retirement() {
        let (state, root) = state_fixture("checkpoint-library-retirement").await;
        let version_id = "checkpoint-library-retirement-version";
        let classified_library = state
            .try_acquire_managed_library()
            .expect("capture classified library generation");
        let publication = axial_minecraft::publish_managed_install_fixture_for_test(
            classified_library.retained_core(),
            version_id,
        )
        .await
        .expect("publish checkpoint fixture");
        let evidence = match axial_minecraft::classify_managed_install_publication(
            classified_library.retained_core(),
            version_id.to_string(),
        )
        .await
        {
            axial_minecraft::ManagedInstallDurableOutcome::Committed(evidence) => evidence,
            _ => panic!("fixture must expose a committed publication"),
        };
        let expected_contract = evidence
            .committed_activation_contract_id()
            .cloned()
            .expect("fixture activation contract");
        let reconstruction =
            axial_minecraft::managed_install_reconstruction_receipt_fixture_for_test(version_id)
                .expect("reconstruct classified fixture");
        let rotating_state = state.clone();
        let foreground = state
            .register_integrity_foreground()
            .expect("register checkpoint foreground")
            .wait_for_settlement()
            .await;
        let producer = state
            .try_claim_producer()
            .expect("claim checkpoint rebuild producer");

        assert_eq!(
            rebuild_registered_known_good_for_version_with(
                &state,
                &foreground,
                &producer,
                &classified_library,
                version_id,
                &expected_contract,
                move |_| async move {
                    rotating_state
                        .retire_managed_library_generation_for_test()
                        .await;
                    Ok(reconstruction)
                },
            )
            .await,
            Err(KnownGoodRebuildError::TargetChanged)
        );

        drop((
            publication,
            evidence,
            producer,
            foreground,
            classified_library,
        ));
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn checkpoint_contract_mismatch_writes_no_registered_authority() {
        let (state, root) = state_fixture("checkpoint-contract-mismatch").await;
        let version_id = "checkpoint-contract-mismatch-version";
        let instance = state
            .instances()
            .insert_for_test("Contract mismatch", version_id)
            .expect("register contract mismatch peer");
        let library_operation = state
            .try_acquire_managed_library()
            .expect("capture checkpoint library");
        let publication = axial_minecraft::publish_managed_install_fixture_for_test(
            library_operation.retained_core(),
            version_id,
        )
        .await
        .expect("publish mismatch fixture");
        let evidence = match axial_minecraft::classify_managed_install_publication(
            library_operation.retained_core(),
            version_id.to_string(),
        )
        .await
        {
            axial_minecraft::ManagedInstallDurableOutcome::Committed(evidence) => evidence,
            _ => panic!("mismatch fixture must expose committed evidence"),
        };
        let actual_contract = evidence
            .committed_activation_contract_id()
            .cloned()
            .expect("fixture activation contract");
        let mut mismatched_contract = actual_contract.to_string();
        let digest_start = mismatched_contract
            .find('.')
            .expect("activation contract separator")
            + 1;
        let replacement = if &mismatched_contract[digest_start..digest_start + 1] == "a" {
            "b"
        } else {
            "a"
        };
        mismatched_contract.replace_range(digest_start..digest_start + 1, replacement);
        let mismatched_contract = ManagedInstallActivationContractId::parse(&mismatched_contract)
            .expect("alternate canonical activation contract");
        let acknowledgement = evidence
            .verify_install_receipt(publication)
            .expect("verify contract mismatch install receipt")
            .activate_with(|_| async { Ok::<(), axial_minecraft::KnownGoodActivationRejected>(()) })
            .await
            .expect("activate contract mismatch publication");
        assert!(matches!(
            acknowledgement.acknowledge().await,
            axial_minecraft::ManagedInstallAcknowledgementOutcome::Acknowledged
        ));
        let reconstruction =
            axial_minecraft::managed_install_reconstruction_receipt_fixture_for_test(version_id)
                .expect("reconstruct mismatch fixture");
        let foreground = state
            .register_integrity_foreground()
            .expect("register mismatch foreground")
            .wait_for_settlement()
            .await;
        let producer = state
            .try_claim_producer()
            .expect("claim mismatch rebuild producer");

        assert_eq!(
            rebuild_registered_known_good_for_version_with(
                &state,
                &foreground,
                &producer,
                &library_operation,
                version_id,
                &mismatched_contract,
                move |_| async move { Ok(reconstruction) },
            )
            .await,
            Err(KnownGoodRebuildError::VerificationFailed)
        );
        assert!(
            !root
                .join("state/known-good")
                .join(format!("{}.json", instance.id))
                .exists(),
            "contract mismatch must precede persistence"
        );
        assert!(
            !registered_known_good_is_live(&state, &foreground, &instance.id).await,
            "contract mismatch must precede live activation"
        );

        drop((producer, foreground, library_operation));
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn startup_rehydrate_never_competes_with_absent_checkpoint_bootstrap() {
        let (state, root) = state_fixture("startup-checkpoint-bootstrap-race").await;
        let version_id = "startup-checkpoint-bootstrap-version";
        let instance = state
            .instances()
            .insert_for_test("Checkpoint bootstrap peer", version_id)
            .expect("register checkpoint bootstrap peer");
        let library_operation = state
            .try_acquire_managed_library()
            .expect("capture checkpoint bootstrap library");
        let publication = axial_minecraft::publish_managed_install_fixture_for_test(
            library_operation.retained_core(),
            version_id,
        )
        .await
        .expect("publish checkpoint bootstrap fixture");
        let evidence = match axial_minecraft::classify_managed_install_publication(
            library_operation.retained_core(),
            version_id.to_string(),
        )
        .await
        {
            axial_minecraft::ManagedInstallDurableOutcome::Committed(evidence) => evidence,
            _ => panic!("checkpoint race fixture must expose committed evidence"),
        };
        let expected_contract = evidence
            .committed_activation_contract_id()
            .cloned()
            .expect("checkpoint race activation contract");
        let acknowledgement = evidence
            .verify_install_receipt(publication)
            .expect("verify checkpoint race install receipt")
            .activate_with(|_| async { Ok::<(), axial_minecraft::KnownGoodActivationRejected>(()) })
            .await
            .expect("activate checkpoint race publication");
        assert!(matches!(
            acknowledgement.acknowledge().await,
            axial_minecraft::ManagedInstallAcknowledgementOutcome::Acknowledged
        ));
        let strict_receipt =
            axial_minecraft::managed_install_reconstruction_receipt_fixture_for_test(version_id)
                .expect("reconstruct checkpoint race fixture");
        let assertion_foreground = state
            .register_integrity_foreground()
            .expect("register assertion foreground")
            .wait_for_settlement()
            .await;
        let strict_foreground = state
            .register_integrity_foreground()
            .expect("register strict checkpoint foreground")
            .wait_for_settlement()
            .await;
        let strict_producer = state
            .try_claim_producer()
            .expect("claim strict checkpoint producer");
        let strict_entered = Arc::new(Notify::new());
        let strict_release = Arc::new(Semaphore::new(0));
        let strict_state = state.clone();
        let strict_entered_for_source = strict_entered.clone();
        let strict_release_for_source = strict_release.clone();
        let strict = tokio::spawn(async move {
            rebuild_registered_known_good_for_version_with(
                &strict_state,
                &strict_foreground,
                &strict_producer,
                &library_operation,
                version_id,
                &expected_contract,
                move |_| async move {
                    strict_entered_for_source.notify_one();
                    let permit = strict_release_for_source
                        .acquire()
                        .await
                        .expect("release strict checkpoint source");
                    permit.forget();
                    Ok(strict_receipt)
                },
            )
            .await
        });

        timeout(Duration::from_secs(5), strict_entered.notified())
            .await
            .expect("strict checkpoint source entered");
        let generic_calls = Arc::new(AtomicUsize::new(0));
        let generic = spawn_startup_known_good_rebuilds_with(
            &state,
            state
                .try_claim_producer()
                .expect("claim generic startup producer"),
            {
                let generic_calls = generic_calls.clone();
                move |_| {
                    let generic_calls = generic_calls.clone();
                    async move {
                        generic_calls.fetch_add(1, Ordering::SeqCst);
                        panic!("absent startup authority must not enter reconstruction")
                    }
                }
            },
        )
        .expect("spawn generic startup rehydrate");
        timeout(Duration::from_secs(5), generic)
            .await
            .expect("generic startup rehydrate completes")
            .expect("generic startup rehydrate task");

        assert_eq!(generic_calls.load(Ordering::SeqCst), 0);
        let persisted_authority = root
            .join("state/known-good")
            .join(format!("{}.json", instance.id));
        assert!(
            !persisted_authority.exists(),
            "generic startup rehydrate must not bootstrap absent authority"
        );
        assert!(
            !registered_known_good_is_live(&state, &assertion_foreground, &instance.id).await,
            "generic startup rehydrate must not activate absent authority"
        );

        strict_release.add_permits(1);
        assert_eq!(
            timeout(Duration::from_secs(5), strict)
                .await
                .expect("strict checkpoint bootstrap completes")
                .expect("strict checkpoint bootstrap task"),
            Ok(())
        );
        assert!(persisted_authority.is_file());
        assert!(registered_known_good_is_live(&state, &assertion_foreground, &instance.id).await);

        drop(assertion_foreground);
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn startup_foreground_waits_for_cancelled_sweep_settlement_before_source_entry() {
        let (state, root) = state_fixture("sweep-settlement").await;
        let instance = state
            .instances()
            .insert_for_test("Sweep", "1.21.1")
            .expect("register instance");
        seed_persisted_startup_authority(&state, &instance.id).await;
        assert!(state.deactivate_registered_known_good_for_test(&instance.id));
        let idle = state.subscribe_integrity_idle();
        let epoch = idle.borrow().epoch();
        let reservation = state
            .try_reserve_idle_sweep(
                epoch,
                state.try_claim_producer().expect("claim sweep producer"),
            )
            .expect("reserve active sweep");
        let cancellation = reservation.cancellation();
        let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let _ = spawn_startup_known_good_rebuilds_with(
            &state,
            state.try_claim_producer().expect("claim startup producer"),
            {
                let calls = calls.clone();
                move |_| {
                    let entered_tx = entered_tx.clone();
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        entered_tx.send(()).expect("record source entry");
                        Err(KnownGoodReconstructionError::Vanilla)
                    }
                }
            },
        );

        assert!(cancellation.is_cancelled());
        for _ in 0..4 {
            tokio::task::yield_now().await;
            assert!(entered_rx.try_recv().is_err());
        }
        drop(reservation);
        timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("source enters after settlement")
            .expect("source entry");
        state.quiesce().await.expect("startup source drains");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn empty_and_pre_shutdown_startup_do_not_register_or_enter_a_source() {
        let (empty_state, empty_root) = state_fixture("empty-admission").await;
        let empty_idle = empty_state.subscribe_integrity_idle();
        let empty_before = *empty_idle.borrow();
        let empty_calls = Arc::new(AtomicUsize::new(0));
        let _ = spawn_startup_known_good_rebuilds_with(
            &empty_state,
            empty_state
                .try_claim_producer()
                .expect("claim empty startup producer"),
            {
                let empty_calls = empty_calls.clone();
                move |_| {
                    let empty_calls = empty_calls.clone();
                    async move {
                        empty_calls.fetch_add(1, Ordering::SeqCst);
                        Err(KnownGoodReconstructionError::Vanilla)
                    }
                }
            },
        );
        assert_eq!(*empty_idle.borrow(), empty_before);
        assert_eq!(empty_calls.load(Ordering::SeqCst), 0);
        close_fixture(empty_state, &empty_root).await;

        let (closing_state, closing_root) = state_fixture("closing-admission").await;
        closing_state
            .instances()
            .insert_for_test("Closing", "1.21.1")
            .expect("register closing instance");
        let closing_idle = closing_state.subscribe_integrity_idle();
        let closing_epoch = closing_idle.borrow().epoch();
        let producer = closing_state
            .try_claim_producer()
            .expect("claim closing startup producer");
        let mut shutdown = closing_state.subscribe_shutdown();
        let quiesce_state = closing_state.clone();
        let quiesce = tokio::spawn(async move { quiesce_state.quiesce().await });
        timeout(Duration::from_secs(5), async {
            loop {
                if *shutdown.borrow_and_update() {
                    return;
                }
                shutdown
                    .changed()
                    .await
                    .expect("shutdown signal remains live");
            }
        })
        .await
        .expect("shutdown starts");
        let closing_calls = Arc::new(AtomicUsize::new(0));
        let _ = spawn_startup_known_good_rebuilds_with(&closing_state, producer, {
            let closing_calls = closing_calls.clone();
            move |_| {
                let closing_calls = closing_calls.clone();
                async move {
                    closing_calls.fetch_add(1, Ordering::SeqCst);
                    Err(KnownGoodReconstructionError::Vanilla)
                }
            }
        });
        assert_eq!(closing_idle.borrow().epoch(), closing_epoch);
        assert_eq!(closing_calls.load(Ordering::SeqCst), 0);
        timeout(Duration::from_secs(5), quiesce)
            .await
            .expect("closing producer releases")
            .expect("quiesce task")
            .expect("quiesce succeeds");
        close_fixture(closing_state, &closing_root).await;
    }

    #[tokio::test]
    async fn startup_same_version_instances_share_one_source_failure() {
        let (state, root) = state_fixture("same-version").await;
        let instances = (0..32)
            .map(|index| {
                state
                    .instances()
                    .insert_for_test(format!("Instance {index}"), "1.21.1".to_string())
                    .expect("register instance")
            })
            .collect::<Vec<_>>();
        seed_persisted_startup_authority(&state, &instances[0].id).await;
        for instance in &instances {
            assert!(state.deactivate_registered_known_good_for_test(&instance.id));
        }
        let warnings_before = state.startup_warnings();
        let source_calls = Arc::new(AtomicUsize::new(0));
        let source_entered = Arc::new(Notify::new());
        let source_release = Arc::new(Semaphore::new(0));
        let producer = state.try_claim_producer().expect("claim startup owner");
        let rebuild = spawn_startup_known_good_rebuilds_with(&state, producer, {
            let source_calls = source_calls.clone();
            let source_entered = source_entered.clone();
            let source_release = source_release.clone();
            move |_| {
                let source_calls = source_calls.clone();
                let source_entered = source_entered.clone();
                let source_release = source_release.clone();
                async move {
                    source_calls.fetch_add(1, Ordering::SeqCst);
                    source_entered.notify_one();
                    let permit = source_release.acquire().await.expect("release source");
                    permit.forget();
                    Err(KnownGoodReconstructionError::Vanilla)
                }
            }
        })
        .expect("spawn same-version startup rebuild");

        timeout(Duration::from_secs(5), source_entered.notified())
            .await
            .expect("source entered");
        assert_eq!(source_calls.load(Ordering::SeqCst), 1);
        let shutdown_state = state.clone();
        let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        assert!(!quiesce.is_finished());
        source_release.add_permits(1);
        let (quiesce, rebuild) = timeout(Duration::from_secs(5), async {
            tokio::join!(quiesce, rebuild)
        })
        .await
        .expect("startup owner drains");
        quiesce.expect("quiesce task").expect("quiesce succeeds");
        rebuild.expect("startup rebuild task");

        assert_eq!(source_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.startup_warnings(), warnings_before);
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn startup_shutdown_drains_two_active_groups_and_skips_queued_groups() {
        let (state, root) = state_fixture("shutdown-groups").await;
        let instances = ["1.21.1", "1.21.2", "1.21.3"]
            .into_iter()
            .map(|version_id| {
                state
                    .instances()
                    .insert_for_test(format!("Instance {version_id}"), version_id.to_string())
                    .expect("register instance")
            })
            .collect::<Vec<_>>();
        for instance in &instances {
            seed_persisted_startup_authority(&state, &instance.id).await;
            assert!(state.deactivate_registered_known_good_for_test(&instance.id));
        }
        let (entered_tx, mut entered_rx) = mpsc::unbounded_channel::<String>();
        let source_release = Arc::new(Semaphore::new(0));
        let producer = state.try_claim_producer().expect("claim startup owner");
        let _ = spawn_startup_known_good_rebuilds_with(&state, producer, {
            let source_release = source_release.clone();
            move |version_id| {
                let entered_tx = entered_tx.clone();
                let source_release = source_release.clone();
                async move {
                    entered_tx.send(version_id).expect("record source entry");
                    let permit = source_release.acquire().await.expect("release source");
                    permit.forget();
                    Err(KnownGoodReconstructionError::Vanilla)
                }
            }
        });

        let first = timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("first source enters")
            .expect("first source id");
        let second = timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("second source enters")
            .expect("second source id");
        assert_ne!(first, second);
        let shutdown_state = state.clone();
        let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        tokio::task::yield_now().await;
        assert!(!quiesce.is_finished());
        source_release.add_permits(2);
        timeout(Duration::from_secs(5), quiesce)
            .await
            .expect("active groups drain")
            .expect("quiesce task")
            .expect("quiesce succeeds");

        assert!(
            entered_rx.try_recv().is_err(),
            "queued group must not enter"
        );
        close_fixture(state, &root).await;
    }
}

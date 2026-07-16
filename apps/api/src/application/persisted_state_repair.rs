use crate::guardian::GuardianMode;
use crate::guardian::persisted_state_repair::{
    PersistedStateRepairDisposition, assess_persisted_state_repair,
};
use crate::state::{AppState, PersistedStateRepairExecutionError};

pub(crate) async fn settle_startup_persisted_state_repairs(state: &AppState) -> bool {
    let Ok(producer) = state.try_claim_producer() else {
        return false;
    };
    let state = state.clone();
    matches!(
        producer
            .spawn_joinable(async move {
                let mut exact_settlement = true;
                for eligibility in state.take_persisted_state_repair_eligibilities() {
                    let mode = GuardianMode::from_config(&state.config().current().guardian_mode);
                    match assess_persisted_state_repair(mode, eligibility) {
                        PersistedStateRepairDisposition::Managed(managed) => {
                            let Ok(admission) = state
                                .admit_persisted_state_repair(managed.into_authorization())
                                .await
                            else {
                                continue;
                            };
                            if let Err(error) =
                                state.execute_persisted_state_repair(admission).await
                            {
                                if execution_failure_blocks_startup(&error) {
                                    exact_settlement = false;
                                    tracing::error!(
                                        error = %error,
                                        "persisted-state startup repair barrier remains unsettled"
                                    );
                                } else {
                                    tracing::warn!(
                                        error = %error,
                                        "persisted-state startup repair did not finish cleanly"
                                    );
                                }
                            }
                        }
                        PersistedStateRepairDisposition::Custom(offer) => offer.discard(),
                        PersistedStateRepairDisposition::RecordOnly(witness) => witness.discard(),
                    }
                }
                exact_settlement
            })
            .await,
        Ok(true)
    )
}

fn execution_failure_blocks_startup(error: &PersistedStateRepairExecutionError) -> bool {
    !matches!(
        error,
        PersistedStateRepairExecutionError::AcceptedJournalPersistence(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::contracts::PersistedStateRepairTerminalOutcome;
    use crate::state::{
        AppLifecyclePhase, AppStateInit, InstallStore, SessionStore,
        persisted_state_rejected_record_eligibility_for_test,
    };
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_performance::PerformanceManager;
    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn custom_and_disabled_consume_candidates_without_effect_or_durable_records() {
        for mode in ["custom", "disabled"] {
            let fixture = Fixture::new(&format!("{mode}-record-only"));
            let source = fixture.publish_candidate(1);
            fixture.set_mode(mode);

            assert!(settle_startup_persisted_state_repairs(&fixture.state).await);
            assert!(source.is_file());
            assert!(persisted_repair_journals(&fixture.state).is_empty());
            assert!(persisted_repair_memories(&fixture.state).is_empty());

            fixture.set_mode("managed");
            assert!(settle_startup_persisted_state_repairs(&fixture.state).await);
            assert!(source.is_file(), "{mode} candidate must be consumed once");
            assert!(persisted_repair_journals(&fixture.state).is_empty());
            assert!(persisted_repair_memories(&fixture.state).is_empty());
            fixture.close().await;
        }
    }

    #[tokio::test]
    async fn dropped_waiter_leaves_the_complete_managed_batch_owned_until_quiescence() {
        let fixture = Fixture::new("managed-cancelled-waiter");
        fixture.set_mode("managed");
        let sources = fixture.publish_candidates(&[1, 2]);
        let config_gate = fixture
            .state
            .config()
            .acquire_mutation()
            .await
            .expect("hold persisted-state repair config gate");

        let mut waiter = Box::pin(settle_startup_persisted_state_repairs(&fixture.state));
        poll_pending(waiter.as_mut());
        drop(waiter);

        let shutdown_state = fixture.state.clone();
        let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        wait_for_phase(&fixture.state, AppLifecyclePhase::QuiescingProducers).await;
        assert!(!quiesce.is_finished());

        drop(config_gate);
        timeout(Duration::from_secs(5), quiesce)
            .await
            .expect("persisted-state repair producer drain deadline")
            .expect("quiesce task")
            .expect("quiesce drains the persisted-state repair producer");

        assert!(sources.iter().all(|source| !source.exists()));
        let journals = persisted_repair_journals(&fixture.state);
        assert_eq!(journals.len(), 2);
        assert!(journals.iter().all(|journal| {
            journal
                .persisted_state_repair_terminal()
                .is_some_and(|terminal| {
                    terminal.outcome() == PersistedStateRepairTerminalOutcome::Quarantined
                })
        }));
        let memories = persisted_repair_memories(&fixture.state);
        assert_eq!(memories.len(), 2);
        for journal in journals {
            let terminal = journal
                .persisted_state_repair_terminal()
                .expect("managed Application repair terminal");
            assert!(
                memories
                    .iter()
                    .any(|memory| { memory.persisted_state_repair_terminal() == Some(terminal) })
            );
        }
        fixture.close().await;
    }

    #[test]
    fn only_exactly_settled_persistence_warnings_may_cross_the_startup_barrier() {
        use crate::state::OperationJournalStoreError;
        use crate::state::failure_memory::FailureMemoryStoreError;

        assert!(execution_failure_blocks_startup(
            &PersistedStateRepairExecutionError::Plan(OperationJournalStoreError::RetryRequired,)
        ));
        assert!(execution_failure_blocks_startup(
            &PersistedStateRepairExecutionError::Terminal(
                OperationJournalStoreError::RetryRequired,
            )
        ));
        assert!(execution_failure_blocks_startup(
            &PersistedStateRepairExecutionError::Memory(FailureMemoryStoreError::Persistence(
                std::io::Error::other("permanent failure")
            ),)
        ));
        assert!(!execution_failure_blocks_startup(
            &PersistedStateRepairExecutionError::AcceptedJournalPersistence(
                OperationJournalStoreError::Persistence(std::io::Error::other(
                    "accepted persistence warning",
                )),
            )
        ));
    }

    struct Fixture {
        state: AppState,
        root: PathBuf,
        records: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let root = test_root(label);
            let paths = test_paths(&root);
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
            let instances = Arc::new(
                InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
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
                    PerformanceManager::load_for_startup(&paths.config_dir)
                        .expect("performance manager"),
                ),
                startup_warnings: Vec::new(),
                frontend_dir: root.join("frontend"),
            });
            let records = root.join("rejected-records");
            fs::create_dir_all(&records).expect("create rejected record directory");
            Self {
                state,
                root,
                records,
            }
        }

        fn set_mode(&self, mode: &str) {
            let mut config = self.state.config().current();
            config.guardian_mode = mode.to_string();
            self.state.replace_config_for_test(config);
        }

        fn publish_candidate(&self, index: u128) -> PathBuf {
            self.publish_candidates(&[index])
                .pop()
                .expect("one published candidate")
        }

        fn publish_candidates(&self, indices: &[u128]) -> Vec<PathBuf> {
            let candidates = indices
                .iter()
                .map(|index| {
                    let record_id = format!("performance-install-{index:032x}");
                    let file_name = format!("{record_id}.json");
                    let source = self.records.join(&file_name);
                    fs::write(&source, br#"{"schema":"invalid"}"#)
                        .expect("write rejected persisted-state record");
                    let eligibility = persisted_state_rejected_record_eligibility_for_test(
                        &self.records,
                        OsStr::new(&file_name),
                        &record_id,
                    )
                    .expect("derive exact rejected persisted-state eligibility");
                    (source, eligibility)
                })
                .collect::<Vec<_>>();
            let (sources, eligibilities) = candidates.into_iter().unzip();
            self.state
                .publish_persisted_state_repair_eligibilities_for_test(eligibilities);
            sources
        }

        async fn close(self) {
            self.state.shutdown().await.expect("fixture shutdown");
            drop(self.state);
            fs::remove_dir_all(self.root).expect("remove fixture root");
        }
    }

    fn persisted_repair_journals(
        state: &AppState,
    ) -> Vec<crate::state::contracts::OperationJournalEntry> {
        state
            .journals()
            .list()
            .into_iter()
            .filter(|journal| journal.persisted_state_repair_attempt().is_some())
            .collect()
    }

    fn persisted_repair_memories(
        state: &AppState,
    ) -> Vec<crate::state::failure_memory::GuardianFailureMemoryEntry> {
        state
            .failure_memory()
            .list()
            .into_iter()
            .filter(|memory| memory.persisted_state_repair_terminal().is_some())
            .collect()
    }

    fn poll_pending<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) {
        let waker = futures_util::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        assert!(matches!(
            std::future::Future::poll(future.as_mut(), &mut context),
            std::task::Poll::Pending
        ));
    }

    async fn wait_for_phase(state: &AppState, expected: AppLifecyclePhase) {
        timeout(Duration::from_secs(5), async {
            while state.lifecycle_phase() != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("lifecycle phase deadline");
    }

    fn test_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "axial-application-persisted-state-repair-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("create fixture root");
        root
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }
}

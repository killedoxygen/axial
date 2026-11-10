//! The settings workflow. A commit projects the selected account under the
//! same transaction, and telemetry consent changes only after a durable commit.

use super::{
    ConfigLaunchAuthMode, ConfigPatch, ConfigView, FlagOverridePatch, FlagsResponse,
    InterfacePreferencesReceipt, InterfacePreferencesSnapshot, InterfacePreferencesUpdate,
    SettingsError, SettingsStore,
};
use crate::{
    accounts::{
        directory::AccountDirectory,
        model::{AccountError, LaunchAuthMode},
    },
    storage::rusqlite::Connection,
    tasks::{CancellationToken, TaskOwner},
    telemetry::{Telemetry, TelemetryErrorKind, TelemetryEvent},
};
use std::{future::Future, sync::Arc};

#[derive(Clone)]
pub struct SettingsService {
    store: Arc<SettingsStore>,
    accounts: Arc<AccountDirectory>,
    telemetry: Arc<Telemetry>,
    tasks: TaskOwner,
}

impl SettingsService {
    pub fn new(
        store: Arc<SettingsStore>,
        accounts: Arc<AccountDirectory>,
        telemetry: Arc<Telemetry>,
        tasks: TaskOwner,
    ) -> Result<Self, SettingsError> {
        if !accounts.uses_metadata(store.metadata())
            || store.telemetry_identity_enabled() != telemetry.export_configured()
        {
            return Err(SettingsError::Unavailable);
        }
        Ok(Self {
            store,
            accounts,
            telemetry,
            tasks,
        })
    }

    pub async fn config(&self) -> Result<ConfigView, SettingsError> {
        self.clone()
            .blocking(|owner| {
                owner.store.current_with_projection(|connection, config| {
                    owner.project_accounts(connection, config)
                })
            })
            .await
    }

    pub async fn commit(&self, patch: ConfigPatch) -> Result<ConfigView, SettingsError> {
        self.owned(move |owner, cancellation| async move {
            let consent = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(SettingsError::Unavailable),
                consent = owner.telemetry.consent_change_owned() => consent,
            };
            if cancellation.is_cancelled() {
                return Err(SettingsError::Unavailable);
            }
            owner
                .blocking(move |owner| {
                    // An opt-out that fails must not export its own failure.
                    let report = patch.telemetry_enabled != Some(false);
                    let commit = owner
                        .store
                        .commit_with_transaction(patch, |transaction, config| {
                            owner.project_accounts(transaction, config)
                        })
                        .inspect_err(|error| {
                            if report {
                                owner.report_save_failure(error);
                            }
                        })?;
                    consent.publish(
                        commit.config.telemetry_enabled,
                        commit.telemetry_identity.as_deref(),
                    );
                    Ok(commit.config)
                })
                .await
        })
        .await
    }

    pub async fn complete_onboarding(
        &self,
        expected_revision: u64,
    ) -> Result<ConfigView, SettingsError> {
        self.commit(ConfigPatch {
            expected_revision,
            onboarding_done: Some(true),
            ..ConfigPatch::default()
        })
        .await
    }

    pub async fn flags(&self) -> Result<FlagsResponse, SettingsError> {
        self.clone()
            .blocking(|owner| owner.store.list_flags())
            .await
    }

    pub async fn update_flag(
        &self,
        key: String,
        patch: FlagOverridePatch,
    ) -> Result<FlagsResponse, SettingsError> {
        self.owned_blocking(move |owner| {
            owner
                .store
                .update_flag(&key, patch)
                .inspect_err(|error| owner.report_save_failure(error))
        })
        .await
    }

    pub async fn interface_preferences(
        &self,
    ) -> Result<InterfacePreferencesSnapshot, SettingsError> {
        self.clone()
            .blocking(|owner| owner.store.interface_preferences())
            .await
    }

    pub async fn update_interface_preferences(
        &self,
        update: InterfacePreferencesUpdate,
    ) -> Result<InterfacePreferencesReceipt, SettingsError> {
        self.owned_blocking(move |owner| owner.store.update_interface_preferences(update))
            .await
    }

    /// The accepted task owns the write through persistence and publication,
    /// even when the requester stops waiting or shutdown begins.
    async fn owned<T, F, Fut>(&self, work: F) -> Result<T, SettingsError>
    where
        T: Send + 'static,
        F: FnOnce(Self, CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, SettingsError>> + Send + 'static,
    {
        let owner = self.clone();
        self.tasks
            .try_spawn((), move |cancellation| work(owner, cancellation))
            .map_err(|_| SettingsError::Unavailable)?
            .join()
            .await
            .map_err(|_| SettingsError::Unavailable)?
    }

    async fn owned_blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Self) -> Result<T, SettingsError> + Send + 'static,
    ) -> Result<T, SettingsError> {
        self.owned(move |owner, cancellation| async move {
            if cancellation.is_cancelled() {
                return Err(SettingsError::Unavailable);
            }
            owner.blocking(work).await
        })
        .await
    }

    async fn blocking<T: Send + 'static>(
        self,
        work: impl FnOnce(&Self) -> Result<T, SettingsError> + Send + 'static,
    ) -> Result<T, SettingsError> {
        tokio::task::spawn_blocking(move || work(&self))
            .await
            .map_err(|_| SettingsError::Unavailable)?
    }

    fn project_accounts(
        &self,
        connection: &Connection,
        config: &mut ConfigView,
    ) -> Result<(), SettingsError> {
        // Construction verifies both owners use this exact metadata connection.
        debug_assert!(self.accounts.uses_metadata(self.store.metadata()));
        config.account_selection_revision =
            AccountDirectory::selection_revision_in_transaction(connection)
                .map_err(account_error)?;
        match AccountDirectory::selection_in_transaction(connection).map_err(account_error)? {
            Some((username, mode)) => {
                config.username = username;
                config.launch_auth_mode = match mode {
                    LaunchAuthMode::Offline => ConfigLaunchAuthMode::Offline,
                    LaunchAuthMode::Online => ConfigLaunchAuthMode::Online,
                };
            }
            None => {
                config.launch_auth_mode = ConfigLaunchAuthMode::Offline;
                // The last persisted projection may belong to a removed online
                // account whose provider name is too short for offline input.
                config.username = config.offline_username().to_owned();
            }
        }
        Ok(())
    }

    fn report_save_failure(&self, error: &SettingsError) {
        if matches!(
            error,
            SettingsError::Storage(_) | SettingsError::Unavailable
        ) {
            self.telemetry.emit(TelemetryEvent::ErrorCaptured {
                kind: TelemetryErrorKind::ConfigSaveFailed,
            });
        }
    }
}

fn account_error(error: AccountError) -> SettingsError {
    match error {
        AccountError::InvalidInput(message) => SettingsError::Validation(message),
        AccountError::NoSelection | AccountError::NotFound => {
            SettingsError::Validation("Select an account for the requested launch mode.")
        }
        AccountError::NotOffline | AccountError::NotMicrosoft => {
            SettingsError::Validation("The selected account does not support this change.")
        }
        AccountError::StaleCapture | AccountError::AlreadyExists => SettingsError::Conflict,
        AccountError::InvalidStoredData => SettingsError::Corrupt,
        AccountError::Storage => SettingsError::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        accounts::{microsoft::MinecraftProfile, model::MicrosoftIdentity},
        settings::{ConfigTheme, STATE_INSPECTOR_FLAG},
        storage::{MetadataStore, StorageError},
    };
    use serde_json::json;
    use std::time::Duration;

    struct Fixture {
        metadata: Arc<MetadataStore>,
        accounts: Arc<AccountDirectory>,
        telemetry: Arc<Telemetry>,
        tasks: TaskOwner,
        settings: SettingsService,
    }

    fn fixture(telemetry: Arc<Telemetry>) -> Fixture {
        let metadata = Arc::new(MetadataStore::in_memory().unwrap());
        let accounts = Arc::new(AccountDirectory::new(metadata.clone()).unwrap());
        let store = Arc::new(
            SettingsStore::new_with_telemetry_identity(
                metadata.clone(),
                telemetry.export_configured(),
            )
            .unwrap(),
        );
        let tasks = TaskOwner::new(8).unwrap();
        let settings =
            SettingsService::new(store, accounts.clone(), telemetry.clone(), tasks.clone())
                .unwrap();
        Fixture {
            metadata,
            accounts,
            telemetry,
            tasks,
            settings,
        }
    }

    fn unconfigured() -> Fixture {
        fixture(Arc::new(Telemetry::new(None)))
    }

    fn patch(value: serde_json::Value) -> ConfigPatch {
        serde_json::from_value(value).unwrap()
    }

    fn reject_settings_writes(metadata: &MetadataStore) {
        metadata.transaction(|transaction| -> Result<(), StorageError> {
            transaction.execute_batch("CREATE TRIGGER reject_settings BEFORE UPDATE ON settings_config BEGIN SELECT RAISE(ABORT,'private-path'); END;")?;
            Ok(())
        }).unwrap();
    }

    fn select_microsoft(accounts: &AccountDirectory, name: &str) {
        let profile_id = "12345678-1234-4234-8234-123456789abc".to_owned();
        accounts
            .commit_microsoft(
                accounts.selection_revision().unwrap(),
                MicrosoftIdentity {
                    login_id: "23456789-1234-4234-8234-123456789abc".into(),
                    profile_id: profile_id.clone(),
                    display_name: name.into(),
                    credential_revision: 1,
                    owns_minecraft_java: true,
                    profile: MinecraftProfile {
                        id: profile_id,
                        name: name.into(),
                        skins: Vec::new(),
                        capes: Vec::new(),
                    },
                },
            )
            .unwrap();
    }

    #[test]
    fn mismatched_storage_or_exporter_policy_is_rejected() {
        let settings =
            Arc::new(SettingsStore::new(Arc::new(MetadataStore::in_memory().unwrap())).unwrap());
        let accounts =
            Arc::new(AccountDirectory::new(Arc::new(MetadataStore::in_memory().unwrap())).unwrap());
        assert!(
            SettingsService::new(
                settings,
                accounts,
                Arc::new(Telemetry::new(None)),
                TaskOwner::new(1).unwrap()
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn account_selection_is_authoritative_over_saved_settings() {
        let f = unconfigured();
        f.accounts.create_offline_account("Alex").unwrap();
        let current = f.settings.config().await.unwrap();
        assert_eq!(
            (
                current.username.as_str(),
                current.launch_auth_mode,
                current.revision
            ),
            ("Alex", ConfigLaunchAuthMode::Offline, 0)
        );
        let saved = f
            .settings
            .commit(patch(json!({"expected_revision":0,"theme":"birch"})))
            .await
            .unwrap();
        assert_eq!((saved.username.as_str(), saved.revision), ("Alex", 1));
        f.accounts.create_offline_account("Third").unwrap();
        assert_eq!(f.settings.config().await.unwrap().username, "Third");
        assert_eq!(f.settings.store.current().unwrap().revision, 1);
    }

    #[tokio::test]
    async fn short_authenticated_names_survive_settings_edits() {
        for name in ["A", "A1"] {
            let f = unconfigured();
            f.accounts.create_offline_account("Alex").unwrap();
            select_microsoft(&f.accounts, name);
            let accounts = f.accounts.snapshot().unwrap();
            let current = f.settings.config().await.unwrap();
            assert_eq!(current.username, name);
            assert_eq!(current.launch_auth_mode, ConfigLaunchAuthMode::Online);
            let saved = f
                .settings
                .commit(patch(json!({"expected_revision":0,"theme":"birch"})))
                .await
                .unwrap();
            assert_eq!(saved.username, name);
            assert_eq!(saved.launch_auth_mode, ConfigLaunchAuthMode::Online);
            assert_eq!(f.accounts.snapshot().unwrap(), accounts);
            let reopened = SettingsStore::new(f.metadata.clone()).unwrap();
            assert_eq!(reopened.current().unwrap().username, name);
        }
    }

    #[tokio::test]
    async fn removing_the_last_short_named_online_account_restores_offline_defaults() {
        let f = unconfigured();
        select_microsoft(&f.accounts, "A");
        f.settings
            .commit(patch(json!({"expected_revision":0,"theme":"birch"})))
            .await
            .unwrap();
        let capture = f.accounts.capture_selected().unwrap();
        f.accounts.remove_microsoft(&capture).unwrap();
        let defaults = ConfigView::default();
        for config in [
            f.settings.config().await.unwrap(),
            f.settings
                .commit(patch(json!({"expected_revision":1,"theme":"nether"})))
                .await
                .unwrap(),
        ] {
            assert_eq!(config.username, defaults.username);
            assert_eq!(config.launch_auth_mode, ConfigLaunchAuthMode::Offline);
        }
    }

    #[tokio::test]
    async fn failed_commit_preserves_accounts_and_onboarding() {
        let f = unconfigured();
        f.accounts.create_offline_account("Alex").unwrap();
        let accounts = f.accounts.snapshot().unwrap();
        reject_settings_writes(&f.metadata);
        let error = f
            .settings
            .commit(patch(json!({"expected_revision":0,"theme":"birch"})))
            .await
            .unwrap_err();
        assert!(matches!(error, SettingsError::Storage(_)));
        assert!(!error.to_string().contains("private-path"));
        assert!(f.settings.complete_onboarding(0).await.is_err());
        assert_eq!(f.accounts.snapshot().unwrap(), accounts);
        let current = f.settings.store.current().unwrap();
        assert_eq!((current.revision, current.onboarding_done), (0, false));
    }

    #[tokio::test]
    async fn onboarding_and_flags_share_the_settings_revision() {
        let f = unconfigured();
        f.settings
            .commit(patch(
                json!({"expected_revision":0,"custom_hue":12,"music_enabled":false}),
            ))
            .await
            .unwrap();
        let override_flag = |revision, enabled| {
            serde_json::from_value::<FlagOverridePatch>(
                json!({"expected_revision":revision,"enabled":enabled}),
            )
            .unwrap()
        };
        let flags = f
            .settings
            .update_flag(STATE_INSPECTOR_FLAG.into(), override_flag(1, json!(true)))
            .await
            .unwrap();
        assert_eq!(flags.revision, 2);
        assert!(matches!(
            f.settings.complete_onboarding(1).await,
            Err(SettingsError::Conflict)
        ));
        f.settings.complete_onboarding(2).await.unwrap();
        let config = f
            .settings
            .commit(patch(json!({"expected_revision":3,"custom_hue":null})))
            .await
            .unwrap();
        assert_eq!(
            (
                config.custom_hue,
                config.music_enabled,
                config.onboarding_done
            ),
            (None, Some(false), true)
        );
        f.settings
            .update_flag(STATE_INSPECTOR_FLAG.into(), override_flag(4, json!(null)))
            .await
            .unwrap();
        assert!(matches!(
            f.settings
                .update_flag("unknown".into(), override_flag(5, json!(true)))
                .await,
            Err(SettingsError::UnknownFlag)
        ));
        assert_eq!(f.settings.store.current().unwrap().revision, 5);
    }

    #[tokio::test]
    async fn accepted_writes_survive_a_dropped_waiter_and_refuse_after_shutdown() {
        let f = unconfigured();
        let mut changes = f.settings.store.subscribe().unwrap();
        let fence = f.telemetry.consent_change_owned().await;
        let settings = f.settings.clone();
        let waiter = tokio::spawn(async move {
            settings
                .commit(patch(json!({"expected_revision":0,"theme":"nether"})))
                .await
        });
        let mut running = f.tasks.subscribe();
        tokio::time::timeout(Duration::from_secs(2), async {
            while f.tasks.status().running.is_empty() {
                running.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        waiter.abort();
        drop(fence);
        tokio::time::timeout(Duration::from_secs(2), changes.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            f.settings.store.current().unwrap().theme,
            ConfigTheme::Nether
        );
        f.tasks.shutdown(Duration::from_secs(2)).await.unwrap();
        assert!(matches!(
            f.settings
                .commit(patch(json!({"expected_revision":1,"theme":"birch"})))
                .await,
            Err(SettingsError::Unavailable)
        ));
        let update = serde_json::from_value(
            json!({"expected_revision":0,"change":{"kind":"replace","value":null}}),
        )
        .unwrap();
        assert!(matches!(
            f.settings.update_interface_preferences(update).await,
            Err(SettingsError::Unavailable)
        ));
        assert_eq!(f.settings.store.current().unwrap().revision, 1);
    }

    #[tokio::test]
    async fn consent_publishes_only_after_commit_and_failed_opt_out_is_not_exported() {
        let f = fixture(Telemetry::configured_for_test());
        let event = TelemetryEvent::AppStarted {
            state_inspector: false,
        };
        assert!(!f.telemetry.emit(event));
        f.settings
            .commit(patch(
                json!({"expected_revision":0,"telemetry_enabled":true}),
            ))
            .await
            .unwrap();
        let first = f.settings.store.telemetry_identity().unwrap().unwrap();
        assert!(f.telemetry.emit(event));
        f.settings
            .commit(patch(
                json!({"expected_revision":1,"telemetry_enabled":false}),
            ))
            .await
            .unwrap();
        assert!(!f.telemetry.emit(event));
        assert_eq!(f.settings.store.telemetry_identity().unwrap(), None);
        f.settings
            .commit(patch(
                json!({"expected_revision":2,"telemetry_enabled":true}),
            ))
            .await
            .unwrap();
        assert_ne!(
            f.settings.store.telemetry_identity().unwrap().unwrap(),
            first
        );
        reject_settings_writes(&f.metadata);
        let save_failures = |telemetry: &Telemetry| {
            telemetry
                .queued_events_for_test()
                .into_iter()
                .filter(|event| {
                    matches!(
                        event,
                        TelemetryEvent::ErrorCaptured {
                            kind: TelemetryErrorKind::ConfigSaveFailed
                        }
                    )
                })
                .count()
        };
        let before = save_failures(&f.telemetry);
        assert!(
            f.settings
                .commit(patch(
                    json!({"expected_revision":3,"telemetry_enabled":false})
                ))
                .await
                .is_err()
        );
        assert!(f.settings.store.current().unwrap().telemetry_enabled);
        assert_eq!(save_failures(&f.telemetry), before);
        assert!(
            f.settings
                .commit(patch(json!({"expected_revision":3,"theme":"nether"})))
                .await
                .is_err()
        );
        assert_eq!(save_failures(&f.telemetry), before + 1);
        assert_eq!(f.settings.store.current().unwrap().revision, 3);
    }
}

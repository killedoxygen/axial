//! Private completed preparation. Retaining this object retains every
//! capability inside the validated command until the session settles.

use crate::{
    accounts::{directory::AccountDirectory, selection::CapturedSelection},
    instances::directory::RegisteredInstance,
    instances::model::InstanceId,
    settings::SettingsStore,
};
use std::sync::Arc;

use super::{coordinator::LaunchError, model::ValidatedLaunchCommand};

pub(super) type PrepareError = LaunchError;

pub(super) struct PreparedSession {
    instance_id: InstanceId,
    version_id: String,
    command: ValidatedLaunchCommand,
    accounts: Arc<AccountDirectory>,
    account: CapturedSelection,
    settings: Arc<SettingsStore>,
    settings_revision: u64,
    secrets: Vec<String>,
    instance: RegisteredInstance,
    performance: crate::performance::PreparedPerformance,
    scenario: super::reports::LaunchProofScenario,
    resource_budget: super::reports::LaunchProofResourceBudget,
    credential_expires_at: Option<u64>,
    telemetry: Arc<super::session::LaunchAttemptTelemetry>,
}

impl PreparedSession {
    /// Called only after the coordinator has completed account/profile,
    /// Performance, runtime and installation preparation under retained
    /// instance authority. No caller-authored command can reach this point.
    pub(super) fn new(
        instance_id: InstanceId,
        version_id: String,
        command: ValidatedLaunchCommand,
        accounts: Arc<AccountDirectory>,
        account: CapturedSelection,
        settings: Arc<SettingsStore>,
        settings_revision: u64,
        secrets: Vec<String>,
        instance: RegisteredInstance,
        performance: crate::performance::PreparedPerformance,
        scenario: super::reports::LaunchProofScenario,
        resource_budget: super::reports::LaunchProofResourceBudget,
        credential_expires_at: Option<u64>,
        telemetry: Arc<super::session::LaunchAttemptTelemetry>,
    ) -> Result<Self, PrepareError> {
        let prepared = Self {
            instance_id,
            version_id,
            command,
            accounts,
            account,
            settings,
            settings_revision,
            secrets,
            instance,
            performance,
            scenario,
            resource_budget,
            credential_expires_at,
            telemetry,
        };
        // Command construction just validated its retained file evidence.
        // The process actor repeats that proof at the actual spawn boundary.
        prepared.validate_context()?;
        Ok(prepared)
    }

    pub(super) fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    pub(super) fn instance(&self) -> &RegisteredInstance {
        &self.instance
    }

    pub(super) fn intent_binding(
        &self,
        application: &crate::library::ApplicationRootPin,
    ) -> Result<super::coordinator::IntentBinding, PrepareError> {
        self.validate_context()?;
        super::coordinator::IntentBinding::capture(&self.instance, application)
    }

    pub(super) fn version_id(&self) -> &str {
        &self.version_id
    }
    pub(super) fn scenario(&self) -> &super::reports::LaunchProofScenario {
        &self.scenario
    }

    pub(super) fn resource_budget(&self) -> &super::reports::LaunchProofResourceBudget {
        &self.resource_budget
    }

    pub(super) fn validated_command(&self) -> &ValidatedLaunchCommand {
        &self.command
    }

    pub(super) fn natives(&self) -> Option<Arc<crate::install::vanilla::PreparedNatives>> {
        self.command.prepared_natives.clone()
    }

    pub(super) fn telemetry(&self) -> Arc<super::session::LaunchAttemptTelemetry> {
        self.telemetry.clone()
    }

    pub(super) fn take_secrets(&mut self) -> Vec<String> {
        std::mem::take(&mut self.secrets)
    }

    /// Repeated by the process actor immediately before spawning. Refreshes,
    /// profile changes and account switches cannot substitute a new identity
    /// into an already prepared command.
    pub(super) fn validate_before_spawn(&self) -> Result<(), PrepareError> {
        self.validate_context()?;
        self.command.revalidate().map_err(|error| {
            tracing::warn!(
                ?error,
                stage = "command_revalidate",
                "Launch plan rejected."
            );
            LaunchError::PlanRejected
        })
    }

    fn validate_context(&self) -> Result<(), PrepareError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| LaunchError::OnlineAccountUnavailable)?
            .as_secs();
        if self
            .credential_expires_at
            .is_some_and(|expires| expires <= now.saturating_add(30))
        {
            return Err(LaunchError::OnlineAccountUnavailable);
        }
        self.instance
            .validate_current()
            .map_err(|_| LaunchError::InstanceChanged)?;
        self.performance
            .validate_current()
            .map_err(|_| LaunchError::PerformanceUnsettled)?;
        self.account
            .validate(&self.accounts)
            .map_err(|_| LaunchError::AccountChanged)?;
        let current = self
            .settings
            .current()
            .map_err(|_| LaunchError::SettingsChanged)?;
        if current.revision != self.settings_revision {
            return Err(LaunchError::SettingsChanged);
        }
        Ok(())
    }
}

/// Accepted launch work retains its admission while exact native cleanup is
/// retried. Cancellation is not permission to abandon an owned directory.
pub(super) async fn try_settle_natives(
    natives: Option<Arc<crate::install::vanilla::PreparedNatives>>,
) -> bool {
    let Some(natives) = natives else {
        return true;
    };
    matches!(
        tokio::task::spawn_blocking(move || natives.settle()).await,
        Ok(Ok(()))
    )
}

pub(super) async fn settle_natives(natives: Option<Arc<crate::install::vanilla::PreparedNatives>>) {
    while !try_settle_natives(natives.clone()).await {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

impl std::fmt::Debug for PreparedSession {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output
            .debug_struct("PreparedSession")
            .field("instance_id", &self.instance_id)
            .field("command", &"[redacted]")
            .field("account", &"[captured]")
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, unix))]
pub(super) mod tests {
    use super::*;
    use crate::{
        content::catalog::ContentService,
        install::artifacts::{ActivatedFile, ActivatedVersion},
        instances::{
            create::{CreateInstanceRequest, CreateTarget, InstanceService},
            directory::{InstanceDirectories, Registry},
        },
        library::{LibraryLifecycle, LibraryOpenOutcome},
        network::{ClientConfig, ProviderClient},
        performance::PerformanceService,
        storage::MetadataStore,
        tasks::{Exclusions, TaskOwner},
    };
    use sha1::{Digest, Sha1};
    use std::os::unix::fs::PermissionsExt;

    pub(in crate::launch) fn attach_game_libraries(
        prepared: &mut PreparedSession,
        libraries: super::super::libraries::Prepared,
    ) {
        prepared.command.game_libraries = Some(libraries);
    }

    pub(in crate::launch) async fn fixture() -> (
        tempfile::TempDir,
        PreparedSession,
        crate::library::ApplicationRootPin,
    ) {
        fixture_with_legacy_arguments(false).await
    }

    pub(in crate::launch) async fn legacy_fixture() -> (
        tempfile::TempDir,
        PreparedSession,
        crate::library::ApplicationRootPin,
    ) {
        fixture_with_legacy_arguments(true).await
    }

    async fn fixture_with_legacy_arguments(
        legacy: bool,
    ) -> (
        tempfile::TempDir,
        PreparedSession,
        crate::library::ApplicationRootPin,
    ) {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let root = if legacy {
            tempfile::Builder::new()
                .prefix("legacy launch ")
                .tempdir_in(parent)
                .unwrap()
        } else {
            tempfile::tempdir_in(parent).unwrap()
        };
        let (version_id, metadata, java_version) = if legacy {
            (
                "1.4.7",
                br#"{"id":"1.4.7","type":"release","mainClass":"net.minecraft.client.Minecraft","minecraftArguments":"${auth_player_name} ${auth_session} --gameDir ${game_directory}"}"#.as_slice(),
                "1.8.0_312",
            )
        } else {
            (
                "1.20.1",
                br#"{"id":"1.20.1","type":"release","mainClass":"net.minecraft.client.main.Main"}"#
                    .as_slice(),
                "17.0.10",
            )
        };
        let library = match LibraryLifecycle::open(root.path()) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("fixture library unavailable: {other:?}"),
        };
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        storage
            .migrate(&[
                crate::instances::directory::MIGRATION,
                crate::instances::create::MIGRATION,
                crate::content::install::MIGRATION,
                crate::performance::mutation::MIGRATION,
                crate::performance::rules::MIGRATION,
            ])
            .unwrap();
        let tasks = TaskOwner::new(4).unwrap();
        let directories = InstanceDirectories::new(
            Registry::new(storage.clone()),
            library.clone(),
            Exclusions::new(),
        );
        let instances = InstanceService::new(directories.clone(), tasks.clone());
        let instance = instances
            .create(
                CreateInstanceRequest {
                    name: "Launch proof".into(),
                    selection_id: version_id.into(),
                    ..Default::default()
                },
                CreateTarget {
                    selection_id: version_id.into(),
                    version_id: version_id.into(),
                    minecraft_version: version_id.into(),
                    loader_key: "vanilla".into(),
                },
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let instance = directories.admit(&instance.id).unwrap();
        let performance = PerformanceService::new(
            storage.clone(),
            directories,
            tasks,
            Arc::new(
                ContentService::new(ProviderClient::new(ClientConfig::default()).unwrap()).unwrap(),
            ),
            crate::performance::public_transfer_resolver(),
        )
        .unwrap();
        let performance = performance
            .prepare_for_launch(
                &instance,
                performance.resolution_request(
                    version_id.into(),
                    "vanilla".into(),
                    axial_performance::PerformanceMode::Vanilla,
                ),
            )
            .await
            .unwrap();
        let mut files = Vec::new();
        for (path, bytes) in [
            (format!("versions/{version_id}/{version_id}.json"), metadata),
            (
                format!("versions/{version_id}/{version_id}.jar"),
                b"verified client".as_slice(),
            ),
        ] {
            let target = root.path().join(&path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, bytes).unwrap();
            files.push(ActivatedFile {
                path,
                sha1: hex::encode(Sha1::digest(bytes)),
                size: bytes.len() as u64,
            });
        }
        let installed = ActivatedVersion {
            version_id: version_id.into(),
            contract_id: format!("managed-install-activation-v1.{}", "A".repeat(43)),
            files,
        }
        .verify(instance.generation().clone())
        .unwrap();
        let java = root.path().join("java");
        std::fs::write(&java, format!(
            "#!/bin/sh\nprintf 'java.version = {java_version}\\nos.arch = {}\\njava.vendor = Eclipse Adoptium\\n' >&2\n",
            std::env::consts::ARCH,
        )).unwrap();
        std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
        let runtime = crate::runtime::probe::probe_java_runtime(&java, None)
            .await
            .unwrap();
        let accounts = Arc::new(AccountDirectory::new(storage.clone()).unwrap());
        accounts.create_offline_account("Player").unwrap();
        let settings = Arc::new(SettingsStore::new(storage).unwrap());
        let operation = instance.generation().managed_library().unwrap();
        let command = super::super::plan::build(super::super::model::LaunchPlanRequest {
            version_guard: axial_minecraft::VersionBundleReadGuard::acquire(&operation).unwrap(),
            library_operation: operation,
            library_dir: instance.generation().read_projection().unwrap(),
            target_version_id: version_id.into(),
            game_dir: instance.game_directory().read_projection().unwrap(),
            auth: super::super::model::LaunchAuthContext::offline("Player"),
            runtime,
            managed_launch: None,
            settings: Default::default(),
            installed,
            prepared_natives: None,
        })
        .unwrap();
        let prepared = PreparedSession {
            instance_id: instance.record().instance.id.clone(),
            version_id: version_id.into(),
            command,
            account: CapturedSelection::capture(&accounts).unwrap(),
            accounts,
            settings_revision: settings.current().unwrap().revision,
            settings,
            secrets: vec![],
            instance,
            performance,
            scenario: super::super::reports::LaunchProofScenario {
                version_id: Some(version_id.into()),
                ..Default::default()
            },
            resource_budget: super::super::resources::capture(
                &super::super::resources::capture_host(),
                (0, 0),
                0,
                4096,
                [root.path(), root.path()],
            ),
            credential_expires_at: None,
            telemetry: super::super::session::LaunchAttemptTelemetry::started(None, "vanilla"),
        };
        (root, prepared, library.admit_application_root().unwrap())
    }

    fn construct(prepared: PreparedSession) -> Result<PreparedSession, PrepareError> {
        PreparedSession::new(
            prepared.instance_id,
            prepared.version_id,
            prepared.command,
            prepared.accounts,
            prepared.account,
            prepared.settings,
            prepared.settings_revision,
            prepared.secrets,
            prepared.instance,
            prepared.performance,
            prepared.scenario,
            prepared.resource_budget,
            prepared.credential_expires_at,
            prepared.telemetry,
        )
    }

    #[tokio::test]
    async fn changed_installation_is_rejected_at_the_actual_pre_spawn_boundary() {
        let (root, prepared, _application) = fixture().await;
        let prepared = construct(prepared).unwrap();
        prepared.validate_before_spawn().unwrap();
        std::fs::write(
            root.path().join("versions/1.20.1/1.20.1.jar"),
            b"changed client payload",
        )
        .unwrap();
        assert!(matches!(
            prepared.validate_before_spawn(),
            Err(LaunchError::PlanRejected)
        ));
    }

    #[tokio::test]
    async fn construction_still_rejects_changed_account_and_settings_context() {
        for change_account in [true, false] {
            let (_root, prepared, _application) = fixture().await;
            if change_account {
                prepared
                    .accounts
                    .create_offline_account("Elsewhere")
                    .unwrap();
                assert!(matches!(
                    construct(prepared),
                    Err(LaunchError::AccountChanged)
                ));
            } else {
                prepared
                    .settings
                    .update(
                        serde_json::from_value(serde_json::json!({
                            "expected_revision": prepared.settings_revision, "max_memory_mb": 8192,
                        }))
                        .unwrap(),
                    )
                    .unwrap();
                assert!(matches!(
                    construct(prepared),
                    Err(LaunchError::SettingsChanged)
                ));
            }
        }
    }

    #[tokio::test]
    async fn absent_offline_selection_is_fenced_at_the_actual_pre_spawn_boundary() {
        for change_account in [true, false] {
            let (_root, mut prepared, _application) = fixture().await;
            let selected = prepared.accounts.capture_selected().unwrap();
            prepared.accounts.remove(selected.account_id()).unwrap();
            prepared.account = CapturedSelection::capture(&prepared.accounts).unwrap();
            let prepared = construct(prepared).unwrap();
            prepared.validate_before_spawn().unwrap();

            let expected = if change_account {
                let changed = prepared
                    .accounts
                    .create_offline_account("Elsewhere")
                    .unwrap();
                prepared
                    .accounts
                    .remove(changed.active_account_id.as_ref().unwrap().as_str())
                    .unwrap();
                assert!(
                    prepared
                        .accounts
                        .snapshot()
                        .unwrap()
                        .active_account_id
                        .is_none()
                );
                LaunchError::AccountChanged
            } else {
                prepared.settings.update(
                    serde_json::from_value(serde_json::json!({
                        "expected_revision": prepared.settings_revision, "username": "Elsewhere",
                    })).unwrap(),
                ).unwrap();
                LaunchError::SettingsChanged
            };
            assert_eq!(prepared.validate_before_spawn().unwrap_err(), expected);
        }
    }

    #[tokio::test]
    async fn acceptance_binding_uses_prepared_capabilities_and_rejects_replacement() {
        let (root, prepared, application) = fixture().await;
        let binding = prepared.intent_binding(&application).unwrap();
        let saved = serde_json::to_value(binding).unwrap();
        assert_eq!(
            saved["directory_name"],
            prepared.instance.record().directory_name
        );
        assert_eq!(
            saved["directory_receipt"],
            prepared.instance.directory().receipt().unwrap()
        );
        assert_eq!(saved["library_root"], saved["application_root"]);
        let instance = root
            .path()
            .join("instances")
            .join(&prepared.instance.record().directory_name);
        std::fs::rename(&instance, root.path().join("displaced")).unwrap();
        std::fs::create_dir(&instance).unwrap();
        assert!(matches!(
            prepared.intent_binding(&application),
            Err(LaunchError::InstanceChanged)
        ));
    }
}

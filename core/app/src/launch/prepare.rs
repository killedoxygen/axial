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
    installed_versions: crate::catalog::InstalledSnapshot,
    accounts: Arc<AccountDirectory>,
    account: CapturedSelection,
    settings: Arc<SettingsStore>,
    settings_revision: u64,
    secrets: Vec<String>,
    instance: RegisteredInstance,
    composition: crate::composition::CompositionSettled,
    scenario: super::reports::LaunchProofScenario,
    resource_budget: super::reports::LaunchProofResourceBudget,
    credential_expires_at: Option<u64>,
    telemetry: Arc<super::session::LaunchAttemptTelemetry>,
    stages: Vec<super::reports::LaunchProofStage>,
    /// This session's host activity and heap reservation, held until the
    /// session actor settles.
    _activity: axial_resource::ActivityLease,
}

impl PreparedSession {
    /// Called only after the coordinator has completed account/profile,
    /// composition, runtime and installation preparation under retained
    /// instance authority. No caller-authored command can reach this point.
    pub(super) fn new(
        instance_id: InstanceId,
        version_id: String,
        command: ValidatedLaunchCommand,
        installed_versions: crate::catalog::InstalledSnapshot,
        accounts: Arc<AccountDirectory>,
        account: CapturedSelection,
        settings: Arc<SettingsStore>,
        settings_revision: u64,
        secrets: Vec<String>,
        instance: RegisteredInstance,
        composition: crate::composition::CompositionSettled,
        scenario: super::reports::LaunchProofScenario,
        resource_budget: super::reports::LaunchProofResourceBudget,
        credential_expires_at: Option<u64>,
        telemetry: Arc<super::session::LaunchAttemptTelemetry>,
        stages: Vec<super::reports::LaunchProofStage>,
        activity: axial_resource::ActivityLease,
    ) -> Result<Self, PrepareError> {
        let prepared = Self {
            instance_id,
            version_id,
            command,
            installed_versions,
            accounts,
            account,
            settings,
            settings_revision,
            secrets,
            instance,
            composition,
            scenario,
            resource_budget,
            credential_expires_at,
            telemetry,
            stages,
            _activity: activity,
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

    /// The heap this session reserved in the host activity ledger.
    #[cfg(test)]
    pub(super) fn reserved_heap_mib(&self) -> u64 {
        self._activity.reserved_mib()
    }

    pub(super) fn stages(&self) -> &[super::reports::LaunchProofStage] {
        &self.stages
    }

    pub(super) fn telemetry(&self) -> Arc<super::session::LaunchAttemptTelemetry> {
        self.telemetry.clone()
    }

    pub(super) fn take_secrets(&mut self) -> Vec<String> {
        std::mem::take(&mut self.secrets)
    }

    /// Repeats the complete composition proof inside the spawn admission.
    pub(super) fn revalidate_composition(
        &self,
        workers: &axial_resource::PhysicalWorkParallelism,
    ) -> Result<(), PrepareError> {
        self.composition.revalidate_admitted(workers).map_err(|error| {
            tracing::warn!(
                stage = "composition_revalidate",
                cause = error.diagnostic_code(),
                "Launch plan rejected."
            );
            LaunchError::PerformanceUnsettled
        })
    }

    /// Repeated by the process actor immediately before spawning, under the
    /// command's `revalidation_request` admission. Refreshes, profile changes
    /// and account switches cannot substitute a new identity into an already
    /// prepared command.
    pub(super) fn validate_before_spawn(
        &self,
        workers: &axial_resource::PhysicalWorkParallelism,
    ) -> Result<(), PrepareError> {
        self.validate_context()?;
        self.revalidate_composition(workers)?;
        self.command.revalidate(workers).map_err(|error| {
            tracing::warn!(
                ?error,
                stage = "command_revalidate",
                "Launch plan rejected."
            );
            LaunchError::PlanRejected
        })
    }

    fn validate_context(&self) -> Result<(), PrepareError> {
        self.installed_versions
            .revalidate_for(self.instance.generation())
            .map_err(|_| LaunchError::PlanRejected)?;
        if !self.installed_versions.admits_target(&self.version_id) {
            return Err(LaunchError::InstalledVersionsDegraded);
        }
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
        self.composition
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
        install::artifacts::{ActivatedFile, ActivatedVersion},
        instances::create::{CreateInstanceRequest, CreateTarget, InstanceService},
        composition::CompositionService,
    };
    use sha1::{Digest, Sha1};
    use std::os::unix::fs::PermissionsExt;

    /// The process actor's pre-spawn proof under the command's own admission.
    pub(in crate::launch) async fn validate_before_spawn(
        prepared: &PreparedSession,
    ) -> Result<(), PrepareError> {
        axial_resource::process_physical_work()
            .admit(prepared.validated_command().revalidation_request())
            .await
            .unwrap()
            .run_inline(|workers| prepared.validate_before_spawn(workers))
    }

    /// The retained command's own proof under its admission.
    pub(in crate::launch) async fn revalidate_command(
        prepared: &PreparedSession,
    ) -> Result<(), super::super::model::LaunchPlanError> {
        let command = prepared.validated_command();
        axial_resource::process_physical_work()
            .admit(command.revalidation_request())
            .await
            .unwrap()
            .run_inline(|workers| command.revalidate(workers))
    }

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
        fixture_with_options(false, false).await
    }

    pub(in crate::launch) async fn legacy_fixture() -> (
        tempfile::TempDir,
        PreparedSession,
        crate::library::ApplicationRootPin,
    ) {
        fixture_with_options(true, false).await
    }

    pub(in crate::launch) async fn managed_fixture() -> (
        tempfile::TempDir,
        PreparedSession,
        crate::library::ApplicationRootPin,
    ) {
        fixture_with_options(false, true).await
    }

    async fn fixture_with_options(
        legacy: bool,
        managed: bool,
    ) -> (
        tempfile::TempDir,
        PreparedSession,
        crate::library::ApplicationRootPin,
    ) {
        let (root, prepared, application) =
            fixture_with_plan(legacy, managed, async |request| {
                super::super::plan::build_admitted(
                    request,
                    &axial_resource::process_physical_work(),
                    &crate::tasks::CancellationToken::new(),
                )
                .await
            })
            .await;
        (root, prepared.unwrap(), application)
    }

    /// A prepared session whose command `plan` builds from the fixture's
    /// verified request.
    async fn fixture_with_plan(
        legacy: bool,
        managed: bool,
        plan: impl AsyncFnOnce(
            super::super::model::LaunchPlanRequest,
        ) -> Result<ValidatedLaunchCommand, LaunchError>,
    ) -> (
        tempfile::TempDir,
        Result<PreparedSession, LaunchError>,
        crate::library::ApplicationRootPin,
    ) {
        let crate::testing::Fixture {
            root,
            library,
            storage,
            directories,
            tasks,
        } = crate::testing::Fixture::builder()
            .prefix(if legacy { "legacy launch " } else { ".tmp" })
            .build();
        let (version_id, metadata, java_version) = if legacy {
            (
                "1.4.7",
                br#"{"id":"1.4.7","type":"release","mainClass":"net.minecraft.client.Minecraft","minecraftArguments":"${auth_player_name} ${auth_session} --gameDir ${game_directory} --assetsDir ${game_assets}","assetIndex":{"id":"pre-1.6"}}"#.as_slice(),
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
                instances.creation_admission_for_tests().await.unwrap(),
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let instance = directories.admit(&instance.id).unwrap();
        if managed {
            crate::composition::duplicate::seed_managed(&instance).await;
        }
        let compositions = CompositionService::new(
            storage.clone(),
            directories,
            tasks,
        )
        .unwrap();
        let composition = compositions.settled_for_launch(&instance).await.unwrap();
        let mut files = Vec::new();
        let object = hex::encode(Sha1::digest(b"step"));
        let index = format!(
            r#"{{"map_to_resources":true,"objects":{{"sound/step.ogg":{{"hash":"{object}","size":4}}}}}}"#
        );
        let mut inventory = vec![
            (format!("versions/{version_id}/{version_id}.json"), metadata),
            (
                format!("versions/{version_id}/{version_id}.jar"),
                b"verified client".as_slice(),
            ),
        ];
        if legacy {
            inventory.extend([
                ("assets/indexes/pre-1.6.json".into(), index.as_bytes()),
                (
                    format!("assets/objects/{}/{object}", &object[..2]),
                    b"step".as_slice(),
                ),
            ]);
        }
        for (path, bytes) in inventory {
            let target = root.path().join(&path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, bytes).unwrap();
            files.push(ActivatedFile {
                path,
                sha1: hex::encode(Sha1::digest(bytes)),
                size: bytes.len() as u64,
            });
        }
        let activated = ActivatedVersion {
            version_id: version_id.into(),
            contract_id: format!("managed-install-activation-v1.{}", "A".repeat(43)),
            files,
        };
        let pin = instance.generation().clone();
        let installed = axial_resource::process_physical_work()
            .admit(axial_resource::PhysicalWorkRequest::foreground(
                axial_resource::PhysicalIoClass::Read,
                0,
            ))
            .await
            .unwrap()
            .run_parallel(move |_, workers| activated.verify(pin, &workers))
            .await
            .unwrap()
            .unwrap();
        let java = root.path().join("java");
        std::fs::write(&java, format!(
            "#!/bin/sh\nprintf 'java.version = {java_version}\\nos.arch = {}\\njava.vendor = Eclipse Adoptium\\n' >&2\n",
            std::env::consts::ARCH,
        )).unwrap();
        std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
        let runtime = axial_minecraft::probe_java(&java, None, std::future::pending())
            .await
            .unwrap();
        let accounts = Arc::new(AccountDirectory::new(storage.clone()).unwrap());
        accounts
            .create_offline_with_preconditions(
                "Player",
                crate::accounts::model::AccountPreconditions::default(),
            )
            .unwrap();
        let settings = Arc::new(SettingsStore::new(storage).unwrap());
        let operation = instance.generation().managed_library().unwrap();
        let library_dir = instance.generation().read_projection().unwrap();
        let game_dir = instance.game_directory().read_projection().unwrap();
        let game_assets = super::super::assets::prepare(
            &installed,
            &operation,
            &library_dir,
            &game_dir,
            Arc::new(super::super::libraries::Retention::isolated_for_test(
                instance.game_directory().clone(),
            )),
            &crate::tasks::CancellationToken::new(),
        )
        .await
        .unwrap();
        let command = plan(super::super::model::LaunchPlanRequest {
            version_guard: axial_minecraft::VersionBundleReadGuard::acquire(&operation).unwrap(),
            library_operation: operation,
            library_dir,
            target_version_id: version_id.into(),
            game_dir,
            auth: super::super::model::LaunchAuthContext::offline("Player"),
            runtime,
            managed_launch: None,
            settings: Default::default(),
            installed,
            prepared_natives: None,
            game_assets,
        })
        .await;
        let application = library.admit_application_root().unwrap();
        let command = match command {
            Ok(command) => command,
            Err(error) => return (root, Err(error), application),
        };
        let installed_versions = crate::catalog::installed_snapshot(instance.generation())
            .await
            .unwrap();
        let prepared = PreparedSession {
            instance_id: instance.record().instance.id.clone(),
            version_id: version_id.into(),
            command,
            installed_versions,
            account: CapturedSelection::capture(&accounts).unwrap(),
            accounts,
            settings_revision: settings.current().unwrap().revision,
            settings,
            secrets: vec![],
            instance,
            composition,
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
            stages: Vec::new(),
            _activity: axial_resource::process_physical_work()
                .begin_activity(|_| Ok::<_, ()>(0), ())
                .unwrap(),
        };
        (root, Ok(prepared), application)
    }

    fn construct(prepared: PreparedSession) -> Result<PreparedSession, PrepareError> {
        PreparedSession::new(
            prepared.instance_id,
            prepared.version_id,
            prepared.command,
            prepared.installed_versions,
            prepared.accounts,
            prepared.account,
            prepared.settings,
            prepared.settings_revision,
            prepared.secrets,
            prepared.instance,
            prepared.composition,
            prepared.scenario,
            prepared.resource_budget,
            prepared.credential_expires_at,
            prepared.telemetry,
            prepared.stages,
            prepared._activity,
        )
    }

    #[tokio::test]
    async fn admitted_plan_waits_for_workers_ends_on_cancellation_and_proves_its_command() {
        use axial_resource::{
            PhysicalIoClass, PhysicalWorkClass, PhysicalWorkRequest, process_physical_work,
        };
        use futures_util::FutureExt;

        if !crate::install::artifacts::tests::preparation_child(
            "launch::prepare::tests::admitted_plan_waits_for_workers_ends_on_cancellation_and_proves_its_command",
        )
        .await
        {
            return;
        }
        let work = process_physical_work();
        let idle = work.snapshot(PhysicalWorkClass::Foreground);
        let saturated = PhysicalWorkRequest::foreground_parallel(PhysicalIoClass::Read, 0, 4);
        let cancellation = crate::tasks::CancellationToken::new();
        let (_cancelled_root, cancelled, _application) = fixture_with_plan(false, false, async |request| {
            let held = work.admit(saturated).await.unwrap();
            let build = super::super::plan::build_admitted(request, &work, &cancellation);
            tokio::pin!(build);
            assert!(build.as_mut().now_or_never().is_none());
            cancellation.cancel();
            let result = build.await;
            drop(held);
            result
        })
        .await;
        let after_cancel = work.snapshot(PhysicalWorkClass::Foreground);

        let mut waited = None;
        let (root, prepared, _application) = fixture_with_plan(false, false, async |request| {
            let held = work.admit(saturated).await.unwrap();
            let uncancelled = crate::tasks::CancellationToken::new();
            let build = super::super::plan::build_admitted(request, &work, &uncancelled);
            tokio::pin!(build);
            waited = Some(build.as_mut().now_or_never().is_none());
            drop(held);
            build.await
        })
        .await;
        let prepared = prepared.unwrap();
        let built = revalidate_command(&prepared).await;
        std::fs::write(
            root.path().join("versions/1.20.1/1.20.1.jar"),
            b"changed client payload",
        )
        .unwrap();
        let changed = revalidate_command(&prepared).await;
        drop(prepared);
        let settled = work.snapshot(PhysicalWorkClass::Foreground);

        assert!(matches!(cancelled, Err(LaunchError::Cancelled)));
        assert_eq!(after_cancel, idle);
        assert_eq!(waited, Some(true));
        assert_eq!(built, Ok(()));
        assert_eq!(
            changed,
            Err(super::super::model::LaunchPlanError::ArtifactChanged)
        );
        assert_eq!(settled, idle);
    }

    #[tokio::test]
    async fn changed_installation_is_rejected_at_the_actual_pre_spawn_boundary() {
        let (root, prepared, _application) = fixture().await;
        let prepared = construct(prepared).unwrap();
        validate_before_spawn(&prepared).await.unwrap();
        std::fs::write(
            root.path().join("versions/1.20.1/1.20.1.jar"),
            b"changed client payload",
        )
        .unwrap();
        assert!(matches!(
            validate_before_spawn(&prepared).await,
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
                    .create_offline_with_preconditions(
                        "Elsewhere",
                        crate::accounts::model::AccountPreconditions::default(),
                    )
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
            validate_before_spawn(&prepared).await.unwrap();

            let expected = if change_account {
                let changed = prepared
                    .accounts
                    .create_offline_with_preconditions(
                        "Elsewhere",
                        crate::accounts::model::AccountPreconditions::default(),
                    )
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
                prepared
                    .settings
                    .update(
                        serde_json::from_value(serde_json::json!({
                            "expected_revision": prepared.settings_revision, "theme": "nether",
                        }))
                        .unwrap(),
                    )
                    .unwrap();
                LaunchError::SettingsChanged
            };
            assert_eq!(
                validate_before_spawn(&prepared).await.unwrap_err(),
                expected
            );
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

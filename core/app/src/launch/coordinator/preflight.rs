use super::*;

pub(crate) struct PreflightProjection {
    pub(super) current: Option<Arc<PreflightArtifacts>>,
    pub(super) inventories: Option<Arc<Mutex<Vec<Arc<PreflightInventory>>>>>,
    pub(super) budget: Arc<Mutex<InventoryBudget>>,
    pub(super) scan: Option<Result<Arc<crate::catalog::InstalledSnapshot>, LaunchError>>,
    pub(super) finalize_scan: bool,
    pub(super) context: Result<(CapturedSelection, u64), LaunchError>,
    pub(super) host: Arc<crate::launch::resources::HostResources>,
    pub(super) diagnostics: bool,
    pub(crate) inspection: Inspection,
    pub(super) installs: InstallQueue,
}

impl PreflightProjection {
    pub(crate) fn versions(&self) -> &[axial_minecraft::VersionEntry] {
        self.scan
            .as_ref()
            .and_then(|scan| scan.as_ref().ok())
            .map(|scan| scan.versions())
            .unwrap_or_default()
    }
}

impl Drop for PreflightProjection {
    fn drop(&mut self) {
        drop(self.current.take());
        drop(self.inventories.take());
        drop(self.scan.take());
        // Admission refusal can release proof without a task-completion wake.
        self.installs.resume_queued();
    }
}

pub(super) struct PreflightArtifacts {
    pub(super) inventory: Arc<PreflightInventory>,
    pub(super) version: axial_minecraft::VersionJson,
}

pub(super) struct PreflightInventory {
    pub(super) pin: GenerationPin,
    pub(super) _lease: ExclusionLease,
    pub(super) bundle: axial_minecraft::VersionBundleReadGuard,
    pub(super) evidence: Arc<InventoryEvidence>,
}

impl PreflightInventory {
    pub(super) fn revalidate(&self) -> Result<(), LaunchError> {
        self.bundle.revalidate().map_err(bundle_read_error)?;
        self.evidence.revalidate().map_err(install_read_error)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct LaunchPreflight {
    pub instance_id: InstanceId,
    pub launchable: bool,
    pub error: Option<LaunchErrorResponse>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<PreflightDiagnostics>,
}

impl LaunchPreflight {
    pub(crate) fn refused(instance_id: InstanceId, error: LaunchError) -> Self {
        Self {
            instance_id,
            launchable: false,
            error: Some(error.into()),
            diagnostics: None,
        }
    }
}

/// Safe facts from a completed diagnostic observation, not launch authority.
#[derive(Clone, Debug, Serialize, TS)]
pub struct PreflightDiagnostics {
    pub status: PreflightStatus,
    pub memory: PreflightMemory,
    pub overrides: PreflightOverrides,
    pub readiness: PreflightReadiness,
    pub resource_budget: PreflightResourceBudget,
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PreflightStatus {
    Ready,
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct PreflightMemory {
    pub max_memory_mb: i32,
    pub min_memory_mb: i32,
    pub min_clamped: bool,
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct PreflightOverrides {
    pub java: PreflightOverride,
    pub preset: PreflightOverride,
    pub raw_jvm_args: PreflightOverride,
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct PreflightOverride {
    pub present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub origin: Option<OverrideOrigin>,
}

impl From<Option<OverrideOrigin>> for PreflightOverride {
    fn from(origin: Option<OverrideOrigin>) -> Self {
        Self {
            present: origin.is_some(),
            origin,
        }
    }
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct PreflightReadiness {
    pub launchable: bool,
    pub reasons: Vec<PreflightReadinessReason>,
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct PreflightReadinessReason {
    pub id: PreflightReadinessReasonId,
    pub severity: PreflightReadinessSeverity,
    pub message: &'static str,
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PreflightReadinessReasonId {
    ClientJarMissing,
    ClientJarCorrupt,
    VersionJsonMissing,
    LibrariesMissing,
    LibrariesCorrupt,
    AssetIndexMissing,
    AssetIndexCorrupt,
    JavaOverrideMissing,
    ManagedRuntimeMissing,
    IncompleteInstall,
    InstalledVersionsDegraded,
}

#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PreflightReadinessSeverity {
    Blocking,
    Recoverable,
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct PreflightResourceBudget {
    pub active_session_count: usize,
    pub active_install_count: usize,
    pub active_memory_allocation_mb: u64,
    pub requested_memory_mb: Option<i32>,
    pub estimated_remaining_memory_mb: Option<i64>,
    pub memory_pressure: bool,
    pub cpu_pressure: bool,
    pub install_pressure: bool,
    pub disk_pressure: bool,
}

impl PreflightDiagnostics {
    pub(super) fn from_capture(
        settings: &EffectiveLaunchSettings,
        budget: crate::launch::reports::LaunchProofResourceBudget,
        readiness: PreflightReadiness,
    ) -> Self {
        Self {
            status: PreflightStatus::Ready,
            memory: PreflightMemory {
                max_memory_mb: settings.max_memory_mb,
                min_memory_mb: settings.min_memory_mb,
                min_clamped: settings.min_memory_clamped,
            },
            overrides: PreflightOverrides {
                java: settings.java_override_origin.into(),
                preset: settings.preset_override_origin.into(),
                raw_jvm_args: settings.raw_jvm_args_origin.into(),
            },
            readiness,
            resource_budget: PreflightResourceBudget {
                active_session_count: budget.active_session_count,
                active_install_count: budget.active_install_count,
                active_memory_allocation_mb: budget.active_memory_allocation_mb,
                requested_memory_mb: budget.requested_memory_mb,
                estimated_remaining_memory_mb: budget.estimated_remaining_memory_mb,
                memory_pressure: budget.memory_pressure,
                cpu_pressure: budget.cpu_pressure,
                install_pressure: budget.install_pressure,
                disk_pressure: budget.disk_pressure,
            },
        }
    }
}

impl LaunchCoordinator {
    /// Read-only readiness with an owned Java diagnostic probe, never game
    /// launch or native extraction. It does not reserve the instance while probing.
    pub async fn preflight(&self, id: InstanceId) -> LaunchPreflight {
        let mut projection = self.preflight_projection(None, InventoryBudget::projection());
        projection.diagnostics = true;
        projection.finalize_scan = true;
        self.preflight_with_projection(id, &mut projection).await
    }

    pub(crate) fn preflight_projection(
        &self,
        scan: Option<Arc<crate::catalog::InstalledSnapshot>>,
        budget: InventoryBudget,
    ) -> PreflightProjection {
        PreflightProjection {
            current: None,
            inventories: Some(Arc::new(Mutex::new(Vec::new()))),
            budget: Arc::new(Mutex::new(budget)),
            scan: scan.map(Ok),
            finalize_scan: false,
            context: (|| {
                Ok((
                    CapturedSelection::capture(&self.accounts)
                        .map_err(|_| LaunchError::AccountUnavailable)?,
                    self.settings
                        .current()
                        .map_err(|_| LaunchError::SettingsChanged)?
                        .revision,
                ))
            })(),
            host: Arc::new(crate::launch::resources::capture_host()),
            diagnostics: false,
            inspection: Inspection::Reverified,
            installs: self.installs.clone(),
        }
    }

    pub(crate) async fn preflight_with_projection(
        &self,
        id: InstanceId,
        projection: &mut PreflightProjection,
    ) -> LaunchPreflight {
        self.check_preflight(&id, projection)
            .await
            .unwrap_or_else(|error| LaunchPreflight::refused(id, error))
    }

    pub(crate) async fn finish_preflight_projection(
        &self,
        projection: &PreflightProjection,
    ) -> Result<(), LaunchError> {
        let Some(scan) = projection.scan.clone() else {
            return Ok(());
        };
        let scan = scan?;
        let inventories = projection
            .inventories
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .clone();
        let checks = inventories
            .iter()
            .try_fold(scan.entry_count(), |sum, inventory| {
                sum.checked_add(inventory.evidence.entry_count())
                    .ok_or(LaunchError::AtCapacity)
            })?;
        projection
            .budget
            .lock()
            .unwrap()
            .reserve_checks(checks)
            .map_err(install_read_error)?;
        let coordinator = self.clone();
        self.tasks
            .try_spawn((scan.clone(), inventories.clone()), move |_| async move {
                coordinator.revalidate_scan(scan).await?;
                axial_resource::process_physical_work()
                    .admit(axial_resource::PhysicalWorkRequest::foreground(
                        axial_resource::PhysicalIoClass::Metadata,
                        0,
                    ))
                    .await
                    .map_err(|_| LaunchError::AtCapacity)?
                    .run(move |_| {
                        for inventory in inventories {
                            inventory
                                .revalidate()
                                .map_err(|_| LaunchError::LibraryUnavailable)?;
                        }
                        Ok::<_, LaunchError>(())
                    })
                    .await
                    .map_err(|_| LaunchError::PreparationFailed)?
            })
            .map_err(|_| LaunchError::AtCapacity)?
            .join()
            .await
            .map_err(|_| LaunchError::PreparationFailed)??;
        let (selection, revision) = projection.context.as_ref().map_err(Clone::clone)?;
        selection
            .validate(&self.accounts)
            .map_err(|_| LaunchError::AccountChanged)?;
        self.settings
            .validate_revision(*revision)
            .map_err(|_| LaunchError::SettingsChanged)?;
        Ok(())
    }

    pub(crate) fn validate_projection_target(
        &self,
        projection: &PreflightProjection,
        instance: &crate::instances::model::Instance,
    ) -> Result<(), LaunchError> {
        let admitted = self
            .instances
            .admit_read(&instance.id)
            .map_err(instance_error)?;
        let _target = self.validate_preflight_target(&admitted)?;
        let pin = self
            .instances
            .library()
            .admit()
            .map_err(|_| LaunchError::LibraryUnavailable)?;
        if let Some(Ok(scan)) = &projection.scan
            && (!scan.generation_matches(admitted.game_directory().pin())
                || !scan.generation_matches(&pin))
        {
            return Err(LaunchError::LibraryUnavailable);
        }
        let current = self
            .instances
            .registry()
            .get_live(&instance.id)
            .map_err(instance_error)?;
        let mut captured = instance.clone();
        captured.java_selection = None;
        if current.instance != captured {
            return Err(LaunchError::InstanceChanged);
        }
        Ok(())
    }

    pub(super) fn validate_preflight_target(
        &self,
        admitted: &ReadInstance,
    ) -> Result<ExclusionLease, LaunchError> {
        let target = self
            .instances
            .exclusions()
            .try_acquire([admitted.record().instance.id.as_str()], [])
            .map_err(|_| LaunchError::InstanceBusy)?;
        admitted.validate_current().map_err(instance_error)?;
        let current = self
            .instances
            .registry()
            .get_live(&admitted.record().instance.id)
            .map_err(instance_error)?;
        if current.instance.settings != admitted.record().instance.settings {
            return Err(LaunchError::InstanceChanged);
        }
        let current = self
            .instances
            .library()
            .admit()
            .map_err(|_| LaunchError::LibraryUnavailable)?;
        let captured = admitted.game_directory().pin();
        if current.generation() != captured.generation()
            || current.library_id() != captured.library_id()
        {
            return Err(LaunchError::LibraryUnavailable);
        }
        Ok(target)
    }

    pub(super) async fn revalidate_scan(
        &self,
        scan: Arc<crate::catalog::InstalledSnapshot>,
    ) -> Result<(), LaunchError> {
        let pin = self
            .instances
            .library()
            .admit()
            .map_err(|_| LaunchError::LibraryUnavailable)?;
        let checked = scan.clone();
        tokio::task::spawn_blocking(move || checked.revalidate_for(&pin))
            .await
            .map_err(|_| LaunchError::PreparationFailed)?
            .map_err(|_| LaunchError::LibraryUnavailable)?;
        let current = self
            .instances
            .library()
            .admit()
            .map_err(|_| LaunchError::LibraryUnavailable)?;
        if !scan.generation_matches(&current) {
            return Err(LaunchError::LibraryUnavailable);
        }
        Ok(())
    }

    pub(super) async fn check_preflight(
        &self,
        id: &InstanceId,
        projection: &mut PreflightProjection,
    ) -> Result<LaunchPreflight, LaunchError> {
        projection
            .budget
            .lock()
            .unwrap()
            .reserve_checks(1)
            .map_err(install_read_error)?;
        let admitted = Arc::new(self.instances.admit_read(id).map_err(instance_error)?);
        let pin = admitted.game_directory().pin().clone();
        if projection.scan.as_ref().is_some_and(|scan| {
            scan.as_ref()
                .is_ok_and(|scan| !scan.generation_matches(&pin))
        }) {
            return Err(LaunchError::LibraryUnavailable);
        }
        if projection.current.as_ref().is_some_and(|proof| {
            proof.inventory.pin.generation() != pin.generation()
                || proof.inventory.pin.library_id() != pin.library_id()
                || proof.version.id != admitted.record().instance.version_id
        }) {
            projection.current = None;
        }
        // The current row owns even a newly created proof if its waiter disappears
        // or its task panics. Only a normally joined task returns it to the list.
        let proof_slot = Arc::new(Mutex::new(projection.current.take()));
        let artifacts = self
            .instances
            .exclusions()
            .try_acquire_read_artifacts(
                std::iter::empty::<String>(),
                [crate::install::queue::library_artifact(
                    &pin.library_id().to_string(),
                )],
            )
            .map_err(|_| LaunchError::InstanceBusy)?;
        let inventories = projection.inventories.as_ref().unwrap().clone();
        let damage_lease = artifacts.clone();
        let retained = (
            admitted.clone(),
            artifacts.clone(),
            proof_slot.clone(),
            inventories.clone(),
        );
        let owned_proof = proof_slot.clone();
        let scan_slot = Arc::new(Mutex::new(projection.scan.clone()));
        let owned_scan = scan_slot.clone();
        let coordinator = self.clone();
        let host = projection.host.clone();
        let diagnostics = projection.diagnostics;
        let inspection = projection.inspection;
        let finalize_scan = projection.finalize_scan;
        let budget = projection.budget.clone();
        let task = self
            .tasks
            .try_spawn(retained, move |cancel| async move {
                let mut bundle_guard = None;
                let mut planned_performance = None;
                let mut observed_damage = None;
                let mut runtime_absence = None;
                let mut refused_reasons = None;
                let mut captured_context = None;
                let mut result = async {
                    // Installation can be repaired without selecting an account.
                    // Only the guarded install proof establishes that need;
                    // catalogue display flags and transient admission failures do not.
                    let previous = owned_proof.lock().unwrap().clone();
                    let proof = if let Some(proof) = previous {
                        budget.lock().unwrap()
                            .reserve_checks(proof.inventory.evidence.entry_count())
                            .map_err(install_read_error)?;
                        let checked = proof.inventory.clone();
                        let result = tokio::task::spawn_blocking(move || checked.revalidate())
                        .await
                        .map_err(|_| LaunchError::PreparationFailed)
                        .and_then(|result| result);
                        if result.is_err() {
                            *owned_proof.lock().unwrap() = None;
                        }
                        result?;
                        proof
                    } else {
                        let operation = pin
                            .managed_library()
                            .map_err(|_| LaunchError::LibraryUnavailable)?;
                        bundle_guard = Some(match axial_minecraft::VersionBundleReadGuard::acquire(&operation) {
                            Ok(guard) => guard,
                            Err(error) => {
                                if diagnostics {
                                    let (id, message) = if error.kind() == std::io::ErrorKind::WouldBlock {
                                        (PreflightReadinessReasonId::IncompleteInstall,
                                            "Installation is changing. Wait for it to finish before launching.")
                                    } else {
                                        (PreflightReadinessReasonId::InstalledVersionsDegraded,
                                            "Installed versions could not be inspected safely.")
                                    };
                                    refused_reasons = Some(vec![PreflightReadinessReason {
                                        id,
                                        severity: PreflightReadinessSeverity::Blocking,
                                        message,
                                    }]);
                                }
                                return Ok(LaunchPreflight::refused(
                                    admitted.record().instance.id.clone(),
                                    bundle_read_error(error),
                                ));
                            }
                        });
                        if owned_scan.lock().unwrap().is_none() {
                            #[cfg(test)]
                            coordinator.installed_scans.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let scan = crate::catalog::installed_snapshot(&pin).await
                                .map(Arc::new).map_err(|_| LaunchError::LibraryUnavailable);
                            *owned_scan.lock().unwrap() = Some(scan);
                        }
                        let scan = owned_scan.lock().unwrap().clone()
                            .ok_or(LaunchError::PreparationFailed)??;
                        budget.lock().unwrap().reserve_checks(scan.entry_count())
                            .map_err(install_read_error)?;
                        coordinator.revalidate_scan(scan.clone()).await?;
                        #[cfg(test)]
                        coordinator
                            .fresh_install_checks
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let collect_damage = diagnostics || scan.is_degraded() || inspection == Inspection::Summary;
                        let (version, evidence) = match coordinator
                            .installs
                            .inspect_version(&pin, &admitted.record().instance.version_id,
                                inspection, collect_damage, Some(budget.clone()))
                            .await
                        {
                            Ok(VersionInspection::Ready(installed)) => {
                                let (mut installed, _) = installed
                                    .prepare_game_libraries(Some(budget.clone()))
                                    .await.map_err(game_libraries_error)?;
                                let evidence = installed.retain_inventory().map_err(install_read_error)?;
                                (installed.version().clone(), evidence)
                            }
                            Ok(VersionInspection::Summary(mut summary)) => {
                                let evidence = InventoryEvidence::retain(&mut summary.evidence).map_err(install_read_error)?;
                                (summary.version, evidence)
                            }
                            Ok(VersionInspection::Damaged(damage)) => {
                                observed_damage = Some(damage);
                                return Ok(LaunchPreflight::refused(
                                    admitted.record().instance.id.clone(),
                                    LaunchError::InstallUnavailable,
                                ));
                            }
                            Err(error) => return Err(install_read_error(error)),
                        };
                        let inventory = Arc::new(PreflightInventory {
                            pin,
                            _lease: artifacts,
                            bundle: bundle_guard.take().unwrap(),
                            evidence,
                        });
                        let proof = Arc::new(PreflightArtifacts {
                            inventory,
                            version,
                        });
                        *owned_proof.lock().unwrap() = Some(proof.clone());
                        proof
                    };
                    let version = &proof.version;
                    let (selection, settings, _) =
                        coordinator.capture(&admitted.record().instance, None)?;
                    captured_context = Some((selection.clone(), settings.global_config_revision));
                    let result = async {
                        if let Some(account) = selection.account()
                            && account.kind() == AccountKind::Microsoft
                        {
                            if !account.owns_minecraft_java() {
                                return Err(LaunchError::OnlineAccountUnavailable);
                            }
                            coordinator
                                .auth
                                .credentials(account)
                                .await
                                .map_err(|_| LaunchError::OnlineAccountUnavailable)?;
                        }
                        let target = &admitted.record().instance;
                        let performance_request = coordinator.performance.resolution_request(
                            target.minecraft_version.clone(),
                            target.loader_key.clone(),
                            crate::performance::plan::configured_mode(settings.performance_mode),
                        );
                        let performance = planned_performance.insert(
                            coordinator
                                .performance
                                .rules()
                                .plan(performance_request)
                                .await
                                .map_err(|_| LaunchError::PerformanceUnavailable)?,
                        );
                        let required = axial_minecraft::effective_java_version_for(
                            &admitted.record().instance.minecraft_version,
                            &version.kind,
                            &version.java_version,
                        );
                        let runtime = match coordinator
                            .runtimes
                            .observe_for_preflight(&required, &settings.java_path, &cancel)
                            .await
                        {
                            Ok(RuntimeObservation::Ready(runtime)) => Some(runtime),
                            Ok(RuntimeObservation::Missing(absence)) => {
                                runtime_absence = Some(absence);
                                None
                            }
                            Err(error @ (JavaDiscoveryError::Missing | JavaDiscoveryError::NotExecutable))
                                if diagnostics && settings.java_override_origin.is_some() =>
                            {
                                proof.inventory.bundle.revalidate().map_err(bundle_read_error)?;
                                let mut refused = LaunchPreflight::refused(
                                    target.id.clone(),
                                    LaunchError::RuntimeFailure(error),
                                );
                                refused.diagnostics = coordinator
                                    .preflight_diagnostics(
                                        &admitted,
                                        &settings,
                                        &host,
                                        PreflightReadiness {
                                            launchable: false,
                                            reasons: vec![PreflightReadinessReason {
                                                id: PreflightReadinessReasonId::JavaOverrideMissing,
                                                severity: PreflightReadinessSeverity::Blocking,
                                                message: "Selected Java override is unavailable. Choose another Java runtime.",
                                            }],
                                        },
                                    )
                                    .ok();
                                return Ok(refused);
                            }
                            Err(error) => return Err(LaunchError::RuntimeFailure(error)),
                        };
                        let contribution =
                            crate::performance::effective::effective_performance_plan(performance.plan());
                        let options = launch_options(&settings, target, &contribution, &host)?;
                        match runtime {
                            Some(runtime) => crate::launch::plan::validate_options(
                                &options,
                                &target.minecraft_version,
                                runtime.probe.info(),
                            ).map(|_| ()),
                            None => crate::launch::plan::validate_settings(&options, None),
                        }
                        .map_err(|error| {
                            tracing::warn!(
                                ?error,
                                stage = "validate_options",
                                "Launch plan rejected."
                            );
                            LaunchError::PlanRejected
                        })?;
                        let diagnostics = if diagnostics {
                            Some(coordinator.preflight_diagnostics(
                                &admitted,
                                &settings,
                                &host,
                                PreflightReadiness {
                                    launchable: true,
                                    reasons: if runtime_absence.is_some() {
                                        vec![PreflightReadinessReason {
                                            id: PreflightReadinessReasonId::ManagedRuntimeMissing,
                                            severity: PreflightReadinessSeverity::Recoverable,
                                            message: "Managed Java runtime is missing and will be prepared before launch.",
                                        }]
                                    } else {
                                        Vec::new()
                                    },
                                },
                            )?)
                        } else {
                            None
                        };
                        Ok::<_, LaunchError>(LaunchPreflight {
                            instance_id: target.id.clone(),
                            launchable: true,
                            error: None,
                            diagnostics,
                        })
                    }
                    .await;
                    selection
                        .validate(&coordinator.accounts)
                        .map_err(|_| LaunchError::AccountChanged)?;
                    coordinator
                        .settings
                        .validate_revision(settings.global_config_revision)
                        .map_err(|_| LaunchError::SettingsChanged)?;
                    result
                }
                .await;
                let observed_damage = match observed_damage {
                    Some(damage) => {
                        let reserved = budget
                            .lock()
                            .unwrap()
                            .reserve_checks(damage.entry_count())
                            .map_err(install_read_error);
                        let checked = match reserved {
                            Ok(()) => tokio::task::spawn_blocking(move || {
                                damage.revalidate()?;
                                Ok::<_, InstallError>(damage)
                            })
                            .await
                            .map_err(|_| LaunchError::PreparationFailed)
                            .and_then(|result| result.map_err(install_read_error)),
                            Err(error) => Err(error),
                        };
                        match checked {
                            Ok(damage) => Some(damage),
                            Err(error) => {
                                result = Err(error);
                                None
                            }
                        }
                    }
                    None => None,
                };
                let scan = owned_scan.lock().unwrap().clone();
                if finalize_scan && let Some(scan) = scan {
                    let scan = scan?;
                    let reserved = budget.lock().unwrap().reserve_checks(scan.entry_count())
                        .map_err(install_read_error);
                    match reserved {
                        Ok(()) => coordinator.revalidate_scan(scan).await?,
                        Err(error) => result = Err(error),
                    }
                }
                let final_proof = owned_proof.lock().unwrap().clone();
                if finalize_scan && let Some(proof) = &final_proof {
                    let reserved = budget.lock().unwrap()
                        .reserve_checks(proof.inventory.evidence.entry_count())
                        .map_err(install_read_error);
                    let checked = match reserved {
                        Ok(()) => {
                            let proof = proof.inventory.clone();
                            tokio::task::spawn_blocking(move || proof.revalidate())
                                .await
                                .map_err(|_| LaunchError::PreparationFailed)
                                .and_then(|result| result)
                        }
                        Err(error) => Err(error),
                    };
                    if let Err(error) = checked {
                        *owned_proof.lock().unwrap() = None;
                        result = Err(error);
                    }
                }
                if let Some((selection, revision)) = captured_context {
                    selection.validate(&coordinator.accounts)
                        .map_err(|_| LaunchError::AccountChanged)?;
                    coordinator.settings.validate_revision(revision)
                        .map_err(|_| LaunchError::SettingsChanged)?;
                }
                let _target = coordinator.validate_preflight_target(&admitted)?;
                if let Some(absence) = runtime_absence {
                    absence.revalidate()
                        .map_err(|_| LaunchError::RuntimeFailure(JavaDiscoveryError::Replaced))?;
                    let proof = final_proof.as_ref().ok_or(LaunchError::PreparationFailed)?;
                    proof.inventory.bundle.revalidate().map_err(bundle_read_error)?;
                }
                if let Some(damage) = observed_damage {
                    bundle_guard
                        .as_ref()
                        .unwrap()
                        .revalidate()
                        .map_err(bundle_read_error)?;
                    refused_reasons = Some(damage.reasons().iter().map(|&error| {
                        let (id, message) = match error {
                            InstallError::ClientJarMissing => (
                                PreflightReadinessReasonId::ClientJarMissing,
                                "Client game files are missing. Install this version before launching.",
                            ),
                            InstallError::ClientJarCorrupt => (
                                PreflightReadinessReasonId::ClientJarCorrupt,
                                "Client game files are corrupt. Repair this version before launching.",
                            ),
                            InstallError::VersionJsonMissing => (
                                PreflightReadinessReasonId::VersionJsonMissing,
                                "Installed version metadata is missing. Install this version before launching.",
                            ),
                            InstallError::LibrariesMissing => (
                                PreflightReadinessReasonId::LibrariesMissing,
                                "Required libraries are missing. Install this version before launching.",
                            ),
                            InstallError::LibrariesCorrupt => (
                                PreflightReadinessReasonId::LibrariesCorrupt,
                                "Required libraries are corrupt. Repair this version before launching.",
                            ),
                            InstallError::AssetIndexMissing => (
                                PreflightReadinessReasonId::AssetIndexMissing,
                                "Asset index is missing. Install this version before launching.",
                            ),
                            InstallError::AssetIndexCorrupt => (
                                PreflightReadinessReasonId::AssetIndexCorrupt,
                                "Asset index is corrupt. Repair this version before launching.",
                            ),
                            _ => return Err(install_read_error(error)),
                        };
                        Ok(PreflightReadinessReason {
                            id,
                            severity: PreflightReadinessSeverity::Blocking,
                            message,
                        })
                    }).collect::<Result<Vec<_>, _>>()?);
                    let evidence = damage.retain_inventory().map_err(install_read_error)?;
                    inventories.lock().unwrap().push(Arc::new(PreflightInventory {
                        pin: admitted.game_directory().pin().clone(),
                        _lease: damage_lease,
                        bundle: bundle_guard.take().unwrap(),
                        evidence,
                    }));
                }
                if diagnostics && let Some(reasons) = refused_reasons {
                    let target = &admitted.record().instance;
                    if let Ok(ref mut refused) = result
                        && let Ok((selection, settings, _)) = coordinator.capture(target, None)
                        && let Ok(facts) = coordinator.preflight_diagnostics(
                            &admitted,
                            &settings,
                            &host,
                            PreflightReadiness { launchable: false, reasons },
                        )
                    {
                        selection.validate(&coordinator.accounts)
                            .map_err(|_| LaunchError::AccountChanged)?;
                        coordinator.settings.validate_revision(settings.global_config_revision)
                            .map_err(|_| LaunchError::SettingsChanged)?;
                        refused.diagnostics = Some(facts);
                    }
                }
                if owned_scan.lock().unwrap().as_ref().is_some_and(|scan|
                    scan.as_ref().is_ok_and(|scan| scan.is_degraded()))
                    && let Ok(ref mut preflight) = result
                    && (preflight.launchable || preflight.diagnostics.is_some()
                        || preflight.error.as_ref().is_some_and(|error|
                            error.code == LaunchError::InstallUnavailable))
                {
                    preflight.launchable = false;
                    preflight.error = Some(LaunchError::InstalledVersionsDegraded.into());
                    if let Some(facts) = &mut preflight.diagnostics {
                        facts.readiness.launchable = false;
                        facts.readiness.reasons.push(PreflightReadinessReason {
                            id: PreflightReadinessReasonId::InstalledVersionsDegraded,
                            severity: PreflightReadinessSeverity::Blocking,
                            message: "Could not verify installed versions. Check the library folder and try again.",
                        });
                    }
                }
                drop((planned_performance, bundle_guard));
                result
            })
            .map_err(|_| LaunchError::AtCapacity)?;
        let result = task.join().await;
        projection.scan = scan_slot.lock().unwrap().clone();
        if result.is_ok() {
            projection.current = proof_slot.lock().unwrap().take();
            let mut inventories = projection.inventories.as_ref().unwrap().lock().unwrap();
            if let Some(proof) = &projection.current
                && !inventories
                    .iter()
                    .any(|inventory| Arc::ptr_eq(inventory, &proof.inventory))
            {
                inventories.push(proof.inventory.clone());
            }
        }
        result.map_err(|_| LaunchError::PreparationFailed)?
    }

    pub(super) fn preflight_diagnostics(
        &self,
        admitted: &ReadInstance,
        settings: &EffectiveLaunchSettings,
        host: &crate::launch::resources::HostResources,
        readiness: PreflightReadiness,
    ) -> Result<PreflightDiagnostics, LaunchError> {
        let game = admitted.game_directory();
        let library_dir = game
            .pin()
            .read_projection()
            .map_err(|_| LaunchError::LibraryUnavailable)?;
        let game_dir = game
            .read_projection()
            .map_err(|_| LaunchError::InstanceChanged)?;
        let budget = crate::launch::resources::capture(
            host,
            self.sessions.resource_use(),
            self.installs.active_count(),
            settings.max_memory_mb,
            [&library_dir, &game_dir],
        );
        Ok(PreflightDiagnostics::from_capture(
            settings, budget, readiness,
        ))
    }
}

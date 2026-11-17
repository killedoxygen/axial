//! Runtime selection retains verified executable and managed-component evidence.
//!
//! A managed runtime's complete tree proof and its Java probe are established
//! once per exact build: one owned, single-flight task per component verifies
//! the tree (reusing this process's full digests of unchanged file revisions)
//! and reprobes only when the verified manifest or executable fingerprint has
//! changed. A projection shares each selection across its rows and rechecks
//! it once before publication.

use super::model::{JavaDiscoveryError, JavaRequirement, JavaRuntimesResponse};
use crate::tasks::{CancellationToken, TaskOwner};
use axial_minecraft::{
    JavaProbeReceipt, JavaVersion, ManagedRuntimeCache, ManagedRuntimeComponent,
    ManagedRuntimeLaunchReceipt, probe_java,
};
use axial_resource::{PhysicalIoClass, PhysicalScratchPermit, PhysicalWorkRequest, process_physical_work};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone)]
pub struct RuntimeDiscovery {
    cache: ManagedRuntimeCache,
    tasks: TaskOwner,
    /// One slot per known managed component; bounded by that vocabulary.
    proven: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<ProvenProbe>>>>>>,
    #[cfg(any(test, feature = "test-support"))]
    test_endpoints: Option<axial_minecraft::download::InstallTestEndpoints>,
    #[cfg(test)]
    pub(crate) probes: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    probe_timeout: Option<std::time::Duration>,
}

/// A successful probe of one exact verified managed build.
#[derive(Clone)]
struct ProvenProbe {
    manifest: [u8; 32],
    probe: JavaProbeReceipt,
}

#[derive(Clone)]
pub(crate) struct SelectedRuntime {
    pub probe: JavaProbeReceipt,
    pub managed: Option<ManagedRuntimeLaunchReceipt>,
    /// The verified manifest digest and the entries its walk covers.
    manifest: Option<([u8; 32], u64)>,
}

#[derive(Clone)]
pub(crate) enum RuntimeObservation {
    Ready(SelectedRuntime),
    Missing(Arc<axial_minecraft::runtime::RuntimeAbsence>),
    /// This exact app-managed runtime is below the admitted floor; launch
    /// replaces it. Its evidence is rechecked like a ready runtime's.
    Outdated(SelectedRuntime),
}

/// Distinct runtime requirements one projection may observe and retain.
const MAX_PROJECTION_RUNTIMES: usize = 8;

/// Successful observations shared by every row of one admitted projection,
/// keyed by the exact requirement and override text. Refusals are not shared.
/// Every observation and recheck is charged to the projection's budget before
/// its walk; `revalidate` rechecks all of them once before publication.
#[derive(Clone)]
pub(crate) struct RuntimeProjection {
    observed: Arc<Mutex<HashMap<(i32, String, String), RuntimeObservation>>>,
    budget: Arc<Mutex<crate::install::artifacts::InventoryBudget>>,
}

impl Default for RuntimeProjection {
    fn default() -> Self {
        Self::new(Arc::new(Mutex::new(
            crate::install::artifacts::InventoryBudget::projection(),
        )))
    }
}

impl SelectedRuntime {
    fn checks(&self) -> u64 {
        if self.managed.is_some() {
            ManagedRuntimeComponent::VERIFICATION_CHECKS
        } else {
            1
        }
    }

    /// Rechecks the complete managed tree against the same manifest, the
    /// retained executable binding and the probed executable fingerprint.
    /// This never starts a process.
    pub(crate) async fn revalidate(&self) -> Result<(), JavaDiscoveryError> {
        let started = tracing::enabled!(tracing::Level::DEBUG).then(Instant::now);
        let selected = self.clone();
        let work = process_physical_work();
        let scratch = work
            .try_reserve_scratch(if self.managed.is_some() {
                ManagedRuntimeComponent::VERIFICATION_SCRATCH_BYTES
            } else {
                JavaProbeReceipt::REVALIDATION_SCRATCH_BYTES
            })
            .map_err(inspection_work_error)?;
        let admission = work
            .admit(PhysicalWorkRequest::foreground(PhysicalIoClass::Read, 0))
            .await
            .map_err(|_| JavaDiscoveryError::Failed)?;
        admission
            .run(move |_| {
                let _scratch = scratch;
                if let Some(receipt) = &selected.managed {
                    if receipt.component().verified_manifest() != selected.manifest {
                        return Err(JavaDiscoveryError::Replaced);
                    }
                    receipt
                        .validate_program(selected.probe.executable())
                        .map_err(|_| JavaDiscoveryError::Replaced)?;
                }
                selected
                    .probe
                    .revalidate()
                    .map_err(|_| JavaDiscoveryError::Replaced)
            })
            .await
            .map_err(|_| JavaDiscoveryError::Failed)??;
        if let Some(started) = started {
            tracing::debug!(
                stage = "runtime_revalidation", checks = self.checks(), managed = self.managed.is_some(),
                elapsed_us = %started.elapsed().as_micros(),
                "Selected runtime revalidated."
            );
        }
        Ok(())
    }
}

impl RuntimeObservation {
    fn checks(&self) -> u64 {
        match self {
            Self::Ready(selected) | Self::Outdated(selected) => selected.checks(),
            Self::Missing(_) => 1,
        }
    }

    async fn revalidate(&self) -> Result<(), JavaDiscoveryError> {
        match self {
            Self::Ready(selected) | Self::Outdated(selected) => selected.revalidate().await,
            Self::Missing(absence) => absence
                .revalidate()
                .map_err(|_| JavaDiscoveryError::Replaced),
        }
    }
}

impl RuntimeProjection {
    pub(crate) fn new(budget: Arc<Mutex<crate::install::artifacts::InventoryBudget>>) -> Self {
        Self {
            observed: Arc::default(),
            budget,
        }
    }

    fn charge(&self, checks: u64) -> Result<(), JavaDiscoveryError> {
        self.budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reserve_checks(checks)
            .map_err(|_| JavaDiscoveryError::AtCapacity)
    }

    pub(crate) async fn revalidate(&self) -> Result<(), JavaDiscoveryError> {
        let started = tracing::enabled!(tracing::Level::DEBUG).then(Instant::now);
        let observed = self
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let runtime_count = observed.len();
        for observation in observed {
            self.charge(observation.checks())?;
            observation.revalidate().await?;
        }
        if let Some(started) = started {
            tracing::debug!(
                stage = "readiness_runtimes", runtimes = runtime_count,
                elapsed_us = %started.elapsed().as_micros(),
                "Retained readiness runtimes revalidated."
            );
        }
        Ok(())
    }
}

impl RuntimeDiscovery {
    pub fn new(cache: ManagedRuntimeCache, tasks: TaskOwner) -> Self {
        Self {
            cache,
            tasks,
            proven: Arc::default(),
            #[cfg(any(test, feature = "test-support"))]
            test_endpoints: None,
            #[cfg(test)]
            probes: Arc::default(),
            #[cfg(test)]
            probe_timeout: None,
        }
    }

    /// Fixture probes are scripts gated on test files; their deadline must not
    /// depend on how loaded the machine running the suite is.
    #[cfg(test)]
    pub(crate) fn with_probe_timeout_for_test(mut self, timeout: std::time::Duration) -> Self {
        self.probe_timeout = Some(timeout);
        self
    }

    async fn run_probe(
        &self,
        path: &Path,
        id_hint: Option<&str>,
        cancellation: impl std::future::Future<Output = ()>,
    ) -> Result<JavaProbeReceipt, axial_minecraft::JavaProbeError> {
        #[cfg(test)]
        if let Some(timeout) = self.probe_timeout {
            return axial_minecraft::runtime::probe_java_within(path, id_hint, cancellation, timeout)
                .await;
        }
        probe_java(path, id_hint, cancellation).await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_test_endpoints(
        mut self,
        endpoints: axial_minecraft::download::InstallTestEndpoints,
    ) -> Self {
        self.test_endpoints = Some(endpoints);
        self
    }

    pub fn list(&self) -> JavaRuntimesResponse {
        JavaRuntimesResponse {
            runtimes: axial_minecraft::runtime::list_java_runtimes(&self.cache),
        }
    }

    /// The task owner retains an accepted probe when its HTTP waiter disappears.
    pub async fn probe(&self, path: PathBuf) -> Result<JavaProbeReceipt, JavaDiscoveryError> {
        let task = self
            .tasks
            .try_spawn((), move |cancel| async move {
                Ok(probe_java(&path, None, cancel.cancelled()).await?)
            })
            .map_err(|_| JavaDiscoveryError::Failed)?;
        task.join().await.map_err(|_| JavaDiscoveryError::Failed)?
    }

    pub(crate) async fn prepare_for_launch(
        &self,
        java: &JavaVersion,
        override_value: &str,
        cancellation: &CancellationToken,
    ) -> Result<SelectedRuntime, JavaDiscoveryError> {
        if !override_value.trim().is_empty() {
            return self.select(java, override_value, cancellation).await;
        }
        let component = axial_minecraft::runtime::preferred_runtime_component(java);
        if !axial_minecraft::runtime::is_known_runtime_component(&component) {
            return Err(JavaDiscoveryError::ManagedNotReady);
        }
        if self
            .cache
            .admit_component(&component)
            .map_err(|_| JavaDiscoveryError::Replaced)?
            .is_some()
        {
            return match self.select(java, override_value, cancellation).await {
                // An app-managed runtime below the admitted floor is replaced
                // by the rebuild owner from the current compliant source.
                Err(JavaDiscoveryError::OutdatedJava8) => {
                    self.rebuild(java, cancellation).await?;
                    self.select(java, override_value, cancellation).await
                }
                selected => selected,
            };
        }
        let source = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(JavaDiscoveryError::Cancelled),
            source = async {
                #[cfg(any(test, feature = "test-support"))]
                if let Some(endpoints) = &self.test_endpoints {
                    return axial_minecraft::runtime::acquire_preferred_runtime_source_at_test_endpoint(java, endpoints).await;
                }
                axial_minecraft::runtime::acquire_preferred_runtime_source(java).await
            } => source.map_err(provision_error)?,
        };
        let (cancel, mut control) = axial_minecraft::runtime::runtime_materialization_control();
        let mut observer = |_| {};
        let result = {
            let producer = axial_minecraft::runtime::materialize_missing_runtime_source(
                &self.cache,
                java,
                source,
                &mut observer,
                &mut control,
            );
            tokio::pin!(producer);
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    cancel.cancel_before_publication();
                    producer.await
                }
                result = &mut producer => result,
            }
        };
        let finished = control.finish();
        let receipt = match result {
            Err(axial_minecraft::ManagedRuntimeRebuildError::Preparation(error)) => {
                return Err(provision_error(error));
            }
            // Missing-only publication cannot displace a runtime. Native effects
            // remain cache-owned and are settled after all producers join.
            Err(axial_minecraft::ManagedRuntimeRebuildError::Effect(_)) => {
                return Err(JavaDiscoveryError::ManagedSettlementRequired);
            }
            Ok(Some(receipt)) if finished => receipt,
            Ok(_) => return Err(JavaDiscoveryError::Cancelled),
        };
        if !receipt
            .revalidate(&self.cache, &component.into())
            .await
            .map_err(provision_error)?
        {
            return Err(JavaDiscoveryError::Replaced);
        }
        drop(receipt);
        self.select(java, override_value, cancellation).await
    }

    /// Replaces the preferred managed component when it does not match the
    /// current source, through the existing verified rebuild owner.
    async fn rebuild(
        &self,
        java: &JavaVersion,
        cancellation: &CancellationToken,
    ) -> Result<(), JavaDiscoveryError> {
        let source = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(JavaDiscoveryError::Cancelled),
            source = async {
                #[cfg(any(test, feature = "test-support"))]
                if let Some(endpoints) = &self.test_endpoints {
                    return axial_minecraft::runtime::acquire_preferred_runtime_source_at_test_endpoint(java, endpoints).await;
                }
                axial_minecraft::runtime::acquire_preferred_runtime_source(java).await
            } => source.map_err(provision_error)?,
        };
        let (cancel, mut control) = axial_minecraft::runtime::runtime_materialization_control();
        let mut observer = |_| {};
        let result = {
            let producer = axial_minecraft::runtime::materialize_preferred_runtime_source(
                &self.cache,
                java,
                source,
                &mut observer,
                &mut control,
            );
            tokio::pin!(producer);
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    cancel.cancel_before_publication();
                    producer.await
                }
                result = &mut producer => result,
            }
        };
        match (result, control.finish()) {
            (Ok(Some(_)), true) => Ok(()),
            (Ok(_), _) | (_, false) => Err(JavaDiscoveryError::Cancelled),
            (Err(error), _) => Err(provision_error(error)),
        }
    }

    /// Observes readiness once per projection key. Later rows reuse the same
    /// observation; the projection rechecks it once before publication.
    pub(crate) async fn observe_for_preflight(
        &self,
        projection: &RuntimeProjection,
        java: &JavaVersion,
        override_value: &str,
        cancellation: &CancellationToken,
    ) -> Result<RuntimeObservation, JavaDiscoveryError> {
        if cancellation.is_cancelled() {
            return Err(JavaDiscoveryError::Cancelled);
        }
        let key = (
            java.major_version,
            java.component.clone(),
            override_value.trim().to_owned(),
        );
        {
            let observed = projection
                .observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(observation) = observed.get(&key) {
                return Ok(observation.clone());
            }
            if observed.len() >= MAX_PROJECTION_RUNTIMES {
                return Err(JavaDiscoveryError::AtCapacity);
            }
        }
        let managed = managed_component(java, override_value).is_some();
        projection.charge(if managed {
            ManagedRuntimeComponent::VERIFICATION_CHECKS
        } else {
            1
        })?;
        let scratch = process_physical_work()
            .try_reserve_scratch(if managed {
                ManagedRuntimeComponent::VERIFICATION_SCRATCH_BYTES
            } else {
                0
            })
            .map_err(inspection_work_error)?;
        let observation = self
            .observe(java, override_value, cancellation, scratch)
            .await?;
        projection
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, observation.clone());
        Ok(observation)
    }

    async fn observe(
        &self,
        java: &JavaVersion,
        override_value: &str,
        cancellation: &CancellationToken,
        scratch: Option<PhysicalScratchPermit>,
    ) -> Result<RuntimeObservation, JavaDiscoveryError> {
        if override_value.trim().is_empty() {
            let component = axial_minecraft::runtime::preferred_runtime_component(java);
            if !axial_minecraft::runtime::is_known_runtime_component(&component) {
                return Err(JavaDiscoveryError::ManagedNotReady);
            }
            if let Some(absence) = self
                .cache
                .observe_missing_component(&component)
                .map_err(|_| JavaDiscoveryError::Replaced)?
            {
                return Ok(RuntimeObservation::Missing(Arc::new(absence)));
            }
        }
        let (selected, admitted) = self
            .select_with_admission(java, override_value, cancellation, scratch)
            .await?;
        match admitted {
            Ok(()) => Ok(RuntimeObservation::Ready(selected)),
            // Launch replaces an outdated app-managed runtime before spawning.
            Err(JavaDiscoveryError::OutdatedJava8)
                if override_value.trim().is_empty() && selected.managed.is_some() =>
            {
                Ok(RuntimeObservation::Outdated(selected))
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn select(
        &self,
        java: &JavaVersion,
        override_value: &str,
        cancellation: &CancellationToken,
    ) -> Result<SelectedRuntime, JavaDiscoveryError> {
        let (selected, admitted) = self
            .select_with_admission(java, override_value, cancellation, None)
            .await?;
        admitted.map(|()| selected)
    }

    /// The verified, probed runtime and, separately, whether it meets the
    /// requirement, so a refused runtime's exact evidence can be retained.
    async fn select_with_admission(
        &self,
        java: &JavaVersion,
        override_value: &str,
        cancellation: &CancellationToken,
        scratch: Option<PhysicalScratchPermit>,
    ) -> Result<(SelectedRuntime, Result<(), JavaDiscoveryError>), JavaDiscoveryError> {
        let requirement = JavaRequirement::for_current_host(
            u32::try_from(java.major_version).map_err(|_| JavaDiscoveryError::InvalidVersion)?,
        );
        let requested = override_value.trim();
        let selected = if let Some(component) = managed_component(java, requested) {
            self.select_managed(component, cancellation, scratch)
                .await?
        } else {
            let path = Path::new(requested);
            if !path.is_absolute() {
                return Err(JavaDiscoveryError::InvalidPath);
            }
            // A custom executable's libraries are outside any verified tree, so
            // its probe is shared only within the projection that observed it.
            #[cfg(test)]
            self.probes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            SelectedRuntime {
                probe: self.run_probe(path, None, cancellation.cancelled()).await?,
                managed: None,
                manifest: None,
            }
        };
        let admitted = requirement
            .validate(selected.probe.info(), selected.probe.architecture())
            .map_err(JavaDiscoveryError::from);
        Ok((selected, admitted))
    }

    /// Verifies the component's complete tree and reuses the probe of the same
    /// verified build. The work is owned by the task owner, so a waiter that
    /// stops waiting never abandons a shared verification or probe.
    async fn select_managed(
        &self,
        component: String,
        cancellation: &CancellationToken,
        scratch: Option<PhysicalScratchPermit>,
    ) -> Result<SelectedRuntime, JavaDiscoveryError> {
        let admitted = self
            .cache
            .admit_component(&component)
            .map_err(|_| JavaDiscoveryError::Replaced)?
            .ok_or(JavaDiscoveryError::Missing)?;
        let slot = self
            .proven
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(component.clone())
            .or_default()
            .clone();
        #[cfg(test)]
        let probes = self.probes.clone();
        let discovery = self.clone();
        let task = self
            .tasks
            .try_spawn(admitted.clone(), move |cancel| async move {
                let mut proven = slot.lock().await;
                let checked = admitted.clone();
                let started = tracing::enabled!(tracing::Level::DEBUG).then(Instant::now);
                let verified = process_physical_work()
                    .admit(PhysicalWorkRequest::foreground(
                        PhysicalIoClass::Read,
                        if scratch.is_some() {
                            0
                        } else {
                            ManagedRuntimeComponent::VERIFICATION_SCRATCH_BYTES
                        },
                    ))
                    .await
                    .map_err(|_| JavaDiscoveryError::Failed)?
                    .run(move |_| {
                        let _scratch = scratch;
                        checked.verified_manifest()
                    })
                    .await
                    .map_err(|_| JavaDiscoveryError::Failed)?
                    // A marker alone is insufficient: the manifest's complete tree must match.
                    .ok_or(JavaDiscoveryError::Replaced)?;
                if let Some(started) = started {
                    tracing::debug!(
                        stage = "runtime_observation", entries = verified.1,
                        elapsed_us = %started.elapsed().as_micros(),
                        "Managed runtime tree observation completed."
                    );
                }
                let manifest = verified.0;
                let receipt = admitted
                    .launch_receipt()
                    .map_err(|_| JavaDiscoveryError::Replaced)?;
                let path = admitted.java_executable_path();
                let reusable = proven.as_ref().filter(|proven| {
                    proven.manifest == manifest
                        && proven.probe.executable() == path
                        && proven.probe.revalidate().is_ok()
                });
                let probe = match reusable {
                    Some(proven) => proven.probe.clone(),
                    None => {
                        *proven = None;
                        #[cfg(test)]
                        probes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let probe = discovery
                            .run_probe(&path, Some(&component), cancel.cancelled())
                            .await?;
                        *proven = Some(ProvenProbe {
                            manifest,
                            probe: probe.clone(),
                        });
                        probe
                    }
                };
                receipt
                    .validate_program(probe.executable())
                    .map_err(|_| JavaDiscoveryError::Replaced)?;
                Ok(SelectedRuntime {
                    probe,
                    managed: Some(receipt),
                    manifest: Some(verified),
                })
            })
            .map_err(|_| JavaDiscoveryError::Failed)?;
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(JavaDiscoveryError::Cancelled),
            result = task.join() => result.map_err(|_| JavaDiscoveryError::Failed)?,
        }
    }
}

fn managed_component(java: &JavaVersion, value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        Some(axial_minecraft::runtime::preferred_runtime_component(java))
    } else if axial_minecraft::runtime::is_known_runtime_component(value) {
        Some(value.to_owned())
    } else {
        None
    }
}

fn inspection_work_error(error: axial_resource::PhysicalWorkError) -> JavaDiscoveryError {
    match error {
        axial_resource::PhysicalWorkError::Unavailable
        | axial_resource::PhysicalWorkError::ScratchLimit => JavaDiscoveryError::AtCapacity,
        axial_resource::PhysicalWorkError::Cancelled => JavaDiscoveryError::Cancelled,
        _ => JavaDiscoveryError::Failed,
    }
}

fn provision_error(error: axial_minecraft::runtime::JavaRuntimeLookupError) -> JavaDiscoveryError {
    match error {
        axial_minecraft::runtime::JavaRuntimeLookupError::RuntimeSource(failure) => {
            JavaDiscoveryError::ManagedSource(failure.kind())
        }
        axial_minecraft::runtime::JavaRuntimeLookupError::UnsupportedPlatform { .. } => {
            JavaDiscoveryError::ManagedUnsupportedPlatform
        }
        _ => JavaDiscoveryError::ManagedNotReady,
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::{fs, time::Duration};

    fn copy_runtime(source: &Path, destination: &Path) -> std::io::Result<()> {
        let mut directories = vec![(source.to_owned(), destination.to_owned())];
        let mut entries = 0_usize;
        let mut bytes = 0_u64;
        while let Some((source, destination)) = directories.pop() {
            fs::create_dir(&destination)?;
            for entry in fs::read_dir(source)? {
                entries += 1;
                if entries > 4096 {
                    return Err(std::io::Error::other("runtime fixture entry limit"));
                }
                let entry = entry?;
                let target = destination.join(entry.file_name());
                let kind = entry.file_type()?;
                if kind.is_dir() {
                    directories.push((entry.path(), target));
                } else if kind.is_file() {
                    let size = entry.metadata()?.len();
                    bytes = bytes
                        .checked_add(size)
                        .filter(|total| *total <= 1 << 30)
                        .ok_or_else(|| std::io::Error::other("runtime fixture byte limit"))?;
                    if fs::copy(entry.path(), target)? != size {
                        return Err(std::io::Error::other("runtime fixture changed during copy"));
                    }
                } else {
                    return Err(std::io::Error::other(
                        "runtime fixture has an unsupported entry",
                    ));
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires AXIAL_TEST_WINDOWS_RUNTIME_ROOT pointing to a frozen installed gamma runtime"]
    async fn windows_managed_readiness_retains_javaw_and_authenticates_console_probe() {
        let source = fs::canonicalize(
            std::env::var_os("AXIAL_TEST_WINDOWS_RUNTIME_ROOT")
                .expect("set the frozen installed java-runtime-gamma root"),
        )
        .unwrap();
        let cache = ManagedRuntimeCache::isolated_for_test().unwrap();
        let root = cache.root().join("java-runtime-gamma");
        copy_runtime(&source, &root).unwrap();
        let tasks = TaskOwner::new(1).unwrap();
        let discovery = RuntimeDiscovery::new(cache.clone(), tasks.clone());
        let task = tasks
            .try_spawn(cache.clone(), move |cancel| async move {
                let required = JavaVersion {
                    component: "java-runtime-gamma".into(),
                    major_version: 17,
                };
                let windowed = root.join("bin/javaw.exe");
                let console = root.join("bin/java.exe");
                let projection = RuntimeProjection::default();
                let observed = match discovery
                    .observe_for_preflight(&projection, &required, "", &cancel)
                    .await?
                {
                    RuntimeObservation::Ready(selected) => selected,
                    RuntimeObservation::Missing(_) => return Err(JavaDiscoveryError::Missing),
                };
                let prepared = discovery.prepare_for_launch(&required, "", &cancel).await?;
                let retained_launch_paths = [&observed, &prepared].into_iter().all(|selected| {
                    selected.probe.executable() == windowed
                        && Path::new(&selected.probe.info().path) == windowed
                        && selected.probe.revalidate_cli_executable().as_deref()
                            == Ok(console.as_path())
                        && selected.managed.as_ref().is_some_and(|receipt| {
                            receipt
                                .validate_program(selected.probe.executable())
                                .is_ok()
                                && receipt.validate_program(&console).is_err()
                        })
                });

                let console_bytes = fs::read(&console).map_err(|_| JavaDiscoveryError::Failed)?;
                let mut changed = console_bytes.clone();
                changed[0] ^= 1;
                fs::write(&console, changed).map_err(|_| JavaDiscoveryError::Failed)?;
                let console_changed = observed
                    .probe
                    .revalidate()
                    .map_err(JavaDiscoveryError::from);
                let prepared_console_changed = prepared
                    .probe
                    .revalidate()
                    .map_err(JavaDiscoveryError::from);
                let console_refusal = discovery
                    .observe_for_preflight(&RuntimeProjection::default(), &required, "", &cancel)
                    .await
                    .err();
                fs::write(&console, console_bytes).map_err(|_| JavaDiscoveryError::Failed)?;
                drop(observed);
                drop(prepared);

                let selected = discovery.prepare_for_launch(&required, "", &cancel).await?;
                let windowed_bytes = fs::read(&windowed).map_err(|_| JavaDiscoveryError::Failed)?;
                let mut changed = windowed_bytes.clone();
                changed[0] ^= 1;
                fs::write(&windowed, changed).map_err(|_| JavaDiscoveryError::Failed)?;
                let windowed_changed = selected
                    .probe
                    .revalidate()
                    .map_err(JavaDiscoveryError::from);
                let windowed_receipt_refuses = selected
                    .managed
                    .as_ref()
                    .is_some_and(|receipt| receipt.validate_program(&windowed).is_err());
                let windowed_refusal = discovery
                    .observe_for_preflight(&RuntimeProjection::default(), &required, "", &cancel)
                    .await
                    .err();
                fs::write(&windowed, windowed_bytes).map_err(|_| JavaDiscoveryError::Failed)?;
                Ok((
                    retained_launch_paths,
                    console_changed,
                    prepared_console_changed,
                    console_refusal,
                    windowed_changed,
                    windowed_receipt_refuses,
                    windowed_refusal,
                ))
            })
            .unwrap();
        let result = task.join().await;
        let shutdown = tasks.shutdown(Duration::from_secs(5)).await;
        let settled = cache.settle();
        let (
            retained,
            console,
            prepared_console,
            console_refusal,
            windowed,
            receipt_refuses,
            windowed_refusal,
        ) = result.unwrap().unwrap();
        shutdown.unwrap();
        settled.unwrap();
        assert!(retained);
        assert_eq!(console, Err(JavaDiscoveryError::Replaced));
        assert_eq!(prepared_console, Err(JavaDiscoveryError::Replaced));
        assert_eq!(console_refusal, Some(JavaDiscoveryError::Replaced));
        assert_eq!(windowed, Err(JavaDiscoveryError::Replaced));
        assert!(receipt_refuses);
        assert_eq!(windowed_refusal, Some(JavaDiscoveryError::Replaced));
    }
}

#[cfg(all(test, unix))]
mod proof_tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    const COMPONENT: &str = "java-runtime-delta";

    fn java(root: &Path, update: u32, gate: bool) -> Vec<u8> {
        let wait = if gate {
            format!(
                "printf started > '{0}/probe-started'\nwhile [ ! -f '{0}/probe-release' ]; do sleep 0.01; done\n",
                root.display()
            )
        } else {
            String::new()
        };
        format!(
            "#!/bin/sh\n{wait}printf 'java.version = 17.0.{update}\\nos.arch = {}\\njava.vendor = Eclipse Adoptium\\n' >&2\n",
            std::env::consts::ARCH
        )
        .into_bytes()
    }

    fn required() -> JavaVersion {
        JavaVersion {
            component: COMPONENT.into(),
            major_version: 17,
        }
    }

    async fn fixture() -> (
        tempfile::TempDir,
        ManagedRuntimeCache,
        TaskOwner,
        RuntimeDiscovery,
    ) {
        let scratch =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let cache = ManagedRuntimeCache::isolated_for_test().unwrap();
        let tasks = TaskOwner::new(8).unwrap();
        let discovery = RuntimeDiscovery::new(cache.clone(), tasks.clone());
        (scratch, cache, tasks, discovery)
    }

    #[tokio::test]
    async fn missing_runtime_releases_publication_before_selection() {
        if !crate::install::artifacts::tests::preparation_child(
            "runtime::discovery::proof_tests::missing_runtime_releases_publication_before_selection",
        ).await { return; }
        use sha1::{Digest as _, Sha1};
        let (scratch, cache, tasks, discovery) = fixture().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let program = java(scratch.path(), 3, true);
        let path = if cfg!(target_os = "macos") {
            "jre.bundle/Contents/Home/bin/java"
        } else {
            "bin/java"
        };
        let manifest = serde_json::to_vec(&serde_json::json!({"files":{(path):{
            "type":"file", "executable":true,
            "downloads":{"raw":{"url":format!("{base}/java"),
                "sha1":format!("{:x}", Sha1::digest(&program)),"size":program.len()}}
        }}}))
        .unwrap();
        let descriptor = serde_json::json!({"manifest":{"url":format!("{base}/manifest"),
            "sha1":format!("{:x}",Sha1::digest(&manifest)),"size":manifest.len()}});
        let catalog = ["mac-os-arm64", "mac-os", "linux", "linux-i386"]
            .into_iter()
            .map(|platform| {
                (
                    platform,
                    serde_json::json!({(COMPONENT):[descriptor.clone()]}),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let bodies = Arc::new(HashMap::from([
            (
                "/java-runtime/all.json".to_owned(),
                serde_json::to_vec(&catalog).unwrap(),
            ),
            ("/manifest".to_owned(), manifest),
            ("/java".to_owned(), program),
        ]));
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let body = bodies.get(uri.path()).cloned();
            async move {
                match body {
                    Some(body) => (axum::http::StatusCode::OK, body),
                    None => (axum::http::StatusCode::NOT_FOUND, Vec::new()),
                }
            }
        });
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let mut server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
        });
        let discovery = discovery.with_test_endpoints(
            axial_minecraft::download::InstallTestEndpoints::from_loopback_base_url(&base).unwrap(),
        );
        let cancel = CancellationToken::new();
        let mut preparation = tokio::spawn({
            let discovery = discovery.clone();
            let cancel = cancel.clone();
            async move { discovery.prepare_for_launch(&required(), "", &cancel).await }
        });
        let reached = tokio::time::timeout(Duration::from_secs(5), async {
            while !scratch.path().join("probe-started").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        let publication_released =
            axial_minecraft::runtime::runtime_publication_lock_available_for_test(
                &cache,
                &axial_minecraft::runtime::RuntimeId::from(COMPONENT),
            );
        let released = std::fs::write(scratch.path().join("probe-release"), b"release");
        if reached.is_err() {
            cancel.cancel();
        }
        let joined = tokio::time::timeout(Duration::from_secs(10), &mut preparation).await;
        let shutdown = tasks.shutdown(Duration::from_secs(5)).await;
        let _ = stop.send(());
        let server_joined = tokio::time::timeout(Duration::from_secs(5), &mut server).await;
        if joined.is_err()
            || shutdown.is_err()
            || !tasks.status().is_idle()
            || !matches!(&server_joined, Ok(Ok(Ok(()))))
        {
            std::mem::forget((scratch, cache, tasks, discovery, preparation, server));
            panic!("missing-runtime fixture has an unsettled owner; fixture retained");
        }
        reached.expect("real installed executable reached the selection probe");
        released.expect("probe released");
        assert!(
            publication_released,
            "selection must release the commit receipt and publication lease first"
        );
        assert_eq!(joined.unwrap().unwrap().unwrap().probe.info().major, 17);
    }

    #[tokio::test]
    async fn budgeted_runtime_proofs_refuse_pressure_and_custom_checks_stay_small() {
        if !crate::install::artifacts::tests::preparation_child(
            "runtime::discovery::proof_tests::budgeted_runtime_proofs_refuse_pressure_and_custom_checks_stay_small",
        ).await { return; }
        use std::os::unix::fs::PermissionsExt;
        let (scratch, cache, tasks, discovery) = fixture().await;
        cache
            .write_ready_component_for_test(COMPONENT, &java(scratch.path(), 3, false))
            .unwrap();
        let path = scratch.path().join("custom-java");
        std::fs::write(&path, java(scratch.path(), 3, false)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cancel = CancellationToken::new();
        let projection = RuntimeProjection::default();
        let ready = discovery
            .observe_for_preflight(&projection, &required(), "", &cancel)
            .await;
        let custom = discovery
            .select(&required(), path.to_str().unwrap(), &cancel)
            .await
            .unwrap();
        let work = process_physical_work();
        let held = work
            .try_reserve_scratch(work.scratch_limit_bytes() - (1 << 20))
            .unwrap();
        let custom_check =
            tokio::time::timeout(Duration::from_millis(100), custom.revalidate()).await;
        let refused = tokio::time::timeout(
            Duration::from_millis(100),
            discovery.observe_for_preflight(
                &RuntimeProjection::default(),
                &required(),
                "",
                &cancel,
            ),
        )
        .await;
        let publication =
            tokio::time::timeout(Duration::from_millis(100), projection.revalidate()).await;
        drop(held);
        let recovered = projection.revalidate().await;
        drop(custom);
        let shutdown = tasks.shutdown(Duration::from_secs(5)).await;
        if shutdown.is_err() || !tasks.status().is_idle() {
            std::mem::forget((scratch, cache, tasks, discovery));
            panic!("runtime capacity fixture has an unsettled owner; fixture retained");
        }
        assert!(matches!(ready, Ok(RuntimeObservation::Ready(_))));
        assert_eq!(
            custom_check.expect("custom executable revalidation fits its fingerprint reservation"),
            Ok(())
        );
        let refused = refused
            .expect("budgeted observation must not wait for scratch")
            .err()
            .unwrap();
        assert_eq!(refused, JavaDiscoveryError::AtCapacity);
        assert_eq!(
            serde_json::to_value(refused).unwrap(),
            serde_json::json!("at_capacity")
        );
        assert_eq!(
            publication.expect("publication must not wait for scratch"),
            Err(JavaDiscoveryError::AtCapacity)
        );
        assert_eq!(recovered, Ok(()));
        assert_eq!(
            work.snapshot(axial_resource::PhysicalWorkClass::Foreground)
                .available_scratch_bytes,
            work.scratch_limit_bytes()
        );
    }

    #[tokio::test]
    async fn one_verified_build_is_probed_once_and_a_new_build_is_probed_again() {
        let (scratch, cache, tasks, discovery) = fixture().await;
        cache
            .write_ready_component_for_test(COMPONENT, &java(scratch.path(), 3, false))
            .unwrap();
        let cancel = CancellationToken::new();
        let first = discovery.select(&required(), "", &cancel).await.unwrap();
        let second = discovery.select(&required(), "", &cancel).await.unwrap();
        let reused = discovery.probes.load(Ordering::Relaxed);

        // Bytes changed under the same manifest are refused, never reprobed.
        let executable = first.probe.executable().to_owned();
        let original = std::fs::read(&executable).unwrap();
        std::fs::write(&executable, java(scratch.path(), 9, false)).unwrap();
        let tampered = discovery.select(&required(), "", &cancel).await.err();
        std::fs::write(&executable, &original).unwrap();

        // A republished build with its own manifest is a new proof.
        cache
            .write_ready_component_for_test(COMPONENT, &java(scratch.path(), 4, false))
            .unwrap();
        let rebuilt = discovery.select(&required(), "", &cancel).await.unwrap();
        tasks.shutdown(Duration::from_secs(5)).await.unwrap();

        assert_eq!(reused, 1);
        assert_eq!(first.probe.info().update, 3);
        assert_eq!(second.probe.executable(), first.probe.executable());
        assert_eq!(tampered, Some(JavaDiscoveryError::Replaced));
        assert_eq!(rebuilt.probe.info().update, 4);
        assert_eq!(discovery.probes.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn projection_reserves_a_managed_walk_before_verification_and_probe() {
        let (scratch, cache, tasks, discovery) = fixture().await;
        cache
            .write_ready_component_for_test(COMPONENT, &java(scratch.path(), 3, false))
            .unwrap();
        let projection = RuntimeProjection::default();
        projection
            .budget
            .lock()
            .unwrap()
            .reserve_checks(
                crate::install::artifacts::MAX_INVENTORY_ENTRIES as u64
                    - (ManagedRuntimeComponent::VERIFICATION_CHECKS - 1),
            )
            .unwrap();
        let result = discovery
            .observe_for_preflight(&projection, &required(), "", &CancellationToken::new())
            .await
            .err();
        let probes = discovery.probes.load(Ordering::Relaxed);
        let retained = projection.observed.lock().unwrap().len();
        let shutdown = tasks.shutdown(Duration::from_secs(5)).await;
        let idle = tasks.status().is_idle();
        if shutdown.is_err() || !idle {
            std::mem::forget((scratch, cache, tasks, discovery));
            panic!("runtime task owner has not settled; fixture retained: {shutdown:?}");
        }
        assert_eq!(result, Some(JavaDiscoveryError::AtCapacity));
        assert_eq!(probes, 0, "a refused budget must not start Java");
        assert_eq!(retained, 0);
    }

    #[tokio::test]
    async fn a_dropped_waiter_leaves_the_shared_probe_owned_to_completion() {
        let (scratch, cache, tasks, discovery) = fixture().await;
        cache
            .write_ready_component_for_test(COMPONENT, &java(scratch.path(), 3, true))
            .unwrap();
        let waiter = tokio::spawn({
            let discovery = discovery.clone();
            async move {
                discovery
                    .select(&required(), "", &CancellationToken::new())
                    .await
            }
        });
        let started = tokio::time::timeout(Duration::from_secs(5), async {
            while !scratch.path().join("probe-started").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        waiter.abort();
        let aborted = waiter.await.is_err_and(|error| error.is_cancelled());
        let owned = !tasks.status().is_idle();
        std::fs::write(scratch.path().join("probe-release"), b"release").unwrap();
        let later = discovery
            .select(&required(), "", &CancellationToken::new())
            .await;
        tasks.shutdown(Duration::from_secs(5)).await.unwrap();

        assert!(started.is_ok() && aborted && owned);
        assert_eq!(later.unwrap().probe.info().update, 3);
        assert_eq!(discovery.probes.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn an_app_managed_java_8_below_the_floor_is_replaced_not_admitted() {
        let (_scratch, cache, tasks, discovery) = fixture().await;
        let legacy = JavaVersion {
            component: "jre-legacy".into(),
            major_version: 8,
        };
        cache
            .write_ready_component_for_test(
                "jre-legacy",
                format!(
                    "#!/bin/sh\nprintf 'java.version = 1.8.0_51\\nos.arch = {}\\njava.vendor = Oracle Corporation\\n' >&2\n",
                    std::env::consts::ARCH
                )
                .as_bytes(),
            )
            .unwrap();
        let cancel = CancellationToken::new();
        let selected = discovery.select(&legacy, "", &cancel).await.err();
        let projection = RuntimeProjection::default();
        let observed = discovery
            .observe_for_preflight(&projection, &legacy, "", &cancel)
            .await;
        let unchanged = projection.revalidate().await;
        let executable = match &observed {
            Ok(RuntimeObservation::Outdated(selected)) => selected.probe.executable().to_owned(),
            _ => panic!("outdated runtime was not retained with its evidence"),
        };
        let original = std::fs::read(&executable).unwrap();
        std::fs::write(&executable, b"#!/bin/sh\nexit 1\n").unwrap();
        let replaced = projection.revalidate().await;
        std::fs::write(&executable, original).unwrap();

        // Every observation and recheck is charged before its walk.
        let exhausted = RuntimeProjection::default();
        exhausted
            .budget
            .lock()
            .unwrap()
            .reserve_checks(crate::install::artifacts::MAX_INVENTORY_ENTRIES as u64)
            .unwrap();
        let refused = discovery
            .observe_for_preflight(&exhausted, &legacy, "", &cancel)
            .await
            .err();
        tasks.shutdown(Duration::from_secs(5)).await.unwrap();
        assert_eq!(selected, Some(JavaDiscoveryError::OutdatedJava8));
        assert_eq!(unchanged, Ok(()));
        assert_eq!(replaced, Err(JavaDiscoveryError::Replaced));
        assert_eq!(refused, Some(JavaDiscoveryError::AtCapacity));
    }

    #[tokio::test]
    async fn a_projection_shares_one_observation_and_rechecks_it_before_publication() {
        let (scratch, cache, tasks, discovery) = fixture().await;
        let executable = cache
            .write_ready_component_for_test(COMPONENT, &java(scratch.path(), 3, false))
            .unwrap();
        let cancel = CancellationToken::new();
        let projection = RuntimeProjection::default();
        for _ in 0..20 {
            assert!(matches!(
                discovery
                    .observe_for_preflight(&projection, &required(), "", &cancel)
                    .await,
                Ok(RuntimeObservation::Ready(_))
            ));
        }
        let unchanged = projection.revalidate().await;
        let original = std::fs::read(&executable).unwrap();
        std::fs::write(&executable, java(scratch.path(), 9, false)).unwrap();
        let changed = projection.revalidate().await;
        std::fs::write(&executable, original).unwrap();
        tasks.shutdown(Duration::from_secs(5)).await.unwrap();

        assert_eq!(discovery.probes.load(Ordering::Relaxed), 1);
        assert_eq!(unchanged, Ok(()));
        assert_eq!(changed, Err(JavaDiscoveryError::Replaced));
    }
}

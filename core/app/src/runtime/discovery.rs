//! Runtime selection retains verified executable and managed-component evidence.
//!
//! A managed runtime's complete tree proof and its Java probe are established
//! once per exact build: one owned, single-flight task per component verifies
//! the tree (reusing this process's full digests of unchanged file revisions)
//! and reprobes only when the verified manifest or executable fingerprint has
//! changed. A projection shares each selection across its rows and rechecks
//! it once before publication.

use super::model::{JavaDiscoveryError, JavaRuntimesResponse, JavaSelectionRequirement};
use crate::tasks::{CancellationToken, TaskOwner};
use axial_minecraft::{
    JavaProbeReceipt, JavaVersion, ManagedRuntimeCache, ManagedRuntimeLaunchReceipt, probe_java,
};
use axial_resource::{PhysicalIoClass, PhysicalWorkRequest, process_physical_work};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The verification read buffer; the walk itself retains only directory handles.
const VERIFY_SCRATCH_BYTES: u64 = 64 << 10;

#[derive(Clone)]
pub struct RuntimeDiscovery {
    cache: ManagedRuntimeCache,
    tasks: TaskOwner,
    /// One slot per known managed component; bounded by that vocabulary.
    proven: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<ProvenProbe>>>>>>,
    #[cfg(feature = "test-support")]
    test_endpoints: Option<axial_minecraft::download::InstallTestEndpoints>,
    #[cfg(test)]
    pub(crate) probes: Arc<std::sync::atomic::AtomicUsize>,
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
    manifest: Option<[u8; 32]>,
}

#[derive(Clone)]
pub(crate) enum RuntimeObservation {
    Ready(SelectedRuntime),
    Missing(Arc<axial_minecraft::runtime::RuntimeAbsence>),
    /// The app-managed runtime is below the admitted floor; launch replaces it.
    Outdated,
}

/// Selections shared by every row of one admitted projection, keyed by the
/// exact requirement and override text. Recheck with `revalidate` once before
/// the projection is published.
#[derive(Clone, Default)]
pub(crate) struct RuntimeProjection {
    observed:
        Arc<Mutex<HashMap<(i32, String, String), Result<RuntimeObservation, JavaDiscoveryError>>>>,
}

impl SelectedRuntime {
    /// Rechecks the complete managed tree against the same manifest, the
    /// retained executable binding and the probed executable fingerprint.
    /// This never starts a process.
    pub(crate) async fn revalidate(&self) -> Result<(), JavaDiscoveryError> {
        let selected = self.clone();
        let admission = process_physical_work()
            .admit(PhysicalWorkRequest::foreground(
                PhysicalIoClass::Read,
                VERIFY_SCRATCH_BYTES,
            ))
            .await
            .map_err(|_| JavaDiscoveryError::Failed)?;
        admission
            .run(move |_| {
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
            .map_err(|_| JavaDiscoveryError::Failed)?
    }
}

impl RuntimeObservation {
    async fn revalidate(&self) -> Result<(), JavaDiscoveryError> {
        match self {
            Self::Ready(selected) => selected.revalidate().await,
            Self::Missing(absence) => absence
                .revalidate()
                .map_err(|_| JavaDiscoveryError::Replaced),
            Self::Outdated => Ok(()),
        }
    }
}

impl RuntimeProjection {
    pub(crate) async fn revalidate(&self) -> Result<(), JavaDiscoveryError> {
        let observed = self
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter_map(|observed| observed.as_ref().ok().cloned())
            .collect::<Vec<_>>();
        for observation in observed {
            observation.revalidate().await?;
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
            #[cfg(feature = "test-support")]
            test_endpoints: None,
            #[cfg(test)]
            probes: Arc::default(),
        }
    }

    #[cfg(feature = "test-support")]
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
                #[cfg(feature = "test-support")]
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
        let selected = self.select(java, override_value, cancellation).await?;
        if !receipt.revalidate(&self.cache, &component.into()).await {
            return Err(JavaDiscoveryError::Replaced);
        }
        drop(receipt);
        Ok(selected)
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
                #[cfg(feature = "test-support")]
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
        if let Some(observed) = projection
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
        {
            return observed.clone();
        }
        let observed = self.observe(java, override_value, cancellation).await;
        if !matches!(observed, Err(JavaDiscoveryError::Cancelled)) {
            projection
                .observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key, observed.clone());
        }
        observed
    }

    async fn observe(
        &self,
        java: &JavaVersion,
        override_value: &str,
        cancellation: &CancellationToken,
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
        match self.select(java, override_value, cancellation).await {
            // Launch replaces an outdated app-managed runtime before spawning.
            Err(JavaDiscoveryError::OutdatedJava8) if override_value.trim().is_empty() => {
                Ok(RuntimeObservation::Outdated)
            }
            selected => selected.map(RuntimeObservation::Ready),
        }
    }

    pub(crate) async fn select(
        &self,
        java: &JavaVersion,
        override_value: &str,
        cancellation: &CancellationToken,
    ) -> Result<SelectedRuntime, JavaDiscoveryError> {
        let requirement = JavaSelectionRequirement::for_current_host(
            u32::try_from(java.major_version).map_err(|_| JavaDiscoveryError::InvalidVersion)?,
        );
        let requested = override_value.trim();
        let component = if requested.is_empty() {
            Some(axial_minecraft::runtime::preferred_runtime_component(java))
        } else if axial_minecraft::runtime::is_known_runtime_component(requested) {
            Some(requested.to_owned())
        } else {
            None
        };
        let selected = if let Some(component) = component {
            self.select_managed(component, cancellation).await?
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
                probe: probe_java(path, None, cancellation.cancelled()).await?,
                managed: None,
                manifest: None,
            }
        };
        requirement.validate(selected.probe.info(), selected.probe.architecture())?;
        Ok(selected)
    }

    /// Verifies the component's complete tree and reuses the probe of the same
    /// verified build. The work is owned by the task owner, so a waiter that
    /// stops waiting never abandons a shared verification or probe.
    async fn select_managed(
        &self,
        component: String,
        cancellation: &CancellationToken,
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
        let task = self
            .tasks
            .try_spawn(admitted.clone(), move |cancel| async move {
                let mut proven = slot.lock().await;
                let checked = admitted.clone();
                let manifest = process_physical_work()
                    .admit(PhysicalWorkRequest::foreground(
                        PhysicalIoClass::Read,
                        VERIFY_SCRATCH_BYTES,
                    ))
                    .await
                    .map_err(|_| JavaDiscoveryError::Failed)?
                    .run(move |_| checked.verified_manifest())
                    .await
                    .map_err(|_| JavaDiscoveryError::Failed)?
                    // A marker alone is insufficient: the manifest's complete tree must match.
                    .ok_or(JavaDiscoveryError::Replaced)?;
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
                        let probe = probe_java(&path, Some(&component), cancel.cancelled()).await?;
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
                    manifest: Some(manifest),
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
        let observed = discovery
            .observe_for_preflight(&RuntimeProjection::default(), &legacy, "", &cancel)
            .await;
        tasks.shutdown(Duration::from_secs(5)).await.unwrap();
        assert_eq!(selected, Some(JavaDiscoveryError::OutdatedJava8));
        assert!(matches!(observed, Ok(RuntimeObservation::Outdated)));
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

//! Runtime selection retains verified executable and managed-component evidence.

use super::{
    model::{JavaDiscoveryError, JavaRuntimesResponse, JavaSelectionRequirement},
    probe::{JavaProbeReceipt, probe_java_runtime_until},
};
use crate::tasks::{CancellationToken, TaskOwner};
use axial_minecraft::{JavaVersion, ManagedRuntimeCache, ManagedRuntimeLaunchReceipt};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct RuntimeDiscovery {
    cache: ManagedRuntimeCache,
    tasks: TaskOwner,
    #[cfg(feature = "test-support")]
    test_endpoints: Option<axial_minecraft::download::InstallTestEndpoints>,
}

pub(crate) struct SelectedRuntime {
    pub probe: JavaProbeReceipt,
    pub managed: Option<ManagedRuntimeLaunchReceipt>,
}

pub(crate) enum RuntimeObservation {
    Ready(SelectedRuntime),
    Missing(axial_minecraft::runtime::RuntimeAbsence),
}

impl RuntimeDiscovery {
    pub fn new(cache: ManagedRuntimeCache, tasks: TaskOwner) -> Self {
        Self {
            cache,
            tasks,
            #[cfg(feature = "test-support")]
            test_endpoints: None,
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
                probe_java_runtime_until(&path, None, cancel.cancelled()).await
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
            return self.select(java, override_value, cancellation).await;
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

    pub(crate) async fn observe_for_preflight(
        &self,
        java: &JavaVersion,
        override_value: &str,
        cancellation: &CancellationToken,
    ) -> Result<RuntimeObservation, JavaDiscoveryError> {
        if cancellation.is_cancelled() {
            return Err(JavaDiscoveryError::Cancelled);
        }
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
                return Ok(RuntimeObservation::Missing(absence));
            }
        }
        self.select(java, override_value, cancellation)
            .await
            .map(RuntimeObservation::Ready)
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
        let (path, managed) = if let Some(component) = component.as_deref() {
            let admitted = self
                .cache
                .admit_component(component)
                .map_err(|_| JavaDiscoveryError::Replaced)?
                .ok_or(JavaDiscoveryError::Missing)?;
            // A marker alone is insufficient: verify the manifest's complete tree.
            let verification = admitted.clone();
            if !tokio::task::spawn_blocking(move || verification.contents_verified())
                .await
                .map_err(|_| JavaDiscoveryError::Failed)?
            {
                return Err(JavaDiscoveryError::Replaced);
            }
            let receipt = admitted
                .launch_receipt()
                .map_err(|_| JavaDiscoveryError::Replaced)?;
            (admitted.java_executable_path(), Some(receipt))
        } else {
            let path = Path::new(requested);
            if !path.is_absolute() {
                return Err(JavaDiscoveryError::InvalidPath);
            }
            (path.to_owned(), None)
        };
        let probe =
            probe_java_runtime_until(&path, component.as_deref(), cancellation.cancelled()).await?;
        requirement.validate(probe.info(), probe.architecture())?;
        if let Some(receipt) = &managed {
            receipt
                .validate_program(probe.executable())
                .map_err(|_| JavaDiscoveryError::Replaced)?;
        }
        Ok(SelectedRuntime { probe, managed })
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
                let observed = match discovery
                    .observe_for_preflight(&required, "", &cancel)
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
                let console_changed = observed.probe.revalidate();
                let prepared_console_changed = prepared.probe.revalidate();
                let console_refusal = discovery
                    .observe_for_preflight(&required, "", &cancel)
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
                let windowed_changed = selected.probe.revalidate();
                let windowed_receipt_refuses = selected
                    .managed
                    .as_ref()
                    .is_some_and(|receipt| receipt.validate_program(&windowed).is_err());
                let windowed_refusal = discovery
                    .observe_for_preflight(&required, "", &cancel)
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

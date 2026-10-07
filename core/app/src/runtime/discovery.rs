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

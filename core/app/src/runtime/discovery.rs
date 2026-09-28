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
}

pub(crate) struct SelectedRuntime {
    pub probe: JavaProbeReceipt,
    pub managed: Option<ManagedRuntimeLaunchReceipt>,
}

impl RuntimeDiscovery {
    pub fn new(cache: ManagedRuntimeCache, tasks: TaskOwner) -> Self {
        Self { cache, tasks }
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

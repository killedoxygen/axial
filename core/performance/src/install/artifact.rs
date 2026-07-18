use super::model::InstallError;
use super::plan::{ManagedArtifactPin, ManagedCompositionInstallPlan};
use crate::types::{
    CompositionState, InstalledMod, ManagedArtifactIntegrity, ManagedArtifactProvider,
    ManagedArtifactSource, ManagedDependencyStateEdge, OwnershipClass,
};
use axial_minecraft::download::{
    VerifiedContentIntegrity, VerifiedStagedContent, download_owned_verified_content_to_staging,
};
use chrono::Utc;
use futures_util::{StreamExt, stream};
use std::path::Path;

const MANAGED_GRAPH_STAGE_CONCURRENCY: usize = 4;

pub(super) struct StagedManagedArtifact {
    installed: InstalledMod,
    staged: VerifiedStagedContent,
}

pub(super) trait ManagedArtifactStage {
    fn installed(&self) -> &InstalledMod;
    fn publish_create_new(self, destination: &Path) -> Result<(), InstallError>;
}

impl ManagedArtifactStage for StagedManagedArtifact {
    fn installed(&self) -> &InstalledMod {
        &self.installed
    }

    fn publish_create_new(self, destination: &Path) -> Result<(), InstallError> {
        self.staged
            .publish_create_new(destination)
            .map(|_| ())
            .map_err(|error| InstallError::Io(std::io::Error::other(error)))
    }
}

pub(super) async fn stage_managed_graph(
    client: &reqwest::Client,
    pins: Vec<ManagedArtifactPin>,
    staging_root: &Path,
) -> Result<Vec<StagedManagedArtifact>, InstallError> {
    let concurrency = pins.len().clamp(1, MANAGED_GRAPH_STAGE_CONCURRENCY);
    let staged_capacity = pins.len();
    let mut downloads = stream::iter(pins.into_iter().map(|pin| async move {
        let installed = installed_from_pin(&pin);
        let expected = VerifiedContentIntegrity {
            size: Some(pin.size()),
            sha1: None,
            sha512: Some(pin.sha512().to_string()),
        };
        let staging_label = staging_root.join(pin.filename());
        let staged = download_owned_verified_content_to_staging(
            client,
            pin.download_url(),
            &staging_label,
            &expected,
        )
        .await
        .map_err(InstallError::Download)?;
        if staged.size() != pin.size()
            || staged.sha512() != pin.sha512()
            || staged.file_name() != pin.filename()
        {
            return Err(InstallError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "owned managed staging does not match its sealed artifact pin",
            )));
        }
        Ok(StagedManagedArtifact { installed, staged })
    }))
    .buffer_unordered(concurrency);

    let mut staged = Vec::with_capacity(staged_capacity);
    while let Some(result) = downloads.next().await {
        match result {
            Ok(artifact) => staged.push(artifact),
            Err(error) => return Err(error),
        }
    }
    staged.sort_by(|left, right| left.installed.project_id.cmp(&right.installed.project_id));
    Ok(staged)
}

pub(super) fn installed_graph_from_plan(plan: &ManagedCompositionInstallPlan) -> Vec<InstalledMod> {
    plan.pins().iter().map(installed_from_pin).collect()
}

fn installed_from_pin(pin: &ManagedArtifactPin) -> InstalledMod {
    InstalledMod {
        project_id: pin.project_id().to_string(),
        version_id: pin.version_id().to_string(),
        filename: pin.filename().to_string(),
        role: pin.role(),
        size: pin.size(),
        ownership_class: OwnershipClass::CompositionManaged,
        source: ManagedArtifactSource {
            provider: ManagedArtifactProvider::Modrinth,
        },
        integrity: ManagedArtifactIntegrity {
            sha512: pin.sha512().to_string(),
        },
    }
}

pub(super) fn state_from_plan(
    plan: &ManagedCompositionInstallPlan,
    installed_mods: Vec<InstalledMod>,
) -> CompositionState {
    CompositionState {
        composition_id: plan.composition_id().to_string(),
        family: plan.family(),
        tier: plan.tier(),
        game_version: plan.game_version().to_string(),
        loader: plan.loader().to_string(),
        graph_sha512: plan.graph_digest().to_string(),
        dependency_edges: plan
            .edges()
            .iter()
            .map(|edge| ManagedDependencyStateEdge {
                parent_project_id: edge.parent_project_id().to_string(),
                child_project_id: edge.child_project_id().to_string(),
                child_version_id: edge.child_version_id().to_string(),
            })
            .collect(),
        installed_mods,
        installed_at: Utc::now().to_rfc3339(),
    }
}

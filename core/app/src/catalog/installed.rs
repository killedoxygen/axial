use super::{CatalogError, CatalogSnapshot};
use crate::library::GenerationPin;
use axial_minecraft::managed_path::ManagedLibraryOperation;
use axial_minecraft::{
    VersionEntry, VersionScanReport, VersionScanSnapshot, VersionScanState,
    compare_version_entries, enrich_version_entries, scan_versions_snapshot,
};
use serde::{Deserialize, Serialize};

pub(crate) struct InstalledSnapshot {
    pin: GenerationPin,
    snapshot: VersionScanSnapshot,
}

impl InstalledSnapshot {
    pub(crate) fn versions(&self) -> &[VersionEntry] {
        &self.snapshot.report.versions
    }

    pub(crate) fn into_versions(self) -> Vec<VersionEntry> {
        self.snapshot.report.versions
    }

    pub(crate) fn entry_count(&self) -> u64 {
        self.snapshot.dependencies().entry_count()
    }

    pub(crate) fn is_degraded(&self) -> bool {
        self.snapshot.report.state == VersionScanState::Degraded
    }

    pub(crate) fn generation_matches(&self, pin: &GenerationPin) -> bool {
        self.pin.generation() == pin.generation() && self.pin.library_id() == pin.library_id()
    }

    pub(crate) fn revalidate_for(&self, pin: &GenerationPin) -> Result<(), CatalogError> {
        if !self.generation_matches(pin) {
            return Err(CatalogError::InstalledUnavailable);
        }
        self.pin
            .revalidate()
            .map_err(|_| CatalogError::InstalledUnavailable)?;
        pin.revalidate()
            .map_err(|_| CatalogError::InstalledUnavailable)?;
        if !self.snapshot.dependencies().is_revalidated() {
            return Err(CatalogError::InstalledUnavailable);
        }
        self.pin
            .revalidate()
            .map_err(|_| CatalogError::InstalledUnavailable)?;
        pin.revalidate()
            .map_err(|_| CatalogError::InstalledUnavailable)
    }
}

pub(crate) async fn installed_snapshot(
    pin: &GenerationPin,
) -> Result<InstalledSnapshot, CatalogError> {
    let pin = pin.clone();
    let operation = pin
        .managed_library()
        .map_err(|_| CatalogError::InstalledUnavailable)?;
    let snapshot = scan_installed(&operation).await?;
    pin.revalidate()
        .map_err(|_| CatalogError::InstalledUnavailable)?;
    Ok(InstalledSnapshot { pin, snapshot })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VersionScanViewModel {
    pub state_id: String,
    pub label: String,
    pub degraded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionsResponse {
    pub versions: Vec<VersionEntry>,
    pub scan_state: VersionScanViewModel,
}

/// Scan only through an admitted, retained library operation. A root path is not authority.
/// The scanner owns publication admission and observes inherited JSON/JAR dependencies.
pub async fn installed_versions(
    operation: &ManagedLibraryOperation,
    catalog: Option<&CatalogSnapshot>,
) -> Result<VersionsResponse, CatalogError> {
    let snapshot = scan_installed(operation).await?;
    Ok(project_installed(snapshot.report, catalog))
}

async fn scan_installed(
    operation: &ManagedLibraryOperation,
) -> Result<VersionScanSnapshot, CatalogError> {
    let operation = operation.clone();
    tokio::task::spawn_blocking(move || {
        let snapshot =
            scan_versions_snapshot(&operation).map_err(|_| CatalogError::InstalledUnavailable)?;
        if !snapshot.dependencies().is_revalidated() {
            return Err(CatalogError::InstalledUnavailable);
        }
        Ok(snapshot)
    })
    .await
    .map_err(|_| CatalogError::InstalledUnavailable)?
}

pub(crate) fn project_installed(
    mut report: VersionScanReport,
    catalog: Option<&CatalogSnapshot>,
) -> VersionsResponse {
    if let Some(catalog) = catalog {
        let mut releases: Vec<_> = catalog
            .versions
            .iter()
            .filter(|entry| entry.raw_kind == "release")
            .map(|entry| axial_minecraft::ReleaseReference {
                id: entry.id.clone(),
                release_time: entry.release_time.clone(),
            })
            .collect();
        releases.sort_by(|left, right| left.release_time.cmp(&right.release_time));
        enrich_version_entries(&mut report.versions, &releases);
        report.versions.sort_by(compare_version_entries);
    }
    let (state_id, label, degraded) = match report.state {
        VersionScanState::Ready => ("ready", "Installed versions ready", false),
        VersionScanState::Empty => ("empty", "No installed versions", false),
        VersionScanState::Degraded => ("degraded", "Installed versions unavailable", true),
    };
    VersionsResponse {
        versions: report.versions,
        scan_state: VersionScanViewModel {
            state_id: state_id.into(),
            label: label.into(),
            degraded,
            detail: degraded.then(|| CatalogError::InstalledUnavailable.to_string()),
        },
    }
}

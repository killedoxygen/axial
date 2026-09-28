use super::{CatalogError, CatalogSnapshot};
use axial_minecraft::managed_path::ManagedLibraryOperation;
use axial_minecraft::{
    VersionEntry, VersionScanReport, VersionScanState, compare_version_entries,
    enrich_version_entries, scan_versions_snapshot,
};
use serde::{Deserialize, Serialize};

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
    let operation = operation.clone();
    let report = tokio::task::spawn_blocking(move || {
        let snapshot =
            scan_versions_snapshot(&operation).map_err(|_| CatalogError::InstalledUnavailable)?;
        if !snapshot.dependencies().is_revalidated() {
            return Err(CatalogError::InstalledUnavailable);
        }
        Ok(snapshot.report)
    })
    .await
    .map_err(|_| CatalogError::InstalledUnavailable)??;
    Ok(project_installed(report, catalog))
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

use crate::state::{AppState, InstalledVersionsSnapshot, ProducerLease};
use axial_minecraft::{
    VersionEntry, VersionScanReport, VersionScanState, enrich_version_entries,
    fetch_version_manifest_cached, managed_path::ManagedLibraryOperation,
    manifest_release_references,
};
use axum::{Json, http::StatusCode};
use serde::Serialize;

pub(crate) const VERSION_SCAN_DEGRADED_MESSAGE: &str =
    "Could not verify installed versions. Check the library folder and try again.";

#[derive(Debug, Serialize)]
pub struct VersionsResponse {
    pub versions: Vec<VersionEntry>,
    pub scan_state: VersionScanViewModel,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct VersionScanViewModel {
    pub state_id: String,
    pub label: String,
    pub degraded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct InstalledVersionsScan {
    pub versions: Vec<VersionEntry>,
    pub view_model: VersionScanViewModel,
}

impl InstalledVersionsScan {
    pub(crate) fn is_degraded(&self) -> bool {
        self.view_model.degraded
    }
}

pub(crate) async fn installed_versions(
    state: &AppState,
    producer: &ProducerLease,
) -> Result<VersionsResponse, (StatusCode, Json<serde_json::Value>)> {
    let snapshot = state
        .installed_versions_snapshot(producer)
        .await
        .ok_or_else(version_library_not_configured_response)?;
    let mut scan = installed_versions_scan(&snapshot.snapshot);
    enrich_versions_from_cached_manifest(snapshot.managed_library_operation(), &mut scan.versions)
        .await;

    Ok(VersionsResponse {
        versions: scan.versions,
        scan_state: scan.view_model,
    })
}

fn version_library_not_configured_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::PRECONDITION_FAILED,
        Json(serde_json::json!({ "error": "Axial library is not configured" })),
    )
}

pub(crate) fn installed_versions_scan(
    snapshot: &InstalledVersionsSnapshot,
) -> InstalledVersionsScan {
    let report = snapshot.report();
    InstalledVersionsScan {
        view_model: version_scan_view_model(report),
        versions: report.versions.clone(),
    }
}

pub(crate) fn version_scan_degraded_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::PRECONDITION_FAILED,
        Json(serde_json::json!({ "error": VERSION_SCAN_DEGRADED_MESSAGE })),
    )
}

fn version_scan_view_model(report: &VersionScanReport) -> VersionScanViewModel {
    match report.state {
        VersionScanState::Ready => VersionScanViewModel {
            state_id: "ready".to_string(),
            label: "Installed versions ready".to_string(),
            degraded: false,
            detail: None,
        },
        VersionScanState::Empty => VersionScanViewModel {
            state_id: "empty".to_string(),
            label: "No installed versions".to_string(),
            degraded: false,
            detail: None,
        },
        VersionScanState::Degraded => VersionScanViewModel {
            state_id: "degraded".to_string(),
            label: "Installed versions unavailable".to_string(),
            degraded: true,
            detail: Some(VERSION_SCAN_DEGRADED_MESSAGE.to_string()),
        },
    }
}

async fn enrich_versions_from_cached_manifest(
    operation: &ManagedLibraryOperation,
    versions: &mut [VersionEntry],
) {
    if let Ok(manifest) = fetch_version_manifest_cached(operation).await {
        let releases = manifest_release_references(&manifest);
        enrich_version_entries(versions, &releases);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_version_scan_view_model_marks_malformed_library_as_degraded() {
        let report = VersionScanReport {
            state: VersionScanState::Degraded,
            versions: Vec::new(),
            issues: Vec::new(),
        };
        let view_model = version_scan_view_model(&report);

        assert!(view_model.degraded);
        assert_eq!(view_model.state_id, "degraded");
        assert_eq!(
            view_model.detail.as_deref(),
            Some(VERSION_SCAN_DEGRADED_MESSAGE)
        );
    }
}

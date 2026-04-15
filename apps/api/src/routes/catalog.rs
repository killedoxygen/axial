use crate::state::AppState;
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use croopor_minecraft::{
    VersionMeta, analyze_version_metadata, fetch_version_manifest, manifest_release_references,
    scan_versions,
};
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
struct CatalogEntry {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    release_time: String,
    meta: VersionMeta,
    url: String,
    installed: bool,
}

#[derive(Debug, Serialize)]
struct CatalogResponse {
    latest: croopor_minecraft::manifest::LatestVersions,
    versions: Vec<CatalogEntry>,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/catalog", get(handle_catalog))
}

async fn handle_catalog(
    State(state): State<AppState>,
) -> Result<Json<CatalogResponse>, (StatusCode, Json<serde_json::Value>)> {
    let Some(mc_dir) = state.library_dir() else {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        ));
    };

    let manifest = fetch_version_manifest().await.map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to fetch catalog: {error}") })),
        )
    })?;

    let installed: HashSet<String> = scan_versions(&PathBuf::from(mc_dir))
        .unwrap_or_default()
        .into_iter()
        .filter(|version| version.launchable)
        .map(|version| version.id)
        .collect();

    let releases = manifest_release_references(&manifest);
    let versions = manifest
        .versions
        .into_iter()
        .map(|version| {
            let metadata = analyze_version_metadata(
                &version.id,
                &version.kind,
                &version.release_time,
                None,
                &releases,
            );
            CatalogEntry {
                installed: installed.contains(&version.id),
                id: version.id,
                kind: metadata.canonical_kind.clone(),
                release_time: version.release_time,
                meta: metadata,
                url: version.url,
            }
        })
        .collect();

    Ok(Json(CatalogResponse {
        latest: manifest.latest,
        versions,
    }))
}

use crate::state::AppState;
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use croopor_minecraft::{fetch_version_manifest, scan_versions};
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
struct CatalogEntry {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    release_time: String,
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

    let versions = manifest
        .versions
        .into_iter()
        .map(|version| CatalogEntry {
            installed: installed.contains(&version.id),
            id: version.id,
            kind: version.kind,
            release_time: version.release_time,
            url: version.url,
        })
        .collect();

    Ok(Json(CatalogResponse {
        latest: manifest.latest,
        versions,
    }))
}

use crate::state::AppState;
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use croopor_minecraft::{
    LifecycleMeta, MinecraftVersionMeta, VersionSubjectKind, analyze_minecraft_version,
    fetch_version_manifest_cached, manifest_release_references, scan_versions,
};
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
struct CatalogEntry {
    subject_kind: VersionSubjectKind,
    id: String,
    raw_kind: String,
    release_time: String,
    minecraft_meta: MinecraftVersionMeta,
    lifecycle: LifecycleMeta,
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

    let mc_dir = PathBuf::from(mc_dir);
    let manifest = fetch_version_manifest_cached(&mc_dir)
        .await
        .map_err(catalog_fetch_error_response)?;

    let installed: HashSet<String> = scan_versions(&mc_dir)
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
            let analysis = analyze_minecraft_version(
                &version.id,
                &version.kind,
                &version.release_time,
                None,
                &releases,
            );
            CatalogEntry {
                subject_kind: VersionSubjectKind::MinecraftVersion,
                installed: installed.contains(&version.id),
                id: version.id,
                raw_kind: version.kind,
                release_time: version.release_time,
                minecraft_meta: analysis.minecraft_meta,
                lifecycle: analysis.lifecycle,
                url: version.url,
            }
        })
        .collect();

    Ok(Json(CatalogResponse {
        latest: manifest.latest,
        versions,
    }))
}

fn catalog_fetch_error_response(
    _error: impl std::fmt::Display,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({
            "error": "Could not load the Minecraft catalog. Check your connection and try again."
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_fetch_error_is_bad_gateway_with_bounded_copy() {
        let (status, Json(body)) = catalog_fetch_error_response(
            "request failed for https://piston-meta.mojang.com/mc/game/version_manifest_v2.json",
        );

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            body["error"],
            "Could not load the Minecraft catalog. Check your connection and try again."
        );
    }

    #[test]
    fn catalog_fetch_error_does_not_expose_upstream_details() {
        let fragments = [
            "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json",
            "error sending request for url",
            "expected value at line 1 column 1",
        ];

        for fragment in fragments {
            let (_status, Json(body)) = catalog_fetch_error_response(format!(
                "failed to fetch version manifest: {fragment}"
            ));
            let rendered = body.to_string();

            assert!(
                !rendered.contains(fragment),
                "public response exposed upstream detail: {fragment}"
            );
        }
    }
}

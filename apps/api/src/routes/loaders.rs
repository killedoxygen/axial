use super::error::{ApiError, ApiResult, Extracted};
use axial_app::catalog::{CatalogService, LoaderBuildsResponse, LoaderVersionsResponse};
use axial_minecraft::LoaderComponentId;
use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::StatusCode,
    routing::get,
};
use serde::Deserialize;

const MAX_MINECRAFT_VERSION_BYTES: usize =
    axial_minecraft::portable_path::MAX_PORTABLE_FILE_NAME_BYTES;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildQuery {
    minecraft_version: String,
}

pub fn router(catalog: CatalogService) -> Router {
    Router::new()
        .route("/api/v1/loaders/{component}/versions", get(versions))
        .route("/api/v1/loaders/{component}/builds", get(builds))
        .with_state(catalog)
}

async fn versions(
    State(catalog): State<CatalogService>,
    Path(component): Path<String>,
) -> ApiResult<LoaderVersionsResponse> {
    let component = LoaderComponentId::parse(&component).ok_or_else(invalid)?;
    catalog
        .loader_versions(component)
        .await
        .map(Json)
        .map_err(|_| unavailable())
}

async fn builds(
    State(catalog): State<CatalogService>,
    Path(component): Path<String>,
    query: Result<Query<BuildQuery>, QueryRejection>,
) -> ApiResult<LoaderBuildsResponse> {
    // Structural and size failures share one fixed envelope before admission.
    let query = query.or_refuse(invalid)?;
    if query.minecraft_version.len() > MAX_MINECRAFT_VERSION_BYTES
        || query.minecraft_version.trim().is_empty()
    {
        return Err(invalid());
    }
    let component = LoaderComponentId::parse(&component).ok_or_else(invalid)?;
    catalog
        .loader_builds(component, query.minecraft_version)
        .await
        .map(Json)
        .map_err(|_| unavailable())
}

fn invalid() -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "The loader selection is invalid.")
}

fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "Loader versions are unavailable.",
    )
}

#[cfg(test)]
mod tests {
    use super::MAX_MINECRAFT_VERSION_BYTES;
    use crate::fixture::Profile;
    use reqwest::StatusCode;
    use serde_json::json;

    #[tokio::test]
    async fn build_query_failures_are_bounded_json_before_admission_and_after_authentication() {
        let profile = Profile::start().await;
        // A query that slipped through would reach closed task admission: 503.
        profile.services.tasks.close_admission();
        let endpoint = "/api/v1/loaders/fabric/builds";
        for query in [
            String::new(),
            "?minecraft_version=".into(),
            "?minecraft_version=%20%20".into(),
            "?minecraft_version=1.21&minecraft_version=private-canary".into(),
            "?minecraft_version=1.21&private_canary=%2FUsers%2Fprivate".into(),
            "?minecraft_version[private_canary]=1.21".into(),
            format!(
                "?minecraft_version={}",
                "x".repeat(MAX_MINECRAFT_VERSION_BYTES + 1)
            ),
        ] {
            let response = profile
                .api
                .builder(reqwest::Method::GET, &format!("{endpoint}{query}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(response.headers()["content-type"], "application/json");
            assert_eq!(
                &response.bytes().await.unwrap()[..],
                br#"{"error":"The loader selection is invalid."}"#
            );
        }
        let bootstrap = profile.services.server.bootstrap();
        let unauthenticated = reqwest::Client::new()
            .get(format!(
                "{}{endpoint}?private_canary=missing",
                bootstrap.base_url
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauthenticated.json::<serde_json::Value>().await.unwrap(),
            json!({"error":"API capability is required"})
        );
        assert!(profile.services.tasks.status().is_idle());
        profile.close().await;
    }
}

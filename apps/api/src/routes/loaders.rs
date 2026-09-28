use axial_app::{
    catalog::Catalog, library::LibraryLifecycle, public::ErrorResponse, tasks::TaskOwner,
};
use axial_minecraft::{LoaderComponentId, loaders};
use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::StatusCode,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

type ApiError = (StatusCode, Json<ErrorResponse>);
const MAX_MINECRAFT_VERSION_BYTES: usize =
    axial_minecraft::portable_path::MAX_PORTABLE_FILE_NAME_BYTES;

#[derive(Clone)]
struct LoaderCatalog {
    library: LibraryLifecycle,
    catalog: Arc<Catalog>,
    tasks: TaskOwner,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildQuery {
    minecraft_version: String,
}
pub fn router(library: LibraryLifecycle, catalog: Arc<Catalog>, tasks: TaskOwner) -> Router {
    Router::new()
        .route("/api/v1/loaders/{component}/versions", get(versions))
        .route("/api/v1/loaders/{component}/builds", get(builds))
        .with_state(LoaderCatalog {
            library,
            catalog,
            tasks,
        })
}
async fn versions(
    State(state): State<LoaderCatalog>,
    Path(component): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let component = LoaderComponentId::parse(&component).ok_or_else(invalid)?;
    let pin = state.library.admit().map_err(|_| unavailable())?;
    let operation = pin.managed_library().map_err(|_| unavailable())?;
    let task = state
        .tasks
        .try_spawn((pin, operation.clone()), move |cancel| async move {
            let (mut versions, catalog_state) =
                loaders::fetch_supported_versions(&operation, component).await?;
            state
                .catalog
                .enrich_loader_versions(&operation, &mut versions, &cancel)
                .await;
            Ok::<_, axial_minecraft::loaders::LoaderError>(
                json!({"versions":versions,"catalog_state":catalog_state}),
            )
        })
        .map_err(|_| unavailable())?;
    task.join()
        .await
        .map_err(|_| unavailable())?
        .map(Json)
        .map_err(|_| unavailable())
}
async fn builds(
    State(state): State<LoaderCatalog>,
    Path(component): Path<String>,
    query: Result<Query<BuildQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    // Extractor diagnostics can contain untrusted query values. All structural
    // and size failures use the same fixed public envelope before admission.
    let Query(query) = query.map_err(|_| invalid())?;
    if query.minecraft_version.len() > MAX_MINECRAFT_VERSION_BYTES
        || query.minecraft_version.trim().is_empty()
    {
        return Err(invalid());
    }
    let component = LoaderComponentId::parse(&component).ok_or_else(invalid)?;
    let pin = state.library.admit().map_err(|_| unavailable())?;
    let operation = pin.managed_library().map_err(|_| unavailable())?;
    let task = state
        .tasks
        .try_spawn((pin, operation.clone()), move |_| async move {
            let (builds, catalog_state) =
                loaders::fetch_builds(&operation, component, &query.minecraft_version).await?;
            Ok::<_, axial_minecraft::loaders::LoaderError>(
                json!({"builds":builds,"catalog_state":catalog_state}),
            )
        })
        .map_err(|_| unavailable())?;
    task.join()
        .await
        .map_err(|_| unavailable())?
        .map(Json)
        .map_err(|_| unavailable())
}
fn invalid() -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new("The loader selection is invalid.")),
    )
}
fn unavailable() -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse::new("Loader versions are unavailable.")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_app::{
        library::LibraryOpenOutcome,
        network::{ClientConfig, ProviderClient},
    };
    use axum::http::header;
    use std::time::Duration;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn http_build_query_failures_are_bounded_json_and_authentication_runs_first() {
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let library = match LibraryLifecycle::open(root.path()) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("isolated loader fixture failed: {other:?}"),
        };
        let before = std::fs::read_dir(root.path()).unwrap().count();
        let tasks = TaskOwner::new(1).unwrap();
        // Rejections must precede domain/task admission. A slipped-through
        // query would receive 503 here instead of fetching a live provider.
        tasks.shutdown(Duration::from_secs(1)).await.unwrap();
        let catalog = Arc::new(Catalog::new(
            ProviderClient::new(ClientConfig::default()).unwrap(),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let authority = crate::transport::LocalApiAuthority::new(address, None).unwrap();
        let capability = authority.bootstrap().capability;
        let app =
            crate::transport::protected_router(router(library, catalog, tasks.clone()), authority);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let endpoint = format!("http://{address}/api/v1/loaders/fabric/builds");
        let invalid_queries = [
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
        ];
        for query in invalid_queries {
            let response = client
                .get(format!("{endpoint}{query}"))
                .header(crate::transport::CAPABILITY_HEADER, &capability)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
            let body = response.bytes().await.unwrap();
            assert!(body.len() < 128);
            assert_eq!(
                &body[..],
                br#"{"error":"The loader selection is invalid."}"#
            );
        }
        let unauthenticated = client
            .get(format!(
                "{endpoint}?private_canary=missing-required-version"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauthenticated.json::<Value>().await.unwrap(),
            json!({"error":"API capability is required"})
        );
        assert!(tasks.status().is_idle());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), before);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

use axial_app::{
    catalog::{Catalog, CatalogSnapshot, VersionsResponse, installed_versions},
    library::LibraryLifecycle,
    tasks::TaskOwner,
};
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
struct Versions {
    library: LibraryLifecycle,
    catalog: Arc<Catalog>,
    tasks: TaskOwner,
}
pub fn router(library: LibraryLifecycle, catalog: Arc<Catalog>, tasks: TaskOwner) -> Router {
    Router::new()
        .route("/api/v1/versions", get(installed))
        .route("/api/v1/versions/catalog", get(catalog_snapshot))
        .with_state(Versions {
            library,
            catalog,
            tasks,
        })
}
async fn installed(
    State(state): State<Versions>,
) -> Result<Json<VersionsResponse>, (StatusCode, Json<Value>)> {
    let pin = state.library.admit().map_err(|_| unavailable())?;
    let operation = pin.managed_library().map_err(|_| unavailable())?;
    let task = state
        .tasks
        .try_spawn((pin, operation.clone()), move |_| async move {
            let catalog = state.catalog.cached_snapshot(&operation).await;
            installed_versions(&operation, Some(&catalog)).await
        })
        .map_err(|_| unavailable())?;
    task.join()
        .await
        .map_err(|_| unavailable())?
        .map(Json)
        .map_err(|_| unavailable())
}
async fn catalog_snapshot(
    State(state): State<Versions>,
) -> Result<Json<CatalogSnapshot>, (StatusCode, Json<Value>)> {
    let pin = state.library.admit().map_err(|_| unavailable())?;
    let operation = pin.managed_library().map_err(|_| unavailable())?;
    let task = state
        .tasks
        .try_spawn((pin, operation.clone()), move |cancel| async move {
            state.catalog.snapshot(&operation, &cancel).await
        })
        .map_err(|_| unavailable())?;
    task.join().await.map(Json).map_err(|_| unavailable())
}
fn unavailable() -> (StatusCode, Json<Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error":"Minecraft versions are unavailable."})),
    )
}

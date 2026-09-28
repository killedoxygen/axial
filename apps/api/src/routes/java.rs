use axial_app::runtime::{discovery::RuntimeDiscovery, model::JavaRuntimesResponse};
use axum::{Json, Router, extract::State, routing::get};

pub fn router(runtime: RuntimeDiscovery) -> Router {
    Router::new()
        .route("/api/v1/java", get(list))
        .with_state(runtime)
}

async fn list(State(runtime): State<RuntimeDiscovery>) -> Json<JavaRuntimesResponse> {
    Json(runtime.list())
}

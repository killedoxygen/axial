use axial_app::system::{SystemResourceResponse, system_resource_status};
use axum::{Json, Router, http::StatusCode, routing::get};
use serde_json::{Value, json};

pub fn router() -> Router {
    Router::new().route("/api/v1/system", get(system))
}

async fn system() -> Result<Json<SystemResourceResponse>, (StatusCode, Json<Value>)> {
    tokio::task::spawn_blocking(system_resource_status)
        .await
        .map(Json)
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "System memory information is unavailable."})),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn system_route_queries_actual_memory_in_the_retained_wire_shape() {
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/system")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        let result: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(result.as_object().unwrap().len(), 4);
        let total = result["total_memory_mb"].as_u64().unwrap();
        assert!(total >= 1);
        assert_eq!(result["max_allocatable_gb"], total / 1024);
        assert!(
            result["recommended_min_mb"].as_u64().unwrap()
                <= result["recommended_max_mb"].as_u64().unwrap()
        );
    }
}

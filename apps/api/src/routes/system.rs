use crate::state::AppState;
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use sysinfo::System;

#[derive(Debug, Serialize)]
struct SystemResponse {
    total_memory_mb: u64,
    recommended_min_mb: u64,
    recommended_max_mb: u64,
    max_allocatable_gb: u64,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/system", get(handle_system))
}

async fn handle_system(State(_state): State<AppState>) -> Json<SystemResponse> {
    let mut system = System::new();
    system.refresh_memory();
    let total_memory_mb = (system.total_memory() / (1024 * 1024)).max(1);
    let (recommended_min_mb, recommended_max_mb) = recommended_memory_range(total_memory_mb);

    Json(SystemResponse {
        total_memory_mb,
        recommended_min_mb,
        recommended_max_mb,
        max_allocatable_gb: total_memory_mb / 1024,
    })
}

fn recommended_memory_range(total_mb: u64) -> (u64, u64) {
    let allocatable_mb = total_mb.saturating_sub(2048);
    if allocatable_mb == 0 {
        return (0, 0);
    }

    let mut min_mb = (total_mb / 4).clamp(2048, 4096).min(allocatable_mb);
    let max_mb = (total_mb / 2).clamp(4096, 8192).min(allocatable_mb);
    if min_mb > max_mb {
        min_mb = max_mb;
    }
    if min_mb < 1024 {
        min_mb = max_mb.min(1024);
    }

    (min_mb, max_mb)
}

#[cfg(test)]
mod tests {
    use super::recommended_memory_range;

    #[test]
    fn keeps_low_memory_recommendations_within_available_budget() {
        assert_eq!(recommended_memory_range(4096), (2048, 2048));
        assert_eq!(recommended_memory_range(3072), (1024, 1024));
        assert_eq!(recommended_memory_range(2048), (0, 0));
    }
}

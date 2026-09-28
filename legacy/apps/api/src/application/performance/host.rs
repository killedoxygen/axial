//! Performance-owned host resource recommendation view model.

use serde::Serialize;
use sysinfo::System;

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct SystemResourceResponse {
    pub total_memory_mb: u64,
    pub recommended_min_mb: u64,
    pub recommended_max_mb: u64,
    pub max_allocatable_gb: u64,
}

pub fn system_resource_status() -> SystemResourceResponse {
    let mut system = System::new();
    system.refresh_memory();
    let total_memory_mb = (system.total_memory() / (1024 * 1024)).max(1);

    system_resource_status_from_total_memory_mb(total_memory_mb)
}

fn system_resource_status_from_total_memory_mb(total_memory_mb: u64) -> SystemResourceResponse {
    let (recommended_min_mb, recommended_max_mb) = recommended_memory_range(total_memory_mb);

    SystemResourceResponse {
        total_memory_mb,
        recommended_min_mb,
        recommended_max_mb,
        max_allocatable_gb: total_memory_mb / 1024,
    }
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
    use super::{recommended_memory_range, system_resource_status_from_total_memory_mb};

    #[test]
    fn keeps_low_memory_recommendations_within_available_budget() {
        assert_eq!(recommended_memory_range(4096), (2048, 2048));
        assert_eq!(recommended_memory_range(3072), (1024, 1024));
        assert_eq!(recommended_memory_range(2048), (0, 0));
    }

    #[test]
    fn preserves_route_response_shape_values() {
        assert_eq!(
            system_resource_status_from_total_memory_mb(8192),
            super::SystemResourceResponse {
                total_memory_mb: 8192,
                recommended_min_mb: 2048,
                recommended_max_mb: 4096,
                max_allocatable_gb: 8,
            }
        );
    }
}

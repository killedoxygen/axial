//! Host memory facts and the retained manual allocation recommendations.

use serde::Serialize;
use sysinfo::System;
use ts_rs::TS;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, TS)]
pub struct SystemResourceResponse {
    pub total_memory_mb: u64,
    pub recommended_min_mb: u64,
    pub recommended_max_mb: u64,
    pub max_allocatable_gb: u64,
}

/// A synchronous host query; HTTP callers run it on a blocking worker.
pub fn system_resource_status() -> SystemResourceResponse {
    let mut system = System::new();
    system.refresh_memory();
    from_total_memory_mb((system.total_memory() / (1024 * 1024)).max(1))
}

fn from_total_memory_mb(total_memory_mb: u64) -> SystemResourceResponse {
    let available = total_memory_mb.saturating_sub(2048);
    let (recommended_min_mb, recommended_max_mb) = if available == 0 {
        (0, 0)
    } else {
        let max = (total_memory_mb / 2).clamp(4096, 8192).min(available);
        let mut min = (total_memory_mb / 4)
            .clamp(2048, 4096)
            .min(available)
            .min(max);
        if min < 1024 {
            min = max.min(1024);
        }
        (min, max)
    };
    SystemResourceResponse {
        total_memory_mb,
        recommended_min_mb,
        recommended_max_mb,
        max_allocatable_gb: total_memory_mb / 1024,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_recommendations_reserve_two_gibibytes_and_respect_small_hosts() {
        for (total, min, max) in [
            (1, 0, 0),
            (2048, 0, 0),
            (2560, 512, 512),
            (3072, 1024, 1024),
            (4096, 2048, 2048),
            (8192, 2048, 4096),
            (16384, 4096, 8192),
            (65536, 4096, 8192),
        ] {
            assert_eq!(
                from_total_memory_mb(total),
                SystemResourceResponse {
                    total_memory_mb: total,
                    recommended_min_mb: min,
                    recommended_max_mb: max,
                    max_allocatable_gb: total / 1024,
                }
            );
        }
    }

    #[test]
    fn actual_host_query_returns_only_the_retained_memory_fields() {
        let result = system_resource_status();
        assert!(result.total_memory_mb >= 1);
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 4);
        assert_eq!(value["max_allocatable_gb"], result.total_memory_mb / 1024);
        assert!(result.recommended_min_mb <= result.recommended_max_mb);
        assert!(result.recommended_max_mb <= result.total_memory_mb.saturating_sub(2048));
    }
}

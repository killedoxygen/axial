//! Performance presentation over the launch owner's canonical proof schema.

pub(crate) use crate::launch::reports::comparison_baseline_matches_report;
pub use crate::launch::reports::{
    LaunchProofComparison, LaunchProofRecord, LaunchProofResourceBudget,
};
use serde_json::{Value, json};

pub fn validate_proof(report: &LaunchProofRecord) -> Result<(), super::benchmarks::BenchmarkError> {
    let encoded = crate::launch::reports::encode_report(report)
        .map_err(|_| super::benchmarks::BenchmarkError::Unavailable)?;
    let canonical = crate::launch::reports::decode_report(&encoded)
        .map_err(|_| super::benchmarks::BenchmarkError::Unavailable)?;
    if canonical == *report {
        Ok(())
    } else {
        Err(super::benchmarks::BenchmarkError::Unavailable)
    }
}

pub fn proof_payload(
    report: &LaunchProofRecord,
) -> Result<Value, super::benchmarks::BenchmarkError> {
    let mut payload =
        serde_json::to_value(report).map_err(|_| super::benchmarks::BenchmarkError::Unavailable)?;
    let evidence = report
        .stages
        .iter()
        .flat_map(|stage| &stage.evidence)
        .last()
        .map(|evidence| {
            json!({
                "tone": "info", "label": evidence.summary, "detail": evidence.details.first(),
            })
        });
    payload["view_model"] = json!({
        "outcome_label": public_token_label(&report.outcome, "Unknown"),
        "outcome_tone": match report.outcome.as_str() { "running" | "exited" | "completed" => "ok", "failed" => "err", "stopped" | "interrupted" => "warn", _ => "neutral" },
        "evidence": evidence,
        "comparison": launch_proof_comparison_view_model(report.comparison.as_ref()),
        "resource_budget": launch_proof_resource_budget_view_model(report.resource_budget.as_ref()),
    });
    Ok(payload)
}

fn launch_proof_comparison_view_model(comparison: Option<&LaunchProofComparison>) -> Value {
    let Some(comparison) = comparison else {
        return json!({
            "label": "No baseline",
            "detail": "No comparable local proof yet",
            "tone": "neutral",
        });
    };

    let percent = launch_proof_percent_label(comparison.delta_percent);
    let current = launch_proof_duration_label(comparison.current_value_ms);
    let baseline = launch_proof_duration_label(comparison.baseline_value_ms);
    let proof_label = if comparison.matched_sample_count == 1 {
        "proof"
    } else {
        "proofs"
    };
    let detail = format!(
        "{current} now, {baseline} baseline, {} matched {proof_label}",
        comparison.matched_sample_count
    );
    let (faster_by, slower_by, matches_baseline) = comparison_metric_copy(&comparison.metric_name);

    if comparison.delta_ms < 0 {
        json!({
            "label": format!("{} {} ({}%)", faster_by, launch_proof_signed_duration_label(comparison.delta_ms), percent),
            "detail": detail,
            "tone": "ok",
        })
    } else if comparison.delta_ms > 0 {
        json!({
            "label": format!("{} {} ({}%)", slower_by, launch_proof_signed_duration_label(comparison.delta_ms), percent),
            "detail": detail,
            "tone": "warn",
        })
    } else {
        json!({
            "label": matches_baseline,
            "detail": detail,
            "tone": "neutral",
        })
    }
}

fn launch_proof_resource_budget_view_model(
    resource_budget: Option<&LaunchProofResourceBudget>,
) -> Option<Value> {
    let resource_budget = resource_budget?;
    let mut pressures = Vec::new();
    if resource_budget.memory_pressure {
        pressures.push("memory");
    }
    if resource_budget.cpu_pressure {
        pressures.push("CPU");
    }
    if resource_budget.install_pressure {
        pressures.push("installs");
    }
    if resource_budget.disk_pressure {
        pressures.push("disk");
    }

    let mut details = Vec::new();
    if let Some(value) = resource_budget.estimated_remaining_memory_mb {
        details.push(format!(
            "{} remaining",
            launch_proof_signed_memory_label(value)
        ));
    } else if let Some(value) = resource_budget.host_available_memory_mb {
        details.push(format!("{} available", launch_proof_memory_label(value)));
    } else if let Some(value) = resource_budget.host_used_memory_mb {
        details.push(format!("{} used", launch_proof_memory_label(value)));
    } else if let Some(value) = resource_budget.launcher_process_memory_mb {
        details.push(format!("{} launcher RSS", launch_proof_memory_label(value)));
    }

    if let Some(value) = resource_budget.host_cpu_load_1m_x100 {
        let threads = resource_budget
            .host_cpu_threads
            .filter(|threads| *threads > 0)
            .map(|threads| format!("/{threads} threads"))
            .unwrap_or_default();
        details.push(format!(
            "load {}{}",
            launch_proof_load_average_label(value),
            threads
        ));
    }

    if resource_budget.active_session_count > 0 {
        let allocation = if resource_budget.active_memory_allocation_mb > 0 {
            format!(
                ", {} allocated",
                launch_proof_memory_label(resource_budget.active_memory_allocation_mb)
            )
        } else {
            String::new()
        };
        details.push(format!(
            "{} active {}{}",
            resource_budget.active_session_count,
            if resource_budget.active_session_count == 1 {
                "session"
            } else {
                "sessions"
            },
            allocation
        ));
    }

    if resource_budget.active_install_count > 0 {
        details.push(format!(
            "{} active {}",
            resource_budget.active_install_count,
            if resource_budget.active_install_count == 1 {
                "install"
            } else {
                "installs"
            }
        ));
    }

    if let Some(value) = resource_budget.launch_disk_available_mb {
        details.push(format!("{} disk free", launch_proof_memory_label(value)));
    }

    Some(json!({
        "pressure_label": if pressures.is_empty() {
            "Pressure clear".to_string()
        } else {
            format!("Pressure: {}", pressures.join(", "))
        },
        "details": details,
        "pressure": !pressures.is_empty(),
    }))
}

fn comparison_metric_copy(metric_name: &str) -> (&'static str, &'static str, &'static str) {
    match metric_name {
        "boot_duration_ms" => ("Boot faster by", "Boot slower by", "Boot matches baseline"),
        "total_completed_stage_duration_ms" => (
            "Launch stages faster by",
            "Launch stages slower by",
            "Launch stages match baseline",
        ),
        _ => ("Faster by", "Slower by", "Matches baseline"),
    }
}

fn launch_proof_duration_label(value_ms: u64) -> String {
    if value_ms >= 1000 {
        if value_ms >= 10_000 {
            format!("{}s", value_ms.saturating_add(500) / 1000)
        } else {
            let tenths = value_ms.saturating_add(50) / 100;
            format!("{}.{:01}s", tenths / 10, tenths % 10)
        }
    } else {
        format!("{value_ms}ms")
    }
}

fn launch_proof_memory_label(value_mb: u64) -> String {
    if value_mb >= 1024 {
        let whole = value_mb / 1024;
        let remainder = value_mb % 1024;
        if remainder == 0 {
            format!("{whole} GB")
        } else {
            let tenths = ((remainder * 10) + 512) / 1024;
            if tenths >= 10 {
                format!("{} GB", whole + 1)
            } else {
                format!("{whole}.{tenths} GB")
            }
        }
    } else {
        format!("{value_mb} MB")
    }
}

fn launch_proof_signed_memory_label(value_mb: i64) -> String {
    if value_mb < 0 {
        format!("-{}", launch_proof_memory_label(value_mb.unsigned_abs()))
    } else {
        launch_proof_memory_label(value_mb as u64)
    }
}

fn launch_proof_load_average_label(value_x100: u64) -> String {
    format!("{}.{:02}", value_x100 / 100, value_x100 % 100)
}

fn launch_proof_signed_duration_label(value_ms: i64) -> String {
    launch_proof_duration_label(value_ms.unsigned_abs())
}

fn launch_proof_percent_label(value: f64) -> String {
    let mut percent = format!("{:.1}", value.abs());
    if percent.ends_with(".0") {
        percent.truncate(percent.len() - 2);
    }
    percent
}

fn public_token_label(value: &str, fallback: &str) -> String {
    let labels = value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            format!(
                "{}{}",
                first.to_ascii_uppercase(),
                chars.as_str().to_ascii_lowercase()
            )
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();

    if labels.is_empty() {
        fallback.to_string()
    } else {
        labels.join(" ")
    }
}

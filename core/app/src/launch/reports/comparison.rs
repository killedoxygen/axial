use super::*;

pub(super) const LAUNCH_STAGE_COMPARISON_METRIC_NAME: &str = "total_completed_stage_duration_ms";
pub(super) const LAUNCH_BOOT_COMPARISON_METRIC_NAME: &str = "boot_duration_ms";
pub(super) type LaunchComparisonMetric = (&'static str, u64, fn(&LaunchProofRecord) -> Option<u64>);

pub(super) fn build_comparison_from_candidates(
    current: &LaunchProofRecord,
    candidates: &[LaunchProofRecord],
) -> Option<LaunchProofComparison> {
    if !launch_proof_outcome_is_comparable(&current.outcome) {
        return None;
    }

    let (metric_name, current_value_ms, metric_value) =
        launch_comparison_metric_for_current(current)?;
    let mut matches = candidates
        .iter()
        .filter(|candidate| report_precedes(candidate, current))
        .filter(|candidate| launch_proof_outcome_is_comparable(&candidate.outcome))
        .filter(|candidate| comparison_dimensions_match(current, candidate))
        .filter_map(|candidate| {
            let value_ms = metric_value(candidate)?;
            (value_ms > 0).then_some((candidate, value_ms))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|(left, _), (right, _)| {
        comparison_baseline_mode_rank(current, left)
            .cmp(&comparison_baseline_mode_rank(current, right))
            .then_with(|| {
                right
                    .recorded_at
                    .cmp(&left.recorded_at)
                    .then_with(|| right.session_id.cmp(&left.session_id))
            })
    });
    let matched_sample_count = matches.len();
    let (baseline, baseline_value_ms) = matches.first()?;
    let delta_ms = metric_delta_ms(current_value_ms, *baseline_value_ms);

    Some(LaunchProofComparison {
        baseline_session_id: baseline.session_id.clone(),
        baseline_recorded_at: baseline.recorded_at.clone(),
        baseline: comparison_baseline_snapshot(baseline)?,
        matched_sample_count,
        metric_name: metric_name.to_string(),
        current_value_ms,
        baseline_value_ms: *baseline_value_ms,
        delta_ms,
        delta_percent: (delta_ms as f64 / *baseline_value_ms as f64) * 100.0,
    })
}

pub(super) fn comparison_baseline_snapshot(
    report: &LaunchProofRecord,
) -> Option<LaunchProofComparisonBaseline> {
    Some(LaunchProofComparisonBaseline {
        performance_mode: known_launch_mode(report)?.to_string(),
        version_id: normalized_version_target(report)?.to_string(),
        requested_memory_mb: report.scenario.requested_memory_mb,
        device_tier: normalized_dimension(&report.device.tier)?.to_string(),
        benchmark_profile: report
            .scenario
            .benchmark_profile
            .as_deref()
            .and_then(normalized_dimension)
            .map(str::to_string),
        benchmark_run_type: report
            .scenario
            .benchmark_run_type
            .as_deref()
            .and_then(normalized_dimension)
            .map(str::to_string),
        benchmark_mode: report
            .scenario
            .benchmark_mode
            .as_deref()
            .and_then(normalized_dimension)
            .map(str::to_string),
    })
}

pub(crate) fn comparison_baseline_matches_report(
    comparison: &LaunchProofComparison,
    baseline: &LaunchProofRecord,
) -> bool {
    let metric_value = match comparison.metric_name.as_str() {
        LAUNCH_STAGE_COMPARISON_METRIC_NAME => launch_total_completed_stage_duration_ms(baseline),
        LAUNCH_BOOT_COMPARISON_METRIC_NAME => baseline.boot_duration_ms,
        _ => None,
    };
    launch_proof_outcome_is_comparable(&baseline.outcome)
        && comparison.baseline_session_id == baseline.session_id
        && comparison.baseline_recorded_at == baseline.recorded_at
        && comparison_baseline_snapshot(baseline).as_ref() == Some(&comparison.baseline)
        && metric_value == Some(comparison.baseline_value_ms)
}

pub(super) fn comparison_baseline_mode_rank(
    current: &LaunchProofRecord,
    candidate: &LaunchProofRecord,
) -> u8 {
    match (known_launch_mode(current), known_launch_mode(candidate)) {
        (Some("managed"), Some("vanilla")) => 0,
        _ => 1,
    }
}

pub(super) fn report_precedes(candidate: &LaunchProofRecord, current: &LaunchProofRecord) -> bool {
    (&candidate.recorded_at, &candidate.session_id) < (&current.recorded_at, &current.session_id)
}

pub(super) fn launch_proof_outcome_is_comparable(outcome: &str) -> bool {
    outcome == "exited"
}

pub(super) fn launch_comparison_metric_for_current(
    current: &LaunchProofRecord,
) -> Option<LaunchComparisonMetric> {
    if let Some(boot_duration_ms) = current.boot_duration_ms {
        return Some((
            LAUNCH_BOOT_COMPARISON_METRIC_NAME,
            boot_duration_ms,
            launch_boot_duration_ms,
        ));
    }

    Some((
        LAUNCH_STAGE_COMPARISON_METRIC_NAME,
        launch_total_completed_stage_duration_ms(current)?,
        launch_total_completed_stage_duration_ms,
    ))
}

pub(super) fn comparison_dimensions_match(
    current: &LaunchProofRecord,
    candidate: &LaunchProofRecord,
) -> bool {
    current.session_id != candidate.session_id
        && launch_modes_are_comparable(current, candidate)
        && required_version_targets_match(current, candidate)
        && current.scenario.requested_memory_mb == candidate.scenario.requested_memory_mb
        && required_dimensions_match(&current.device.tier, &candidate.device.tier)
        && (optional_benchmark_dimensions_match(
            current.scenario.benchmark_profile.as_deref(),
            candidate.scenario.benchmark_profile.as_deref(),
        ) || (known_launch_mode(current) == Some("managed")
            && known_launch_mode(candidate) == Some("vanilla")
            && current.scenario.benchmark_profile.as_deref() == Some("managed_default")
            && candidate.scenario.benchmark_profile.as_deref() == Some("vanilla_baseline")
            && matches!(
                current.scenario.benchmark_mode.as_deref(),
                Some("qualification" | "release_validation")
            )))
        && optional_benchmark_dimensions_match(
            current.scenario.benchmark_run_type.as_deref(),
            candidate.scenario.benchmark_run_type.as_deref(),
        )
        && optional_benchmark_dimensions_match(
            current.scenario.benchmark_mode.as_deref(),
            candidate.scenario.benchmark_mode.as_deref(),
        )
}

pub(super) fn launch_modes_are_comparable(
    current: &LaunchProofRecord,
    candidate: &LaunchProofRecord,
) -> bool {
    matches!(
        (known_launch_mode(current), known_launch_mode(candidate)),
        (Some("managed"), Some("vanilla" | "managed"))
            | (Some("vanilla"), Some("vanilla"))
            | (Some("custom"), Some("custom"))
    )
}

pub(super) fn known_launch_mode(report: &LaunchProofRecord) -> Option<&str> {
    let mode = normalized_dimension(&report.scenario.performance_mode)?;
    match mode {
        "managed" | "vanilla" | "custom"
            if required_dimensions_match(
                &report.scenario.scenario_id,
                scenario_id_for_performance_mode(mode),
            ) =>
        {
            Some(mode)
        }
        _ => None,
    }
}

pub(super) fn required_dimensions_match(left: &str, right: &str) -> bool {
    match (normalized_dimension(left), normalized_dimension(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

pub(super) fn optional_benchmark_dimensions_match(left: Option<&str>, right: Option<&str>) -> bool {
    match (
        left.and_then(normalized_dimension),
        right.and_then(normalized_dimension),
    ) {
        (Some(left), Some(right)) => left == right,
        (None, None) => true,
        _ => false,
    }
}

pub(super) fn required_version_targets_match(
    current: &LaunchProofRecord,
    candidate: &LaunchProofRecord,
) -> bool {
    match (
        normalized_version_target(current),
        normalized_version_target(candidate),
    ) {
        (Some(current), Some(candidate)) => current == candidate,
        _ => false,
    }
}

pub(super) fn normalized_dimension(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() || value == "unknown" {
        None
    } else {
        Some(value)
    }
}

pub(super) fn normalized_version_target(report: &LaunchProofRecord) -> Option<&str> {
    report
        .scenario
        .version_id
        .as_deref()
        .and_then(normalized_dimension)
        .or_else(|| normalized_dimension(&report.version_id))
}

// Metric source: launch stage history. The value is the sum of completed stage
// durations, using duration_ms when present and falling back to ended-started.
pub(super) fn launch_total_completed_stage_duration_ms(report: &LaunchProofRecord) -> Option<u64> {
    let mut total = 0_u64;
    let mut completed = false;
    for stage in &report.stages {
        let Some(ended_at_ms) = stage.ended_at_ms else {
            continue;
        };
        let duration_ms = stage
            .duration_ms
            .unwrap_or_else(|| ended_at_ms.saturating_sub(stage.started_at_ms));
        total = total.saturating_add(duration_ms);
        completed = true;
    }
    completed.then_some(total)
}

pub(super) fn launch_boot_duration_ms(report: &LaunchProofRecord) -> Option<u64> {
    report.boot_duration_ms
}

pub(super) fn metric_delta_ms(current_value_ms: u64, baseline_value_ms: u64) -> i64 {
    if current_value_ms >= baseline_value_ms {
        i64::try_from(current_value_ms - baseline_value_ms).unwrap_or(i64::MAX)
    } else {
        -i64::try_from(baseline_value_ms - current_value_ms).unwrap_or(i64::MAX)
    }
}

pub(super) fn scenario_id_for_performance_mode(performance_mode: &str) -> &'static str {
    match performance_mode.trim() {
        "managed" => "managed_launch",
        "vanilla" => "vanilla_launch",
        "custom" => "custom_launch",
        _ => "unknown_launch",
    }
}

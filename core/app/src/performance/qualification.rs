//! Family C qualification retains exact suite, proof, resource and managed-file checks.

use super::benchmarks::{
    BenchmarkSuiteManifest, BenchmarkSuiteManifestRun, BenchmarkSuiteRunSpec,
    benchmark_suite_run_id, bounded_descriptor_token,
};
use super::proofs::{
    LaunchProofComparison, LaunchProofRecord, LaunchProofResourceBudget,
    comparison_baseline_matches_report,
};
use axial_minecraft::loaders::{LoaderComponentId, api::decode_installed_version_id};
use serde_json::{Value, json};

pub const FAMILY_C_QUALIFICATION_PROOF_SCAN_LIMIT: usize = 100;
pub const FAMILY_C_QUALIFICATION_SCHEMA: &str =
    "axial.launch.benchmark.qualification.family_c_1_12_2";
pub const FAMILY_C_QUALIFICATION_SCHEMA_VERSION: u32 = 1;
pub const FAMILY_C_QUALIFICATION_MODE: &str = "release_validation";
pub const FAMILY_C_QUALIFICATION_VERSION: &str = "1.12.2";
pub const FAMILY_C_QUALIFICATION_LOADER: &str = "Forge";
pub const FAMILY_C_BASELINE_TARGET_ID: &str = "family_c_forge_1_12_2_vanilla_baseline";
pub const FAMILY_C_MANAGED_TARGET_ID: &str = "family_c_forge_1_12_2_family_c_forge_core";
pub const FAMILY_C_MANAGED_COMPOSITION_ID: &str = "family-c-forge-core";

const FAMILY_C_COMPARISON_STAGE_METRIC_NAME: &str = "total_completed_stage_duration_ms";
const FAMILY_C_COMPARISON_BOOT_METRIC_NAME: &str = "boot_duration_ms";
const FAMILY_C_MANAGED_EXPECTED_PROJECT_IDS: [&str; 3] = ["jupr7Bf5", "DSVgwcji", "Wnxd13zP"];

pub fn qualification_payload(
    manifest: &BenchmarkSuiteManifest,
    proofs: &[LaunchProofRecord],
    managed_install: Option<&ManagedInstallEvidence>,
    suite_present: bool,
) -> Value {
    let [baseline_target, managed_target] = family_c_qualification_targets();
    let extra_missing = if suite_present {
        vec![]
    } else {
        vec!["suite_manifest_missing"]
    };
    let mut managed_extra_missing = extra_missing.clone();
    if !suite_present {
        managed_extra_missing.push("managed_comparison_missing");
    }
    let preview_install = (!suite_present).then(|| {
        let mut evidence = unobserved_managed_install_evidence(managed_target);
        evidence.missing.clear();
        evidence
    });
    let baseline_proof = family_c_qualification_target_proof(baseline_target, manifest, proofs);
    let baseline = family_c_qualification_target_payload(
        baseline_target,
        manifest,
        proofs,
        baseline_proof,
        managed_install,
        &extra_missing,
    );
    let managed = family_c_qualification_target_payload(
        managed_target,
        manifest,
        proofs,
        baseline_proof,
        managed_install.or(preview_install.as_ref()),
        &managed_extra_missing,
    );
    let status = if family_c_qualification_target_ready(&baseline)
        && family_c_qualification_target_ready(&managed)
    {
        "ready"
    } else {
        "incomplete"
    };

    json!({
        "schema": FAMILY_C_QUALIFICATION_SCHEMA,
        "schema_version": FAMILY_C_QUALIFICATION_SCHEMA_VERSION,
        "status": status,
        "view_model": family_c_qualification_view_model(status, suite_present, manifest, [&baseline, &managed]),
        "suite": if suite_present { json!({
            "suite_id": bounded_descriptor_token(&manifest.suite_id, "suite"),
            "mode": bounded_descriptor_token(&manifest.mode, "mode"),
            "run_count": manifest.runs.len(),
        }) } else { json!({
            "present": false,
            "mode": bounded_descriptor_token(&manifest.mode, "mode"),
            "run_count": manifest.runs.len(),
        }) },
        "target": {
            "family": "C",
            "loader": FAMILY_C_QUALIFICATION_LOADER,
            "version": FAMILY_C_QUALIFICATION_VERSION,
            "mode": FAMILY_C_QUALIFICATION_MODE,
        },
        "targets": [baseline, managed],
    })
}

fn family_c_qualification_view_model(
    status: &str,
    suite_present: bool,
    manifest: &BenchmarkSuiteManifest,
    targets: [&Value; 2],
) -> Value {
    json!({
        "status_label": family_c_qualification_status_label(status),
        "status_tone": family_c_qualification_status_tone(status),
        "target_label": family_c_qualification_target_label(),
        "suite_label": family_c_qualification_suite_summary(suite_present, manifest),
        "schema_label": format!("v{FAMILY_C_QUALIFICATION_SCHEMA_VERSION}"),
        "missing_summary": family_c_qualification_missing_summary(targets),
        "suite_summary": family_c_qualification_suite_summary(suite_present, manifest),
        "evidence_summary": family_c_qualification_evidence_summary(targets),
    })
}

fn family_c_qualification_status_label(status: &str) -> &'static str {
    if status == "ready" {
        "Ready"
    } else {
        "Incomplete"
    }
}

fn family_c_qualification_status_tone(status: &str) -> &'static str {
    if status == "ready" { "ok" } else { "warn" }
}

fn family_c_qualification_target_label() -> String {
    format!(
        "{}, {}, {}, {}",
        qualification_family_label("C"),
        FAMILY_C_QUALIFICATION_LOADER,
        FAMILY_C_QUALIFICATION_VERSION,
        qualification_token_label(FAMILY_C_QUALIFICATION_MODE, "Unknown mode")
    )
}

fn family_c_qualification_suite_summary(
    suite_present: bool,
    manifest: &BenchmarkSuiteManifest,
) -> String {
    if !suite_present {
        return "Suite missing".to_string();
    }
    let mode = qualification_token_label(&manifest.mode, "Suite present");
    format!("{}, {} runs", mode, manifest.runs.len())
}

fn family_c_qualification_missing_summary(targets: [&Value; 2]) -> String {
    let missing = targets
        .iter()
        .flat_map(|target| {
            target
                .get("missing")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
        })
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return "No missing evidence".to_string();
    }

    let mut labels = Vec::new();
    for value in missing.iter() {
        let label = qualification_missing_token_label(value);
        if !labels.contains(&label) {
            labels.push(label);
        }
        if labels.len() >= 2 {
            break;
        }
    }
    let suffix = missing
        .len()
        .checked_sub(labels.len())
        .filter(|count| *count > 0)
        .map(|count| format!(", +{count}"))
        .unwrap_or_default();
    format!("{} missing: {}{}", missing.len(), labels.join(", "), suffix)
}

fn family_c_qualification_evidence_summary(targets: [&Value; 2]) -> String {
    let mut selected = Vec::new();
    for role in ["baseline", "managed"] {
        if let Some(target) = targets
            .iter()
            .find(|target| target.get("role").and_then(Value::as_str) == Some(role))
        {
            selected.push(*target);
        }
    }
    if selected.is_empty() {
        selected.extend(targets);
    }

    selected
        .into_iter()
        .take(2)
        .map(|target| {
            let view_model = target.get("view_model").unwrap_or(&Value::Null);
            let role = view_model
                .get("role_label")
                .and_then(Value::as_str)
                .unwrap_or("Target");
            let suite = view_model
                .get("suite_label")
                .and_then(Value::as_str)
                .unwrap_or("Suite unknown");
            let proof = view_model
                .get("proof_label")
                .and_then(Value::as_str)
                .unwrap_or("Proof unknown");
            format!("{role}: {suite}, {proof}")
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn family_c_qualification_required_label(required: &Value) -> String {
    ["profile", "run_type", "mode", "performance_mode"]
        .into_iter()
        .filter_map(|key| required.get(key).and_then(Value::as_str))
        .filter(|value| !value.trim().is_empty())
        .map(|value| qualification_token_label(value, value))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn family_c_qualification_suite_run_label(suite_run: &Value) -> String {
    if !suite_run
        .get("present")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return "Suite missing".to_string();
    }

    let state = suite_run
        .get("state")
        .and_then(Value::as_str)
        .map(|value| qualification_token_label(value, value))
        .unwrap_or_else(|| "Suite present".to_string());
    suite_run
        .get("run_index")
        .and_then(Value::as_u64)
        .map(|run_index| format!("{state}, run #{}", run_index + 1))
        .unwrap_or(state)
}

fn family_c_qualification_proof_label(proof: &Value) -> String {
    if !proof
        .get("present")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return "Proof missing".to_string();
    }

    let outcome = proof
        .get("outcome")
        .and_then(Value::as_str)
        .map(|value| qualification_token_label(value, value))
        .unwrap_or_else(|| "Proof present".to_string());
    let matched = proof
        .get("comparison")
        .and_then(|comparison| {
            comparison
                .get("present")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                .then_some(comparison)
        })
        .and_then(|comparison| comparison.get("matched_sample_count"))
        .and_then(Value::as_u64)
        .map(|count| format!(", {count} matched"))
        .unwrap_or_default();
    format!("{outcome}{matched}")
}

fn family_c_qualification_missing_label(missing: &[&str]) -> String {
    if missing.is_empty() {
        "Complete".to_string()
    } else {
        format!("{} missing", missing.len())
    }
}

fn qualification_missing_token_label(value: &str) -> String {
    let cleaned = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '_' | '-') {
                ch
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.is_empty() {
        "Evidence".to_string()
    } else {
        qualification_token_label(&cleaned.chars().take(40).collect::<String>(), "Evidence")
    }
}

fn qualification_family_label(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return "Unknown family".to_string();
    }
    if value.len() <= 3
        && value
            .chars()
            .all(|character| character.is_ascii_uppercase() || character == '-')
    {
        format!("Family {value}")
    } else {
        qualification_token_label(value, value)
    }
}

fn qualification_token_label(value: &str, fallback: &str) -> String {
    let parts = value
        .trim()
        .split(|character: char| matches!(character, '_' | '-' | ' ') || character.is_whitespace())
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

    if parts.is_empty() {
        fallback.to_string()
    } else {
        parts.join(" ")
    }
}

fn family_c_qualification_targets() -> [FamilyCQualificationTarget; 2] {
    [
        FamilyCQualificationTarget {
            role: "baseline",
            run_index: 0,
            target_id: FAMILY_C_BASELINE_TARGET_ID,
            profile: "vanilla_baseline",
            run_type: "coldish",
            performance_mode: "vanilla",
            comparison_required: false,
        },
        FamilyCQualificationTarget {
            role: "managed",
            run_index: 1,
            target_id: FAMILY_C_MANAGED_TARGET_ID,
            profile: "managed_default",
            run_type: "coldish",
            performance_mode: "managed",
            comparison_required: true,
        },
    ]
}

fn family_c_qualification_target_payload(
    target: FamilyCQualificationTarget,
    manifest: &BenchmarkSuiteManifest,
    proofs: &[LaunchProofRecord],
    baseline_proof: Option<&LaunchProofRecord>,
    managed_install: Option<&ManagedInstallEvidence>,
    extra_missing: &[&'static str],
) -> Value {
    let mut missing = Vec::new();
    missing.extend(extra_missing.iter().copied());
    let expected_benchmark_id = benchmark_suite_run_id(
        FAMILY_C_QUALIFICATION_MODE,
        target.run_index,
        BenchmarkSuiteRunSpec {
            profile: target.profile,
            run_type: target.run_type,
            target_id: Some(target.target_id),
        },
    );
    let run = manifest
        .runs
        .iter()
        .find(|run| run.target_id.trim() == target.target_id);
    let proof = run.and_then(|run| family_c_qualification_matching_proof(run, proofs));

    if run.is_none() {
        missing.push("suite_run_missing");
    }
    if manifest.mode.trim() != FAMILY_C_QUALIFICATION_MODE {
        missing.push("suite_mode_mismatch");
    }

    if let Some(run) = run {
        if run.profile.trim() != target.profile {
            missing.push("suite_run_profile_mismatch");
        }
        if run.run_type.trim() != target.run_type {
            missing.push("suite_run_type_mismatch");
        }
        if run.benchmark_id.trim().is_empty() {
            missing.push("suite_run_benchmark_id_missing");
        } else if run.benchmark_id != expected_benchmark_id {
            missing.push("suite_run_benchmark_id_mismatch");
        }
        if run.session_id.as_deref().and_then(trimmed_string).is_none() {
            missing.push("suite_run_session_missing");
        }
    }

    match proof {
        Some(proof) => {
            if proof.instance_id != manifest.instance_id {
                missing.push("proof_instance_mismatch");
            }
            if super::proofs::validate_proof(proof).is_err() {
                missing.push("proof_invalid");
            }
            if proof.scenario.benchmark_id.as_deref() != run.map(|run| run.benchmark_id.as_str()) {
                missing.push("proof_benchmark_id_mismatch");
            }
            if proof.scenario.benchmark_profile.as_deref() != Some(target.profile) {
                missing.push("proof_profile_mismatch");
            }
            if proof.scenario.benchmark_run_type.as_deref() != Some(target.run_type) {
                missing.push("proof_run_type_mismatch");
            }
            if proof.scenario.benchmark_mode.as_deref() != Some(FAMILY_C_QUALIFICATION_MODE) {
                missing.push("proof_mode_mismatch");
            }
            if family_c_proof_version(proof).as_deref() != Some(FAMILY_C_QUALIFICATION_VERSION) {
                missing.push("proof_version_mismatch");
            }
            if proof.scenario.performance_mode.trim() != target.performance_mode {
                missing.push("proof_performance_mode_mismatch");
            }
            if !family_c_qualification_outcome_is_acceptable(&proof.outcome) {
                missing.push("proof_outcome_not_comparable");
            }
            if target.comparison_required {
                match proof.comparison.as_ref() {
                    Some(comparison) => {
                        let evidence = family_c_qualification_managed_comparison_evidence(
                            comparison,
                            baseline_proof,
                        );
                        if !evidence.baseline_matches {
                            missing.push("managed_comparison_baseline_mismatch");
                        }
                        if !evidence.metric_valid {
                            missing.push("managed_comparison_metric_missing");
                        }
                        if !evidence.samples_present {
                            missing.push("managed_comparison_sample_missing");
                        }
                        if !evidence.values_present {
                            missing.push("managed_comparison_value_missing");
                        }
                    }
                    None => missing.push("managed_comparison_missing"),
                }
            }
            match proof.resource_budget.as_ref() {
                Some(resource_budget) => {
                    if !family_c_qualification_resource_memory_evidence(resource_budget) {
                        missing.push("proof_resource_memory_evidence_missing");
                    }
                    if !family_c_qualification_resource_cpu_evidence(resource_budget) {
                        missing.push("proof_resource_cpu_evidence_missing");
                    }
                    if !family_c_qualification_resource_install_evidence(resource_budget) {
                        missing.push("proof_resource_install_evidence_missing");
                    }
                    if !family_c_qualification_resource_disk_evidence(resource_budget) {
                        missing.push("proof_resource_disk_evidence_missing");
                    }
                }
                None => {
                    missing.push("proof_resource_budget_missing");
                    missing.push("proof_resource_memory_evidence_missing");
                    missing.push("proof_resource_cpu_evidence_missing");
                    missing.push("proof_resource_install_evidence_missing");
                    missing.push("proof_resource_disk_evidence_missing");
                }
            }
        }
        None => missing.push("proof_missing"),
    }

    let managed_install = managed_install
        .filter(|_| {
            target.target_id == FAMILY_C_MANAGED_TARGET_ID && target.performance_mode == "managed"
        })
        .cloned()
        .unwrap_or_else(|| unobserved_managed_install_evidence(target));
    missing.extend(managed_install.missing.iter().copied());
    missing.sort_unstable();
    missing.dedup();

    let required = json!({
        "profile": target.profile,
        "run_type": target.run_type,
        "mode": FAMILY_C_QUALIFICATION_MODE,
        "performance_mode": target.performance_mode,
    });
    let suite_run = family_c_qualification_suite_run_payload(run);
    let proof_payload = family_c_qualification_proof_payload(proof, target, baseline_proof);
    let view_model = family_c_qualification_target_view_model(
        target,
        &required,
        &suite_run,
        &proof_payload,
        &missing,
    );

    json!({
        "role": target.role,
        "target_id": target.target_id,
        "family": "C",
        "loader": FAMILY_C_QUALIFICATION_LOADER,
        "version": FAMILY_C_QUALIFICATION_VERSION,
        "required": required,
        "suite_run": suite_run,
        "proof": proof_payload,
        "managed_install": managed_install.payload,
        "missing": missing,
        "view_model": view_model,
    })
}

fn family_c_qualification_target_view_model(
    target: FamilyCQualificationTarget,
    required: &Value,
    suite_run: &Value,
    proof: &Value,
    missing: &[&str],
) -> Value {
    json!({
        "role_label": qualification_token_label(target.role, "Target"),
        "target_label": family_c_qualification_target_label(),
        "required_label": family_c_qualification_required_label(required),
        "suite_label": family_c_qualification_suite_run_label(suite_run),
        "suite_present": suite_run
            .get("present")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "proof_label": family_c_qualification_proof_label(proof),
        "proof_present": proof
            .get("present")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "missing_label": family_c_qualification_missing_label(missing),
        "missing_tone": if missing.is_empty() { "ok" } else { "warn" },
    })
}

pub fn managed_install_evidence(
    composition_state: &axial_performance::CompositionState,
    health: axial_performance::BundleHealth,
) -> ManagedInstallEvidence {
    let mut evidence = ManagedInstallEvidence {
        missing: Vec::new(),
        payload: Value::Null,
    };
    let installed_count = composition_state.installed_mods.len();
    let has_installed = installed_count > 0;
    let composition_matches = composition_state.composition_id == FAMILY_C_MANAGED_COMPOSITION_ID;
    let expected_artifacts_present =
        has_installed && family_c_managed_expected_artifacts_present(composition_state);
    let ownership = has_installed
        && composition_state.installed_mods.iter().all(|installed| {
            installed.ownership_class == axial_performance::OwnershipClass::CompositionManaged
        });
    let source = has_installed
        && composition_state.installed_mods.iter().all(|installed| {
            installed.source.provider == axial_performance::ManagedArtifactProvider::Modrinth
        });
    let integrity = matches!(health, axial_performance::BundleHealth::Healthy)
        && has_installed
        && composition_state.installed_mods.iter().all(|installed| {
            installed.integrity.sha512.len() == 128
                && installed
                    .integrity
                    .sha512
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
        });

    if !composition_matches {
        evidence
            .missing
            .push("managed_install_composition_mismatch");
    }
    if !expected_artifacts_present {
        evidence.missing.push("managed_install_artifacts_missing");
    }
    if has_installed && !ownership {
        evidence.missing.push("managed_install_ownership_missing");
    }
    if has_installed && !source {
        evidence.missing.push("managed_install_source_missing");
    }
    if has_installed && !integrity {
        evidence.missing.push("managed_install_integrity_missing");
    }

    evidence.payload = json!({
        "required": true,
        "present": true,
        "composition_id": bounded_descriptor_token(&composition_state.composition_id, "composition"),
        "installed_count": installed_count,
        "expected_artifacts_present": expected_artifacts_present,
        "ownership": ownership,
        "source": source,
        "integrity": integrity,
    });
    evidence
}

fn unobserved_managed_install_evidence(
    target: FamilyCQualificationTarget,
) -> ManagedInstallEvidence {
    if target.target_id != FAMILY_C_MANAGED_TARGET_ID || target.performance_mode != "managed" {
        return ManagedInstallEvidence {
            missing: Vec::new(),
            payload: json!({ "required": false }),
        };
    }
    ManagedInstallEvidence {
        missing: vec!["managed_install_state_missing"],
        payload: json!({
            "required": true,
            "present": false,
            "composition_id": null,
            "installed_count": 0,
            "expected_artifacts_present": false,
            "ownership": false,
            "source": false,
            "integrity": false,
        }),
    }
}

fn family_c_managed_expected_artifacts_present(
    composition_state: &axial_performance::CompositionState,
) -> bool {
    FAMILY_C_MANAGED_EXPECTED_PROJECT_IDS
        .iter()
        .all(|project_id| {
            composition_state
                .installed_mods
                .iter()
                .any(|installed| installed.project_id == *project_id)
        })
}

fn family_c_qualification_target_proof<'a>(
    target: FamilyCQualificationTarget,
    manifest: &BenchmarkSuiteManifest,
    proofs: &'a [LaunchProofRecord],
) -> Option<&'a LaunchProofRecord> {
    manifest
        .runs
        .iter()
        .find(|run| run.target_id.trim() == target.target_id)
        .and_then(|run| family_c_qualification_matching_proof(run, proofs))
}

fn family_c_qualification_matching_proof<'a>(
    run: &BenchmarkSuiteManifestRun,
    proofs: &'a [LaunchProofRecord],
) -> Option<&'a LaunchProofRecord> {
    if let Some(session_id) = run.session_id.as_deref().and_then(trimmed_string) {
        return proofs.iter().find(|proof| proof.session_id == session_id);
    }

    None
}

fn family_c_qualification_managed_comparison_evidence(
    comparison: &LaunchProofComparison,
    baseline_proof: Option<&LaunchProofRecord>,
) -> FamilyCManagedComparisonEvidence {
    FamilyCManagedComparisonEvidence {
        baseline_matches: baseline_proof
            .is_some_and(|baseline| comparison_baseline_matches_report(comparison, baseline)),
        metric_valid: matches!(
            comparison.metric_name.as_str(),
            FAMILY_C_COMPARISON_STAGE_METRIC_NAME | FAMILY_C_COMPARISON_BOOT_METRIC_NAME
        ),
        samples_present: comparison.matched_sample_count > 0,
        values_present: comparison.baseline_value_ms > 0 && comparison.current_value_ms > 0,
    }
}

fn family_c_qualification_suite_run_payload(run: Option<&BenchmarkSuiteManifestRun>) -> Value {
    let Some(run) = run else {
        return json!({ "present": false });
    };

    json!({
        "present": true,
        "run_index": run.run_index,
        "profile": bounded_descriptor_token(&run.profile, "profile"),
        "run_type": bounded_descriptor_token(&run.run_type, "run-type"),
        "target_id": bounded_descriptor_token(&run.target_id, "target"),
        "benchmark_id": bounded_descriptor_token(&run.benchmark_id, "benchmark"),
        "session_id": run.session_id.as_deref().map(|value| bounded_descriptor_token(value, "session")),
        "state": bounded_descriptor_token(&run.state, "state"),
    })
}

fn family_c_qualification_proof_payload(
    proof: Option<&LaunchProofRecord>,
    target: FamilyCQualificationTarget,
    baseline_proof: Option<&LaunchProofRecord>,
) -> Value {
    let Some(proof) = proof else {
        return json!({ "present": false });
    };
    let comparison = proof.comparison.as_ref().map(|comparison| {
        let mut payload = json!({
            "present": true,
            "baseline_session_id": bounded_descriptor_token(
                &comparison.baseline_session_id,
                "session"
            ),
            "metric_name": bounded_descriptor_token(&comparison.metric_name, "metric"),
            "matched_sample_count": comparison.matched_sample_count,
        });
        if target.comparison_required {
            let evidence =
                family_c_qualification_managed_comparison_evidence(comparison, baseline_proof);
            payload["baseline_matches"] = json!(evidence.baseline_matches);
            payload["metric_valid"] = json!(evidence.metric_valid);
            payload["samples_present"] = json!(evidence.samples_present);
            payload["values_present"] = json!(evidence.values_present);
        }
        payload
    });

    json!({
        "present": true,
        "session_id": bounded_descriptor_token(&proof.session_id, "session"),
        "benchmark_id": proof
            .scenario
            .benchmark_id
            .as_deref()
            .map(|value| bounded_descriptor_token(value, "benchmark")),
        "profile": proof
            .scenario
            .benchmark_profile
            .as_deref()
            .map(|value| bounded_descriptor_token(value, "profile")),
        "run_type": proof
            .scenario
            .benchmark_run_type
            .as_deref()
            .map(|value| bounded_descriptor_token(value, "run-type")),
        "mode": proof
            .scenario
            .benchmark_mode
            .as_deref()
            .map(|value| bounded_descriptor_token(value, "mode")),
        "performance_mode": bounded_descriptor_token(&proof.scenario.performance_mode, "mode"),
        "version": family_c_proof_version(proof)
            .as_deref()
            .map(|value| bounded_descriptor_token(value, "version")),
        "outcome": bounded_descriptor_token(&proof.outcome, "outcome"),
        "comparison": comparison.unwrap_or_else(|| json!({ "present": false })),
        "resource_budget": family_c_qualification_resource_budget_payload(
            proof.resource_budget.as_ref()
        ),
    })
}

fn family_c_qualification_resource_budget_payload(
    resource_budget: Option<&LaunchProofResourceBudget>,
) -> Value {
    let Some(resource_budget) = resource_budget else {
        return json!({
            "present": false,
            "memory": false,
            "cpu": false,
            "install": false,
            "disk": false,
        });
    };

    json!({
        "present": true,
        "memory": family_c_qualification_resource_memory_evidence(resource_budget),
        "cpu": family_c_qualification_resource_cpu_evidence(resource_budget),
        "install": family_c_qualification_resource_install_evidence(resource_budget),
        "disk": family_c_qualification_resource_disk_evidence(resource_budget),
    })
}

fn family_c_qualification_resource_memory_evidence(
    resource_budget: &LaunchProofResourceBudget,
) -> bool {
    resource_budget.host_total_memory_mb.is_some()
        && resource_budget.requested_memory_mb.is_some()
        && resource_budget.estimated_remaining_memory_mb.is_some()
}

fn family_c_qualification_resource_cpu_evidence(
    resource_budget: &LaunchProofResourceBudget,
) -> bool {
    resource_budget.host_cpu_threads.is_some()
        || resource_budget.host_cpu_load_1m_x100.is_some()
        || resource_budget.host_cpu_load_5m_x100.is_some()
        || resource_budget.host_cpu_load_15m_x100.is_some()
}

fn family_c_qualification_resource_install_evidence(
    _resource_budget: &LaunchProofResourceBudget,
) -> bool {
    true
}

fn family_c_qualification_resource_disk_evidence(
    resource_budget: &LaunchProofResourceBudget,
) -> bool {
    resource_budget.launch_disk_available_mb.is_some()
}

fn family_c_qualification_target_ready(target: &Value) -> bool {
    target
        .get("missing")
        .and_then(|missing| missing.as_array())
        .is_some_and(Vec::is_empty)
}

fn family_c_proof_version(proof: &LaunchProofRecord) -> Option<String> {
    let version = proof
        .scenario
        .version_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "unknown")
        .or_else(|| {
            let value = proof.version_id.trim();
            (!value.is_empty() && value != "unknown").then_some(value)
        })?;
    Some(match decode_installed_version_id(version) {
        Ok(identity) if identity.component_id() == LoaderComponentId::Forge => {
            identity.minecraft_version().to_owned()
        }
        _ => version.to_owned(),
    })
}

fn family_c_qualification_outcome_is_acceptable(outcome: &str) -> bool {
    matches!(outcome.trim(), "running" | "exited" | "completed")
}

fn trimmed_string(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[derive(Clone, Copy)]
struct FamilyCQualificationTarget {
    role: &'static str,
    run_index: usize,
    target_id: &'static str,
    profile: &'static str,
    run_type: &'static str,
    performance_mode: &'static str,
    comparison_required: bool,
}

#[derive(Clone, Debug)]
pub struct ManagedInstallEvidence {
    missing: Vec<&'static str>,
    payload: Value,
}

#[derive(Clone, Copy, Debug)]
struct FamilyCManagedComparisonEvidence {
    baseline_matches: bool,
    metric_valid: bool,
    samples_present: bool,
    values_present: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::{
        outcome::{SessionExitReason, SessionOutcome, SessionOutcomeKind},
        reports::{SessionReportInput, decode_report, encode_report},
    };
    use crate::performance::benchmarks::{
        benchmark_suite_manifest_run_inputs, benchmark_suite_plan,
    };
    use axial_minecraft::loaders::{LoaderComponentId, installed_version_id_for};

    fn baseline_payload(version: &str, scenario_version: Option<&str>) -> Value {
        let plan = benchmark_suite_plan(FAMILY_C_QUALIFICATION_MODE).unwrap();
        let mut runs = benchmark_suite_manifest_run_inputs(FAMILY_C_QUALIFICATION_MODE, &plan)
            .into_iter()
            .map(|run| BenchmarkSuiteManifestRun {
                run_index: run.run_index,
                profile: run.profile,
                run_type: run.run_type,
                target_id: run.target_id.unwrap(),
                benchmark_id: run.benchmark_id,
                session_id: None,
                launched_at: None,
                state: "pending".into(),
                launch_intent: Some(uuid::Uuid::new_v4().to_string()),
            })
            .collect::<Vec<_>>();
        let mut report = LaunchProofRecord::from_session(SessionReportInput {
            session_id: uuid::Uuid::new_v4().to_string(),
            instance_id: crate::instances::model::InstanceId::new().to_string(),
            version_id: version.into(),
            launched_at: "2026-01-01T00:00:00.000Z".into(),
            ended_at: "2026-01-01T00:00:02.000Z".into(),
            outcome: SessionOutcome {
                kind: SessionOutcomeKind::Clean,
                reason: SessionExitReason::CleanExit,
                failure_class: None,
                summary: String::new(),
            },
            entries: Vec::new(),
            exit_code: Some(0),
            boot_duration_ms: Some(1000),
            logs_dropped: 0,
        });
        let baseline = &mut runs[0];
        assert_eq!(baseline.target_id, FAMILY_C_BASELINE_TARGET_ID);
        baseline.session_id = Some(report.session_id.clone());
        baseline.launched_at = Some(report.launched_at.clone());
        baseline.state = "exited".into();
        report.scenario.performance_mode = "vanilla".into();
        report.scenario.version_id = scenario_version.map(str::to_owned);
        report.scenario.benchmark_profile = Some(baseline.profile.clone());
        report.scenario.benchmark_run_type = Some(baseline.run_type.clone());
        report.scenario.benchmark_mode = Some(FAMILY_C_QUALIFICATION_MODE.into());
        report.scenario.benchmark_id = Some(baseline.benchmark_id.clone());
        let report = decode_report(&encode_report(&report).unwrap()).unwrap();
        super::super::proofs::validate_proof(&report).unwrap();
        assert_eq!(report.version_id, version);
        assert_eq!(
            report.scenario.version_id.as_deref(),
            scenario_version.filter(|value| !value.is_empty())
        );
        let manifest = BenchmarkSuiteManifest {
            schema: "axial.launch.benchmark.suite".into(),
            schema_version: 2,
            suite_id: uuid::Uuid::new_v4().to_string(),
            instance_id: report.instance_id.clone(),
            mode: FAMILY_C_QUALIFICATION_MODE.into(),
            created_at: report.launched_at.clone(),
            updated_at: report.recorded_at.clone(),
            runs,
        };
        let payload = qualification_payload(&manifest, &[report], None, true);
        assert_eq!(payload["status"], "incomplete");
        let baseline = &payload["targets"][0];
        assert_eq!(baseline["role"], "baseline");
        assert_eq!(baseline["proof"]["present"], true);
        assert_eq!(baseline["proof"]["outcome"], "exited");
        let other_missing = baseline["missing"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .filter(|value| *value != "proof_version_mismatch")
            .collect::<Vec<_>>();
        assert_eq!(
            other_missing,
            [
                "proof_resource_budget_missing",
                "proof_resource_cpu_evidence_missing",
                "proof_resource_disk_evidence_missing",
                "proof_resource_install_evidence_missing",
                "proof_resource_memory_evidence_missing",
            ]
        );
        baseline.clone()
    }

    fn version_matches(baseline: &Value) -> bool {
        !baseline["missing"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "proof_version_mismatch")
    }

    #[test]
    fn canonical_forge_report_matches_family_c_version() {
        for build in ["14.23.5.2860", "14.23.5.2859"] {
            let version =
                installed_version_id_for(LoaderComponentId::Forge, "1.12.2", build).unwrap();
            let baseline = baseline_payload(&version, Some(&version));
            assert!(version_matches(&baseline), "Forge {build}");
            assert_eq!(baseline["proof"]["version"], "1.12.2");
        }
    }

    #[test]
    fn scenario_version_precedes_report_version_with_absent_or_unknown_fallback() {
        let current =
            installed_version_id_for(LoaderComponentId::Forge, "1.12.2", "14.23.5.2860").unwrap();
        let other =
            installed_version_id_for(LoaderComponentId::Forge, "1.7.10", "10.13.4.1614").unwrap();
        let baseline = baseline_payload(&other, Some(&current));
        assert!(version_matches(&baseline));
        assert_eq!(baseline["proof"]["version"], "1.12.2");
        assert!(!version_matches(&baseline_payload(&current, Some(&other))));
        let fabric =
            installed_version_id_for(LoaderComponentId::Fabric, "1.12.2", "0.19.5").unwrap();
        assert!(!version_matches(&baseline_payload(&current, Some(&fabric))));
        for scenario in [None, Some(""), Some("unknown")] {
            let baseline = baseline_payload(&current, scenario);
            assert!(version_matches(&baseline));
            assert_eq!(baseline["proof"]["version"], "1.12.2");
        }
    }

    #[test]
    fn different_minecraft_or_loader_does_not_qualify_as_family_c_forge() {
        for (loader, minecraft, build) in [
            (LoaderComponentId::Forge, "1.7.10", "10.13.4.1614"),
            (LoaderComponentId::Fabric, "1.12.2", "0.19.5"),
        ] {
            let version = installed_version_id_for(loader, minecraft, build).unwrap();
            let baseline = baseline_payload(&version, Some(&version));
            assert!(!version_matches(&baseline));
        }
    }
}

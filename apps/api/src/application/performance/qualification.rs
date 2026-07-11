use super::{
    BenchmarkSuiteRunSpec, benchmark_suite_manifest_run_inputs, benchmark_suite_plan,
    benchmark_suite_run_id,
};
use crate::observability::bounded_descriptor_token;
use crate::state::AppState;
use axum::{Json, http::StatusCode};
use serde_json::{Value, json};

pub(crate) const FAMILY_C_QUALIFICATION_PROOF_SCAN_LIMIT: usize = 100;
pub(crate) const FAMILY_C_QUALIFICATION_SCHEMA: &str =
    "axial.launch.benchmark.qualification.family_c_1_12_2";
pub(crate) const FAMILY_C_QUALIFICATION_SCHEMA_VERSION: u32 = 1;
pub(crate) const FAMILY_C_QUALIFICATION_MODE: &str = "release_validation";
pub(crate) const FAMILY_C_QUALIFICATION_VERSION: &str = "1.12.2";
pub(crate) const FAMILY_C_QUALIFICATION_LOADER: &str = "Forge";
pub(crate) const FAMILY_C_BASELINE_TARGET_ID: &str = "family_c_forge_1_12_2_vanilla_baseline";
pub(crate) const FAMILY_C_MANAGED_TARGET_ID: &str = "family_c_forge_1_12_2_family_c_forge_core";
pub(crate) const FAMILY_C_MANAGED_COMPOSITION_ID: &str = "family-c-forge-core";

const FAMILY_C_COMPARISON_STAGE_METRIC_NAME: &str = "total_completed_stage_duration_ms";
const FAMILY_C_COMPARISON_BOOT_METRIC_NAME: &str = "boot_duration_ms";
const FAMILY_C_MANAGED_EXPECTED_ARTIFACTS: [(&str, &str, &str); 3] = [
    ("foamfix", "jupr7Bf5", "foamfix"),
    ("ai-improvements", "DSVgwcji", "ai-improvements"),
    ("clumps", "clumps", "clumps"),
];
const BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE: &str =
    "Could not load benchmark suite data. Check app data permissions and try again.";

pub(crate) async fn family_c_qualification_payload(
    state: &AppState,
    suite_id: &str,
) -> Result<Value, (StatusCode, Json<Value>)> {
    let normalized_suite_id = crate::state::benchmark_suites::normalize_suite_id(suite_id)
        .ok_or_else(benchmark_suite_not_found_error)?;
    let manifest = state
        .benchmark_suites()
        .get(&normalized_suite_id)
        .map_err(|_| benchmark_suite_storage_error_response())?
        .ok_or_else(benchmark_suite_not_found_error)?;
    if manifest.schema != "axial.launch.benchmark.suite" || manifest.schema_version != 2 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "benchmark suite manifest is not current schema" })),
        ));
    }

    let proofs = family_c_qualification_proofs(state, &manifest);
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        family_c_qualification_manifest_payload(
            Some(&state),
            &manifest,
            &proofs,
            [Vec::new(), Vec::new()],
            true,
        )
    })
    .await
    .map_err(|_| benchmark_suite_storage_error_response())
}

pub(crate) fn family_c_qualification_preview_payload() -> Result<Value, (StatusCode, Json<Value>)> {
    let manifest = family_c_qualification_preview_manifest()?;
    let mut payload = family_c_qualification_manifest_payload(
        None,
        &manifest,
        &[],
        [
            vec!["suite_manifest_missing"],
            vec!["suite_manifest_missing", "managed_comparison_missing"],
        ],
        false,
    );
    payload["suite"] = json!({
        "present": false,
        "mode": FAMILY_C_QUALIFICATION_MODE,
        "run_count": manifest.runs.len(),
    });

    Ok(payload)
}

fn family_c_qualification_preview_manifest()
-> Result<crate::state::benchmark_suites::BenchmarkSuiteManifest, (StatusCode, Json<Value>)> {
    let plan = benchmark_suite_plan(FAMILY_C_QUALIFICATION_MODE)
        .ok_or_else(unsupported_suite_mode_error)?;
    let runs = benchmark_suite_manifest_run_inputs(FAMILY_C_QUALIFICATION_MODE, &plan)
        .into_iter()
        .map(
            |run| crate::state::benchmark_suites::BenchmarkSuiteManifestRun {
                run_index: run.run_index,
                profile: run.profile,
                run_type: run.run_type,
                target_id: run.target_id.unwrap_or_default(),
                benchmark_id: run.benchmark_id,
                session_id: None,
                launched_at: None,
                state: "pending".to_string(),
            },
        )
        .collect();

    Ok(crate::state::benchmark_suites::BenchmarkSuiteManifest {
        schema: "axial.launch.benchmark.suite".to_string(),
        schema_version: 2,
        suite_id: crate::state::benchmark_suites::derive_suite_id(
            "preview",
            FAMILY_C_QUALIFICATION_MODE,
        ),
        instance_id: "preview".to_string(),
        mode: FAMILY_C_QUALIFICATION_MODE.to_string(),
        created_at: "preview".to_string(),
        updated_at: "preview".to_string(),
        runs,
    })
}

fn family_c_qualification_manifest_payload(
    state: Option<&AppState>,
    manifest: &crate::state::benchmark_suites::BenchmarkSuiteManifest,
    proofs: &[crate::state::launch_reports::LaunchProofRecord],
    extra_missing: [Vec<&'static str>; 2],
    suite_present: bool,
) -> Value {
    let [baseline_target, managed_target] = family_c_qualification_targets();
    let [baseline_extra_missing, managed_extra_missing] = extra_missing;
    let baseline_proof = family_c_qualification_target_proof(baseline_target, manifest, proofs);
    let baseline = family_c_qualification_target_payload(
        state,
        baseline_target,
        manifest,
        proofs,
        baseline_proof,
        &baseline_extra_missing,
    );
    let managed = family_c_qualification_target_payload(
        state,
        managed_target,
        manifest,
        proofs,
        baseline_proof,
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
        "suite": {
            "suite_id": bounded_descriptor_token(&manifest.suite_id, "suite"),
            "mode": bounded_descriptor_token(&manifest.mode, "mode"),
            "run_count": manifest.runs.len(),
        },
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
    manifest: &crate::state::benchmark_suites::BenchmarkSuiteManifest,
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
    manifest: &crate::state::benchmark_suites::BenchmarkSuiteManifest,
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

fn family_c_qualification_proofs(
    state: &AppState,
    manifest: &crate::state::benchmark_suites::BenchmarkSuiteManifest,
) -> Vec<crate::state::launch_reports::LaunchProofRecord> {
    let mut proofs = state
        .launch_reports()
        .list_recent(FAMILY_C_QUALIFICATION_PROOF_SCAN_LIMIT);
    for session_id in manifest
        .runs
        .iter()
        .filter_map(|run| run.session_id.as_deref())
    {
        let already_loaded = proofs.iter().any(|proof| proof.session_id == session_id);
        if already_loaded {
            continue;
        }
        if let Some(proof) = state.launch_reports().load(session_id) {
            proofs.push(proof);
        }
    }

    proofs
}

fn family_c_qualification_target_payload(
    state: Option<&AppState>,
    target: FamilyCQualificationTarget,
    manifest: &crate::state::benchmark_suites::BenchmarkSuiteManifest,
    proofs: &[crate::state::launch_reports::LaunchProofRecord],
    baseline_proof: Option<&crate::state::launch_reports::LaunchProofRecord>,
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
            if family_c_proof_version(proof) != Some(FAMILY_C_QUALIFICATION_VERSION) {
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
            if proof.guardian.is_none() {
                missing.push("proof_guardian_missing");
            } else if family_c_qualification_guardian_decision(proof).is_none() {
                missing.push("proof_guardian_decision_missing");
            }
        }
        None => missing.push("proof_missing"),
    }

    let managed_install = family_c_qualification_managed_install_evidence(state, target, proof);
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

fn family_c_qualification_managed_install_evidence(
    state: Option<&AppState>,
    target: FamilyCQualificationTarget,
    proof: Option<&crate::state::launch_reports::LaunchProofRecord>,
) -> FamilyCManagedInstallEvidence {
    if target.target_id != FAMILY_C_MANAGED_TARGET_ID || target.performance_mode != "managed" {
        return FamilyCManagedInstallEvidence {
            missing: Vec::new(),
            payload: json!({ "required": false }),
        };
    }

    let mut evidence = FamilyCManagedInstallEvidence {
        missing: Vec::new(),
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
    };
    let (Some(state), Some(proof)) = (state, proof) else {
        return evidence;
    };
    let Some(instance_id) = trimmed_string(&proof.instance_id) else {
        evidence.missing.push("managed_install_state_missing");
        return evidence;
    };
    if state.instances().get(&instance_id).is_none() {
        evidence.missing.push("managed_install_state_missing");
        return evidence;
    }

    let mods_dir = state.instances().game_dir(&instance_id).join("mods");
    let composition_state = match axial_performance::load_state(&mods_dir) {
        Ok(Some(state)) => state,
        Ok(None) => {
            evidence.missing.push("managed_install_state_missing");
            return evidence;
        }
        Err(axial_performance::StateError::InvalidOwnership { .. }) => {
            evidence.missing.push("managed_install_ownership_missing");
            return evidence;
        }
        Err(axial_performance::StateError::InvalidIntegrity { .. }) => {
            evidence.missing.push("managed_install_integrity_missing");
            return evidence;
        }
        Err(_) => {
            evidence.missing.push("managed_install_state_invalid");
            return evidence;
        }
    };

    let installed_count = composition_state.installed_mods.len();
    let has_installed = installed_count > 0;
    let composition_matches = composition_state.composition_id == FAMILY_C_MANAGED_COMPOSITION_ID;
    let expected_artifacts_present =
        has_installed && family_c_managed_expected_artifacts_present(&composition_state);
    let ownership = has_installed
        && composition_state.installed_mods.iter().all(|installed| {
            installed.ownership_class == axial_performance::OwnershipClass::CompositionManaged
        });
    let source = has_installed
        && composition_state.installed_mods.iter().all(|installed| {
            installed.source.provider == axial_performance::ManagedArtifactProvider::Modrinth
        });
    let integrity = has_installed
        && composition_state.installed_mods.iter().all(|installed| {
            installed.integrity.sha512_verified && !installed.integrity.sha512.trim().is_empty()
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

fn family_c_managed_expected_artifacts_present(
    composition_state: &axial_performance::CompositionState,
) -> bool {
    FAMILY_C_MANAGED_EXPECTED_ARTIFACTS
        .iter()
        .all(|(artifact_id, project_id, slug)| {
            composition_state.installed_mods.iter().any(|installed| {
                let installed_project_id = installed.project_id.trim();
                installed_project_id == *artifact_id
                    || installed_project_id == *project_id
                    || installed_project_id == *slug
            })
        })
}

fn family_c_qualification_target_proof<'a>(
    target: FamilyCQualificationTarget,
    manifest: &crate::state::benchmark_suites::BenchmarkSuiteManifest,
    proofs: &'a [crate::state::launch_reports::LaunchProofRecord],
) -> Option<&'a crate::state::launch_reports::LaunchProofRecord> {
    manifest
        .runs
        .iter()
        .find(|run| run.target_id.trim() == target.target_id)
        .and_then(|run| family_c_qualification_matching_proof(run, proofs))
}

fn family_c_qualification_matching_proof<'a>(
    run: &crate::state::benchmark_suites::BenchmarkSuiteManifestRun,
    proofs: &'a [crate::state::launch_reports::LaunchProofRecord],
) -> Option<&'a crate::state::launch_reports::LaunchProofRecord> {
    if let Some(session_id) = run.session_id.as_deref().and_then(trimmed_string) {
        return proofs.iter().find(|proof| proof.session_id == session_id);
    }

    proofs
        .iter()
        .find(|proof| proof.scenario.benchmark_id.as_deref() == Some(run.benchmark_id.as_str()))
}

fn family_c_qualification_managed_comparison_evidence(
    comparison: &crate::state::launch_reports::LaunchProofComparison,
    baseline_proof: Option<&crate::state::launch_reports::LaunchProofRecord>,
) -> FamilyCManagedComparisonEvidence {
    FamilyCManagedComparisonEvidence {
        baseline_matches: baseline_proof.is_some_and(|baseline| {
            crate::state::launch_reports::comparison_baseline_matches_report(comparison, baseline)
        }),
        metric_valid: matches!(
            comparison.metric_name.as_str(),
            FAMILY_C_COMPARISON_STAGE_METRIC_NAME | FAMILY_C_COMPARISON_BOOT_METRIC_NAME
        ),
        samples_present: comparison.matched_sample_count > 0,
        values_present: comparison.baseline_value_ms > 0 && comparison.current_value_ms > 0,
    }
}

fn family_c_qualification_suite_run_payload(
    run: Option<&crate::state::benchmark_suites::BenchmarkSuiteManifestRun>,
) -> Value {
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
    proof: Option<&crate::state::launch_reports::LaunchProofRecord>,
    target: FamilyCQualificationTarget,
    baseline_proof: Option<&crate::state::launch_reports::LaunchProofRecord>,
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
            .map(|value| bounded_descriptor_token(value, "version")),
        "outcome": bounded_descriptor_token(&proof.outcome, "outcome"),
        "comparison": comparison.unwrap_or_else(|| json!({ "present": false })),
        "resource_budget": family_c_qualification_resource_budget_payload(
            proof.resource_budget.as_ref()
        ),
        "guardian": family_c_qualification_guardian_payload(proof),
    })
}

fn family_c_qualification_resource_budget_payload(
    resource_budget: Option<&crate::state::launch_reports::LaunchProofResourceBudget>,
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
    resource_budget: &crate::state::launch_reports::LaunchProofResourceBudget,
) -> bool {
    resource_budget.host_total_memory_mb.is_some()
        && resource_budget.requested_memory_mb.is_some()
        && resource_budget.estimated_remaining_memory_mb.is_some()
}

fn family_c_qualification_resource_cpu_evidence(
    resource_budget: &crate::state::launch_reports::LaunchProofResourceBudget,
) -> bool {
    resource_budget.host_cpu_threads.is_some()
        || resource_budget.host_cpu_load_1m_x100.is_some()
        || resource_budget.host_cpu_load_5m_x100.is_some()
        || resource_budget.host_cpu_load_15m_x100.is_some()
}

fn family_c_qualification_resource_install_evidence(
    _resource_budget: &crate::state::launch_reports::LaunchProofResourceBudget,
) -> bool {
    true
}

fn family_c_qualification_resource_disk_evidence(
    resource_budget: &crate::state::launch_reports::LaunchProofResourceBudget,
) -> bool {
    resource_budget.launch_disk_available_mb.is_some()
}

fn family_c_qualification_guardian_payload(
    proof: &crate::state::launch_reports::LaunchProofRecord,
) -> Value {
    let Some(guardian) = proof.guardian.as_ref() else {
        return json!({ "present": false });
    };
    let decision = guardian
        .get("decision")
        .and_then(|value| value.as_str())
        .and_then(trimmed_string);

    json!({
        "present": true,
        "decision": decision.map(|value| bounded_descriptor_token(&value, "decision")),
    })
}

fn family_c_qualification_guardian_decision(
    proof: &crate::state::launch_reports::LaunchProofRecord,
) -> Option<&str> {
    proof
        .guardian
        .as_ref()
        .and_then(|guardian| guardian.get("decision"))
        .and_then(|decision| decision.as_str())
        .map(str::trim)
        .filter(|decision| !decision.is_empty())
}

fn family_c_qualification_target_ready(target: &Value) -> bool {
    target
        .get("missing")
        .and_then(|missing| missing.as_array())
        .is_some_and(Vec::is_empty)
}

fn family_c_proof_version(proof: &crate::state::launch_reports::LaunchProofRecord) -> Option<&str> {
    proof
        .scenario
        .version_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "unknown")
        .or_else(|| {
            let value = proof.version_id.trim();
            (!value.is_empty() && value != "unknown").then_some(value)
        })
}

fn family_c_qualification_outcome_is_acceptable(outcome: &str) -> bool {
    matches!(outcome.trim(), "running" | "exited" | "completed")
}

fn trimmed_string(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn benchmark_suite_not_found_error() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "benchmark suite not found" })),
    )
}

fn unsupported_suite_mode_error() -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "suite_mode is not supported" })),
    )
}

fn benchmark_suite_storage_error_response() -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE })),
    )
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

#[derive(Debug)]
struct FamilyCManagedInstallEvidence {
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

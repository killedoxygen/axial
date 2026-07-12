use super::summary::GuardianDecision;
use super::{
    DiagnosisId, GuardianIntervention, GuardianInterventionKind, GuardianSummary,
    guardian_install_outcome_fact_group, guardian_install_outcome_from_persisted_group,
    guardian_proof_evidence,
};
use axial_launcher::GuardianMode;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const COPY_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/guardian/guardian-projection-copy-v1.json"
));
const REGENERATE_ENV: &str = "AXIAL_REGENERATE_GUARDIAN_PROJECTION_COPY_SNAPSHOT";
const EXPECTED_CASE_IDS: [&str; 20] = [
    "proof.blocked",
    "proof.warned",
    "proof.intervened",
    "proof.allowed_note",
    "proof.allowed_empty",
    "proof.hostile_lead",
    "install.retry",
    "install.suppressed",
    "install.metadata_invalid",
    "install.dependency_failed",
    "install.runtime_unavailable",
    "install.rosetta",
    "install.filesystem_permission",
    "install.temp_leftover",
    "install.atomic_promotion",
    "install.ownership_unsafe",
    "install.malformed_summary",
    "install.missing_detail",
    "install.unsafe_detail",
    "install.duplicate_summary",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum SnapshotSchema {
    #[serde(rename = "axial.guardian.projection_copy.v1")]
    V1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianProjectionCopySnapshot {
    schema: SnapshotSchema,
    cases: Vec<GuardianProjectionCopyCase>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianProjectionCopyCase {
    id: String,
    input: GuardianProjectionCopyInput,
    output: GuardianProjectionCopyOutput,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "surface", rename_all = "snake_case", deny_unknown_fields)]
enum GuardianProjectionCopyInput {
    Proof {
        decision: GuardianDecision,
        message: Option<String>,
        #[serde(default)]
        details: Vec<String>,
        #[serde(default)]
        guidance: Vec<String>,
        #[serde(default)]
        intervention_details: Vec<String>,
    },
    InstallPersistence {
        diagnosis_id: DiagnosisId,
        facts: Vec<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "surface", rename_all = "snake_case", deny_unknown_fields)]
enum GuardianProjectionCopyOutput {
    Proof {
        evidence: Option<GuardianProofEvidenceFixture>,
    },
    InstallPersistence {
        outcome: Option<GuardianInstallOutcomeFixture>,
        #[serde(skip_serializing_if = "Option::is_none")]
        retry_disabled_reason: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianProofEvidenceFixture {
    tone: String,
    label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianInstallOutcomeFixture {
    diagnosis_id: DiagnosisId,
    decision: String,
    label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    guidance: Vec<String>,
}

#[test]
fn checked_in_guardian_projection_copy_is_byte_stable_and_complete() {
    let fixture = committed_fixture();
    assert_snapshot_coverage(&fixture);
    let replayed = replay_snapshot(&fixture);

    assert_eq!(fixture, replayed);
    assert_eq!(snapshot_bytes(&replayed), COPY_FIXTURE.as_bytes());
    assert_public_bounds_and_privacy(&replayed);
}

#[test]
fn guardian_projection_copy_rejects_unknown_and_malformed_fields() {
    let mut unknown =
        serde_json::from_str::<serde_json::Value>(COPY_FIXTURE).expect("projection fixture JSON");
    unknown["cases"][0]["input"]["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<GuardianProjectionCopySnapshot>(unknown).is_err());

    let mut unknown_nested =
        serde_json::from_str::<serde_json::Value>(COPY_FIXTURE).expect("projection fixture JSON");
    unknown_nested["cases"][6]["output"]["outcome"]["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<GuardianProjectionCopySnapshot>(unknown_nested).is_err());

    let mut malformed =
        serde_json::from_str::<serde_json::Value>(COPY_FIXTURE).expect("projection fixture JSON");
    malformed["cases"][0]["input"]["decision"] = serde_json::json!("unknown");
    assert!(serde_json::from_value::<GuardianProjectionCopySnapshot>(malformed).is_err());
}

#[test]
#[ignore = "explicit fixture regeneration only"]
fn regenerate_guardian_projection_copy_fixture() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to regenerate the Guardian projection copy snapshot"
    );
    let committed = committed_fixture();
    assert_snapshot_coverage(&committed);
    let replayed = replay_snapshot(&committed);
    assert_public_bounds_and_privacy(&replayed);
    std::fs::write(snapshot_fixture_path(), snapshot_bytes(&replayed))
        .expect("write regenerated Guardian projection copy fixture");
}

fn committed_fixture() -> GuardianProjectionCopySnapshot {
    serde_json::from_str(COPY_FIXTURE).expect("strict committed Guardian projection copy fixture")
}

fn replay_snapshot(snapshot: &GuardianProjectionCopySnapshot) -> GuardianProjectionCopySnapshot {
    GuardianProjectionCopySnapshot {
        schema: snapshot.schema,
        cases: snapshot
            .cases
            .iter()
            .map(|case| GuardianProjectionCopyCase {
                id: case.id.clone(),
                input: case.input.clone(),
                output: render_output(&case.input),
            })
            .collect(),
    }
}

fn render_output(input: &GuardianProjectionCopyInput) -> GuardianProjectionCopyOutput {
    match input {
        GuardianProjectionCopyInput::Proof {
            decision,
            message,
            details,
            guidance,
            intervention_details,
        } => {
            let guardian = GuardianSummary {
                mode: GuardianMode::Managed,
                decision: *decision,
                message: message.clone(),
                details: details.clone(),
                guidance: guidance.clone(),
                interventions: intervention_details
                    .iter()
                    .cloned()
                    .map(|detail| GuardianIntervention {
                        kind: GuardianInterventionKind::SwitchManagedRuntime,
                        public_detail: Some(detail.clone()),
                        detail: Some(detail),
                        silent: Some(false),
                    })
                    .collect(),
            };
            GuardianProjectionCopyOutput::Proof {
                evidence: guardian_proof_evidence(&guardian).map(project_serialized),
            }
        }
        GuardianProjectionCopyInput::InstallPersistence {
            diagnosis_id,
            facts,
        } => {
            let outcome = guardian_install_outcome_fact_group(facts.iter().map(String::as_str))
                .and_then(|group| {
                    guardian_install_outcome_from_persisted_group(*diagnosis_id, group)
                });
            let retry_disabled_reason = outcome
                .as_ref()
                .filter(|outcome| outcome.decision() == "block")
                .map(|outcome| outcome.retry_disabled_reason().to_string());
            GuardianProjectionCopyOutput::InstallPersistence {
                outcome: outcome.map(project_serialized),
                retry_disabled_reason,
            }
        }
    }
}

fn project_serialized<T: Serialize, U: DeserializeOwned>(value: T) -> U {
    serde_json::from_value(serde_json::to_value(value).expect("serialize Guardian projection"))
        .expect("deserialize strict Guardian projection")
}

fn assert_snapshot_coverage(snapshot: &GuardianProjectionCopySnapshot) {
    assert_eq!(snapshot.schema, SnapshotSchema::V1);
    assert_eq!(snapshot.cases.len(), EXPECTED_CASE_IDS.len());
    for (case, expected_id) in snapshot.cases.iter().zip(EXPECTED_CASE_IDS) {
        assert_eq!(case.id, expected_id);
    }

    let expected_install_coordinates = [
        (DiagnosisId::DownloadUnavailable, "retry"),
        (DiagnosisId::DownloadUnavailable, "block"),
        (DiagnosisId::InstallArtifactMetadataInvalid, "block"),
        (DiagnosisId::InstallDependencyFailed, "block"),
        (DiagnosisId::ManagedRuntimeUnavailableForPlatform, "block"),
        (DiagnosisId::ManagedRuntimeRosettaRequired, "block"),
        (DiagnosisId::FilesystemPermissionDenied, "block"),
        (DiagnosisId::TempFileLeftover, "block"),
        (DiagnosisId::AtomicPromotionFailed, "block"),
        (DiagnosisId::ArtifactOwnershipUnsafe, "block"),
    ];
    for (diagnosis_id, decision) in expected_install_coordinates {
        assert_eq!(
            snapshot
                .cases
                .iter()
                .filter_map(|case| match &case.output {
                    GuardianProjectionCopyOutput::InstallPersistence {
                        outcome: Some(outcome),
                        ..
                    } if outcome.diagnosis_id == diagnosis_id && outcome.decision == decision =>
                        Some(()),
                    _ => None,
                })
                .count(),
            1,
            "missing or duplicate install projection for {diagnosis_id:?}/{decision}"
        );
    }
}

fn assert_public_bounds_and_privacy(snapshot: &GuardianProjectionCopySnapshot) {
    for case in &snapshot.cases {
        match &case.output {
            GuardianProjectionCopyOutput::Proof { evidence } => {
                if let Some(evidence) = evidence {
                    assert!(!evidence.label.is_empty() && evidence.label.len() <= 180);
                    assert!(
                        evidence
                            .detail
                            .as_ref()
                            .is_none_or(|detail| !detail.is_empty() && detail.len() <= 150)
                    );
                }
            }
            GuardianProjectionCopyOutput::InstallPersistence {
                outcome,
                retry_disabled_reason,
            } => {
                if let Some(outcome) = outcome {
                    assert!(!outcome.decision.is_empty());
                    assert!(!outcome.label.is_empty() && outcome.label.len() <= 180);
                    assert!(
                        outcome
                            .detail
                            .as_ref()
                            .is_none_or(|detail| !detail.is_empty() && detail.len() <= 240)
                    );
                    assert!(outcome.guidance.len() <= 6);
                    assert!(
                        outcome
                            .guidance
                            .iter()
                            .all(|line| !line.is_empty() && line.len() <= 240)
                    );
                }
                assert!(
                    retry_disabled_reason
                        .as_ref()
                        .is_none_or(|reason| !reason.is_empty() && reason.len() <= 240)
                );
            }
        }
    }
    let encoded = serde_json::to_string(
        &snapshot
            .cases
            .iter()
            .map(|case| &case.output)
            .collect::<Vec<_>>(),
    )
    .expect("serialize Guardian projection outputs");
    for sensitive in ["/home/alice", "accessToken", "raw-secret-token"] {
        assert!(
            !encoded.contains(sensitive),
            "leaked {sensitive} in fixture output"
        );
    }
}

fn snapshot_bytes(snapshot: &GuardianProjectionCopySnapshot) -> Vec<u8> {
    let pretty = serde_json::to_string_pretty(snapshot).expect("serialize projection snapshot");
    format!("{pretty}\n").into_bytes()
}

fn snapshot_fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/guardian/guardian-projection-copy-v1.json")
}

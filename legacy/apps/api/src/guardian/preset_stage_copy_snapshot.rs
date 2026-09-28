use super::{
    GuardianActionKind, GuardianMode, guardian_jvm_preset_notice, guardian_jvm_preset_options,
    guardian_launch_stage_evidence_for_test, normalize_create_jvm_preset,
};
use axial_launcher::LaunchStageEvidence;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const COPY_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/guardian/guardian-preset-stage-copy-v1.json"
));
const REGENERATE_ENV: &str = "AXIAL_REGENERATE_GUARDIAN_PRESET_STAGE_COPY_SNAPSHOT";
const EXPECTED_CASE_COUNT: usize = 27;
const EXPECTED_CASE_IDS: [&str; EXPECTED_CASE_COUNT] = [
    "preset_catalog.all",
    "preset_normalization.absent",
    "preset_normalization.empty",
    "preset_normalization.whitespace",
    "preset_normalization.auto_lower",
    "preset_normalization.auto_upper",
    "preset_normalization.smooth",
    "preset_normalization.performance",
    "preset_normalization.ultra_low_latency",
    "preset_normalization.graalvm",
    "preset_normalization.legacy",
    "preset_normalization.legacy_pvp",
    "preset_normalization.legacy_heavy",
    "preset_normalization.unknown",
    "preset_normalization.hostile",
    "preset_normalization.multibyte",
    "launch_stage.allow",
    "launch_stage.warn",
    "launch_stage.repair",
    "launch_stage.retry",
    "launch_stage.strip",
    "launch_stage.downgrade",
    "launch_stage.fallback",
    "launch_stage.quarantine",
    "launch_stage.ask_user",
    "launch_stage.block",
    "launch_stage.record_only",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum SnapshotSchema {
    #[serde(rename = "axial.guardian.preset_stage_copy.v1")]
    V1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianPresetStageCopySnapshot {
    schema: SnapshotSchema,
    cases: Vec<GuardianPresetStageCopyCase>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianPresetStageCopyCase {
    id: String,
    input: GuardianPresetStageCopyInput,
    output: GuardianPresetStageCopyOutput,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "surface", rename_all = "snake_case", deny_unknown_fields)]
enum GuardianPresetStageCopyInput {
    PresetCatalog {},
    PresetNormalization {
        requested: Option<String>,
    },
    LaunchStage {
        mode: GuardianMode,
        decision: GuardianActionKind,
        diagnosis_count: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "surface", rename_all = "snake_case", deny_unknown_fields)]
enum GuardianPresetStageCopyOutput {
    PresetCatalog {
        options: Vec<GuardianJvmPresetOptionProjection>,
    },
    PresetNormalization {
        stored_preset: String,
        notice: Option<GuardianJvmPresetNoticeProjection>,
    },
    LaunchStage {
        evidence: LaunchStageEvidenceProjection,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianJvmPresetOptionProjection {
    id: String,
    label: String,
    detail: String,
    default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disabled_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianJvmPresetNoticeProjection {
    state_id: String,
    tone: String,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchStageEvidenceProjection {
    id: String,
    system: String,
    summary: String,
    details: Vec<String>,
}

impl From<LaunchStageEvidence> for LaunchStageEvidenceProjection {
    fn from(evidence: LaunchStageEvidence) -> Self {
        Self {
            id: evidence.id,
            system: evidence.system,
            summary: evidence.summary,
            details: evidence.details,
        }
    }
}

#[test]
fn checked_in_guardian_preset_stage_copy_is_byte_stable_and_complete() {
    let fixture = committed_fixture();
    assert_snapshot_coverage(&fixture);
    let replayed = replay_snapshot(&fixture);

    assert_eq!(fixture, replayed);
    assert_eq!(
        snapshot_bytes(&replayed).as_slice(),
        COPY_FIXTURE.as_bytes()
    );
    assert_public_bounds_and_privacy(&replayed);
}

#[test]
fn guardian_preset_stage_copy_rejects_unknown_and_malformed_fields() {
    let mut unknown =
        serde_json::from_str::<serde_json::Value>(COPY_FIXTURE).expect("preset/stage fixture JSON");
    unknown["cases"][0]["input"]["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<GuardianPresetStageCopySnapshot>(unknown).is_err());

    let mut malformed =
        serde_json::from_str::<serde_json::Value>(COPY_FIXTURE).expect("preset/stage fixture JSON");
    malformed["cases"][16]["input"]["decision"] = serde_json::json!("Unknown");
    assert!(serde_json::from_value::<GuardianPresetStageCopySnapshot>(malformed).is_err());
}

#[test]
#[ignore = "explicit fixture regeneration only"]
fn regenerate_guardian_preset_stage_copy_fixture() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to regenerate the Guardian preset/stage copy snapshot"
    );
    let committed = committed_fixture();
    assert_snapshot_coverage(&committed);
    let replayed = replay_snapshot(&committed);
    assert_snapshot_coverage(&replayed);
    assert_public_bounds_and_privacy(&replayed);
    std::fs::write(snapshot_fixture_path(), snapshot_bytes(&replayed))
        .expect("write regenerated Guardian preset/stage copy fixture");
}

fn committed_fixture() -> GuardianPresetStageCopySnapshot {
    serde_json::from_str(COPY_FIXTURE).expect("strict committed Guardian preset/stage copy fixture")
}

fn replay_snapshot(snapshot: &GuardianPresetStageCopySnapshot) -> GuardianPresetStageCopySnapshot {
    GuardianPresetStageCopySnapshot {
        schema: snapshot.schema,
        cases: snapshot
            .cases
            .iter()
            .map(|case| GuardianPresetStageCopyCase {
                id: case.id.clone(),
                input: case.input.clone(),
                output: render_output(&case.input),
            })
            .collect(),
    }
}

fn render_output(input: &GuardianPresetStageCopyInput) -> GuardianPresetStageCopyOutput {
    match input {
        GuardianPresetStageCopyInput::PresetCatalog {} => {
            GuardianPresetStageCopyOutput::PresetCatalog {
                options: project_serialized(guardian_jvm_preset_options()),
            }
        }
        GuardianPresetStageCopyInput::PresetNormalization { requested } => {
            let resolution = normalize_create_jvm_preset(requested.as_deref());
            GuardianPresetStageCopyOutput::PresetNormalization {
                stored_preset: resolution.stored_preset().to_string(),
                notice: guardian_jvm_preset_notice(resolution).map(project_serialized),
            }
        }
        GuardianPresetStageCopyInput::LaunchStage {
            mode,
            decision,
            diagnosis_count,
        } => GuardianPresetStageCopyOutput::LaunchStage {
            evidence: guardian_launch_stage_evidence_for_test(*mode, *decision, *diagnosis_count)
                .into(),
        },
    }
}

fn project_serialized<T: Serialize, U: DeserializeOwned>(value: T) -> U {
    let value = serde_json::to_value(value).expect("serialize Guardian copy projection");
    serde_json::from_value(value).expect("deserialize strict Guardian copy projection")
}

fn assert_snapshot_coverage(snapshot: &GuardianPresetStageCopySnapshot) {
    assert_eq!(snapshot.schema, SnapshotSchema::V1);
    assert_eq!(snapshot.cases.len(), EXPECTED_CASE_COUNT);
    for (case, expected_id) in snapshot.cases.iter().zip(EXPECTED_CASE_IDS) {
        assert_eq!(case.id, expected_id);
    }

    let expected_actions = [
        GuardianActionKind::Allow,
        GuardianActionKind::Warn,
        GuardianActionKind::Repair,
        GuardianActionKind::Retry,
        GuardianActionKind::Strip,
        GuardianActionKind::Downgrade,
        GuardianActionKind::Fallback,
        GuardianActionKind::Quarantine,
        GuardianActionKind::AskUser,
        GuardianActionKind::Block,
        GuardianActionKind::RecordOnly,
    ];
    for action in expected_actions {
        assert!(snapshot.cases.iter().any(|case| {
            matches!(
                case.input,
                GuardianPresetStageCopyInput::LaunchStage { decision, .. }
                    if decision == action
            )
        }));
    }
    for mode in [
        GuardianMode::Managed,
        GuardianMode::Custom,
        GuardianMode::Disabled,
    ] {
        assert!(snapshot.cases.iter().any(|case| {
            matches!(
                case.input,
                GuardianPresetStageCopyInput::LaunchStage { mode: candidate, .. }
                    if candidate == mode
            )
        }));
    }
}

fn assert_public_bounds_and_privacy(snapshot: &GuardianPresetStageCopySnapshot) {
    for case in &snapshot.cases {
        match &case.output {
            GuardianPresetStageCopyOutput::PresetCatalog { options } => {
                assert_eq!(options.len(), 8);
                assert!(options.iter().all(|option| {
                    !option.label.is_empty()
                        && option.label.len() <= 180
                        && !option.detail.is_empty()
                        && option.detail.len() <= 240
                }));
            }
            GuardianPresetStageCopyOutput::PresetNormalization { notice, .. } => {
                if let Some(notice) = notice {
                    assert!(!notice.message.is_empty() && notice.message.len() <= 180);
                    assert!(
                        notice
                            .detail
                            .as_ref()
                            .is_some_and(|detail| detail.len() <= 240)
                    );
                }
            }
            GuardianPresetStageCopyOutput::LaunchStage { evidence } => {
                assert!(!evidence.summary.is_empty() && evidence.summary.len() <= 160);
                assert_eq!(evidence.details.len(), 3);
                assert!(evidence.details.iter().all(|detail| detail.len() <= 120));
            }
        }
    }
    for case in &snapshot.cases {
        let encoded =
            serde_json::to_string(&case.output).expect("serialize Guardian public copy output");
        for sensitive in ["Alice", "java.exe", "accessToken", "secret"] {
            assert!(
                !encoded.contains(sensitive),
                "leaked {sensitive} in public fixture output"
            );
        }
    }
}

fn snapshot_bytes(snapshot: &GuardianPresetStageCopySnapshot) -> Vec<u8> {
    let pretty =
        serde_json::to_string_pretty(snapshot).expect("serialize Guardian preset/stage snapshot");
    format!("{pretty}\n").into_bytes()
}

fn snapshot_fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/guardian/guardian-preset-stage-copy-v1.json")
}

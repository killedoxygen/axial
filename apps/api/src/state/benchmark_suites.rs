use crate::logging::timestamp_utc;
use croopor_config::AppPaths;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;

const BENCHMARK_SUITE_SCHEMA: &str = "croopor.launch.benchmark.suite";
const BENCHMARK_SUITE_SCHEMA_VERSION: u32 = 2;
const MAX_SUITE_ID_STEM_CHARS: usize = 96;
const MAX_DERIVED_INSTANCE_ID_CHARS: usize = 40;
const MAX_MANIFEST_FIELD_CHARS: usize = 96;
const MAX_MANIFEST_RUNS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkSuiteManifest {
    pub schema: String,
    pub schema_version: u32,
    pub suite_id: String,
    pub instance_id: String,
    pub mode: String,
    pub created_at: String,
    pub updated_at: String,
    pub runs: Vec<BenchmarkSuiteManifestRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkSuiteManifestRun {
    pub run_index: usize,
    pub profile: String,
    pub run_type: String,
    pub target_id: String,
    pub benchmark_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launched_at: Option<String>,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchmarkSuiteRunInput {
    pub run_index: usize,
    pub profile: String,
    pub run_type: String,
    pub target_id: Option<String>,
    pub benchmark_id: String,
}

pub fn derive_suite_id(instance_id: &str, mode: &str) -> String {
    let safe_instance = safe_stem(instance_id, MAX_DERIVED_INSTANCE_ID_CHARS)
        .unwrap_or_else(|| "instance".to_string());
    let safe_mode =
        safe_stem(mode, MAX_DERIVED_INSTANCE_ID_CHARS).unwrap_or_else(|| "development".to_string());
    format!(
        "suite-{safe_instance}-{safe_mode}-{:016x}",
        stable_hash(&[instance_id.trim(), mode.trim()])
    )
}

pub fn normalize_suite_id(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut stem = trimmed
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() || matches!(value, '-' | '_') {
                value
            } else {
                '_'
            }
        })
        .collect::<String>();
    let changed = stem != trimmed || stem.chars().count() > MAX_SUITE_ID_STEM_CHARS;
    stem.truncate(MAX_SUITE_ID_STEM_CHARS);
    let stem = stem.trim_matches('_');
    if stem.is_empty() {
        return Some(format!("suite-{:016x}", stable_hash(&[trimmed])));
    }
    if !changed {
        return Some(stem.to_string());
    }

    let hash_suffix = format!("{:016x}", stable_hash(&[trimmed]));
    let max_prefix = MAX_SUITE_ID_STEM_CHARS
        .saturating_sub(hash_suffix.len())
        .saturating_sub(1);
    let mut prefix = stem.chars().take(max_prefix).collect::<String>();
    prefix = prefix.trim_matches('_').to_string();
    if prefix.is_empty() {
        Some(format!("suite-{hash_suffix}"))
    } else {
        Some(format!("{prefix}-{hash_suffix}"))
    }
}

pub fn suite_path(paths: &AppPaths, suite_id: &str) -> PathBuf {
    suite_dir(paths).join(format!(
        "{}.json",
        normalize_suite_id(suite_id).unwrap_or_else(|| "suite".to_string())
    ))
}

pub fn load(paths: &AppPaths, suite_id: &str) -> io::Result<Option<BenchmarkSuiteManifest>> {
    let path = suite_path(paths, suite_id);
    match load_file(path) {
        Ok(manifest) => Ok(Some(manifest)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn next_pending_run_index(
    manifest: Option<&BenchmarkSuiteManifest>,
    planned_run_count: usize,
) -> Option<usize> {
    let Some(manifest) = manifest else {
        return (planned_run_count > 0).then_some(0);
    };

    (0..planned_run_count).find(|run_index| {
        manifest
            .runs
            .iter()
            .find(|run| run.run_index == *run_index)
            .and_then(|run| run.session_id.as_ref())
            .is_none()
    })
}

#[allow(clippy::too_many_arguments)]
pub fn persist_launched_run(
    paths: &AppPaths,
    suite_id: &str,
    instance_id: &str,
    mode: &str,
    plan: &[BenchmarkSuiteRunInput],
    selected_run_index: usize,
    session_id: &str,
    launched_at: &str,
) -> io::Result<BenchmarkSuiteManifest> {
    let suite_id =
        normalize_suite_id(suite_id).unwrap_or_else(|| derive_suite_id(instance_id, mode));
    let now = timestamp_utc();
    let mut manifest = load(paths, &suite_id)?.unwrap_or_else(|| BenchmarkSuiteManifest {
        schema: BENCHMARK_SUITE_SCHEMA.to_string(),
        schema_version: BENCHMARK_SUITE_SCHEMA_VERSION,
        suite_id: suite_id.clone(),
        instance_id: safe_manifest_field(instance_id).unwrap_or_else(|| "instance".to_string()),
        mode: safe_manifest_field(mode).unwrap_or_else(|| "development".to_string()),
        created_at: now.clone(),
        updated_at: now.clone(),
        runs: Vec::new(),
    });

    manifest.schema = BENCHMARK_SUITE_SCHEMA.to_string();
    manifest.schema_version = BENCHMARK_SUITE_SCHEMA_VERSION;
    manifest.suite_id = suite_id.clone();
    manifest.instance_id =
        safe_manifest_field(instance_id).unwrap_or_else(|| "instance".to_string());
    manifest.mode = safe_manifest_field(mode).unwrap_or_else(|| "development".to_string());
    manifest.updated_at = now;

    for run in plan.iter().take(MAX_MANIFEST_RUNS) {
        upsert_plan_run(&mut manifest.runs, run);
    }
    if let Some(selected) = plan.iter().find(|run| run.run_index == selected_run_index) {
        upsert_launched_run(
            &mut manifest.runs,
            selected,
            safe_manifest_field(session_id),
            safe_manifest_timestamp(launched_at),
        );
    }

    manifest.runs.sort_by_key(|run| run.run_index);
    manifest.runs.truncate(MAX_MANIFEST_RUNS);

    let path = suite_path(paths, &suite_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(&manifest)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(path, data)?;
    Ok(manifest)
}

pub fn update_run_state_for_session(
    paths: &AppPaths,
    launch_session_id: &str,
    outcome: &str,
) -> io::Result<()> {
    let Some(safe_session_id) = safe_manifest_field(launch_session_id) else {
        return Ok(());
    };
    let state = safe_manifest_run_state(outcome);
    let dir = suite_dir(paths);
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }

        let mut manifest = match load_file(path.clone()) {
            Ok(manifest) => manifest,
            Err(_) => continue,
        };
        let mut matched = false;
        for run in &mut manifest.runs {
            let Some(run_session_id) = run.session_id.as_deref() else {
                continue;
            };
            if run_session_id == launch_session_id || run_session_id == safe_session_id {
                matched = true;
                if run.state != state {
                    run.state = state.clone();
                }
            }
        }
        if !matched {
            continue;
        }

        manifest.updated_at = timestamp_utc();
        let data = serde_json::to_string_pretty(&manifest)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(path, data)?;
    }

    Ok(())
}

fn upsert_plan_run(runs: &mut Vec<BenchmarkSuiteManifestRun>, run: &BenchmarkSuiteRunInput) {
    let target_id = run
        .target_id
        .as_deref()
        .and_then(safe_manifest_field)
        .unwrap_or_default();
    if let Some(existing) = runs
        .iter_mut()
        .find(|existing| existing.run_index == run.run_index)
    {
        existing.profile = safe_manifest_field(&run.profile).unwrap_or_default();
        existing.run_type = safe_manifest_field(&run.run_type).unwrap_or_default();
        existing.target_id = target_id;
        existing.benchmark_id = safe_manifest_field(&run.benchmark_id).unwrap_or_default();
        if existing.state.trim().is_empty() {
            existing.state = "pending".to_string();
        }
        return;
    }

    runs.push(BenchmarkSuiteManifestRun {
        run_index: run.run_index,
        profile: safe_manifest_field(&run.profile).unwrap_or_default(),
        run_type: safe_manifest_field(&run.run_type).unwrap_or_default(),
        target_id,
        benchmark_id: safe_manifest_field(&run.benchmark_id).unwrap_or_default(),
        session_id: None,
        launched_at: None,
        state: "pending".to_string(),
    });
}

fn upsert_launched_run(
    runs: &mut Vec<BenchmarkSuiteManifestRun>,
    run: &BenchmarkSuiteRunInput,
    session_id: Option<String>,
    launched_at: Option<String>,
) {
    upsert_plan_run(runs, run);
    if let Some(existing) = runs
        .iter_mut()
        .find(|existing| existing.run_index == run.run_index)
    {
        existing.session_id = session_id;
        existing.launched_at = launched_at;
        existing.state = "launching".to_string();
    }
}

fn load_file(path: PathBuf) -> io::Result<BenchmarkSuiteManifest> {
    let data = fs::read_to_string(path)?;
    let manifest: BenchmarkSuiteManifest = serde_json::from_str(&data)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    validate_manifest_schema(&manifest)?;
    Ok(manifest)
}

fn suite_dir(paths: &AppPaths) -> PathBuf {
    paths.config_dir.join("benchmarks").join("suites")
}

fn validate_manifest_schema(manifest: &BenchmarkSuiteManifest) -> io::Result<()> {
    if manifest.schema != BENCHMARK_SUITE_SCHEMA {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "benchmark suite manifest schema is not supported",
        ));
    }
    if manifest.schema_version != BENCHMARK_SUITE_SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "benchmark suite manifest schema version is not supported",
        ));
    }

    Ok(())
}

fn safe_stem(value: &str, max_chars: usize) -> Option<String> {
    let mut stem = value
        .trim()
        .chars()
        .filter(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
        .take(max_chars)
        .collect::<String>();
    stem = stem.trim_matches('_').to_string();
    (!stem.is_empty()).then_some(stem)
}

fn safe_manifest_field(value: &str) -> Option<String> {
    let value = value
        .trim()
        .chars()
        .filter(|value| {
            !value.is_control() && *value != '/' && *value != '\\' && *value != ':' && *value != ';'
        })
        .take(MAX_MANIFEST_FIELD_CHARS)
        .collect::<String>();
    (!value.is_empty()).then_some(value)
}

fn safe_manifest_timestamp(value: &str) -> Option<String> {
    let value = value
        .trim()
        .chars()
        .filter(|value| !value.is_control() && *value != '/' && *value != '\\')
        .take(MAX_MANIFEST_FIELD_CHARS)
        .collect::<String>();
    (!value.is_empty()).then_some(value)
}

fn safe_manifest_run_state(value: &str) -> String {
    let value = value
        .trim()
        .chars()
        .take_while(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
        .take(MAX_MANIFEST_FIELD_CHARS)
        .collect::<String>()
        .trim_matches(['-', '_'])
        .to_ascii_lowercase();
    if value.is_empty() {
        "unknown".to_string()
    } else {
        value
    }
}

fn stable_hash(parts: &[&str]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use croopor_config::AppPaths;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn launch_suite_path_sanitizes_id_to_config_subdirectory() {
        let root = test_root("suite-safe-path");
        let paths = test_paths(&root);

        let path = suite_path(&paths, "../bad/suite\\id:?");

        assert_eq!(path.parent(), Some(suite_dir(&paths).as_path()));
        assert!(path.starts_with(paths.config_dir.join("benchmarks").join("suites")));
        assert_eq!(
            path.extension().and_then(|value| value.to_str()),
            Some("json")
        );
        assert!(
            path.file_name()
                .and_then(|value| value.to_str())
                .expect("filename")
                .chars()
                .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.'))
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn persist_launched_run_creates_and_updates_manifest() {
        let root = test_root("suite-create-update");
        let paths = test_paths(&root);
        let suite_id = "suite-dev";
        let plan = vec![
            run_input(0, "vanilla_baseline", "coldish"),
            run_input(1, "managed_default", "repeat"),
        ];

        let first = persist_launched_run(
            &paths,
            suite_id,
            "instance",
            "development",
            &plan,
            0,
            "session-1",
            "2026-01-01T00:00:00.000Z",
        )
        .expect("persist first run");
        let second = persist_launched_run(
            &paths,
            suite_id,
            "instance",
            "development",
            &plan,
            1,
            "session-2",
            "2026-01-01T00:01:00.000Z",
        )
        .expect("persist second run");

        assert_eq!(first.runs.len(), 2);
        assert_eq!(first.runs[0].state, "launching");
        assert_eq!(first.runs[0].session_id.as_deref(), Some("session-1"));
        assert_eq!(
            first.runs[0].launched_at.as_deref(),
            Some("2026-01-01T00:00:00.000Z")
        );
        assert_eq!(first.runs[1].state, "pending");
        assert_eq!(second.created_at, first.created_at);
        assert_eq!(second.runs.len(), 2);
        assert_eq!(second.runs[0].session_id.as_deref(), Some("session-1"));
        assert_eq!(second.runs[0].state, "launching");
        assert_eq!(second.runs[1].session_id.as_deref(), Some("session-2"));
        assert_eq!(second.runs[1].state, "launching");

        let loaded = load(&paths, suite_id)
            .expect("load suite")
            .expect("suite should exist");
        assert_eq!(loaded, second);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_suite_manifest_without_runs_is_invalid() {
        let root = test_root("suite-missing-runs");
        let paths = test_paths(&root);
        let suite_id = "suite-missing-runs";
        let path = suite_path(&paths, suite_id);
        fs::create_dir_all(path.parent().expect("suite parent")).expect("create suite dir");
        fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": BENCHMARK_SUITE_SCHEMA,
                "schema_version": BENCHMARK_SUITE_SCHEMA_VERSION,
                "suite_id": suite_id,
                "instance_id": "instance",
                "mode": "development",
                "created_at": "2026-01-01T00:00:00.000Z",
                "updated_at": "2026-01-01T00:00:00.000Z"
            }))
            .expect("serialize manifest"),
        )
        .expect("write manifest");

        let error = load(&paths, suite_id).expect_err("missing runs should be invalid");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_suite_manifest_with_unknown_fields_is_invalid() {
        let root = test_root("suite-unknown-field");
        let paths = test_paths(&root);
        let suite_id = "suite-unknown-field";
        let path = suite_path(&paths, suite_id);
        fs::create_dir_all(path.parent().expect("suite parent")).expect("create suite dir");
        fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": BENCHMARK_SUITE_SCHEMA,
                "schema_version": BENCHMARK_SUITE_SCHEMA_VERSION,
                "suite_id": suite_id,
                "instance_id": "instance",
                "mode": "development",
                "created_at": "2026-01-01T00:00:00.000Z",
                "updated_at": "2026-01-01T00:00:00.000Z",
                "runs": [{
                    "run_index": 0,
                    "profile": "vanilla_baseline",
                    "run_type": "coldish",
                    "target_id": "",
                    "benchmark_id": "suite-development-00-vanilla_baseline-coldish",
                    "state": "pending",
                    "unexpected_state": true
                }]
            }))
            .expect("serialize manifest"),
        )
        .expect("write manifest");

        let error = load(&paths, suite_id).expect_err("unknown run field should be invalid");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_suite_manifest_excludes_sensitive_fields_and_path_shapes() {
        let root = test_root("suite-sensitive");
        let paths = test_paths(&root);
        let plan = vec![BenchmarkSuiteRunInput {
            run_index: 0,
            profile: "vanilla_baseline".to_string(),
            run_type: "coldish".to_string(),
            target_id: Some("family_c_forge_1_12_2_vanilla_baseline/C:/runtime".to_string()),
            benchmark_id: "suite-development-00-vanilla_baseline-coldish".to_string(),
        }];

        let manifest = persist_launched_run(
            &paths,
            "../suite",
            "instance/C:/Users/Secret",
            "development",
            &plan,
            0,
            "session\\secret",
            "2026-01-01T00:00:00.000Z",
        )
        .expect("persist suite");
        let data = serde_json::to_string(&manifest).expect("serialize manifest");
        let lower_data = data.to_ascii_lowercase();

        assert!(!data.contains('/'));
        assert!(!data.contains('\\'));
        assert!(!data.contains("SecretUser"));
        assert!(!lower_data.contains("java_path"));
        assert!(!lower_data.contains("command"));
        assert!(!lower_data.contains("jvm"));
        assert!(!lower_data.contains("username"));
        assert!(!lower_data.contains("filesystem"));
        assert!(!lower_data.contains("args"));
        assert_eq!(
            manifest.runs[0].target_id,
            "family_c_forge_1_12_2_vanilla_baselineCruntime"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn update_run_state_for_session_updates_matching_run_only() {
        let root = test_root("suite-update-state");
        let paths = test_paths(&root);
        let suite_id = "suite-dev";
        let plan = vec![
            run_input(0, "vanilla_baseline", "coldish"),
            run_input(1, "managed_default", "repeat"),
        ];
        persist_launched_run(
            &paths,
            suite_id,
            "instance",
            "development",
            &plan,
            0,
            "session-1",
            "2026-01-01T00:00:00.000Z",
        )
        .expect("persist first run");
        persist_launched_run(
            &paths,
            suite_id,
            "instance",
            "development",
            &plan,
            1,
            "session-2",
            "2026-01-01T00:01:00.000Z",
        )
        .expect("persist second run");

        update_run_state_for_session(&paths, "session-1", "running").expect("update state");

        let loaded = load(&paths, suite_id)
            .expect("load suite")
            .expect("suite should exist");
        assert_eq!(loaded.runs[0].session_id.as_deref(), Some("session-1"));
        assert_eq!(loaded.runs[0].state, "running");
        assert_eq!(loaded.runs[1].session_id.as_deref(), Some("session-2"));
        assert_eq!(loaded.runs[1].state, "launching");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn update_run_state_for_session_ignores_missing_session() {
        let root = test_root("suite-update-missing-session");
        let paths = test_paths(&root);
        let suite_id = "suite-dev";
        let plan = vec![run_input(0, "vanilla_baseline", "coldish")];
        let original = persist_launched_run(
            &paths,
            suite_id,
            "instance",
            "development",
            &plan,
            0,
            "session-1",
            "2026-01-01T00:00:00.000Z",
        )
        .expect("persist run");

        update_run_state_for_session(&paths, "missing-session", "running")
            .expect("missing session is ok");

        let loaded = load(&paths, suite_id)
            .expect("load suite")
            .expect("suite should exist");
        assert_eq!(loaded, original);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn update_run_state_for_session_sanitizes_and_bounds_outcome() {
        let root = test_root("suite-update-state-safe");
        let paths = test_paths(&root);
        let suite_id = "suite-dev";
        let plan = vec![run_input(0, "vanilla_baseline", "coldish")];
        persist_launched_run(
            &paths,
            suite_id,
            "instance",
            "development",
            &plan,
            0,
            "session-1",
            "2026-01-01T00:00:00.000Z",
        )
        .expect("persist run");

        update_run_state_for_session(
            &paths,
            "session-1",
            "failed/C:/Users/Secret/.minecraft --jvm-args -Duser.name=Secret",
        )
        .expect("update state");

        let loaded = load(&paths, suite_id)
            .expect("load suite")
            .expect("suite should exist");
        assert_eq!(loaded.runs[0].state, "failed");
        assert!(loaded.runs[0].state.len() <= MAX_MANIFEST_FIELD_CHARS);
        assert!(!loaded.runs[0].state.contains('/'));
        assert!(!loaded.runs[0].state.contains('\\'));
        assert!(!loaded.runs[0].state.contains(':'));
        assert!(!loaded.runs[0].state.contains(';'));
        assert!(!loaded.runs[0].state.contains("Secret"));

        let long_outcome = "x".repeat(MAX_MANIFEST_FIELD_CHARS + 16);
        update_run_state_for_session(&paths, "session-1", &long_outcome).expect("update state");
        let loaded = load(&paths, suite_id)
            .expect("load suite")
            .expect("suite should exist");
        assert_eq!(loaded.runs[0].state.len(), MAX_MANIFEST_FIELD_CHARS);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_suite_derived_suite_id_is_deterministic_and_bounded() {
        let first = derive_suite_id("instance", "development");
        let second = derive_suite_id("instance", "development");

        assert_eq!(first, second);
        assert!(first.len() <= MAX_SUITE_ID_STEM_CHARS);
        assert!(first.starts_with("suite-instance-development-"));
    }

    fn run_input(run_index: usize, profile: &str, run_type: &str) -> BenchmarkSuiteRunInput {
        BenchmarkSuiteRunInput {
            run_index,
            profile: profile.to_string(),
            run_type: run_type.to_string(),
            target_id: None,
            benchmark_id: format!("suite-development-{run_index:02}-{profile}-{run_type}"),
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("croopor-{name}-{nanos}"))
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: config_dir.join("instances"),
            music_dir: config_dir.join("music"),
            library_dir: config_dir.join("library"),
            config_dir,
        }
    }
}

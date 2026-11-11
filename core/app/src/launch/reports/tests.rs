use super::*;

fn session_report() -> LaunchProofRecord {
    let mut outcome = SessionOutcome {
        kind: SessionOutcomeKind::Clean,
        reason: super::super::outcome::SessionExitReason::CleanExit,
        failure_class: None,
        summary: String::new(),
    };
    outcome.summary = outcome.summary().to_owned();
    let mut report = LaunchProofRecord::from_session(SessionReportInput {
        session_id: uuid::Uuid::new_v4().to_string(),
        instance_id: crate::instances::model::InstanceId::new().to_string(),
        version_id: "1.21.1".into(),
        launched_at: "2026-01-01T00:00:00.000Z".into(),
        ended_at: "2026-01-01T00:00:02.000Z".into(),
        outcome,
        entries: Vec::new(),
        exit_code: Some(0),
        boot_duration_ms: Some(1000),
        logs_dropped: 0,
    });
    report.scenario.performance_mode = "vanilla".into();
    report.scenario.scenario_id = "vanilla_launch".into();
    report.scenario.version_id = Some("1.21.1".into());
    report.device.tier = "mid".into();
    report.comparison = Some(LaunchProofComparison {
        baseline_session_id: uuid::Uuid::new_v4().to_string(),
        baseline_recorded_at: "2025-12-31T00:00:00.000Z".into(),
        baseline: LaunchProofComparisonBaseline {
            performance_mode: "vanilla".into(),
            version_id: "1.21.1".into(),
            requested_memory_mb: None,
            device_tier: "mid".into(),
            benchmark_profile: None,
            benchmark_run_type: None,
            benchmark_mode: None,
        },
        matched_sample_count: 3,
        metric_name: "boot_duration_ms".into(),
        current_value_ms: 1000,
        baseline_value_ms: 2000,
        delta_ms: -1000,
        delta_percent: -50.0,
    });
    report
}

#[test]
fn device_uses_the_lower_observed_tier_without_fallbacks() {
    let mut budget: LaunchProofResourceBudget = serde_json::from_value(serde_json::json!({
        "active_session_count":0, "active_install_count":0, "active_memory_allocation_mb":0,
        "memory_headroom_mb":2048, "memory_pressure":false, "cpu_pressure":false,
        "install_pressure":false, "launch_disk_headroom_mb":2048, "disk_pressure":false
    }))
    .unwrap();
    for (threads, memory, tier) in [
        (None, None, "unknown"),
        (Some(0), Some(0), "unknown"),
        (Some(4), None, "low"),
        (Some(5), None, "mid"),
        (Some(7), None, "mid"),
        (Some(8), None, "high"),
        (None, Some(8192), "low"),
        (None, Some(8193), "mid"),
        (None, Some(32767), "mid"),
        (None, Some(32768), "high"),
        (Some(8), Some(8192), "low"),
        (Some(4), Some(32768), "low"),
        (Some(8), Some(18432), "mid"),
    ] {
        budget.host_cpu_threads = threads;
        budget.host_total_memory_mb = memory;
        let device = LaunchProofDevice::from_budget(&budget);
        assert_eq!(device.tier, tier);
        assert_eq!(device.cpu_threads, threads.filter(|value| *value > 0));
        assert_eq!(device.total_memory_mb, memory.filter(|value| *value > 0));
        let wire = serde_json::to_value(&device).unwrap();
        assert_eq!(
            wire.get("cpu_threads").is_some(),
            device.cpu_threads.is_some()
        );
        assert_eq!(
            wire.get("total_memory_mb").is_some(),
            device.total_memory_mb.is_some()
        );
    }
}

#[test]
fn canonical_loader_versions_survive_reports_but_not_unstructured_logs() {
    use axial_minecraft::loaders::{LoaderComponentId, installed_version_id_for};
    // The identity codec, not the generic 96-byte field bound, limits length.
    let long = "x".repeat(100);
    for (component, loader) in [
        (LoaderComponentId::Fabric, "0.19.5"),
        (LoaderComponentId::Quilt, "0.19.5"),
        (LoaderComponentId::Forge, "0.19.5"),
        (LoaderComponentId::NeoForge, "0.19.5"),
        (LoaderComponentId::Fabric, long.as_str()),
    ] {
        let version = installed_version_id_for(component, "1.20.1", loader).unwrap();
        let mut report = session_report();
        report.version_id = version.clone();
        report.scenario.version_id = Some(version.clone());
        report.comparison.as_mut().unwrap().baseline.version_id = version.clone();
        report.logs.push(LogEntry {
            sequence: 1,
            source: super::super::logs::LogStream::Stdout,
            text: version.clone(),
            truncated: false,
        });
        let encoded = encode_report(&report).unwrap();
        let decoded = decode_report(&encoded).unwrap();
        assert_eq!(decoded.version_id, version);
        assert_eq!(
            decoded.scenario.version_id.as_deref(),
            Some(version.as_str())
        );
        assert_eq!(
            decoded.comparison.as_ref().unwrap().baseline.version_id,
            version
        );
        assert_eq!(decoded.logs[0].text, super::super::logs::REDACTED_LINE);
        assert_eq!(encode_report(&decoded).unwrap(), encoded);
        let metadata = Arc::new(MetadataStore::in_memory().unwrap());
        let store = LaunchReportStore::new(metadata).unwrap();
        store
            .record(decoded.clone(), &Redactor::new(vec![]))
            .unwrap();
        let saved = store.get(&decoded.session_id).unwrap().unwrap();
        assert_eq!(saved.version_id, version);
        assert_eq!(saved.scenario.version_id.as_deref(), Some(version.as_str()));
        let mut next = decoded;
        next.session_id = uuid::Uuid::new_v4().to_string();
        next.recorded_at = "2026-01-01T00:00:03.000Z".into();
        next.boot_duration_ms = Some(500);
        store.record(next.clone(), &Redactor::new(vec![])).unwrap();
        let saved = store.get(&next.session_id).unwrap().unwrap();
        assert_eq!(saved.comparison.unwrap().baseline.version_id, version);
    }
}

#[test]
fn typed_loader_versions_reject_malformed_and_encoded_sensitive_values() {
    use axial_minecraft::loaders::{LoaderComponentId, installed_version_id_for};
    let canonical =
        installed_version_id_for(LoaderComponentId::Fabric, "1.20.1", "0.19.5").unwrap();
    let cases = [
        ("loader-v2-bad".to_owned(), vec![]),
        (format!("{canonical}="), vec![]),
        ("abc123".repeat(12), vec![]),
        (canonical.clone(), vec![canonical.clone()]),
        (canonical.clone(), vec![canonical[10..25].to_owned()]),
        (canonical.clone(), vec!["1.20.1".to_owned()]),
        (canonical.clone(), vec!["19.5".to_owned()]),
        (
            installed_version_id_for(LoaderComponentId::Fabric, "1.20.1", "raw-secret-token")
                .unwrap(),
            vec![],
        ),
        (
            installed_version_id_for(LoaderComponentId::Fabric, "1.20.1", "/private/credential")
                .unwrap(),
            vec![],
        ),
    ];
    for (version, secrets) in cases {
        let redactor = Redactor::new(secrets);
        let mut report = session_report();
        report.comparison = None;
        report.version_id = version.clone();
        report.scenario.version_id = Some(version.clone());
        sanitize_report(&mut report, &redactor).unwrap();
        assert_eq!(report.version_id, "unknown");
        assert!(report.scenario.version_id.is_none());
        let mut comparison = session_report();
        comparison.comparison.as_mut().unwrap().baseline.version_id = version;
        assert!(sanitize_report(&mut comparison, &redactor).is_err());
    }
}

#[test]
fn session_report_reopen_preserves_its_committed_comparison() {
    let temporary_parent = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    let root = tempfile::tempdir_in(temporary_parent).unwrap();
    let path = root.path().join("metadata.sqlite");
    let metadata = Arc::new(MetadataStore::open(&path).unwrap());
    let store = LaunchReportStore::new(metadata.clone()).unwrap();
    let redactor = Redactor::new(vec![]);
    let baseline = session_report();
    store.record(baseline.clone(), &redactor).unwrap();
    let mut report = baseline.clone();
    report.session_id = uuid::Uuid::new_v4().to_string();
    report.recorded_at = "2026-01-01T00:00:03.000Z".into();
    report.boot_duration_ms = Some(500);
    store.record(report.clone(), &redactor).unwrap();
    let saved = store.get(&report.session_id).unwrap().unwrap();
    let comparison = saved.comparison.as_ref().unwrap();
    assert_eq!(comparison.baseline_session_id, baseline.session_id);
    assert_eq!(comparison.matched_sample_count, 1);
    assert_eq!(comparison.current_value_ms, 500);
    assert_eq!(comparison.baseline_value_ms, 1000);
    let mut later = report.clone();
    later.session_id = uuid::Uuid::new_v4().to_string();
    later.recorded_at = "2026-01-01T00:00:04.000Z".into();
    store.record(later, &redactor).unwrap();
    drop(store);
    drop(metadata);

    let metadata = Arc::new(MetadataStore::open(&path).unwrap());
    let store = LaunchReportStore::new(metadata).unwrap();
    store.record(report.clone(), &redactor).unwrap();
    assert_eq!(store.get(&report.session_id).unwrap(), Some(saved.clone()));
    assert_eq!(store.list_recent(25).unwrap()[1], saved);
    report.exit_code = Some(1);
    assert!(matches!(
        store.record(report.clone(), &redactor),
        Err(ReportError::ConflictingSession)
    ));
    assert_eq!(store.get(&report.session_id).unwrap(), Some(saved));
}

#[test]
fn release_managed_report_compares_to_planned_vanilla_baseline_after_reopen() {
    use crate::performance::benchmarks::{
        benchmark_suite_manifest_run_inputs, benchmark_suite_plan,
    };

    let temporary_parent = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    let root = tempfile::tempdir_in(temporary_parent).unwrap();
    let path = root.path().join("metadata.sqlite");
    let metadata = Arc::new(MetadataStore::open(&path).unwrap());
    let store = LaunchReportStore::new(metadata.clone()).unwrap();
    let redactor = Redactor::new(vec![]);
    let mode = "release_validation";
    let plan = benchmark_suite_plan(mode).unwrap();
    let runs = benchmark_suite_manifest_run_inputs(mode, &plan);
    assert_eq!(runs[0].profile, "vanilla_baseline");
    assert_eq!(runs[1].profile, "managed_default");
    let version = axial_minecraft::loaders::installed_version_id_for(
        axial_minecraft::loaders::LoaderComponentId::Forge,
        "1.12.2",
        "14.23.5.2860",
    )
    .unwrap();
    let instance_id = crate::instances::model::InstanceId::new().to_string();
    let mut reports = Vec::new();
    for (index, (performance_mode, launched_at, recorded_at, boot_ms)) in [
        (
            "vanilla",
            "2026-01-01T00:00:00.000Z",
            "2026-01-01T00:00:02.000Z",
            1000,
        ),
        (
            "managed",
            "2026-01-01T00:00:03.000Z",
            "2026-01-01T00:00:04.000Z",
            500,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(runs[index].run_type, "coldish");
        let mut report = session_report();
        report.instance_id = instance_id.clone();
        report.version_id = version.clone();
        report.launched_at = launched_at.into();
        report.recorded_at = recorded_at.into();
        report.boot_duration_ms = Some(boot_ms);
        report.comparison = None;
        report.scenario.version_id = Some(version.clone());
        report.scenario.performance_mode = performance_mode.into();
        report.scenario.requested_memory_mb = Some(2048);
        report.scenario.benchmark_profile = Some(runs[index].profile.clone());
        report.scenario.benchmark_run_type = Some(runs[index].run_type.clone());
        report.scenario.benchmark_mode = Some(mode.into());
        report.scenario.benchmark_id = Some(runs[index].benchmark_id.clone());
        report.device.tier = "mid".into();
        store.record(report.clone(), &redactor).unwrap();
        reports.push(report);
    }
    let baseline = store.get(&reports[0].session_id).unwrap().unwrap();
    let managed = store.get(&reports[1].session_id).unwrap().unwrap();
    assert!(baseline.comparison.is_none());
    assert_eq!(baseline.outcome, "exited");
    assert_eq!(managed.outcome, "exited");
    let comparison = managed
        .comparison
        .as_ref()
        .expect("the planned Managed run must bind its Vanilla baseline");
    assert_eq!(comparison.baseline_session_id, baseline.session_id);
    assert_eq!(comparison.baseline_recorded_at, baseline.recorded_at);
    assert_eq!(comparison.baseline.performance_mode, "vanilla");
    assert_eq!(
        comparison.baseline.benchmark_profile.as_deref(),
        Some("vanilla_baseline")
    );
    assert_eq!(comparison.matched_sample_count, 1);
    assert_eq!(comparison.current_value_ms, 500);
    assert_eq!(comparison.baseline_value_ms, 1000);
    for dimension in [
        "profile",
        "baseline_profile",
        "mode",
        "suite",
        "no_suite",
        "run_type",
        "version",
        "memory",
        "tier",
        "outcome",
        "baseline_outcome",
        "order",
    ] {
        let mut current = managed.clone();
        let mut prior = baseline.clone();
        match dimension {
            "profile" => current.scenario.benchmark_profile = Some("custom_profile".into()),
            "baseline_profile" => prior.scenario.benchmark_profile = Some("other_baseline".into()),
            "mode" => current.scenario.performance_mode = "custom".into(),
            "suite" | "no_suite" => {
                let mode = (dimension == "suite").then(|| "development".into());
                current.scenario.benchmark_mode = mode.clone();
                prior.scenario.benchmark_mode = mode;
            }
            "run_type" => current.scenario.benchmark_run_type = Some("repeat".into()),
            "version" => {
                current.version_id = "1.21.1".into();
                current.scenario.version_id = Some("1.21.1".into());
            }
            "memory" => current.scenario.requested_memory_mb = Some(4096),
            "tier" => current.device.tier = "unknown".into(),
            "outcome" | "baseline_outcome" => {
                let outcome = if dimension == "outcome" {
                    &mut current.session_outcome
                } else {
                    &mut prior.session_outcome
                };
                outcome.kind = SessionOutcomeKind::Stopped;
                outcome.reason = super::super::outcome::SessionExitReason::LauncherStopped;
            }
            "order" => {
                current.launched_at = prior.launched_at.clone();
                current.recorded_at = prior.recorded_at.clone();
            }
            _ => unreachable!(),
        }
        if dimension == "order" {
            current.session_id = "a".into();
            prior.session_id = "b".into();
        }
        let isolated =
            LaunchReportStore::new(Arc::new(MetadataStore::in_memory().unwrap())).unwrap();
        isolated.record(prior, &redactor).unwrap();
        isolated.record(current.clone(), &redactor).unwrap();
        assert!(
            isolated
                .get(&current.session_id)
                .unwrap()
                .unwrap()
                .comparison
                .is_none(),
            "{dimension}"
        );
    }
    drop(store);
    drop(metadata);

    let store = LaunchReportStore::new(Arc::new(MetadataStore::open(&path).unwrap())).unwrap();
    store.record(reports[1].clone(), &redactor).unwrap();
    assert_eq!(
        store.get(&managed.session_id).unwrap(),
        Some(managed.clone())
    );
    assert_eq!(store.list_recent(25).unwrap(), vec![managed, baseline]);
}

#[test]
fn comparison_reports_with_fractional_percentages_survive_reopen() {
    let temporary_parent = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    let root = tempfile::tempdir_in(temporary_parent).unwrap();
    let redactor = Redactor::new(vec![]);
    for (baseline_ms, current_ms, delta_ms) in [(101, 99, -2), (99, 101, 2), (101, 101, 0)] {
        let path = root
            .path()
            .join(format!("{baseline_ms}-{current_ms}.sqlite"));
        let store = LaunchReportStore::new(Arc::new(MetadataStore::open(&path).unwrap())).unwrap();
        let mut baseline = session_report();
        baseline.comparison = None;
        baseline.boot_duration_ms = Some(baseline_ms);
        baseline.scenario.requested_memory_mb = Some(2048);
        baseline.scenario.benchmark_profile = Some("vanilla_baseline".into());
        baseline.scenario.benchmark_run_type = Some("coldish".into());
        baseline.scenario.benchmark_mode = Some("release_validation".into());
        store.record(baseline.clone(), &redactor).unwrap();
        assert_eq!(
            store.get(&baseline.session_id).unwrap(),
            Some(baseline.clone())
        );

        let mut managed = baseline.clone();
        managed.session_id = uuid::Uuid::new_v4().to_string();
        managed.launched_at = "2026-01-01T00:00:03.000Z".into();
        managed.recorded_at = "2026-01-01T00:00:04.000Z".into();
        managed.boot_duration_ms = Some(current_ms);
        managed.scenario.performance_mode = "managed".into();
        managed.scenario.scenario_id = "managed_launch".into();
        managed.scenario.benchmark_profile = Some("managed_default".into());
        let result = store.record(managed.clone(), &redactor);
        assert!(
            result.is_ok(),
            "baseline={baseline_ms}ms current={current_ms}ms: {result:?}"
        );
        let saved = store.get(&managed.session_id).unwrap().unwrap();
        let comparison = saved.comparison.as_ref().unwrap();
        assert_eq!(comparison.baseline_session_id, baseline.session_id);
        assert_eq!(comparison.baseline_value_ms, baseline_ms);
        assert_eq!(comparison.current_value_ms, current_ms);
        assert_eq!(comparison.delta_ms, delta_ms);
        assert_eq!(comparison.matched_sample_count, 1);
        assert!(comparison.delta_percent.is_finite());
        assert_eq!(
            store.get(&baseline.session_id).unwrap(),
            Some(baseline.clone())
        );
        drop(store);

        let reopened =
            LaunchReportStore::new(Arc::new(MetadataStore::open(&path).unwrap())).unwrap();
        assert_eq!(
            reopened.get(&managed.session_id).unwrap(),
            Some(saved.clone())
        );
        reopened.record(managed, &redactor).unwrap();
        assert_eq!(reopened.list_recent(25).unwrap(), vec![saved, baseline]);
    }
}

#[test]
fn session_report_retry_rejects_misindexed_or_noncanonical_storage() {
    for corruption in ["instance", "recorded_at", "payload"] {
        let metadata = Arc::new(MetadataStore::in_memory().unwrap());
        let store = LaunchReportStore::new(metadata.clone()).unwrap();
        let redactor = Redactor::new(vec![]);
        let report = session_report();
        store.record(report.clone(), &redactor).unwrap();
        metadata
            .transaction(|tx| -> Result<(), ReportError> {
                match corruption {
                    "instance" => {
                        tx.execute("UPDATE launch_reports SET instance_id='different'", [])?;
                    }
                    "recorded_at" => {
                        tx.execute(
                            "UPDATE launch_reports SET recorded_at='2026-01-02T00:00:02.000Z'",
                            [],
                        )?;
                    }
                    "payload" => {
                        let mut saved = stored_report(tx, &report.session_id)?.unwrap();
                        saved.session_outcome.summary = "Invented summary".into();
                        tx.execute(
                            "UPDATE launch_reports SET payload=?1",
                            [serde_json::to_vec(&saved).unwrap()],
                        )?;
                    }
                    _ => unreachable!(),
                }
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            store.get(&report.session_id),
            Err(ReportError::Invalid)
        ));
        assert!(matches!(
            store.record(report, &redactor),
            Err(ReportError::Invalid)
        ));
    }
}

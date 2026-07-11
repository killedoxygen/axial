use crate::guardian::GuardianDecisionKind;
use std::time::Duration;

pub(crate) fn ms(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

pub(crate) struct InstancesListTiming {
    pub total: Duration,
    pub scan: Duration,
    pub enrich: Duration,
    pub version_count: usize,
    pub instance_count: usize,
    pub degraded: bool,
    pub scan_source: &'static str,
    pub refresh_count: u32,
}

pub(crate) fn trace_instances_list(timing: InstancesListTiming) {
    tracing::debug!(
        target: "axial::timing",
        route = "/api/v1/instances",
        total_ms = ms(timing.total),
        scan_ms = ms(timing.scan),
        enrich_ms = ms(timing.enrich),
        version_count = timing.version_count,
        instance_count = timing.instance_count,
        degraded = timing.degraded,
        installed_versions_source = timing.scan_source,
        installed_versions_refresh_count = timing.refresh_count,
        "instances list timing"
    );
}

pub(crate) struct CreateViewTiming<'a> {
    pub source_id: &'a str,
    pub total: Duration,
    pub scan: Duration,
    pub catalog: Duration,
    pub policy: Duration,
    pub version_count: usize,
    pub source_cache_hit: bool,
    pub scan_source: &'static str,
    pub refresh_count: u32,
}

pub(crate) fn trace_create_view(timing: CreateViewTiming<'_>) {
    tracing::debug!(
        target: "axial::timing",
        route = "/api/v1/instances/create-view",
        source_id = %timing.source_id,
        total_ms = ms(timing.total),
        scan_ms = ms(timing.scan),
        catalog_ms = ms(timing.catalog),
        policy_ms = ms(timing.policy),
        version_count = timing.version_count,
        source_cache_hit = timing.source_cache_hit,
        installed_versions_source = timing.scan_source,
        installed_versions_refresh_count = timing.refresh_count,
        "create view timing"
    );
}

pub(crate) struct CreateInstanceTiming {
    pub total: Duration,
    pub version_count: usize,
    pub scan_source: &'static str,
    pub refresh_count: u32,
    pub queued_install: bool,
}

pub(crate) fn trace_create_instance(timing: CreateInstanceTiming) {
    tracing::debug!(
        target: "axial::timing",
        route = "/api/v1/instances",
        operation = "create",
        total_ms = ms(timing.total),
        version_count = timing.version_count,
        installed_versions_source = timing.scan_source,
        installed_versions_refresh_count = timing.refresh_count,
        queued_install = timing.queued_install,
        "create instance timing"
    );
}

pub(crate) fn trace_slow_instance_readiness(
    instance_id: &str,
    version_id: &str,
    readiness: Duration,
    launchable: bool,
    reason_count: usize,
) {
    tracing::debug!(
        target: "axial::timing",
        route = "/api/v1/instances",
        instance_id = %instance_id,
        version_id = %version_id,
        readiness_ms = ms(readiness),
        launchable,
        reason_count,
        "slow instance summary readiness"
    );
}

pub(crate) struct LaunchSessionTiming<'a> {
    pub route: &'static str,
    pub session_id: Option<&'a str>,
    pub instance_id: &'a str,
    pub version_id: &'a str,
    pub total: Duration,
    pub layout: Duration,
    pub auth: Duration,
    pub preflight: Duration,
    pub runtime_repair: Duration,
    pub insert: Option<Duration>,
    pub readiness_launchable: bool,
    pub guardian_decision: GuardianDecisionKind,
}

pub(crate) fn trace_launch_session(timing: LaunchSessionTiming<'_>, message: &'static str) {
    tracing::debug!(
        target: "axial::timing",
        route = timing.route,
        session_id = timing.session_id.unwrap_or(""),
        instance_id = %timing.instance_id,
        version_id = %timing.version_id,
        total_ms = ms(timing.total),
        layout_ms = ms(timing.layout),
        auth_ms = ms(timing.auth),
        preflight_ms = ms(timing.preflight),
        runtime_repair_ms = ms(timing.runtime_repair),
        insert_ms = timing.insert.map(ms).unwrap_or_default(),
        readiness_launchable = timing.readiness_launchable,
        guardian_decision = ?timing.guardian_decision,
        message
    );
}

pub(crate) struct LaunchPreflightFactTiming<'a> {
    pub instance_id: &'a str,
    pub version_id: &'a str,
    pub total: Duration,
    pub memory: Duration,
    pub scan: Duration,
    pub overrides: Duration,
    pub resources: Duration,
    pub readiness: Duration,
    pub guardian: Duration,
    pub version_count: usize,
    pub readiness_launchable: bool,
    pub reason_count: usize,
    pub fact_count: usize,
    pub guardian_decision: GuardianDecisionKind,
    pub java_probe_count: u8,
    pub java_probe_source: &'a str,
    pub installed_versions_source: &'a str,
    pub installed_versions_refresh_count: u32,
}

pub(crate) fn trace_launch_preflight_facts(timing: LaunchPreflightFactTiming<'_>) {
    tracing::debug!(
        target: "axial::timing",
        boundary = "launch_preflight_facts",
        instance_id = %timing.instance_id,
        version_id = %timing.version_id,
        total_ms = ms(timing.total),
        memory_ms = ms(timing.memory),
        scan_ms = ms(timing.scan),
        overrides_ms = ms(timing.overrides),
        resources_ms = ms(timing.resources),
        readiness_ms = ms(timing.readiness),
        guardian_ms = ms(timing.guardian),
        version_count = timing.version_count,
        readiness_launchable = timing.readiness_launchable,
        reason_count = timing.reason_count,
        fact_count = timing.fact_count,
        guardian_decision = ?timing.guardian_decision,
        java_probe_count = timing.java_probe_count,
        java_probe_source = timing.java_probe_source,
        installed_versions_source = timing.installed_versions_source,
        installed_versions_refresh_count = timing.installed_versions_refresh_count,
        "launch preflight fact timing"
    );
}

pub(crate) struct LaunchPreflightResponseTiming<'a> {
    pub instance_id: &'a str,
    pub version_id: &'a str,
    pub total: Duration,
    pub readiness_launchable: bool,
    pub guardian_decision: GuardianDecisionKind,
    pub reason_count: usize,
    pub fact_count: usize,
}

pub(crate) fn trace_launch_preflight_response(timing: LaunchPreflightResponseTiming<'_>) {
    tracing::debug!(
        target: "axial::timing",
        route = "/api/v1/launch/preflight/{id}",
        instance_id = %timing.instance_id,
        version_id = %timing.version_id,
        total_ms = ms(timing.total),
        readiness_launchable = timing.readiness_launchable,
        guardian_decision = ?timing.guardian_decision,
        reason_count = timing.reason_count,
        fact_count = timing.fact_count,
        "launch preflight response timing"
    );
}

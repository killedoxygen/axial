use crate::guardian::GuardianActionKind;
use std::time::Duration;

pub(crate) const LAUNCH_PREFLIGHT_SENSE_TIMING_SIGNAL: &str = "launch_preflight_sense_timing";

macro_rules! launch_preflight_sense_cost_classes {
    ($($variant:ident => $name:literal),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub(crate) enum LaunchPreflightSenseCostClass {
            $($variant),+
        }

        impl LaunchPreflightSenseCostClass {
            #[cfg(test)]
            pub(crate) const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub(crate) const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $name),+
                }
            }
        }
    };
}

launch_preflight_sense_cost_classes! {
    InProcess => "in_process",
    MetadataIo => "metadata_io",
    ContentIo => "content_io",
    ExternalProbe => "external_probe",
}

macro_rules! launch_preflight_senses {
    ($($variant:ident => ($id:literal, $cost:ident)),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub(crate) enum LaunchPreflightSenseId {
            $($variant),+
        }

        impl LaunchPreflightSenseId {
            pub(crate) const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub(crate) const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $id),+
                }
            }

            pub(crate) const fn declared_cost_class(self) -> LaunchPreflightSenseCostClass {
                match self {
                    $(Self::$variant => LaunchPreflightSenseCostClass::$cost),+
                }
            }
        }
    };
}

launch_preflight_senses! {
    Memory => ("memory", MetadataIo),
    InstalledVersions => ("installed_versions", ContentIo),
    Overrides => ("overrides", ExternalProbe),
    Resources => ("resources", MetadataIo),
    Readiness => ("readiness", ContentIo),
    GuardianPolicy => ("guardian_policy", InProcess),
}

pub(crate) struct LaunchPreflightSenseTimings {
    pub memory: Duration,
    pub installed_versions: Duration,
    pub overrides: Duration,
    pub resources: Duration,
    pub readiness: Duration,
    pub guardian_policy: Duration,
}

impl LaunchPreflightSenseTimings {
    fn duration(&self, id: LaunchPreflightSenseId) -> Duration {
        match id {
            LaunchPreflightSenseId::Memory => self.memory,
            LaunchPreflightSenseId::InstalledVersions => self.installed_versions,
            LaunchPreflightSenseId::Overrides => self.overrides,
            LaunchPreflightSenseId::Resources => self.resources,
            LaunchPreflightSenseId::Readiness => self.readiness,
            LaunchPreflightSenseId::GuardianPolicy => self.guardian_policy,
        }
    }
}

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
    pub guardian_decision: GuardianActionKind,
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
    pub senses: LaunchPreflightSenseTimings,
    pub version_count: usize,
    pub readiness_launchable: bool,
    pub reason_count: usize,
    pub fact_count: usize,
    pub guardian_decision: GuardianActionKind,
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
        memory_ms = ms(timing.senses.duration(LaunchPreflightSenseId::Memory)),
        memory_cost_class = LaunchPreflightSenseId::Memory.declared_cost_class().as_str(),
        installed_versions_ms = ms(timing.senses.duration(LaunchPreflightSenseId::InstalledVersions)),
        installed_versions_cost_class = LaunchPreflightSenseId::InstalledVersions.declared_cost_class().as_str(),
        overrides_ms = ms(timing.senses.duration(LaunchPreflightSenseId::Overrides)),
        overrides_cost_class = LaunchPreflightSenseId::Overrides.declared_cost_class().as_str(),
        resources_ms = ms(timing.senses.duration(LaunchPreflightSenseId::Resources)),
        resources_cost_class = LaunchPreflightSenseId::Resources.declared_cost_class().as_str(),
        readiness_ms = ms(timing.senses.duration(LaunchPreflightSenseId::Readiness)),
        readiness_cost_class = LaunchPreflightSenseId::Readiness.declared_cost_class().as_str(),
        guardian_policy_ms = ms(timing.senses.duration(LaunchPreflightSenseId::GuardianPolicy)),
        guardian_policy_cost_class = LaunchPreflightSenseId::GuardianPolicy.declared_cost_class().as_str(),
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
    for sense in LaunchPreflightSenseId::ALL {
        tracing::debug!(
            target: "axial::timing",
            timing_signal = LAUNCH_PREFLIGHT_SENSE_TIMING_SIGNAL,
            sense = sense.as_str(),
            declared_cost_class = sense.declared_cost_class().as_str(),
            duration_ms = ms(timing.senses.duration(*sense)),
            "launch preflight sense timing"
        );
    }
}

pub(crate) struct LaunchPreflightResponseTiming<'a> {
    pub instance_id: &'a str,
    pub version_id: &'a str,
    pub total: Duration,
    pub readiness_launchable: bool,
    pub guardian_decision: GuardianActionKind,
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

#[cfg(test)]
mod launch_preflight_sense_tests {
    use super::{
        LAUNCH_PREFLIGHT_SENSE_TIMING_SIGNAL, LaunchPreflightSenseCostClass,
        LaunchPreflightSenseId, LaunchPreflightSenseTimings,
    };
    use std::collections::HashSet;
    use std::time::Duration;

    #[test]
    fn preflight_senses_are_unique_and_have_timing_and_cost_coverage() {
        let ids = LaunchPreflightSenseId::ALL
            .iter()
            .map(|id| id.as_str())
            .collect::<HashSet<_>>();
        let cost_class_names = LaunchPreflightSenseCostClass::ALL
            .iter()
            .map(|cost| cost.as_str())
            .collect::<HashSet<_>>();
        let cost_classes = LaunchPreflightSenseId::ALL
            .iter()
            .map(|id| id.declared_cost_class())
            .collect::<HashSet<_>>();

        assert_eq!(ids.len(), LaunchPreflightSenseId::ALL.len());
        assert_eq!(
            cost_class_names.len(),
            LaunchPreflightSenseCostClass::ALL.len()
        );
        assert_eq!(cost_classes.len(), LaunchPreflightSenseCostClass::ALL.len());
        assert_eq!(
            LAUNCH_PREFLIGHT_SENSE_TIMING_SIGNAL,
            "launch_preflight_sense_timing"
        );

        let timings = LaunchPreflightSenseTimings {
            memory: Duration::from_millis(1),
            installed_versions: Duration::from_millis(2),
            overrides: Duration::from_millis(3),
            resources: Duration::from_millis(4),
            readiness: Duration::from_millis(5),
            guardian_policy: Duration::from_millis(6),
        };
        for (index, id) in LaunchPreflightSenseId::ALL.iter().enumerate() {
            assert_eq!(
                timings.duration(*id),
                Duration::from_millis(index as u64 + 1)
            );
        }
    }
}

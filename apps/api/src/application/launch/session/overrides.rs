use crate::execution::ExecutionFact;
use crate::execution::jvm::{JvmArgsInspection, JvmArgsInspectionRequest, inspect_jvm_args};
use crate::execution::runtime::{
    JavaOverrideInspection, RuntimeProbeRequest, inspect_java_override_value, probe_java_runtime,
};
use crate::guardian::GuardianPreflightOverrideSignals;
use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};
use croopor_config::{AppConfig, Instance};
use croopor_launcher::LaunchGuardianContext;
use croopor_minecraft::{RuntimeOverride, parse_runtime_override};

pub(super) fn inspect_explicit_java_override(
    instance: &Instance,
    config: &AppConfig,
    required_java_major: Option<u32>,
) -> Option<JavaOverrideInspection> {
    if !instance.java_path.trim().is_empty() {
        return Some(inspect_java_override(
            "instance_java_override",
            &instance.java_path,
            required_java_major,
        ));
    }
    if !config.java_path_override.trim().is_empty() {
        return Some(inspect_java_override(
            "global_java_override",
            &config.java_path_override,
            required_java_major,
        ));
    }
    None
}

fn inspect_java_override(
    target_id: &str,
    raw_value: &str,
    required_java_major: Option<u32>,
) -> JavaOverrideInspection {
    let target = java_override_target(target_id);
    let mut inspection = inspect_java_override_value(None, target.clone(), raw_value);
    if inspection.facts.is_empty() {
        inspection
            .facts
            .extend(probe_java_override(raw_value, &target, required_java_major));
    }
    inspection
}

fn java_override_target(id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Execution,
        TargetKind::Config,
        id,
        OwnershipClass::UserOwned,
    )
}

fn probe_java_override(
    raw_value: &str,
    target: &TargetDescriptor,
    required_java_major: Option<u32>,
) -> Vec<ExecutionFact> {
    let RuntimeOverride::ExecutablePath(path) = parse_runtime_override(raw_value.trim()) else {
        return Vec::new();
    };

    let mut request = RuntimeProbeRequest::new(target.clone(), &path);
    if let Some(required_java_major) = required_java_major.filter(|major| *major > 0) {
        request = request.with_required_major(required_java_major);
        if required_java_major == 8 {
            request = request.with_required_min_update(312);
        }
    }

    match probe_java_runtime(request) {
        Ok(report) => report.facts,
        Err(error) => error.facts,
    }
}

pub(super) fn inspect_explicit_jvm_args(raw_args: &str) -> JvmArgsInspection {
    if raw_args.trim().is_empty() {
        return JvmArgsInspection {
            args: Vec::new(),
            facts: Vec::new(),
        };
    }
    inspect_jvm_args(JvmArgsInspectionRequest::new(
        explicit_jvm_args_target(),
        raw_args,
    ))
}

fn explicit_jvm_args_target() -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Execution,
        TargetKind::Config,
        "explicit_jvm_args",
        OwnershipClass::UserOwned,
    )
}

pub(super) fn preflight_override_signals(
    guardian: &LaunchGuardianContext,
) -> GuardianPreflightOverrideSignals {
    GuardianPreflightOverrideSignals {
        explicit_java_override: guardian.has_java_override(),
        explicit_jvm_preset: guardian.has_named_preset(),
        explicit_jvm_args: guardian.has_raw_jvm_args(),
    }
}

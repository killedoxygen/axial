//! Execution-owned JVM argument inspection.
//!
//! This module reports typed facts about explicit JVM arguments. It does not
//! decide whether Guardian should strip, repair, warn, or block.

use super::{ExecutionFact, ExecutionFactKind};
use crate::observability::{EvidenceField, EvidenceSensitivity};
use crate::state::contracts::{OperationId, TargetDescriptor};

#[derive(Clone, Debug)]
pub struct JvmArgsInspectionRequest<'a> {
    pub operation_id: Option<OperationId>,
    pub target: TargetDescriptor,
    pub raw_args: &'a str,
}

impl<'a> JvmArgsInspectionRequest<'a> {
    pub fn new(target: TargetDescriptor, raw_args: &'a str) -> Self {
        Self {
            operation_id: None,
            target,
            raw_args,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JvmArgsInspection {
    pub args: Vec<String>,
    pub facts: Vec<ExecutionFact>,
}

pub fn inspect_jvm_args(request: JvmArgsInspectionRequest<'_>) -> JvmArgsInspection {
    let raw = request.raw_args.trim();
    if raw.is_empty() {
        return JvmArgsInspection {
            args: Vec::new(),
            facts: vec![jvm_fact(
                ExecutionFactKind::JvmArgsEmpty,
                request.operation_id,
                &request.target,
                Vec::new(),
            )],
        };
    }

    let (args, mut facts) = match shlex::split(raw) {
        Some(args) => (args, Vec::new()),
        None => {
            let fallback = raw
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>();
            let facts = vec![jvm_fact(
                ExecutionFactKind::JvmArgsParseFailed,
                request.operation_id.clone(),
                &request.target,
                vec![EvidenceField::new(
                    "parser",
                    "shell_words",
                    EvidenceSensitivity::Public,
                )],
            )];
            (fallback, facts)
        }
    };

    facts.extend(classify_jvm_args(
        request.operation_id,
        &request.target,
        &args,
    ));
    JvmArgsInspection { args, facts }
}

pub fn jvm_fact(
    kind: ExecutionFactKind,
    operation_id: Option<OperationId>,
    target: &TargetDescriptor,
    fields: Vec<EvidenceField>,
) -> ExecutionFact {
    ExecutionFact {
        operation_id,
        kind,
        target: Some(target.clone()),
        fields,
    }
}

fn classify_jvm_args(
    operation_id: Option<OperationId>,
    target: &TargetDescriptor,
    args: &[String],
) -> Vec<ExecutionFact> {
    let mut facts = Vec::new();
    let unlock_index = args
        .iter()
        .position(|arg| arg == "-XX:+UnlockExperimentalVMOptions");

    for (index, arg) in args.iter().enumerate() {
        if let Some(kind) = classify_jvm_arg(arg) {
            facts.push(jvm_fact(
                kind,
                operation_id.clone(),
                target,
                fact_fields(arg_family(arg)),
            ));
        }
        if is_experimental_g1_arg(arg) && !unlock_index.is_some_and(|unlock| unlock < index) {
            facts.push(jvm_fact(
                ExecutionFactKind::JvmArgUnlockOrderInvalid,
                operation_id.clone(),
                target,
                fact_fields("experimental_g1_tuning"),
            ));
        }
    }

    facts
}

fn classify_jvm_arg(arg: &str) -> Option<ExecutionFactKind> {
    let lower = arg.to_ascii_lowercase();
    if is_memory_arg(&lower) {
        Some(ExecutionFactKind::JvmArgMemoryConflict)
    } else if is_unsafe_classpath_arg(&lower) {
        Some(ExecutionFactKind::JvmArgUnsafeClasspathOverride)
    } else if is_unsafe_native_path_arg(&lower) {
        Some(ExecutionFactKind::JvmArgUnsafeNativePathOverride)
    } else if is_agent_arg(&lower) {
        Some(ExecutionFactKind::JvmArgAgentOverride)
    } else if is_reserved_launcher_arg(&lower) {
        Some(ExecutionFactKind::JvmArgReservedLauncherFlag)
    } else if is_runtime_sensitive_gc_arg(&lower) {
        Some(ExecutionFactKind::JvmArgUnsupportedGc)
    } else {
        None
    }
}

fn is_memory_arg(arg: &str) -> bool {
    arg.starts_with("-xmx")
        || arg.starts_with("-xms")
        || arg.starts_with("-xx:maxram")
        || arg.starts_with("-xx:initialram")
}

fn is_unsafe_classpath_arg(arg: &str) -> bool {
    matches!(arg, "-cp" | "-classpath" | "--class-path" | "--classpath")
        || arg.starts_with("-djava.class.path=")
        || arg.starts_with("-xbootclasspath")
}

fn is_unsafe_native_path_arg(arg: &str) -> bool {
    arg.starts_with("-djava.library.path=")
        || arg.starts_with("-dorg.lwjgl.librarypath=")
        || arg.starts_with("-djna.tmpdir=")
        || arg.starts_with("-djava.io.tmpdir=")
}

fn is_agent_arg(arg: &str) -> bool {
    arg.starts_with("-javaagent") || arg.starts_with("-agentlib") || arg.starts_with("-agentpath")
}

fn is_reserved_launcher_arg(arg: &str) -> bool {
    matches!(arg, "-jar" | "--module-path" | "-p" | "--module")
}

fn is_runtime_sensitive_gc_arg(arg: &str) -> bool {
    matches!(
        arg,
        "-xx:+useshenandoahgc" | "-xx:+usezgc" | "-xx:+zgenerational"
    )
}

fn is_experimental_g1_arg(arg: &str) -> bool {
    let lower = arg.to_ascii_lowercase();
    lower.starts_with("-xx:g1newsizepercent=") || lower.starts_with("-xx:g1maxnewsizepercent=")
}

fn arg_family(arg: &str) -> &'static str {
    let lower = arg.to_ascii_lowercase();
    if is_memory_arg(&lower) {
        "memory"
    } else if is_unsafe_classpath_arg(&lower) {
        "classpath"
    } else if is_unsafe_native_path_arg(&lower) {
        "native_path"
    } else if is_agent_arg(&lower) {
        "agent"
    } else if is_experimental_g1_arg(arg) {
        "experimental_g1_tuning"
    } else if is_runtime_sensitive_gc_arg(&lower) {
        "runtime_sensitive_gc"
    } else {
        "launcher_reserved"
    }
}

fn fact_fields(arg_family: &'static str) -> Vec<EvidenceField> {
    vec![EvidenceField::new(
        "arg_family",
        arg_family,
        EvidenceSensitivity::Public,
    )]
}

#[cfg(test)]
mod tests {
    use super::{JvmArgsInspectionRequest, inspect_jvm_args};
    use crate::execution::ExecutionFactKind;
    use crate::state::contracts::{
        OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };

    #[test]
    fn malformed_jvm_args_emit_parse_fact_without_raw_args() {
        let inspection = inspect_jvm_args(JvmArgsInspectionRequest::new(
            jvm_target(),
            r#"-Xmx2G "unterminated C:\Users\Alice\java"#,
        ));

        assert!(
            inspection
                .facts
                .iter()
                .any(|fact| fact.kind == ExecutionFactKind::JvmArgsParseFailed)
        );
        assert_no_sensitive_jvm_material(&inspection.facts);
    }

    #[test]
    fn memory_classpath_native_and_agent_overrides_emit_distinct_facts() {
        let inspection = inspect_jvm_args(JvmArgsInspectionRequest::new(
            jvm_target(),
            "-Xmx8G -cp secret.jar -Djava.library.path=/tmp/native -javaagent:/tmp/agent.jar",
        ));
        let kinds = inspection
            .facts
            .iter()
            .map(|fact| fact.kind)
            .collect::<Vec<_>>();

        assert!(kinds.contains(&ExecutionFactKind::JvmArgMemoryConflict));
        assert!(kinds.contains(&ExecutionFactKind::JvmArgUnsafeClasspathOverride));
        assert!(kinds.contains(&ExecutionFactKind::JvmArgUnsafeNativePathOverride));
        assert!(kinds.contains(&ExecutionFactKind::JvmArgAgentOverride));
        assert_no_sensitive_jvm_material(&inspection.facts);
    }

    #[test]
    fn invalid_unlock_order_emits_jvm_fact() {
        let inspection = inspect_jvm_args(JvmArgsInspectionRequest::new(
            jvm_target(),
            "-XX:+UseZGC -XX:G1NewSizePercent=30 -XX:+UnlockExperimentalVMOptions",
        ));
        let kinds = inspection
            .facts
            .iter()
            .map(|fact| fact.kind)
            .collect::<Vec<_>>();

        assert!(kinds.contains(&ExecutionFactKind::JvmArgUnlockOrderInvalid));
        assert_no_sensitive_jvm_material(&inspection.facts);
    }

    #[test]
    fn runtime_sensitive_gc_flags_emit_unsupported_gc_fact() {
        let inspection = inspect_jvm_args(JvmArgsInspectionRequest::new(
            jvm_target(),
            "-XX:+UseZGC -XX:+UseShenandoahGC -XX:+ZGenerational",
        ));
        let unsupported_gc_count = inspection
            .facts
            .iter()
            .filter(|fact| fact.kind == ExecutionFactKind::JvmArgUnsupportedGc)
            .count();

        assert_eq!(unsupported_gc_count, 3);
        assert_no_sensitive_jvm_material(&inspection.facts);
    }

    fn jvm_target() -> TargetDescriptor {
        TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Config,
            "explicit_jvm_args",
            OwnershipClass::UserOwned,
        )
    }

    fn assert_no_sensitive_jvm_material(facts: &[crate::execution::ExecutionFact]) {
        let encoded = serde_json::to_string(facts).expect("facts json");
        let lower = encoded.to_ascii_lowercase();
        assert!(!lower.contains("alice"));
        assert!(!lower.contains("secret.jar"));
        assert!(!lower.contains("javaagent:/tmp"));
        assert!(!lower.contains("-xmx"));
        assert!(!lower.contains("c:\\"));
        assert!(!lower.contains("/tmp/native"));
    }
}

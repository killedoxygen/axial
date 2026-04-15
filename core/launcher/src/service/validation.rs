use crate::runtime::RuntimeSelection;
use crate::types::LaunchFailureClass;
use croopor_minecraft::JavaRuntimeInfo;

pub(crate) fn validate_requested_java_override(
    requested_java: &str,
    info: &JavaRuntimeInfo,
    required_major: i32,
) -> Result<(), (LaunchFailureClass, String)> {
    if requested_java.trim().is_empty() {
        return Ok(());
    }
    if required_major > 0 && info.major > 0 && info.major as i32 != required_major {
        return Err((
            LaunchFailureClass::JavaRuntimeMismatch,
            format!(
                "explicit Java override targets Java {} but this version requires Java {}",
                info.major, required_major
            ),
        ));
    }
    if required_major == 8 && info.major == 8 && info.update > 0 && info.update < 312 {
        return Err((
            LaunchFailureClass::JavaRuntimeMismatch,
            format!(
                "explicit Java 8 override is too old for legacy support (8u{} detected; use 8u312 or newer)",
                info.update
            ),
        ));
    }
    Ok(())
}

pub(crate) fn validate_manual_java_override(
    requested_java: &str,
    runtime: &RuntimeSelection,
    required_major: i32,
) -> Result<(), (LaunchFailureClass, String)> {
    if requested_java.trim().is_empty() || requested_java.trim() != runtime.effective_path.trim() {
        return Ok(());
    }
    validate_requested_java_override(requested_java, &runtime.effective_info, required_major)
}

pub(crate) fn validate_manual_jvm_args(
    args: &[String],
    info: &JavaRuntimeInfo,
) -> Result<(), (LaunchFailureClass, String)> {
    if args.is_empty() {
        return Ok(());
    }
    let unlock_index = args
        .iter()
        .position(|arg| arg == "-XX:+UnlockExperimentalVMOptions");
    for (index, arg) in args.iter().enumerate() {
        match () {
            _ if arg == "-XX:+UseShenandoahGC" && !crate::jvm::supports_shenandoah(info) => {
                return Err((
                    LaunchFailureClass::JvmUnsupportedOption,
                    "explicit JVM args request Shenandoah on an unsupported runtime".to_string(),
                ));
            }
            _ if arg == "-XX:+UseZGC" && !crate::jvm::supports_zgc(info) => {
                return Err((
                    LaunchFailureClass::JvmUnsupportedOption,
                    "explicit JVM args request ZGC on an unsupported runtime".to_string(),
                ));
            }
            _ if arg == "-XX:+ZGenerational" && !crate::jvm::supports_generational_zgc(info) => {
                return Err((
                    LaunchFailureClass::JvmUnsupportedOption,
                    "explicit JVM args request Generational ZGC on an unsupported runtime"
                        .to_string(),
                ));
            }
            _ if arg.starts_with("-XX:G1NewSizePercent=")
                || arg.starts_with("-XX:G1MaxNewSizePercent=") =>
            {
                if !crate::jvm::supports_hotspot_tuning(info) {
                    return Err((
                        LaunchFailureClass::JvmUnsupportedOption,
                        "explicit JVM args request experimental G1 tuning on an unsupported runtime"
                            .to_string(),
                    ));
                }
                if unlock_index.is_none() {
                    return Err((
                        LaunchFailureClass::JvmExperimentalUnlock,
                        "explicit JVM args require -XX:+UnlockExperimentalVMOptions".to_string(),
                    ));
                }
                if unlock_index.is_some_and(|unlock| unlock > index) {
                    return Err((
                        LaunchFailureClass::JvmOptionOrdering,
                        "explicit JVM args place -XX:+UnlockExperimentalVMOptions after dependent flags"
                            .to_string(),
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

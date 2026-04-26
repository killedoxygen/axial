use croopor_minecraft::{DownloadProgress, ensure_java_runtime, resolve_version};
use std::path::Path;

pub async fn prewarm_version_runtime<F>(
    library_dir: &Path,
    version_id: &str,
    mut send: F,
) -> Result<(), String>
where
    F: FnMut(DownloadProgress),
{
    let version = resolve_version(library_dir, version_id).map_err(|error| error.to_string())?;
    let component = if version.java_version.component.trim().is_empty() {
        "managed runtime".to_string()
    } else {
        version.java_version.component.clone()
    };

    send(DownloadProgress {
        phase: "java_runtime".to_string(),
        current: 0,
        total: 1,
        file: Some(format!(
            "Preparing {} (Java {})",
            component, version.java_version.major_version
        )),
        error: None,
        done: false,
    });

    ensure_java_runtime(library_dir, &version.java_version, "")
        .await
        .map_err(|error| error.to_string())?;

    send(DownloadProgress {
        phase: "java_runtime".to_string(),
        current: 1,
        total: 1,
        file: Some(format!(
            "Ready {} (Java {})",
            component, version.java_version.major_version
        )),
        error: None,
        done: false,
    });

    Ok(())
}

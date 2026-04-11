use croopor_minecraft::{JavaRuntimeInfo, JavaRuntimeResult, JavaVersion};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSelection {
    pub requested_path: String,
    pub selected_path: String,
    pub selected_info: JavaRuntimeInfo,
    pub effective_path: String,
    pub effective_info: JavaRuntimeInfo,
    pub effective_source: String,
    pub bypassed_requested_runtime: bool,
}

#[derive(Debug, Error)]
pub enum RuntimeSelectionError<E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    #[error("runtime resolver is required")]
    MissingResolver,
    #[error(transparent)]
    Resolve(#[from] E),
}

pub fn resolve_runtime<E, F>(
    required: &JavaVersion,
    requested_path: impl Into<String>,
    force_managed: bool,
    resolver: Option<F>,
) -> Result<RuntimeSelection, RuntimeSelectionError<E>>
where
    E: std::error::Error + Send + Sync + 'static,
    F: FnMut(&str) -> Result<(Option<JavaRuntimeResult>, JavaRuntimeInfo), E>,
{
    let mut selection = RuntimeSelection {
        requested_path: requested_path.into().trim().to_string(),
        selected_path: String::new(),
        selected_info: JavaRuntimeInfo {
            id: String::new(),
            major: 0,
            update: 0,
            distribution: "unknown".to_string(),
            path: String::new(),
        },
        effective_path: String::new(),
        effective_info: JavaRuntimeInfo {
            id: String::new(),
            major: 0,
            update: 0,
            distribution: "unknown".to_string(),
            path: String::new(),
        },
        effective_source: String::new(),
        bypassed_requested_runtime: false,
    };
    let Some(mut resolver) = resolver else {
        return Err(RuntimeSelectionError::MissingResolver);
    };

    if !force_managed && !selection.requested_path.is_empty() {
        let (selected_result, selected_info) = resolver(&selection.requested_path)?;
        if let Some(result) = selected_result.as_ref() {
            if result.source == "override" {
                selection.selected_path = result.path.clone();
                selection.selected_info = selected_info.clone();
            } else {
                selection.bypassed_requested_runtime = true;
            }
        }
        selection.apply_effective(selected_result.as_ref(), &selected_info);

        if should_bypass_requested_runtime(required, selected_result.as_ref(), &selected_info) {
            let (managed_result, managed_info) = resolver("")?;
            selection.apply_effective(managed_result.as_ref(), &managed_info);
            selection.bypassed_requested_runtime = true;
        }

        return Ok(selection);
    }

    let (effective_result, effective_info) = resolver("")?;
    selection.apply_effective(effective_result.as_ref(), &effective_info);
    if force_managed && !selection.requested_path.is_empty() {
        selection.bypassed_requested_runtime = true;
    }

    Ok(selection)
}

pub fn should_bypass_requested_runtime(
    required: &JavaVersion,
    result: Option<&JavaRuntimeResult>,
    info: &JavaRuntimeInfo,
) -> bool {
    let Some(result) = result else {
        return false;
    };
    if result.source != "override" || info.major == 0 || required.major_version == 0 {
        return false;
    }
    if info.major as i32 != required.major_version {
        return true;
    }
    info.major == 8
}

impl RuntimeSelection {
    fn apply_effective(&mut self, result: Option<&JavaRuntimeResult>, info: &JavaRuntimeInfo) {
        let Some(result) = result else {
            return;
        };
        self.effective_path = result.path.clone();
        self.effective_info = info.clone();
        self.effective_source = result.source.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn info(path: &str, major: u32) -> JavaRuntimeInfo {
        JavaRuntimeInfo {
            id: "runtime".to_string(),
            major,
            update: 0,
            distribution: "openjdk".to_string(),
            path: path.to_string(),
        }
    }

    fn result(path: &str, source: &str) -> JavaRuntimeResult {
        JavaRuntimeResult {
            path: path.to_string(),
            component: "java-runtime-delta".to_string(),
            source: source.to_string(),
        }
    }

    #[test]
    fn bypasses_override_when_major_mismatches() {
        let required = JavaVersion {
            component: "java-runtime-gamma".to_string(),
            major_version: 21,
        };
        let selection = resolve_runtime::<io::Error, _>(
            &required,
            "C:/java-8/bin/javaw.exe",
            false,
            Some(|override_path: &str| {
                Ok(if override_path.is_empty() {
                    (
                        Some(result("C:/managed/bin/javaw.exe", "minecraft-runtime")),
                        info("C:/managed/bin/javaw.exe", 21),
                    )
                } else {
                    (
                        Some(result(override_path, "override")),
                        info(override_path, 8),
                    )
                })
            }),
        )
        .expect("resolve runtime");

        assert!(selection.bypassed_requested_runtime);
        assert_eq!(selection.selected_path, "C:/java-8/bin/javaw.exe");
        assert_eq!(selection.effective_path, "C:/managed/bin/javaw.exe");
        assert_eq!(selection.effective_info.major, 21);
    }

    #[test]
    fn keeps_override_when_matching_required_major() {
        let required = JavaVersion {
            component: "java-runtime-gamma".to_string(),
            major_version: 21,
        };
        let selection = resolve_runtime::<io::Error, _>(
            &required,
            "C:/java-21/bin/javaw.exe",
            false,
            Some(|override_path: &str| {
                Ok((
                    Some(result(override_path, "override")),
                    info(override_path, 21),
                ))
            }),
        )
        .expect("resolve runtime");

        assert!(!selection.bypassed_requested_runtime);
        assert_eq!(selection.effective_path, "C:/java-21/bin/javaw.exe");
        assert_eq!(selection.selected_path, "C:/java-21/bin/javaw.exe");
    }
}

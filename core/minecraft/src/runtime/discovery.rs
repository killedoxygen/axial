use super::layout::{
    ManagedRuntimeCache, ManagedRuntimeComponent, managed_runtime_executable_ready,
    runtime_java_relative_path,
};
use super::manifest::{
    COMPONENT_MANIFEST_PROOF_FILE, ComponentManifest, MAX_RUNTIME_MANIFEST_BYTES,
    component_manifest_proof_bytes,
};
use super::model::{
    JavaRuntimeInfo, JavaRuntimeLookupError, JavaRuntimeResult, RuntimeId, RuntimeInstallState,
    RuntimeOverride, RuntimeRecord, RuntimeRequirement, RuntimeSource,
};
use super::rosetta::rosetta_required_error_for_current_host;
use crate::launch::{JavaVersion, java_component_for_major};
use crate::portable_path::PortableRelativePath;
use sha2::Digest as _;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedRuntimeMarkerState {
    Ready,
    Missing,
    Corrupt,
}

pub fn runtime_requirement(java_version: &JavaVersion) -> RuntimeRequirement {
    RuntimeRequirement {
        required_java: java_version.clone(),
        preferred_component: RuntimeId(preferred_runtime_component(java_version)),
    }
}

pub fn parse_runtime_override(value: &str) -> RuntimeOverride {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        RuntimeOverride::None
    } else if is_known_runtime_component(trimmed) {
        RuntimeOverride::Component(RuntimeId(trimmed.to_string()))
    } else {
        RuntimeOverride::ExecutablePath(PathBuf::from(trimmed))
    }
}

fn list_runtime_records(cache: &ManagedRuntimeCache) -> Vec<RuntimeRecord> {
    if cache.validate_projection().is_err() {
        return Vec::new();
    }
    let components = known_runtime_components();
    let mut results = Vec::new();
    for component in &components {
        if let Ok(Some(runtime)) = inspect_axial_cached_runtime(cache, component)
            && runtime.install_state == RuntimeInstallState::Ready
        {
            results.push(runtime);
        }
    }
    if cache.validate_projection().is_ok() {
        results
    } else {
        Vec::new()
    }
}

pub fn list_java_runtimes(cache: &ManagedRuntimeCache) -> Vec<JavaRuntimeResult> {
    list_runtime_records(cache)
        .into_iter()
        .filter(|record| record.install_state == RuntimeInstallState::Ready)
        .map(|record| JavaRuntimeResult {
            path: record.java_path,
            component: record.id.0,
            source: record.source.as_str().to_string(),
        })
        .collect()
}

pub fn preferred_runtime_component(java_version: &JavaVersion) -> String {
    if java_version.component.trim().is_empty() {
        java_component_for_major(java_version.major_version)
            .unwrap_or("java-runtime-delta")
            .to_string()
    } else {
        java_version.component.trim().to_string()
    }
}

pub fn is_known_runtime_component(value: &str) -> bool {
    known_runtime_components().contains(&value)
}

impl ManagedRuntimeCache {
    pub fn admit_component(
        &self,
        component: &str,
    ) -> std::io::Result<Option<ManagedRuntimeComponent>> {
        if !is_known_runtime_component(component) {
            return Ok(None);
        }
        self.validate_projection()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let root_path = self.root().join(component);
        let root = self
            .authority()
            .and_then(|root| root.open_child_if_exists(component))
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let Some(root) = root else {
            return Ok(None);
        };
        root.validate_absolute_projection(&root_path)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        self.validate_projection()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(Some(ManagedRuntimeComponent {
            cache: self.clone(),
            component: component.to_string(),
            root,
            root_path,
        }))
    }

    pub(crate) fn component_root(&self, component: &str) -> Option<PathBuf> {
        self.validate_projection().ok()?;
        let root = is_known_runtime_component(component).then(|| self.root().join(component))?;
        self.validate_projection().ok()?;
        Some(root)
    }

    #[cfg(feature = "test-support")]
    pub fn component_root_for_test(&self, component: &str) -> Option<PathBuf> {
        self.component_root(component)
    }

    /// Writes a ready component whose only file is `java` with these bytes,
    /// its exact manifest proof and its ready marker. Returns the executable.
    #[cfg(any(test, feature = "test-support"))]
    pub fn write_ready_component_for_test(
        &self,
        component: &str,
        java: &[u8],
    ) -> std::io::Result<PathBuf> {
        use sha1::Digest as _;
        let root = self
            .component_root(component)
            .ok_or_else(|| std::io::Error::other("unknown runtime component"))?;
        let executable = super::layout::java_executable(&root);
        std::fs::create_dir_all(executable.parent().expect("executable parent"))?;
        std::fs::write(&executable, java)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;
        }
        let relative = runtime_java_relative_path();
        let mut files = serde_json::Map::new();
        files.insert(
            relative.into(),
            serde_json::json!({"type": "file", "executable": cfg!(unix), "downloads": {"raw": {
                "url": "https://example.invalid/java",
                "sha1": format!("{:x}", sha1::Sha1::digest(java)),
                "size": java.len(),
            }}}),
        );
        if cfg!(windows) {
            let config = root.join("lib").join("jvm.cfg");
            std::fs::create_dir_all(config.parent().expect("config parent"))?;
            std::fs::write(&config, b"jvm")?;
            files.insert(
                "lib/jvm.cfg".into(),
                serde_json::json!({"type": "file", "downloads": {"raw": {
                    "url": "https://example.invalid/jvm.cfg",
                    "sha1": format!("{:x}", sha1::Sha1::digest(b"jvm")),
                    "size": 3,
                }}}),
            );
        }
        let manifest =
            serde_json::from_value::<ComponentManifest>(serde_json::json!({ "files": files }))
                .map_err(std::io::Error::other)?;
        std::fs::write(
            root.join(COMPONENT_MANIFEST_PROOF_FILE),
            component_manifest_proof_bytes(&manifest)
                .map_err(|_| std::io::Error::other("manifest"))?,
        )?;
        std::fs::write(root.join(".axial-ready"), b"ready")?;
        Ok(executable)
    }
}

impl ManagedRuntimeComponent {
    pub fn launch_receipt(&self) -> std::io::Result<super::layout::ManagedRuntimeLaunchReceipt> {
        self.validate_projection()?;
        if !self.structurally_ready() {
            return Err(std::io::Error::other(
                "managed runtime is not structurally ready for launch",
            ));
        }
        let java_relative = PortableRelativePath::new_exact(runtime_java_relative_path())
            .map_err(|_| std::io::Error::other("managed Java path is not portable"))?;
        let java_guard = self
            .root
            .inspect_relative_executable(&java_relative)
            .map_err(|error| std::io::Error::other(error.to_string()))?
            .ok_or_else(|| std::io::Error::other("managed Java executable is missing"))?;
        if !self.structurally_ready()
            || !matches!(self.root.executable_guard_matches(&java_guard), Ok(true))
        {
            return Err(std::io::Error::other(
                "managed Java executable changed during launch admission",
            ));
        }
        self.validate_projection()?;
        Ok(super::layout::ManagedRuntimeLaunchReceipt::new(
            self.clone(),
            java_guard,
        ))
    }

    pub fn marker_state(&self) -> ManagedRuntimeMarkerState {
        if self.validate_projection().is_err() {
            return ManagedRuntimeMarkerState::Corrupt;
        }
        let state = match self.root.exact_entry_kind(".axial-ready") {
            Ok(None) => ManagedRuntimeMarkerState::Missing,
            Ok(Some(axial_fs::EntryKind::File))
                if matches!(
                    self.root.read_authenticated(".axial-ready", Some(5), None),
                    Ok(bytes) if bytes == b"ready"
                ) =>
            {
                ManagedRuntimeMarkerState::Ready
            }
            _ => ManagedRuntimeMarkerState::Corrupt,
        };
        if self.validate_projection().is_ok() {
            state
        } else {
            ManagedRuntimeMarkerState::Corrupt
        }
    }

    pub fn executable_ready(&self) -> bool {
        self.validate_projection().is_ok()
            && managed_runtime_executable_ready(&self.root)
            && self.validate_projection().is_ok()
    }

    pub fn structurally_ready(&self) -> bool {
        self.executable_ready()
            && self.marker_state() == ManagedRuntimeMarkerState::Ready
            && matches!(
                self.root.exact_entry_kind(COMPONENT_MANIFEST_PROOF_FILE),
                Ok(Some(axial_fs::EntryKind::File))
            )
            && self.validate_projection().is_ok()
    }

    fn persisted_manifest(&self) -> Option<(ComponentManifest, [u8; 32])> {
        self.validate_projection().ok()?;
        let guard = self
            .root
            .inspect_regular_file(COMPONENT_MANIFEST_PROOF_FILE)
            .ok()??;
        if guard.size() > MAX_RUNTIME_MANIFEST_BYTES {
            return None;
        }
        let bytes = self
            .root
            .read_guarded_file_bounded(
                COMPONENT_MANIFEST_PROOF_FILE,
                &guard,
                MAX_RUNTIME_MANIFEST_BYTES,
            )
            .ok()?;
        let manifest = serde_json::from_slice::<ComponentManifest>(&bytes).ok()?;
        let component = RuntimeId::from(self.component.clone());
        if component_manifest_proof_bytes(&manifest).ok()? != bytes
            || !super::install::persisted_runtime_manifest_contract_is_valid(
                &component,
                &manifest,
                bytes.len() as u64,
            )
        {
            return None;
        }
        self.validate_projection().ok()?;
        Some((manifest, sha2::Sha256::digest(&bytes).into()))
    }

    /// Verifies exact tree membership, kinds, links, executable bits and every
    /// file digest against the persisted manifest proof. A file this process
    /// already hashed in full at its exact unchanged revision is not reread;
    /// the first verification after a restart hashes every byte.
    pub fn contents_verified(&self) -> bool {
        self.verified_manifest().is_some()
    }

    /// `contents_verified`, returning the SHA-256 of the verified manifest
    /// proof (the identity of the exact runtime build these contents are)
    /// and the number of tree entries the verification walked.
    pub fn verified_manifest(&self) -> Option<([u8; 32], u64)> {
        let (manifest, digest) = self.persisted_manifest()?;
        let entries = manifest.files.len() as u64 + 2;
        if !self.executable_ready() {
            return None;
        }
        let (_sender, cancellation) = crate::cancellation::channel();
        (super::install::managed_runtime_tree_matches_manifest(
            &RuntimeId::from(self.component.clone()),
            &self.root,
            &manifest,
            crate::managed::fs::DigestProof::Reverified,
            &cancellation.thread(),
        ) && self.validate_projection().is_ok())
        .then_some((digest, entries))
    }
}

fn known_runtime_components() -> [&'static str; 6] {
    [
        "java-runtime-epsilon",
        "java-runtime-delta",
        "java-runtime-gamma",
        "java-runtime-beta",
        "java-runtime-alpha",
        "jre-legacy",
    ]
}

pub(super) fn resolve_component_runtime(
    cache: &ManagedRuntimeCache,
    component: &RuntimeId,
    required_major: i32,
) -> Result<RuntimeRecord, JavaRuntimeLookupError> {
    resolve_axial_cached_runtime(cache, component, required_major)
}

pub(super) fn resolve_axial_cached_runtime(
    cache: &ManagedRuntimeCache,
    component: &RuntimeId,
    required_major: i32,
) -> Result<RuntimeRecord, JavaRuntimeLookupError> {
    let inspected = inspect_axial_cached_runtime(cache, component.as_str());
    match inspected? {
        Some(record) if record.install_state == RuntimeInstallState::Ready => Ok(record),
        _ => Err(JavaRuntimeLookupError::NotFound {
            component: component.0.clone(),
            major: required_major,
        }),
    }
}

fn inspect_axial_cached_runtime(
    cache: &ManagedRuntimeCache,
    component: &str,
) -> Result<Option<RuntimeRecord>, JavaRuntimeLookupError> {
    let Some(authority) = cache
        .admit_component(component)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?
    else {
        return Ok(None);
    };
    let state = if authority.structurally_ready() {
        RuntimeInstallState::Ready
    } else {
        RuntimeInstallState::Broken
    };
    let java_exe = authority.java_executable_path();
    authority
        .validate_projection()
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    if state == RuntimeInstallState::Ready
        && rosetta_required_error_for_current_host(&java_exe, component).is_some()
    {
        return Err(JavaRuntimeLookupError::RosettaRequired {
            component: component.to_string(),
        });
    }
    authority
        .validate_projection()
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let java_path = java_exe.to_string_lossy().to_string();
    Ok(Some(RuntimeRecord {
        id: RuntimeId(component.to_string()),
        java_path: java_path.clone(),
        info: JavaRuntimeInfo {
            id: component.to_string(),
            major: 0,
            update: 0,
            distribution: "unknown".to_string(),
            path: java_path,
        },
        source: RuntimeSource::Managed,
        install_state: state,
        root_dir: authority.root_path().to_string_lossy().to_string(),
    }))
}

#[cfg(test)]
mod processor_runtime_tests {
    use super::inspect_axial_cached_runtime;
    use crate::runtime::{ManagedRuntimeCache, RuntimeInstallState, RuntimeSource};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn exact_axial_inspection_stamps_managed_even_under_packages_parent() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let parent = crate::test_temp_root().join(format!("Packages-parent-{nonce}"));
        fs::create_dir_all(&parent).expect("cache root");
        let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let component = "java-runtime-delta";
        let root = cache.component_root(component).expect("runtime root");
        fs::create_dir(&root).expect("runtime shell");
        let record = inspect_axial_cached_runtime(&cache, component)
            .expect("exact inspection")
            .expect("broken runtime record");
        assert_eq!(record.source, RuntimeSource::Managed);
        assert_eq!(record.install_state, RuntimeInstallState::Broken);
        assert_eq!(record.id.as_str(), component);
        let _ = fs::remove_dir_all(parent);
    }
}

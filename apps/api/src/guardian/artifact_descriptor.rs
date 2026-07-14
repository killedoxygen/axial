//! Guardian artifact repair descriptors for Minecraft install metadata.
//!
//! Descriptors are inert backend values. They adapt already-selected metadata
//! into the source/destination shape required by Guardian artifact repair, but
//! they do not resolve providers, start downloads, or mutate files.

use super::artifact_repair::GuardianArtifactRepairSource;
use crate::execution::download::{
    DownloadChecksum, DownloadChecksumAlgorithm, valid_download_checksum_metadata,
};
use crate::state::contracts::{
    OwnershipClass, ReconciliationComponent, StabilizationSystem, TargetDescriptor, TargetKind,
    sanitize_target_id,
};
use axial_minecraft::download::{SelectedDownloadArtifactDescriptor, SelectedDownloadArtifactKind};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};
use url::Url;

const MAX_MINECRAFT_REPAIR_ARTIFACT_BYTES: u64 = 512 << 20;
const RECONCILIATION_TARGET_DOMAIN: &[u8] = b"axial.guardian.artifact-target.v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GuardianArtifactDescriptorError {
    UnsupportedAtomicBundleMember,
    MissingDestination,
    MissingProviderUrl,
    UnsupportedProviderUrl,
    MissingChecksum,
    #[cfg(test)]
    UnsupportedChecksumAlgorithm,
    InvalidChecksum,
    MissingTargetId,
    UnsafeTargetId,
    MissingMaxBytes,
    MaxBytesTooLarge,
    ExpectedSizeExceedsMaxBytes,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct GuardianMinecraftArtifactRepairDescriptor {
    target: TargetDescriptor,
    reconciliation_target: TargetDescriptor,
    component: ReconciliationComponent,
    destination: PathBuf,
    source: GuardianMinecraftArtifactRepairSource,
}

impl GuardianMinecraftArtifactRepairDescriptor {
    pub(crate) fn from_core_selected_descriptor(
        descriptor: &SelectedDownloadArtifactDescriptor,
    ) -> Result<Self, GuardianArtifactDescriptorError> {
        let component = reconciliation_component(descriptor.kind)?;
        if descriptor.destination().as_os_str().is_empty() {
            return Err(GuardianArtifactDescriptorError::MissingDestination);
        }
        let target_id = safe_target_id(&descriptor.target)?;
        let provider_url = safe_provider_url(descriptor.provider_url())?;
        let sha1 = safe_sha1(descriptor.sha1())?;
        let max_bytes = bounded_max_bytes(descriptor.max_bytes)?;
        if let Some(expected_size) = descriptor.expected_size
            && expected_size > max_bytes
        {
            return Err(GuardianArtifactDescriptorError::ExpectedSizeExceedsMaxBytes);
        }

        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            target_id,
            OwnershipClass::LauncherManaged,
        );
        let reconciliation_target = exact_reconciliation_target(
            &target,
            component,
            descriptor.destination(),
            DownloadChecksumAlgorithm::Sha1,
            &sha1,
            descriptor.expected_size,
        );

        Ok(Self {
            target,
            reconciliation_target,
            component,
            destination: descriptor.destination().to_path_buf(),
            source: GuardianMinecraftArtifactRepairSource {
                url: provider_url,
                checksum_algorithm: DownloadChecksumAlgorithm::Sha1,
                checksum: sha1,
                expected_size: descriptor.expected_size,
                max_bytes,
            },
        })
    }

    pub(crate) fn target(&self) -> &TargetDescriptor {
        &self.target
    }

    pub(super) fn reconciliation_target(&self) -> &TargetDescriptor {
        &self.reconciliation_target
    }

    pub(crate) fn destination(&self) -> &Path {
        &self.destination
    }

    pub(crate) const fn component(&self) -> ReconciliationComponent {
        self.component
    }

    pub(super) fn repair_source(&self) -> GuardianArtifactRepairSource<'_> {
        GuardianArtifactRepairSource {
            url: &self.source.url,
            checksum_algorithm: self.source.checksum_algorithm.as_str(),
            expected_checksum: &self.source.checksum,
            expected_size: self.source.expected_size,
            max_bytes: Some(self.source.max_bytes),
        }
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_test(
        target: TargetDescriptor,
        destination: &Path,
        provider_url: &str,
        checksum_algorithm: &str,
        checksum: &str,
        expected_size: Option<u64>,
        max_bytes: u64,
    ) -> Result<Self, GuardianArtifactDescriptorError> {
        if destination.as_os_str().is_empty() {
            return Err(GuardianArtifactDescriptorError::MissingDestination);
        }
        let provider_url = safe_provider_url(provider_url)?;
        let checksum_algorithm = DownloadChecksumAlgorithm::parse(checksum_algorithm)
            .ok_or(GuardianArtifactDescriptorError::UnsupportedChecksumAlgorithm)?;
        let checksum = checksum.trim();
        if checksum.is_empty() {
            return Err(GuardianArtifactDescriptorError::MissingChecksum);
        }
        if !valid_download_checksum_metadata(DownloadChecksum::new(checksum_algorithm, checksum)) {
            return Err(GuardianArtifactDescriptorError::InvalidChecksum);
        }
        let max_bytes = bounded_max_bytes(max_bytes)?;
        if expected_size.is_some_and(|expected_size| expected_size > max_bytes) {
            return Err(GuardianArtifactDescriptorError::ExpectedSizeExceedsMaxBytes);
        }
        let component = ReconciliationComponent::Libraries;
        let reconciliation_target = exact_reconciliation_target(
            &target,
            component,
            destination,
            checksum_algorithm,
            checksum,
            expected_size,
        );
        Ok(Self {
            target,
            reconciliation_target,
            component,
            destination: destination.to_path_buf(),
            source: GuardianMinecraftArtifactRepairSource {
                url: provider_url,
                checksum_algorithm,
                checksum: checksum.to_ascii_lowercase(),
                expected_size,
                max_bytes,
            },
        })
    }
}

const fn reconciliation_component(
    kind: SelectedDownloadArtifactKind,
) -> Result<ReconciliationComponent, GuardianArtifactDescriptorError> {
    match kind {
        SelectedDownloadArtifactKind::VersionJson
        | SelectedDownloadArtifactKind::ClientJar
        | SelectedDownloadArtifactKind::LogConfig => {
            Err(GuardianArtifactDescriptorError::UnsupportedAtomicBundleMember)
        }
        SelectedDownloadArtifactKind::Library => Ok(ReconciliationComponent::Libraries),
        SelectedDownloadArtifactKind::AssetIndex | SelectedDownloadArtifactKind::AssetObject => {
            Ok(ReconciliationComponent::Assets)
        }
    }
}

fn exact_reconciliation_target(
    target: &TargetDescriptor,
    component: ReconciliationComponent,
    destination: &Path,
    checksum_algorithm: DownloadChecksumAlgorithm,
    checksum: &str,
    expected_size: Option<u64>,
) -> TargetDescriptor {
    let mut hasher = Sha256::new();
    update_digest_frame(&mut hasher, b"domain", RECONCILIATION_TARGET_DOMAIN);
    update_digest_frame(
        &mut hasher,
        b"component",
        reconciliation_component_id(component).as_bytes(),
    );
    update_digest_frame(&mut hasher, b"public_target", target.id.as_bytes());
    update_path_digest_frame(&mut hasher, b"destination", destination);
    update_digest_frame(
        &mut hasher,
        b"checksum_algorithm",
        checksum_algorithm.as_str().as_bytes(),
    );
    update_digest_frame(&mut hasher, b"checksum", checksum.as_bytes());
    update_digest_frame(
        &mut hasher,
        b"expected_size",
        &expected_size.unwrap_or(u64::MAX).to_le_bytes(),
    );
    let digest = format!("{:x}", hasher.finalize());
    let digest = digest
        .as_bytes()
        .chunks(8)
        .map(|chunk| std::str::from_utf8(chunk).expect("SHA-256 hex is ASCII"))
        .collect::<Vec<_>>()
        .join(".");
    TargetDescriptor::new(
        target.system,
        target.kind,
        format!("artifact.sha256.{digest}"),
        target.ownership,
    )
}

const fn reconciliation_component_id(component: ReconciliationComponent) -> &'static str {
    match component {
        ReconciliationComponent::VersionBundle => "version_bundle",
        ReconciliationComponent::Libraries => "libraries",
        ReconciliationComponent::Assets => "assets",
        ReconciliationComponent::Runtime => "runtime",
        ReconciliationComponent::WholeInstance => "whole_instance",
    }
}

fn update_digest_frame(hasher: &mut Sha256, label: &[u8], value: &[u8]) {
    hasher.update((label.len() as u64).to_le_bytes());
    hasher.update(label);
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

#[cfg(unix)]
fn update_path_digest_frame(hasher: &mut Sha256, label: &[u8], path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    update_digest_frame(hasher, label, path.as_os_str().as_bytes());
}

#[cfg(windows)]
fn update_path_digest_frame(hasher: &mut Sha256, label: &[u8], path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    let encoded = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    update_digest_frame(hasher, label, &encoded);
}

#[cfg(not(any(unix, windows)))]
fn update_path_digest_frame(hasher: &mut Sha256, label: &[u8], path: &Path) {
    update_digest_frame(hasher, label, path.to_string_lossy().as_bytes());
}

impl fmt::Debug for GuardianMinecraftArtifactRepairDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuardianMinecraftArtifactRepairDescriptor")
            .field("target", &self.target)
            .field("reconciliation_target", &self.reconciliation_target)
            .field("component", &self.component)
            .field("destination", &"<redacted>")
            .field("source", &self.source)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
struct GuardianMinecraftArtifactRepairSource {
    url: String,
    checksum_algorithm: DownloadChecksumAlgorithm,
    checksum: String,
    expected_size: Option<u64>,
    max_bytes: u64,
}

impl fmt::Debug for GuardianMinecraftArtifactRepairSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuardianMinecraftArtifactRepairSource")
            .field("url", &"<redacted>")
            .field("checksum_algorithm", &self.checksum_algorithm.as_str())
            .field("checksum", &"<redacted>")
            .field("expected_size", &self.expected_size)
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

fn safe_target_id(value: &str) -> Result<String, GuardianArtifactDescriptorError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(GuardianArtifactDescriptorError::MissingTargetId);
    }
    let normalized = sanitize_target_id(value, "target");
    if normalized == "target" && value != "target" {
        Err(GuardianArtifactDescriptorError::UnsafeTargetId)
    } else {
        Ok(normalized)
    }
}

fn safe_provider_url(value: &str) -> Result<String, GuardianArtifactDescriptorError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(GuardianArtifactDescriptorError::MissingProviderUrl);
    }
    let url =
        Url::parse(value).map_err(|_| GuardianArtifactDescriptorError::UnsupportedProviderUrl)?;
    if matches!(url.scheme(), "http" | "https") {
        Ok(value.to_string())
    } else {
        Err(GuardianArtifactDescriptorError::UnsupportedProviderUrl)
    }
}

fn safe_sha1(value: &str) -> Result<String, GuardianArtifactDescriptorError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(GuardianArtifactDescriptorError::MissingChecksum);
    }
    let checksum = DownloadChecksum::sha1(value);
    if valid_download_checksum_metadata(checksum) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err(GuardianArtifactDescriptorError::InvalidChecksum)
    }
}

fn bounded_max_bytes(value: u64) -> Result<u64, GuardianArtifactDescriptorError> {
    if value == 0 {
        return Err(GuardianArtifactDescriptorError::MissingMaxBytes);
    }
    if value > MAX_MINECRAFT_REPAIR_ARTIFACT_BYTES {
        return Err(GuardianArtifactDescriptorError::MaxBytesTooLarge);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianArtifactDescriptorError, GuardianMinecraftArtifactRepairDescriptor,
        MAX_MINECRAFT_REPAIR_ARTIFACT_BYTES,
    };
    use crate::state::contracts::{OwnershipClass, ReconciliationComponent};
    use axial_minecraft::download::{
        SelectedDownloadArtifactDescriptor, SelectedDownloadArtifactKind,
    };
    use sha1::{Digest, Sha1};
    use std::path::{Path, PathBuf};

    const ONE_MIB: u64 = 1 << 20;

    #[test]
    fn typed_library_descriptor_maps_to_guardian_repair_descriptor_and_redacts_debug() {
        let root = PathBuf::from("/tmp/axial/selected");
        let destination = root.join("libraries/example/library.jar");
        let checksum = sha1_hex(b"library");
        let core_descriptor = SelectedDownloadArtifactDescriptor::new(
            SelectedDownloadArtifactKind::Library,
            "minecraft_library_example",
            destination.clone(),
            "https://example.invalid/library.jar?token=secret",
            checksum.clone(),
            Some(7),
            ONE_MIB,
        );

        let descriptor = GuardianMinecraftArtifactRepairDescriptor::from_core_selected_descriptor(
            &core_descriptor,
        )
        .expect("guardian descriptor");

        assert_eq!(descriptor.target().id, "minecraft_library_example");
        assert!(
            descriptor
                .reconciliation_target()
                .id
                .starts_with("artifact.sha256.")
        );
        assert_ne!(descriptor.reconciliation_target(), descriptor.target());
        assert_eq!(
            descriptor.target().ownership,
            OwnershipClass::LauncherManaged
        );
        assert_eq!(descriptor.component(), ReconciliationComponent::Libraries);
        assert_eq!(descriptor.destination(), destination);
        let source = descriptor.repair_source();
        assert_eq!(source.checksum_algorithm, "sha1");
        assert_eq!(source.expected_checksum, checksum);
        assert_eq!(source.expected_size, Some(7));
        assert_eq!(source.max_bytes, Some(ONE_MIB));

        let debug = format!("{descriptor:?}").to_ascii_lowercase();
        assert!(!debug.contains(root.to_string_lossy().as_ref()));
        assert!(!debug.contains("example.invalid"));
        assert!(!debug.contains("token"));
        assert!(!debug.contains("secret"));
        assert!(!debug.contains(&checksum));
        assert!(debug.contains("minecraft_library_example"));
        assert!(debug.contains("sha1"));
    }

    #[test]
    fn typed_selected_descriptor_rejects_atomic_version_bundle_members() {
        for kind in [
            SelectedDownloadArtifactKind::VersionJson,
            SelectedDownloadArtifactKind::ClientJar,
            SelectedDownloadArtifactKind::LogConfig,
        ] {
            let descriptor = SelectedDownloadArtifactDescriptor::new(
                kind,
                "atomic_bundle_member",
                "/tmp/axial/selected/artifact",
                "https://example.invalid/artifact",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                Some(128),
                ONE_MIB,
            );

            assert_eq!(
                GuardianMinecraftArtifactRepairDescriptor::from_core_selected_descriptor(
                    &descriptor,
                )
                .expect_err("bundle members require atomic publication"),
                GuardianArtifactDescriptorError::UnsupportedAtomicBundleMember,
            );
        }
    }

    #[test]
    fn reconciliation_target_binds_destination_and_expected_content_without_path_copy() {
        let checksum = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let descriptor = |destination: &Path, checksum: &str| {
            GuardianMinecraftArtifactRepairDescriptor::from_core_selected_descriptor(
                &SelectedDownloadArtifactDescriptor::new(
                    SelectedDownloadArtifactKind::Library,
                    "minecraft_library_shared",
                    destination,
                    "https://example.invalid/shared.jar",
                    checksum,
                    Some(128),
                    ONE_MIB,
                ),
            )
            .expect("guardian descriptor")
        };
        let first = descriptor(Path::new("/managed/a/shared.jar"), checksum);
        let second = descriptor(Path::new("/managed/b/shared.jar"), checksum);
        let changed = descriptor(
            Path::new("/managed/a/shared.jar"),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );

        assert_eq!(first.target(), second.target());
        assert_ne!(
            first.reconciliation_target(),
            second.reconciliation_target()
        );
        assert_ne!(
            first.reconciliation_target(),
            changed.reconciliation_target()
        );
        for exact in [
            first.reconciliation_target(),
            second.reconciliation_target(),
        ] {
            assert_eq!(exact.id.len(), 87);
            assert!(!exact.id.contains("managed"));
            assert!(!exact.id.contains("shared"));
            assert!(!exact.id.contains(['/', '\\']));
        }
    }

    #[test]
    fn typed_selected_descriptor_rejects_unsafe_metadata_before_effects() {
        let destination = Path::new("/tmp/axial/artifact.jar");
        let checksum = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let cases = [
            (
                selected_descriptor(
                    "",
                    destination,
                    "https://example.invalid/artifact.jar",
                    checksum,
                    Some(128),
                    ONE_MIB,
                ),
                GuardianArtifactDescriptorError::MissingTargetId,
            ),
            (
                selected_descriptor("target", destination, "", checksum, Some(128), ONE_MIB),
                GuardianArtifactDescriptorError::MissingProviderUrl,
            ),
            (
                selected_descriptor(
                    "target",
                    destination,
                    "file:///tmp/artifact.jar",
                    checksum,
                    Some(128),
                    ONE_MIB,
                ),
                GuardianArtifactDescriptorError::UnsupportedProviderUrl,
            ),
            (
                selected_descriptor(
                    "target",
                    destination,
                    "https://example.invalid/artifact.jar",
                    "",
                    Some(128),
                    ONE_MIB,
                ),
                GuardianArtifactDescriptorError::MissingChecksum,
            ),
            (
                selected_descriptor(
                    "target",
                    destination,
                    "https://example.invalid/artifact.jar",
                    "-Xmx8192M",
                    Some(128),
                    ONE_MIB,
                ),
                GuardianArtifactDescriptorError::InvalidChecksum,
            ),
            (
                selected_descriptor(
                    "C:\\Users\\Alice\\artifact.jar",
                    destination,
                    "https://example.invalid/artifact.jar",
                    checksum,
                    Some(128),
                    ONE_MIB,
                ),
                GuardianArtifactDescriptorError::UnsafeTargetId,
            ),
            (
                selected_descriptor(
                    "target",
                    Path::new(""),
                    "https://example.invalid/artifact.jar",
                    checksum,
                    Some(128),
                    ONE_MIB,
                ),
                GuardianArtifactDescriptorError::MissingDestination,
            ),
            (
                selected_descriptor(
                    "target",
                    destination,
                    "https://example.invalid/artifact.jar",
                    checksum,
                    Some(128),
                    0,
                ),
                GuardianArtifactDescriptorError::MissingMaxBytes,
            ),
            (
                selected_descriptor(
                    "target",
                    destination,
                    "https://example.invalid/artifact.jar",
                    checksum,
                    Some(128),
                    MAX_MINECRAFT_REPAIR_ARTIFACT_BYTES + 1,
                ),
                GuardianArtifactDescriptorError::MaxBytesTooLarge,
            ),
            (
                selected_descriptor(
                    "target",
                    destination,
                    "https://example.invalid/artifact.jar",
                    checksum,
                    Some(2 * ONE_MIB),
                    ONE_MIB,
                ),
                GuardianArtifactDescriptorError::ExpectedSizeExceedsMaxBytes,
            ),
        ];

        for (descriptor, expected) in cases {
            assert_eq!(
                GuardianMinecraftArtifactRepairDescriptor::from_core_selected_descriptor(
                    &descriptor,
                )
                .expect_err("unsafe descriptor"),
                expected,
            );
        }
    }

    #[test]
    fn test_descriptor_constructor_rejects_unsupported_checksum_algorithm() {
        let error = GuardianMinecraftArtifactRepairDescriptor::for_test(
            crate::state::contracts::TargetDescriptor::new(
                crate::state::contracts::StabilizationSystem::Execution,
                crate::state::contracts::TargetKind::Artifact,
                "artifact",
                OwnershipClass::LauncherManaged,
            ),
            Path::new("/tmp/axial/artifact.jar"),
            "https://example.invalid/artifact.jar",
            "sha512",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Some(128),
            ONE_MIB,
        )
        .expect_err("unsupported checksum algorithm");

        assert_eq!(
            error,
            GuardianArtifactDescriptorError::UnsupportedChecksumAlgorithm
        );
    }

    fn selected_descriptor(
        target: &str,
        destination: &Path,
        provider_url: &str,
        checksum: &str,
        expected_size: Option<u64>,
        max_bytes: u64,
    ) -> SelectedDownloadArtifactDescriptor {
        SelectedDownloadArtifactDescriptor::new(
            SelectedDownloadArtifactKind::AssetObject,
            target,
            destination,
            provider_url,
            checksum,
            expected_size,
            max_bytes,
        )
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha1::digest(bytes))
    }
}

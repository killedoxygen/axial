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
    OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind, sanitize_target_id,
};
use axial_minecraft::download::SelectedDownloadArtifactDescriptor;
use std::fmt;
use std::path::{Path, PathBuf};
use url::Url;

const MAX_MINECRAFT_REPAIR_ARTIFACT_BYTES: u64 = 512 << 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GuardianArtifactDescriptorError {
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
    destination: PathBuf,
    source: GuardianMinecraftArtifactRepairSource,
}

impl GuardianMinecraftArtifactRepairDescriptor {
    pub(crate) fn from_core_selected_descriptor(
        descriptor: &SelectedDownloadArtifactDescriptor,
    ) -> Result<Self, GuardianArtifactDescriptorError> {
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

        Ok(Self {
            target: TargetDescriptor::new(
                StabilizationSystem::Execution,
                TargetKind::Artifact,
                target_id,
                OwnershipClass::LauncherManaged,
            ),
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

    pub(crate) fn destination(&self) -> &Path {
        &self.destination
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
        Ok(Self {
            target,
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

impl fmt::Debug for GuardianMinecraftArtifactRepairDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuardianMinecraftArtifactRepairDescriptor")
            .field("target", &self.target)
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
    use crate::state::contracts::OwnershipClass;
    use axial_minecraft::download::{
        SelectedDownloadArtifactDescriptor, SelectedDownloadArtifactKind,
    };
    use sha1::{Digest, Sha1};
    use std::path::{Path, PathBuf};

    const ONE_MIB: u64 = 1 << 20;

    #[test]
    fn typed_selected_descriptor_maps_to_guardian_repair_descriptor_and_redacts_debug() {
        let root = PathBuf::from("/tmp/axial/selected");
        let destination = root.join("logs/log4j2.xml");
        let checksum = sha1_hex(b"log config");
        let core_descriptor = SelectedDownloadArtifactDescriptor::new(
            SelectedDownloadArtifactKind::LogConfig,
            "log4j2.xml",
            destination.clone(),
            "https://example.invalid/log4j2.xml?token=secret",
            checksum.clone(),
            Some(10),
            ONE_MIB,
        );

        let descriptor = GuardianMinecraftArtifactRepairDescriptor::from_core_selected_descriptor(
            &core_descriptor,
        )
        .expect("guardian descriptor");

        assert_eq!(descriptor.target().id, "log4j2.xml");
        assert_eq!(
            descriptor.target().ownership,
            OwnershipClass::LauncherManaged
        );
        assert_eq!(descriptor.destination(), destination);
        let source = descriptor.repair_source();
        assert_eq!(source.checksum_algorithm, "sha1");
        assert_eq!(source.expected_checksum, checksum);
        assert_eq!(source.expected_size, Some(10));
        assert_eq!(source.max_bytes, Some(ONE_MIB));

        let debug = format!("{descriptor:?}").to_ascii_lowercase();
        assert!(!debug.contains(root.to_string_lossy().as_ref()));
        assert!(!debug.contains("example.invalid"));
        assert!(!debug.contains("token"));
        assert!(!debug.contains("secret"));
        assert!(!debug.contains(&checksum));
        assert!(debug.contains("log4j2.xml"));
        assert!(debug.contains("sha1"));
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

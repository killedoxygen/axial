//! Guardian artifact repair descriptors for Minecraft install metadata.
//!
//! Descriptors are inert backend values. They adapt already-selected metadata
//! into the source/destination shape required by Guardian artifact repair, but
//! they do not resolve providers, start downloads, or mutate files.

use super::GuardianArtifactRepairSource;
use crate::execution::download::{
    DownloadChecksum, DownloadChecksumAlgorithm, valid_download_checksum_metadata,
};
use crate::state::contracts::{
    OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind, sanitize_target_id,
};
use croopor_minecraft::download::{
    SelectedDownloadArtifactDescriptor, SelectedDownloadArtifactKind,
};
use croopor_minecraft::launch::{AssetIndex, DownloadEntry, LibraryArtifact};
use croopor_minecraft::manifest::ManifestEntry;
use std::fmt;
use std::path::{Path, PathBuf};
use url::Url;

pub const MAX_MINECRAFT_REPAIR_ARTIFACT_BYTES: u64 = 512 << 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianMinecraftArtifactKind {
    VersionJson,
    ClientJar,
    Library,
    AssetIndex,
    AssetObject,
    LogConfig,
    LoaderCache,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianArtifactDescriptorError {
    MissingDestination,
    MissingProviderUrl,
    UnsupportedProviderUrl,
    MissingChecksum,
    InvalidChecksum,
    MissingTargetId,
    UnsafeTargetId,
    InvalidExpectedSize,
    MissingMaxBytes,
    MaxBytesTooLarge,
    ExpectedSizeExceedsMaxBytes,
    UnsafeOwnership,
}

#[derive(Clone, Eq, PartialEq)]
pub struct GuardianMinecraftArtifactRepairDescriptor {
    kind: GuardianMinecraftArtifactKind,
    target: TargetDescriptor,
    destination: PathBuf,
    source: GuardianMinecraftArtifactRepairSource,
}

impl GuardianMinecraftArtifactRepairDescriptor {
    pub fn from_version_manifest_entry(
        entry: &ManifestEntry,
        destination: &Path,
        max_bytes: u64,
    ) -> Result<Self, GuardianArtifactDescriptorError> {
        let target_id = prefixed_target_id("minecraft_version_json", &entry.id)?;
        Self::from_selected_metadata(GuardianMinecraftArtifactRepairMetadata {
            kind: GuardianMinecraftArtifactKind::VersionJson,
            target_id: &target_id,
            destination,
            provider_url: &entry.url,
            sha1: &entry.sha1,
            expected_size: None,
            max_bytes,
            ownership: OwnershipClass::LauncherManaged,
        })
    }

    pub fn from_client_download(
        version_id: &str,
        destination: &Path,
        download: &DownloadEntry,
        max_bytes: u64,
    ) -> Result<Self, GuardianArtifactDescriptorError> {
        let target_id = prefixed_target_id("minecraft_client", version_id)?;
        Self::from_selected_metadata(GuardianMinecraftArtifactRepairMetadata {
            kind: GuardianMinecraftArtifactKind::ClientJar,
            target_id: &target_id,
            destination,
            provider_url: &download.url,
            sha1: &download.sha1,
            expected_size: expected_size_from_i64(download.size)?,
            max_bytes,
            ownership: OwnershipClass::LauncherManaged,
        })
    }

    pub fn from_library_artifact(
        library_id: &str,
        destination: &Path,
        artifact: &LibraryArtifact,
        max_bytes: u64,
    ) -> Result<Self, GuardianArtifactDescriptorError> {
        let target_id = prefixed_target_id("minecraft_library", library_id)?;
        Self::from_selected_metadata(GuardianMinecraftArtifactRepairMetadata {
            kind: GuardianMinecraftArtifactKind::Library,
            target_id: &target_id,
            destination,
            provider_url: &artifact.url,
            sha1: &artifact.sha1,
            expected_size: expected_size_from_i64(artifact.size)?,
            max_bytes,
            ownership: OwnershipClass::LauncherManaged,
        })
    }

    pub fn from_asset_index(
        asset_index: &AssetIndex,
        destination: &Path,
        max_bytes: u64,
    ) -> Result<Self, GuardianArtifactDescriptorError> {
        let target_id = prefixed_target_id("minecraft_asset_index", &asset_index.id)?;
        Self::from_selected_metadata(GuardianMinecraftArtifactRepairMetadata {
            kind: GuardianMinecraftArtifactKind::AssetIndex,
            target_id: &target_id,
            destination,
            provider_url: &asset_index.url,
            sha1: &asset_index.sha1,
            expected_size: expected_size_from_i64(asset_index.size)?,
            max_bytes,
            ownership: OwnershipClass::LauncherManaged,
        })
    }

    pub fn from_selected_metadata(
        metadata: GuardianMinecraftArtifactRepairMetadata<'_>,
    ) -> Result<Self, GuardianArtifactDescriptorError> {
        if metadata.destination.as_os_str().is_empty() {
            return Err(GuardianArtifactDescriptorError::MissingDestination);
        }
        let target_id = safe_target_id(metadata.target_id)?;
        let provider_url = safe_provider_url(metadata.provider_url)?;
        let sha1 = safe_sha1(metadata.sha1)?;
        let max_bytes = bounded_max_bytes(metadata.max_bytes)?;
        if let Some(expected_size) = metadata.expected_size
            && expected_size > max_bytes
        {
            return Err(GuardianArtifactDescriptorError::ExpectedSizeExceedsMaxBytes);
        }
        if metadata.ownership != OwnershipClass::LauncherManaged {
            return Err(GuardianArtifactDescriptorError::UnsafeOwnership);
        }

        Ok(Self {
            kind: metadata.kind,
            target: TargetDescriptor::new(
                StabilizationSystem::Execution,
                TargetKind::Artifact,
                target_id,
                OwnershipClass::LauncherManaged,
            ),
            destination: metadata.destination.to_path_buf(),
            source: GuardianMinecraftArtifactRepairSource {
                url: provider_url,
                checksum_algorithm: DownloadChecksumAlgorithm::Sha1,
                checksum: sha1,
                expected_size: metadata.expected_size,
                max_bytes,
            },
        })
    }

    pub fn from_core_selected_descriptor(
        descriptor: &SelectedDownloadArtifactDescriptor,
    ) -> Result<Self, GuardianArtifactDescriptorError> {
        Self::from_selected_metadata(GuardianMinecraftArtifactRepairMetadata {
            kind: guardian_kind_from_core_kind(descriptor.kind),
            target_id: &descriptor.target,
            destination: descriptor.destination(),
            provider_url: descriptor.provider_url(),
            sha1: descriptor.sha1(),
            expected_size: descriptor.expected_size,
            max_bytes: descriptor.max_bytes,
            ownership: OwnershipClass::LauncherManaged,
        })
    }

    pub fn kind(&self) -> GuardianMinecraftArtifactKind {
        self.kind
    }

    pub fn target(&self) -> &TargetDescriptor {
        &self.target
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }

    pub fn repair_source(&self) -> GuardianArtifactRepairSource<'_> {
        GuardianArtifactRepairSource {
            url: &self.source.url,
            checksum_algorithm: self.source.checksum_algorithm.as_str(),
            expected_checksum: &self.source.checksum,
            expected_size: self.source.expected_size,
            max_bytes: Some(self.source.max_bytes),
        }
    }
}

fn guardian_kind_from_core_kind(
    kind: SelectedDownloadArtifactKind,
) -> GuardianMinecraftArtifactKind {
    match kind {
        SelectedDownloadArtifactKind::VersionJson => GuardianMinecraftArtifactKind::VersionJson,
        SelectedDownloadArtifactKind::ClientJar => GuardianMinecraftArtifactKind::ClientJar,
        SelectedDownloadArtifactKind::Library => GuardianMinecraftArtifactKind::Library,
        SelectedDownloadArtifactKind::AssetIndex => GuardianMinecraftArtifactKind::AssetIndex,
        SelectedDownloadArtifactKind::AssetObject => GuardianMinecraftArtifactKind::AssetObject,
        SelectedDownloadArtifactKind::LogConfig => GuardianMinecraftArtifactKind::LogConfig,
    }
}

impl fmt::Debug for GuardianMinecraftArtifactRepairDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuardianMinecraftArtifactRepairDescriptor")
            .field("kind", &self.kind)
            .field("target", &self.target)
            .field("destination", &"<redacted>")
            .field("source", &self.source)
            .finish()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct GuardianMinecraftArtifactRepairMetadata<'a> {
    pub kind: GuardianMinecraftArtifactKind,
    pub target_id: &'a str,
    pub destination: &'a Path,
    pub provider_url: &'a str,
    pub sha1: &'a str,
    pub expected_size: Option<u64>,
    pub max_bytes: u64,
    pub ownership: OwnershipClass,
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

fn expected_size_from_i64(value: i64) -> Result<Option<u64>, GuardianArtifactDescriptorError> {
    if value < 0 {
        return Err(GuardianArtifactDescriptorError::InvalidExpectedSize);
    }
    Ok(u64::try_from(value).ok().filter(|value| *value > 0))
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

fn prefixed_target_id(
    prefix: &'static str,
    value: &str,
) -> Result<String, GuardianArtifactDescriptorError> {
    let value = safe_target_id(value)?;
    safe_target_id(&format!("{prefix}_{value}"))
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
        GuardianArtifactDescriptorError, GuardianMinecraftArtifactKind,
        GuardianMinecraftArtifactRepairDescriptor, GuardianMinecraftArtifactRepairMetadata,
        MAX_MINECRAFT_REPAIR_ARTIFACT_BYTES,
    };
    use crate::state::contracts::OwnershipClass;
    use croopor_minecraft::download::{
        SelectedDownloadArtifactDescriptor, SelectedDownloadArtifactKind,
    };
    use croopor_minecraft::launch::{AssetIndex, DownloadEntry, LibraryArtifact};
    use sha1::{Digest, Sha1};
    use std::path::{Path, PathBuf};

    const ONE_MIB: u64 = 1 << 20;

    #[test]
    fn client_download_descriptor_maps_to_repair_source_inputs() {
        let destination = Path::new("/tmp/croopor/versions/1.21.5/1.21.5.jar");
        let body = b"client jar";
        let descriptor = GuardianMinecraftArtifactRepairDescriptor::from_client_download(
            "1.21.5",
            destination,
            &DownloadEntry {
                sha1: sha1_hex(body),
                size: body.len() as i64,
                url: "https://piston-data.mojang.com/v1/objects/client.jar".to_string(),
            },
            ONE_MIB,
        )
        .expect("descriptor");

        assert_eq!(descriptor.kind(), GuardianMinecraftArtifactKind::ClientJar);
        assert_eq!(descriptor.target().id, "minecraft_client_1.21.5");
        assert_eq!(
            descriptor.target().ownership,
            OwnershipClass::LauncherManaged
        );
        assert_eq!(descriptor.destination(), destination);

        let source = descriptor.repair_source();
        assert_eq!(
            source.url,
            "https://piston-data.mojang.com/v1/objects/client.jar"
        );
        assert_eq!(source.checksum_algorithm, "sha1");
        assert_eq!(source.expected_checksum, sha1_hex(body));
        assert_eq!(source.expected_size, Some(body.len() as u64));
        assert_eq!(source.max_bytes, Some(ONE_MIB));
    }

    #[test]
    fn library_artifact_descriptor_maps_non_client_metadata() {
        let destination = Path::new("/tmp/croopor/libraries/org/example/lib/1.0.0/lib-1.0.0.jar");
        let descriptor = GuardianMinecraftArtifactRepairDescriptor::from_library_artifact(
            "org.example.lib.1.0.0",
            destination,
            &LibraryArtifact {
                path: "org/example/lib/1.0.0/lib-1.0.0.jar".to_string(),
                sha1: sha1_hex(b"library"),
                size: 7,
                url: "https://libraries.minecraft.net/org/example/lib/1.0.0/lib-1.0.0.jar"
                    .to_string(),
            },
            ONE_MIB,
        )
        .expect("descriptor");

        assert_eq!(descriptor.kind(), GuardianMinecraftArtifactKind::Library);
        assert_eq!(
            descriptor.target().id,
            "minecraft_library_org.example.lib.1.0.0"
        );
        let source = descriptor.repair_source();
        assert_eq!(source.checksum_algorithm, "sha1");
        assert_eq!(source.expected_size, Some(7));
        assert_eq!(source.max_bytes, Some(ONE_MIB));
    }

    #[test]
    fn asset_index_descriptor_maps_asset_metadata() {
        let destination = Path::new("/tmp/croopor/assets/indexes/17.json");
        let descriptor = GuardianMinecraftArtifactRepairDescriptor::from_asset_index(
            &AssetIndex {
                id: "17".to_string(),
                sha1: sha1_hex(b"asset index"),
                size: 11,
                total_size: 99,
                url: "https://piston-meta.mojang.com/v1/packages/assets/17.json".to_string(),
            },
            destination,
            ONE_MIB,
        )
        .expect("descriptor");

        assert_eq!(descriptor.kind(), GuardianMinecraftArtifactKind::AssetIndex);
        assert_eq!(descriptor.target().id, "minecraft_asset_index_17");
        assert_eq!(descriptor.repair_source().expected_size, Some(11));
    }

    #[test]
    fn descriptor_rejects_unsafe_or_incomplete_metadata_before_effects() {
        let destination = Path::new("/tmp/croopor/artifact.jar");
        assert_eq!(
            selected_metadata("target", destination)
                .with_provider_url("")
                .build()
                .expect_err("missing url"),
            GuardianArtifactDescriptorError::MissingProviderUrl
        );
        assert_eq!(
            selected_metadata("target", destination)
                .with_provider_url("file:///tmp/artifact.jar")
                .build()
                .expect_err("unsupported url"),
            GuardianArtifactDescriptorError::UnsupportedProviderUrl
        );
        assert_eq!(
            selected_metadata("target", destination)
                .with_sha1("-Xmx8192M")
                .build()
                .expect_err("invalid checksum"),
            GuardianArtifactDescriptorError::InvalidChecksum
        );
        assert_eq!(
            selected_metadata("C:\\Users\\Alice\\artifact.jar", destination)
                .build()
                .expect_err("unsafe target"),
            GuardianArtifactDescriptorError::UnsafeTargetId
        );
        assert_eq!(
            selected_metadata("target", Path::new(""))
                .build()
                .expect_err("missing destination"),
            GuardianArtifactDescriptorError::MissingDestination
        );
        assert_eq!(
            selected_metadata("target", destination)
                .with_max_bytes(0)
                .build()
                .expect_err("missing max bytes"),
            GuardianArtifactDescriptorError::MissingMaxBytes
        );
        assert_eq!(
            selected_metadata("target", destination)
                .with_max_bytes(MAX_MINECRAFT_REPAIR_ARTIFACT_BYTES + 1)
                .build()
                .expect_err("too large max bytes"),
            GuardianArtifactDescriptorError::MaxBytesTooLarge
        );
        assert_eq!(
            selected_metadata("target", destination)
                .with_expected_size(Some(2 * ONE_MIB))
                .build()
                .expect_err("size exceeds bound"),
            GuardianArtifactDescriptorError::ExpectedSizeExceedsMaxBytes
        );
        assert_eq!(
            selected_metadata("target", destination)
                .with_ownership(OwnershipClass::UserOwned)
                .build()
                .expect_err("unsafe ownership"),
            GuardianArtifactDescriptorError::UnsafeOwnership
        );
    }

    #[test]
    fn descriptor_debug_output_is_redacted() {
        let root = PathBuf::from("/tmp/croopor/redaction");
        let destination = root.join("artifact.jar");
        let checksum = sha1_hex(b"artifact");
        let descriptor = selected_metadata("artifact_target", &destination)
            .with_provider_url("https://example.invalid/artifact.jar?token=secret")
            .with_sha1(&checksum)
            .build()
            .expect("descriptor");

        let debug = format!("{descriptor:?}").to_ascii_lowercase();
        assert!(!debug.contains(root.to_string_lossy().as_ref()));
        assert!(!debug.contains("example.invalid"));
        assert!(!debug.contains("token"));
        assert!(!debug.contains("secret"));
        assert!(!debug.contains(&checksum));
        assert!(debug.contains("artifact_target"));
        assert!(debug.contains("sha1"));
    }

    #[test]
    fn core_selected_descriptor_maps_to_guardian_repair_descriptor() {
        let root = PathBuf::from("/tmp/croopor/selected");
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

        assert_eq!(descriptor.kind(), GuardianMinecraftArtifactKind::LogConfig);
        assert_eq!(descriptor.target().id, "log4j2.xml");
        assert_eq!(
            descriptor.target().ownership,
            OwnershipClass::LauncherManaged
        );
        assert_eq!(descriptor.destination(), destination);
        assert_eq!(descriptor.repair_source().checksum_algorithm, "sha1");
        assert_eq!(descriptor.repair_source().expected_checksum, checksum);

        let debug = format!("{descriptor:?}").to_ascii_lowercase();
        assert!(!debug.contains(root.to_string_lossy().as_ref()));
        assert!(!debug.contains("example.invalid"));
        assert!(!debug.contains("token"));
        assert!(!debug.contains("secret"));
        assert!(!debug.contains(&checksum));
    }

    fn selected_metadata<'a>(
        target_id: &'a str,
        destination: &'a Path,
    ) -> SelectedMetadataBuilder<'a> {
        SelectedMetadataBuilder {
            target_id,
            destination,
            provider_url: "https://example.invalid/artifact.bin",
            sha1: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            expected_size: Some(128),
            max_bytes: ONE_MIB,
            ownership: OwnershipClass::LauncherManaged,
        }
    }

    struct SelectedMetadataBuilder<'a> {
        target_id: &'a str,
        destination: &'a Path,
        provider_url: &'a str,
        sha1: &'a str,
        expected_size: Option<u64>,
        max_bytes: u64,
        ownership: OwnershipClass,
    }

    impl<'a> SelectedMetadataBuilder<'a> {
        fn with_provider_url(mut self, provider_url: &'a str) -> Self {
            self.provider_url = provider_url;
            self
        }

        fn with_sha1(mut self, sha1: &'a str) -> Self {
            self.sha1 = sha1;
            self
        }

        fn with_expected_size(mut self, expected_size: Option<u64>) -> Self {
            self.expected_size = expected_size;
            self
        }

        fn with_max_bytes(mut self, max_bytes: u64) -> Self {
            self.max_bytes = max_bytes;
            self
        }

        fn with_ownership(mut self, ownership: OwnershipClass) -> Self {
            self.ownership = ownership;
            self
        }

        fn build(
            self,
        ) -> Result<GuardianMinecraftArtifactRepairDescriptor, GuardianArtifactDescriptorError>
        {
            GuardianMinecraftArtifactRepairDescriptor::from_selected_metadata(
                GuardianMinecraftArtifactRepairMetadata {
                    kind: GuardianMinecraftArtifactKind::AssetObject,
                    target_id: self.target_id,
                    destination: self.destination,
                    provider_url: self.provider_url,
                    sha1: self.sha1,
                    expected_size: self.expected_size,
                    max_bytes: self.max_bytes,
                    ownership: self.ownership,
                },
            )
        }
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha1::digest(bytes))
    }
}

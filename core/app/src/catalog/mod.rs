//! Minecraft catalog projections and exact provider descriptors.
//!
//! Structural metadata, installed scans, and loader identities remain the proven Minecraft leaf.
//! This boundary owns freshness and safe transport errors, without a second version parser.

mod installed;
mod model;

pub use installed::{VersionScanViewModel, VersionsResponse, installed_versions};
pub use model::{
    CatalogError, CatalogFailure, CatalogSnapshot, CatalogState, CatalogStateId, VersionDescriptor,
};

use crate::network::{
    Checksum, DownloadError, DownloadRequest, DownloadedBytes, HashAlgorithm, IntegrityPolicy,
    OriginPolicy, ProviderClient, ResponseLimits, UnhashedReason,
};
use crate::tasks::CancellationToken;
use axial_minecraft::managed_path::ManagedLibraryOperation;
use axial_minecraft::portable_path::{MAX_PORTABLE_FILE_NAME_BYTES, PortableFileName};
use axial_minecraft::{
    LoaderGameVersion, VersionJson, VersionManifest, enrich_loader_game_versions,
    manifest_release_references,
};
use model::{MAX_MANIFEST_BYTES, MAX_VERSION_BYTES, decode_manifest, snapshot_from};
use std::collections::{BTreeMap, BTreeSet, HashMap};

const MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const METADATA_ORIGINS: [&str; 3] = [
    "https://piston-meta.mojang.com",
    "https://launchermeta.mojang.com",
    "https://launcher.mojang.com",
];
const MAX_SELECTION_IDS: usize = 4096;
const MAX_SELECTION_ID_BYTES: usize = MAX_PORTABLE_FILE_NAME_BYTES - ".json".len();

pub struct Catalog {
    client: ProviderClient,
    #[cfg(test)]
    source_fixture: Option<(String, OriginPolicy)>,
}

struct ManifestLookup {
    manifest: VersionManifest,
    fresh: bool,
    cache_hit: bool,
    failure: Option<CatalogFailure>,
}

impl Catalog {
    pub fn new(client: ProviderClient) -> Self {
        Self {
            client,
            #[cfg(test)]
            source_fixture: None,
        }
    }

    pub async fn snapshot(
        &self,
        operation: &ManagedLibraryOperation,
        cancel: &CancellationToken,
    ) -> CatalogSnapshot {
        match self.load_manifest(operation, cancel).await {
            Ok(lookup) => snapshot_from(
                Some(&lookup.manifest),
                lookup.fresh,
                lookup.cache_hit,
                lookup.failure,
            ),
            Err(error) => snapshot_from(None, false, false, Some(error.failure())),
        }
    }

    /// Offline enrichment never waits on a network request or modifies the cache.
    pub async fn cached_snapshot(&self, operation: &ManagedLibraryOperation) -> CatalogSnapshot {
        match read_cache(operation).await {
            Ok((manifest, fresh)) => snapshot_from(Some(&manifest), fresh, true, None),
            Err(error) => snapshot_from(None, false, false, Some(error.failure())),
        }
    }

    /// Selection for a new install must be supported by a current catalog. The downloader still
    /// owns final authenticated installation; this descriptor is not a publication capability.
    pub async fn resolve_install(
        &self,
        operation: &ManagedLibraryOperation,
        id: &str,
        cancel: &CancellationToken,
    ) -> Result<VersionDescriptor, CatalogError> {
        let lookup = self.load_manifest(operation, cancel).await?;
        if !lookup.fresh {
            return Err(CatalogError::Unavailable);
        }
        lookup
            .manifest
            .versions
            .into_iter()
            .find(|entry| entry.id == id)
            .map(VersionDescriptor::new)
            .ok_or(CatalogError::UnknownVersion)
    }

    /// Resolve requested selections against one fresh provider manifest without touching the
    /// library or its cache. Missing IDs stay absent; descriptors do not prove installed files.
    pub(crate) async fn resolve_fresh_selections(
        &self,
        ids: &BTreeSet<String>,
        cancel: &CancellationToken,
    ) -> Result<BTreeMap<String, VersionDescriptor>, CatalogError> {
        if cancel.is_cancelled() {
            return Err(CatalogError::Cancelled);
        }
        if ids.len() > MAX_SELECTION_IDS || ids.iter().any(|id| id.len() > MAX_SELECTION_ID_BYTES) {
            return Err(CatalogError::Malformed);
        }
        for id in ids {
            PortableFileName::new_exact(id)
                .and_then(|name| name.with_suffix(".json"))
                .map_err(|_| CatalogError::Malformed)?;
        }
        if ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let (manifest, _) = self.fetch_manifest(cancel).await?;
        if cancel.is_cancelled() {
            return Err(CatalogError::Cancelled);
        }
        Ok(manifest
            .versions
            .into_iter()
            .filter(|entry| ids.contains(&entry.id))
            .map(|entry| (entry.id.clone(), VersionDescriptor::new(entry)))
            .collect())
    }

    pub async fn fetch_version(
        &self,
        descriptor: &VersionDescriptor,
        cancel: &CancellationToken,
    ) -> Result<VersionJson, CatalogError> {
        let checksum = Checksum::from_hex(HashAlgorithm::Sha1, descriptor.metadata_sha1())
            .map_err(|_| CatalogError::Malformed)?;
        let source = (descriptor.metadata_url().to_owned(), metadata_origins()?);
        #[cfg(test)]
        let source = self.source_fixture.clone().unwrap_or(source);
        let request = DownloadRequest::get(
            source.0,
            source.1,
            ResponseLimits::new(MAX_VERSION_BYTES as u64, MAX_VERSION_BYTES as u64),
            IntegrityPolicy::Checksum {
                checksum,
                expected_size: None,
            },
        );
        let bytes = self.fetch_with_retries(request, cancel).await?;
        decode_version(descriptor, bytes.bytes())
    }

    /// Loader providers own stable hints and build availability; Minecraft interpretation and
    /// ordering are the same leaf functions used for Vanilla and installed records.
    pub async fn enrich_loader_versions(
        &self,
        operation: &ManagedLibraryOperation,
        versions: &mut Vec<LoaderGameVersion>,
        cancel: &CancellationToken,
    ) -> CatalogState {
        let lookup = self.load_manifest(operation, cancel).await;
        let state = match &lookup {
            Ok(lookup) => {
                snapshot_from(
                    Some(&lookup.manifest),
                    lookup.fresh,
                    lookup.cache_hit,
                    lookup.failure,
                )
                .catalog_state
            }
            Err(error) => snapshot_from(None, false, false, Some(error.failure())).catalog_state,
        };
        let mut order = None;
        if let Ok(lookup) = lookup {
            let releases = manifest_release_references(&lookup.manifest);
            enrich_loader_game_versions(versions, &lookup.manifest.versions, &releases);
            order = Some(
                lookup
                    .manifest
                    .versions
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| (entry.id.clone(), index))
                    .collect::<HashMap<_, _>>(),
            );
        } else {
            enrich_loader_game_versions(versions, &[], &[]);
        }
        *versions = axial_minecraft::loaders::index::normalize_supported_versions(
            std::mem::take(versions),
            order.as_ref(),
        );
        state
    }

    async fn load_manifest(
        &self,
        operation: &ManagedLibraryOperation,
        cancel: &CancellationToken,
    ) -> Result<ManifestLookup, CatalogError> {
        if cancel.is_cancelled() {
            return Err(CatalogError::Cancelled);
        }
        operation
            .revalidate()
            .map_err(|_| CatalogError::Unavailable)?;
        let cached = read_cache(operation).await;
        if let Ok((manifest, true)) = &cached {
            return Ok(ManifestLookup {
                manifest: manifest.clone(),
                fresh: true,
                cache_hit: true,
                failure: None,
            });
        }
        let live = self.fetch_manifest(cancel).await;
        match live {
            Ok((manifest, bytes)) => {
                operation
                    .revalidate()
                    .map_err(|_| CatalogError::Unavailable)?;
                if cancel.is_cancelled() {
                    return Err(CatalogError::Cancelled);
                }
                // Publication is an owned atomic cache write. Once it starts, finish it rather
                // than reporting cancellation while the cache silently commits afterwards.
                let cache_write =
                    axial_minecraft::manifest::cache_version_manifest(operation, &bytes).await;
                Ok(ManifestLookup {
                    manifest,
                    fresh: true,
                    cache_hit: false,
                    failure: cache_write.err().map(|_| CatalogFailure::CacheUnavailable),
                })
            }
            Err(CatalogError::Cancelled) => Err(CatalogError::Cancelled),
            Err(error) => match cached {
                Ok((manifest, _)) => Ok(ManifestLookup {
                    manifest,
                    fresh: false,
                    cache_hit: true,
                    failure: Some(error.failure()),
                }),
                Err(CatalogError::Malformed) if error == CatalogError::Unavailable => {
                    Err(CatalogError::Malformed)
                }
                Err(_) => Err(error),
            },
        }
    }

    async fn fetch_manifest(
        &self,
        cancel: &CancellationToken,
    ) -> Result<(VersionManifest, Vec<u8>), CatalogError> {
        let source = (MANIFEST_URL.to_owned(), metadata_origins()?);
        #[cfg(test)]
        let source = self.source_fixture.clone().unwrap_or(source);
        let request = DownloadRequest::get(
            source.0,
            source.1,
            ResponseLimits::new(MAX_MANIFEST_BYTES as u64, MAX_MANIFEST_BYTES as u64),
            IntegrityPolicy::Unhashed {
                reason: UnhashedReason::ProviderMetadata,
                expected_size: None,
            },
        );
        let bytes = self.fetch_with_retries(request, cancel).await?.into_bytes();
        let manifest = decode_manifest(&bytes)?;
        Ok((manifest, bytes))
    }

    async fn fetch_with_retries(
        &self,
        request: DownloadRequest,
        cancel: &CancellationToken,
    ) -> Result<DownloadedBytes, CatalogError> {
        const RETRY_DELAYS_MILLIS: [u64; 3] = [500, 1_500, 4_000];
        for attempt in 0..=RETRY_DELAYS_MILLIS.len() {
            // HTTP decoding retains bounded scratch buffers across yields.
            // Keep that provider future off every catalog/API caller's stack,
            // which also needs room for capability-backed cache publication.
            match Box::pin(self.client.fetch(request.clone(), cancel)).await {
                Ok(bytes) => return Ok(bytes),
                Err(error) if error.is_retryable() && attempt < RETRY_DELAYS_MILLIS.len() => {
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(CatalogError::Cancelled),
                        _ = tokio::time::sleep(std::time::Duration::from_millis(RETRY_DELAYS_MILLIS[attempt])) => {}
                    }
                }
                Err(error) => return Err(download_error(error)),
            }
        }
        Err(CatalogError::Unavailable)
    }
}

fn download_error(error: DownloadError) -> CatalogError {
    match error {
        DownloadError::Cancelled => CatalogError::Cancelled,
        DownloadError::EncodedLimitExceeded { .. }
        | DownloadError::DecodedLimitExceeded { .. }
        | DownloadError::InvalidEncoding
        | DownloadError::UnsupportedEncoding
        | DownloadError::SizeMismatch
        | DownloadError::ChecksumMismatch => CatalogError::Malformed,
        _ => CatalogError::Unavailable,
    }
}

fn metadata_origins() -> Result<OriginPolicy, CatalogError> {
    OriginPolicy::https(METADATA_ORIGINS, 3).map_err(|_| CatalogError::Unavailable)
}

async fn read_cache(
    operation: &ManagedLibraryOperation,
) -> Result<(VersionManifest, bool), CatalogError> {
    let operation = operation.clone();
    tokio::task::spawn_blocking(move || {
        let (bytes, fresh) = axial_minecraft::manifest::read_cached_manifest_bytes(&operation)
            .map_err(|_| CatalogError::Unavailable)?;
        Ok((decode_manifest(&bytes)?, fresh))
    })
    .await
    .map_err(|_| CatalogError::Unavailable)?
}

fn decode_version(
    descriptor: &VersionDescriptor,
    bytes: &[u8],
) -> Result<VersionJson, CatalogError> {
    if bytes.len() > MAX_VERSION_BYTES {
        return Err(CatalogError::Malformed);
    }
    let version: VersionJson =
        serde_json::from_slice(bytes).map_err(|_| CatalogError::Malformed)?;
    if version.id != descriptor.id() || version.materialized || !version.inherits_from.is_empty() {
        return Err(CatalogError::Malformed);
    }
    Ok(version)
}

#[cfg(test)]
pub(crate) mod tests;

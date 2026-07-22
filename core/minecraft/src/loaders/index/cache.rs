use crate::loaders::types::{
    CachedCatalog, LOADER_CATALOG_SCHEMA_VERSION, LoaderAvailability, LoaderCatalogState,
    LoaderError, LoaderProviderFailureKind,
};
use crate::managed_fs::{ManagedDir, ManagedDirectoryIdentity, ManagedLibraryOperation};
use crate::portable_path::PortableFileName;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const CACHE_PERSIST_WARNING: &str =
    "Loader catalog is available, but the offline cache could not be updated.";
const MAX_LOADER_CATALOG_CACHE_BYTES: u64 = 16 << 20;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CatalogCacheKey {
    library: ManagedDirectoryIdentity,
    name: PortableFileName,
}

static MEMORY_CATALOG_CACHE: OnceLock<
    Mutex<HashMap<CatalogCacheKey, CachedCatalog<serde_json::Value>>>,
> = OnceLock::new();

pub async fn resolve_cached<T, F, Fut>(
    operation: &ManagedLibraryOperation,
    cache_name: PortableFileName,
    ttl: Duration,
    live_fetch: F,
) -> Result<(T, LoaderCatalogState), LoaderError>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Clone,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, LoaderError>>,
{
    if let Some(cached) = resolve_fresh_cached::<T>(operation, cache_name.clone(), ttl) {
        return Ok(cached);
    }

    let checked_at_ms = Utc::now().timestamp_millis();
    let cached = read_cache::<T>(operation, &cache_name);
    match live_fetch().await {
        Ok(value) => {
            let cached = CachedCatalog::new(value.clone());
            let cache_error = write_cache(operation, &cache_name, &cached)
                .await
                .err()
                .map(|_| CACHE_PERSIST_WARNING.to_string());
            update_memory_cache(operation, &cache_name, &cached);
            Ok((
                value,
                LoaderCatalogState {
                    availability: LoaderAvailability {
                        fresh: true,
                        stale: false,
                        cache_hit: false,
                        checked_at_ms,
                        last_success_at_ms: Some(cached.fetched_at_ms),
                        last_error: cache_error,
                        last_failure_kind: None,
                    },
                },
            ))
        }
        Err(error) => {
            let Some(failure_kind) = error.availability_failure_kind() else {
                return Err(error);
            };
            let last_error = failure_kind.as_str().to_string();
            let provider_failure_kind = error.provider_failure_kind().or_else(|| {
                matches!(error, LoaderError::ArtifactMissing(_))
                    .then_some(LoaderProviderFailureKind::HttpNotFound)
            });
            let provider_status = error
                .provider_status()
                .or_else(|| matches!(error, LoaderError::ArtifactMissing(_)).then_some(404));
            if let Ok(cached) = cached {
                update_memory_cache(operation, &cache_name, &cached);
                return Ok((
                    cached.value,
                    LoaderCatalogState {
                        availability: LoaderAvailability {
                            fresh: false,
                            stale: true,
                            cache_hit: true,
                            checked_at_ms,
                            last_success_at_ms: Some(cached.fetched_at_ms),
                            last_error: Some(last_error),
                            last_failure_kind: Some(failure_kind),
                        },
                    },
                ));
            }
            Err(LoaderError::CatalogUnavailable {
                message: last_error,
                provider_failure_kind,
                provider_status,
            })
        }
    }
}

pub fn resolve_fresh_cached<T>(
    operation: &ManagedLibraryOperation,
    cache_name: PortableFileName,
    ttl: Duration,
) -> Option<(T, LoaderCatalogState)>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let checked_at_ms = Utc::now().timestamp_millis();
    let cached = fresh_memory_cache::<T>(operation, &cache_name, ttl).or_else(|| {
        let cached = read_cache::<T>(operation, &cache_name).ok()?;
        if !is_cache_fresh(cached.fetched_at_ms, ttl) {
            return None;
        }
        update_memory_cache(operation, &cache_name, &cached);
        Some(cached)
    })?;
    Some((
        cached.value,
        fresh_cache_state(checked_at_ms, cached.fetched_at_ms),
    ))
}

fn fresh_cache_state(checked_at_ms: i64, fetched_at_ms: i64) -> LoaderCatalogState {
    LoaderCatalogState {
        availability: LoaderAvailability {
            fresh: true,
            stale: false,
            cache_hit: true,
            checked_at_ms,
            last_success_at_ms: Some(fetched_at_ms),
            last_error: None,
            last_failure_kind: None,
        },
    }
}

fn fresh_memory_cache<T>(
    operation: &ManagedLibraryOperation,
    name: &PortableFileName,
    ttl: Duration,
) -> Option<CachedCatalog<T>>
where
    T: serde::de::DeserializeOwned,
{
    let key = catalog_cache_key(operation, name).ok()?;
    let cache = MEMORY_CATALOG_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()?;
    let cached = cache.get(&key)?;
    if cached.schema_version != LOADER_CATALOG_SCHEMA_VERSION
        || !is_cache_fresh(cached.fetched_at_ms, ttl)
    {
        return None;
    }
    let value = serde_json::from_value::<T>(cached.value.clone()).ok()?;
    Some(CachedCatalog {
        schema_version: cached.schema_version,
        fetched_at_ms: cached.fetched_at_ms,
        value,
    })
}

fn update_memory_cache<T>(
    operation: &ManagedLibraryOperation,
    name: &PortableFileName,
    cached: &CachedCatalog<T>,
)
where
    T: serde::Serialize,
{
    let Ok(key) = catalog_cache_key(operation, name) else {
        return;
    };
    let Ok(value) = serde_json::to_value(&cached.value) else {
        return;
    };
    let Some(mut cache) = MEMORY_CATALOG_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()
    else {
        return;
    };
    cache.insert(
        key,
        CachedCatalog {
            schema_version: cached.schema_version,
            fetched_at_ms: cached.fetched_at_ms,
            value,
        },
    );
}

fn read_cache<T>(
    operation: &ManagedLibraryOperation,
    name: &PortableFileName,
) -> Result<CachedCatalog<T>, LoaderError>
where
    T: serde::de::DeserializeOwned,
{
    let directory = open_loader_catalog(operation)?;
    let guard = directory
        .inspect_regular_file(name.as_str())?
        .ok_or(LoaderError::CatalogStale)?;
    let data = directory.read_guarded_file_bounded(
        name.as_str(),
        &guard,
        MAX_LOADER_CATALOG_CACHE_BYTES,
    )?;
    let cached = serde_json::from_slice::<CachedCatalog<T>>(&data)?;
    if cached.schema_version != LOADER_CATALOG_SCHEMA_VERSION {
        return Err(LoaderError::CatalogStale);
    }
    Ok(cached)
}

async fn write_cache<T>(
    operation: &ManagedLibraryOperation,
    name: &PortableFileName,
    value: &CachedCatalog<T>,
) -> Result<(), LoaderError>
where
    T: serde::Serialize,
{
    let data = serialize_cache(value)?;
    open_or_create_loader_catalog(operation)?
        .write_exact(name.as_str(), &data)
        .await
}

fn serialize_cache<T>(value: &CachedCatalog<T>) -> Result<Vec<u8>, LoaderError>
where
    T: serde::Serialize,
{
    let data = serde_json::to_vec(value)?;
    if data.len() as u64 > MAX_LOADER_CATALOG_CACHE_BYTES {
        return Err(LoaderError::Verify(
            "loader catalog cache exceeds its byte limit".to_string(),
        ));
    }
    Ok(data)
}

fn catalog_cache_key(
    operation: &ManagedLibraryOperation,
    name: &PortableFileName,
) -> Result<CatalogCacheKey, LoaderError> {
    Ok(CatalogCacheKey {
        library: operation.managed_directory()?.identity()?,
        name: name.clone(),
    })
}

fn open_loader_catalog(operation: &ManagedLibraryOperation) -> Result<ManagedDir, LoaderError> {
    operation
        .managed_directory()?
        .open_child("cache")?
        .open_child("loaders")?
        .open_child("catalog")
}

fn open_or_create_loader_catalog(
    operation: &ManagedLibraryOperation,
) -> Result<ManagedDir, LoaderError> {
    operation
        .managed_directory()?
        .open_or_create_child("cache")?
        .open_or_create_child("loaders")?
        .open_or_create_child("catalog")
}

fn is_cache_fresh(fetched_at_ms: i64, ttl: Duration) -> bool {
    let now = Utc::now().timestamp_millis();
    fetched_at_ms >= 0
        && fetched_at_ms <= now
        && (now - fetched_at_ms) as u128 < ttl.as_millis()
}

#[cfg(feature = "test-support")]
pub(crate) fn write_cache_fixture<T>(
    operation: &ManagedLibraryOperation,
    name: &PortableFileName,
    value: &CachedCatalog<T>,
) -> Result<(), LoaderError>
where
    T: serde::Serialize,
{
    let data = serialize_cache(value)?;
    open_or_create_loader_catalog(operation)?
        .write_exact_fixture(name.as_str(), &data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_fs::{ManagedLibraryOperation, ManagedLibraryRoot};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn live_fetch_success_survives_cache_write_failure() {
        let library = TestLibrary::new("live-cache-write-failure");
        let name = cache_name();
        open_or_create_loader_catalog(library.operation())
            .expect("open loader catalog")
            .open_or_create_child(name.as_str())
            .expect("create directory at cache destination");

        let (value, state) = resolve_cached(
            library.operation(),
            name,
            Duration::ZERO,
            || async { Ok::<_, LoaderError>(vec!["live".to_string()]) },
        )
        .await
        .expect("live value should win over cache persistence failure");

        assert_eq!(value, vec!["live".to_string()]);
        assert!(state.availability.fresh);
        assert!(!state.availability.stale);
        assert!(!state.availability.cache_hit);
        assert_eq!(
            state.availability.last_error.as_deref(),
            Some(CACHE_PERSIST_WARNING)
        );
    }

    #[test]
    fn read_cache_rejects_immediately_previous_schema_version() {
        let library = TestLibrary::new("previous-cache-schema");
        let name = cache_name();
        let cached = CachedCatalog {
            schema_version: LOADER_CATALOG_SCHEMA_VERSION - 1,
            fetched_at_ms: 1,
            value: vec!["old".to_string()],
        };
        open_or_create_loader_catalog(library.operation())
            .expect("open loader catalog")
            .write_exact_fixture(
                name.as_str(),
                &serde_json::to_vec(&cached).expect("serialize old cache"),
            )
            .expect("write old cache");

        read_cache::<Vec<String>>(library.operation(), &name)
            .expect_err("old schema should be rejected");
    }

    #[tokio::test]
    async fn stale_cache_is_returned_when_live_fetch_response_is_too_large() {
        let library = TestLibrary::new("stale-cache-oversized-live-fetch");
        let name = cache_name();
        let cached = CachedCatalog {
            schema_version: LOADER_CATALOG_SCHEMA_VERSION,
            fetched_at_ms: 1,
            value: vec!["cached".to_string()],
        };
        write_cache(library.operation(), &name, &cached)
            .await
            .expect("write stale cache");

        let (value, state) = resolve_cached(
            library.operation(),
            name,
            Duration::ZERO,
            || async {
                Err::<Vec<String>, _>(LoaderError::ProviderDataInvalid {
                    kind: crate::loaders::types::LoaderProviderFailureKind::ResponseTooLarge,
                    status: None,
                })
            },
        )
        .await
        .expect("stale cache should cover oversized live fetch");

        assert_eq!(value, vec!["cached".to_string()]);
        assert!(!state.availability.fresh);
        assert!(state.availability.stale);
        assert!(state.availability.cache_hit);
        assert_eq!(state.availability.last_success_at_ms, Some(1));
        assert_eq!(
            state.availability.last_error.as_deref(),
            Some("provider_response_too_large")
        );
        assert_eq!(
            state.availability.last_failure_kind,
            Some(crate::loaders::types::LoaderPreOperationFailureKind::ProviderResponseTooLarge)
        );
    }

    #[tokio::test]
    async fn missing_catalog_is_normalized_to_pre_operation_http_failure() {
        let library = TestLibrary::new("missing-live-catalog");

        let error = resolve_cached(
            library.operation(),
            cache_name(),
            Duration::ZERO,
            || async {
                Err::<Vec<String>, _>(LoaderError::ArtifactMissing(
                    "https://provider.invalid/catalog?token=secret".to_string(),
                ))
            },
        )
        .await
        .expect_err("missing live catalog without cache must fail");

        assert!(matches!(
            error,
            LoaderError::CatalogUnavailable {
                provider_failure_kind: Some(LoaderProviderFailureKind::HttpNotFound),
                provider_status: Some(404),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn fresh_disk_catalog_warms_memory_cache() {
        let library = TestLibrary::new("memory-cache-warm");
        let name = cache_name();
        let cached = CachedCatalog::new(vec!["cached".to_string()]);
        write_cache(library.operation(), &name, &cached)
            .await
            .expect("write fresh cache");

        let (value, state) = resolve_cached(
            library.operation(),
            name.clone(),
            Duration::from_secs(60),
            || async {
                Err::<Vec<String>, _>(LoaderError::CatalogStale)
            },
        )
        .await
        .expect("fresh disk cache should satisfy first lookup");

        assert_eq!(value, vec!["cached".to_string()]);
        assert!(state.availability.fresh);
        assert!(state.availability.cache_hit);

        remove_catalog_file(library.operation(), &name);

        let (value, state) = resolve_cached(
            library.operation(),
            name,
            Duration::from_secs(60),
            || async { Err::<Vec<String>, _>(LoaderError::CatalogStale) },
        )
        .await
        .expect("memory cache should satisfy second lookup");

        assert_eq!(value, vec!["cached".to_string()]);
        assert!(state.availability.fresh);
        assert!(state.availability.cache_hit);
    }

    #[tokio::test]
    async fn resolve_fresh_cached_ignores_stale_disk_catalog() {
        let library = TestLibrary::new("fresh-cache-only-stale");
        let name = cache_name();
        let cached = CachedCatalog {
            schema_version: LOADER_CATALOG_SCHEMA_VERSION,
            fetched_at_ms: 1,
            value: vec!["stale".to_string()],
        };
        write_cache(library.operation(), &name, &cached)
            .await
            .expect("write stale cache");

        assert!(
            resolve_fresh_cached::<Vec<String>>(library.operation(), name, Duration::ZERO)
                .is_none()
        );
    }

    #[tokio::test]
    async fn write_cache_replaces_existing_catalog() {
        let library = TestLibrary::new("replace-cache");
        let name = cache_name();
        let old = CachedCatalog {
            schema_version: LOADER_CATALOG_SCHEMA_VERSION,
            fetched_at_ms: 1,
            value: vec!["old".to_string()],
        };
        write_cache(library.operation(), &name, &old)
            .await
            .expect("write old cache");

        let next = CachedCatalog {
            schema_version: LOADER_CATALOG_SCHEMA_VERSION,
            fetched_at_ms: 2,
            value: vec!["new".to_string()],
        };
        write_cache(library.operation(), &name, &next)
            .await
            .expect("replace cache");

        let cached = read_cache::<Vec<String>>(library.operation(), &name)
            .expect("read replaced cache");
        assert_eq!(cached.value, vec!["new".to_string()]);
        assert_eq!(cached.fetched_at_ms, 2);
    }

    #[tokio::test]
    async fn write_cache_preserves_directory_destination() {
        let library = TestLibrary::new("directory-destination");
        let name = cache_name();
        let catalog = open_or_create_loader_catalog(library.operation())
            .expect("open loader catalog");
        catalog
            .open_or_create_child(name.as_str())
            .expect("create directory at cache destination");
        let next = CachedCatalog {
            schema_version: LOADER_CATALOG_SCHEMA_VERSION,
            fetched_at_ms: 2,
            value: vec!["new".to_string()],
        };

        let result = write_cache(library.operation(), &name, &next).await;

        assert!(result.is_err());
        assert!(catalog.open_child(name.as_str()).is_ok());
    }

    #[tokio::test]
    async fn memory_cache_is_isolated_by_physical_library_root() {
        let first = TestLibrary::new("memory-isolation-first");
        let second = TestLibrary::new("memory-isolation-second");
        let name = cache_name();
        write_cache(
            first.operation(),
            &name,
            &CachedCatalog::new(vec!["first".to_string()]),
        )
        .await
        .expect("write first cache");
        write_cache(
            second.operation(),
            &name,
            &CachedCatalog::new(vec!["second".to_string()]),
        )
        .await
        .expect("write second cache");

        for (library, expected) in [(&first, "first"), (&second, "second")] {
            let (value, _) = resolve_cached(
                library.operation(),
                name.clone(),
                Duration::from_secs(60),
                || async { Err::<Vec<String>, _>(LoaderError::CatalogStale) },
            )
            .await
            .expect("warm memory cache");
            assert_eq!(value, vec![expected.to_string()]);
            remove_catalog_file(library.operation(), &name);
        }

        for (library, expected) in [(&first, "first"), (&second, "second")] {
            let (value, _) = resolve_cached(
                library.operation(),
                name.clone(),
                Duration::from_secs(60),
                || async { Err::<Vec<String>, _>(LoaderError::CatalogStale) },
            )
            .await
            .expect("read isolated memory cache");
            assert_eq!(value, vec![expected.to_string()]);
        }
    }

    #[test]
    fn future_timestamp_is_not_fresh() {
        assert!(!is_cache_fresh(
            Utc::now().timestamp_millis() + 60_000,
            Duration::from_secs(120)
        ));
    }

    #[test]
    fn negative_timestamp_is_not_fresh() {
        assert!(!is_cache_fresh(-1, Duration::from_secs(u64::MAX)));
    }

    #[test]
    fn oversized_cache_is_rejected_before_persistence() {
        let cached = CachedCatalog::new("x".repeat(MAX_LOADER_CATALOG_CACHE_BYTES as usize));
        let error = serialize_cache(&cached).expect_err("oversized cache should fail");
        assert!(matches!(error, LoaderError::Verify(_)));
    }

    fn cache_name() -> PortableFileName {
        PortableFileName::new_exact("catalog.json").expect("portable cache name")
    }

    fn remove_catalog_file(operation: &ManagedLibraryOperation, name: &PortableFileName) {
        let catalog = open_loader_catalog(operation).expect("open loader catalog");
        let guard = catalog
            .inspect_regular_file(name.as_str())
            .expect("inspect catalog file")
            .expect("catalog file");
        catalog
            .remove_guarded_file(name.as_str(), &guard)
            .expect("remove catalog file");
    }

    struct TestLibrary {
        path: PathBuf,
        operation: Option<ManagedLibraryOperation>,
        root: Option<ManagedLibraryRoot>,
    }

    impl TestLibrary {
        fn new(prefix: &str) -> Self {
            let path = temp_dir(prefix);
            fs::create_dir_all(&path).expect("create managed library");
            let root = ManagedLibraryRoot::open_for_test(&path).expect("open managed library");
            let operation = root.try_acquire().expect("acquire managed operation");
            operation.prepare_layout().expect("prepare managed layout");
            Self {
                path,
                operation: Some(operation),
                root: Some(root),
            }
        }

        fn operation(&self) -> &ManagedLibraryOperation {
            self.operation.as_ref().expect("managed operation")
        }
    }

    impl Drop for TestLibrary {
        fn drop(&mut self) {
            drop(self.operation.take());
            drop(self.root.take());
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "axial-loader-cache-{prefix}-{}-{nanos:x}",
            std::process::id()
        ))
    }
}

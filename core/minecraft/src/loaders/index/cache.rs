use crate::download::{
    DownloadError, ExecutionDownloadError, ExecutionDownloadReport,
    write_launcher_managed_artifact_bytes_to_temp,
};
use crate::loaders::types::{
    CachedCatalog, LOADER_CATALOG_SCHEMA_VERSION, LoaderAvailability, LoaderCatalogState,
    LoaderError, LoaderProviderFailureKind,
};
use chrono::Utc;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CACHE_PERSIST_WARNING: &str =
    "Loader catalog is available, but the offline cache could not be updated.";

static MEMORY_CATALOG_CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedCatalog<serde_json::Value>>>> =
    OnceLock::new();

pub async fn resolve_cached<T, F, Fut>(
    cache_path: PathBuf,
    ttl: Duration,
    live_fetch: F,
) -> Result<(T, LoaderCatalogState), LoaderError>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Clone,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, LoaderError>>,
{
    if let Some(cached) = resolve_fresh_cached::<T>(cache_path.clone(), ttl) {
        return Ok(cached);
    }

    let checked_at_ms = Utc::now().timestamp_millis();
    let cached = read_cache::<T>(&cache_path);
    match live_fetch().await {
        Ok(value) => {
            let cached = CachedCatalog::new(value.clone());
            let cache_error = Box::pin(write_cache(&cache_path, &cached))
                .await
                .err()
                .map(|_| CACHE_PERSIST_WARNING.to_string());
            update_memory_cache(&cache_path, &cached);
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
                update_memory_cache(&cache_path, &cached);
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
    cache_path: PathBuf,
    ttl: Duration,
) -> Option<(T, LoaderCatalogState)>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let checked_at_ms = Utc::now().timestamp_millis();
    let cached = fresh_memory_cache::<T>(&cache_path, ttl).or_else(|| {
        let cached = read_cache::<T>(&cache_path).ok()?;
        if !is_cache_fresh(cached.fetched_at_ms, ttl) {
            return None;
        }
        update_memory_cache(&cache_path, &cached);
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

fn fresh_memory_cache<T>(path: &Path, ttl: Duration) -> Option<CachedCatalog<T>>
where
    T: serde::de::DeserializeOwned,
{
    let cache = MEMORY_CATALOG_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()?;
    let cached = cache.get(path)?;
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

fn update_memory_cache<T>(path: &Path, cached: &CachedCatalog<T>)
where
    T: serde::Serialize,
{
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
        path.to_path_buf(),
        CachedCatalog {
            schema_version: cached.schema_version,
            fetched_at_ms: cached.fetched_at_ms,
            value,
        },
    );
}

fn read_cache<T>(path: &Path) -> Result<CachedCatalog<T>, LoaderError>
where
    T: serde::de::DeserializeOwned,
{
    let data = fs::read(path)?;
    let cached = serde_json::from_slice::<CachedCatalog<T>>(&data)?;
    if cached.schema_version != LOADER_CATALOG_SCHEMA_VERSION {
        return Err(LoaderError::CatalogStale);
    }
    Ok(cached)
}

async fn write_cache<T>(
    path: &Path,
    value: &CachedCatalog<T>,
) -> Result<ExecutionDownloadReport, LoaderError>
where
    T: serde::Serialize,
{
    let data = serde_json::to_vec_pretty(value)?;
    let tmp_path = cache_tmp_path(path);
    write_launcher_managed_artifact_bytes_to_temp(path, &tmp_path, &data)
        .await
        .map_err(loader_execution_download_error)
}

fn loader_execution_download_error(error: ExecutionDownloadError) -> LoaderError {
    loader_download_error(error.into_download_error())
}

fn loader_download_error(error: DownloadError) -> LoaderError {
    match error {
        DownloadError::FileOperation(error) => LoaderError::Io(error),
        error => LoaderError::InstallExecutionFailed(error.to_string()),
    }
}

fn is_cache_fresh(fetched_at_ms: i64, ttl: Duration) -> bool {
    let now = Utc::now().timestamp_millis();
    now.saturating_sub(fetched_at_ms) <= ttl.as_millis() as i64
}

fn cache_tmp_path(path: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let extension = format!("tmp-{}-{nanos:x}", std::process::id());
    path.with_extension(extension)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn live_fetch_success_survives_cache_write_failure() {
        let root = temp_dir("live-cache-write-failure");
        fs::create_dir_all(&root).expect("create root");
        let blocked_parent = root.join("not-a-directory");
        fs::write(&blocked_parent, b"file").expect("write blocked parent");
        let cache_path = blocked_parent.join("catalog.json");

        let (value, state) = resolve_cached(cache_path, Duration::ZERO, || async {
            Ok::<_, LoaderError>(vec!["live".to_string()])
        })
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

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_cache_rejects_immediately_previous_schema_version() {
        let root = temp_dir("previous-cache-schema");
        fs::create_dir_all(&root).expect("create root");
        let cache_path = root.join("catalog.json");
        let cached = CachedCatalog {
            schema_version: LOADER_CATALOG_SCHEMA_VERSION - 1,
            fetched_at_ms: 1,
            value: vec!["old".to_string()],
        };
        fs::write(
            &cache_path,
            serde_json::to_vec(&cached).expect("serialize old cache"),
        )
        .expect("write old cache");

        read_cache::<Vec<String>>(&cache_path).expect_err("old schema should be rejected");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stale_cache_is_returned_when_live_fetch_response_is_too_large() {
        let root = temp_dir("stale-cache-oversized-live-fetch");
        fs::create_dir_all(&root).expect("create root");
        let cache_path = root.join("catalog.json");
        let cached = CachedCatalog {
            schema_version: LOADER_CATALOG_SCHEMA_VERSION,
            fetched_at_ms: 1,
            value: vec!["cached".to_string()],
        };
        write_cache(&cache_path, &cached)
            .await
            .expect("write stale cache");

        let (value, state) = resolve_cached(cache_path, Duration::ZERO, || async {
            Err::<Vec<String>, _>(LoaderError::ProviderDataInvalid {
                kind: crate::loaders::types::LoaderProviderFailureKind::ResponseTooLarge,
                status: None,
            })
        })
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

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn missing_catalog_is_normalized_to_pre_operation_http_failure() {
        let root = temp_dir("missing-live-catalog");
        fs::create_dir_all(&root).expect("create root");
        let cache_path = root.join("catalog.json");

        let error = resolve_cached(cache_path, Duration::ZERO, || async {
            Err::<Vec<String>, _>(LoaderError::ArtifactMissing(
                "https://provider.invalid/catalog?token=secret".to_string(),
            ))
        })
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

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fresh_disk_catalog_warms_memory_cache() {
        let root = temp_dir("memory-cache-warm");
        fs::create_dir_all(&root).expect("create root");
        let cache_path = root.join("catalog.json");
        let cached = CachedCatalog::new(vec!["cached".to_string()]);
        write_cache(&cache_path, &cached)
            .await
            .expect("write fresh cache");

        let (value, state) =
            resolve_cached(cache_path.clone(), Duration::from_secs(60), || async {
                Err::<Vec<String>, _>(LoaderError::CatalogStale)
            })
            .await
            .expect("fresh disk cache should satisfy first lookup");

        assert_eq!(value, vec!["cached".to_string()]);
        assert!(state.availability.fresh);
        assert!(state.availability.cache_hit);

        fs::remove_file(&cache_path).expect("remove disk cache");

        let (value, state) = resolve_cached(cache_path, Duration::from_secs(60), || async {
            Err::<Vec<String>, _>(LoaderError::CatalogStale)
        })
        .await
        .expect("memory cache should satisfy second lookup");

        assert_eq!(value, vec!["cached".to_string()]);
        assert!(state.availability.fresh);
        assert!(state.availability.cache_hit);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn resolve_fresh_cached_ignores_stale_disk_catalog() {
        let root = temp_dir("fresh-cache-only-stale");
        fs::create_dir_all(&root).expect("create root");
        let cache_path = root.join("catalog.json");
        let cached = CachedCatalog {
            schema_version: LOADER_CATALOG_SCHEMA_VERSION,
            fetched_at_ms: 1,
            value: vec!["stale".to_string()],
        };
        write_cache(&cache_path, &cached)
            .await
            .expect("write stale cache");

        assert!(resolve_fresh_cached::<Vec<String>>(cache_path, Duration::ZERO).is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn write_cache_uses_temp_file_before_replacement() {
        let root = temp_dir("atomic-cache-write");
        fs::create_dir_all(&root).expect("create root");
        let cache_path = root.join("catalog.json");
        let old = CachedCatalog {
            schema_version: LOADER_CATALOG_SCHEMA_VERSION,
            fetched_at_ms: 1,
            value: vec!["old".to_string()],
        };
        write_cache(&cache_path, &old)
            .await
            .expect("write old cache");

        let next = CachedCatalog {
            schema_version: LOADER_CATALOG_SCHEMA_VERSION,
            fetched_at_ms: 2,
            value: vec!["new".to_string()],
        };
        let report = write_cache(&cache_path, &next)
            .await
            .expect("replace cache");

        let cached = read_cache::<Vec<String>>(&cache_path).expect("read replaced cache");
        assert_eq!(cached.value, vec!["new".to_string()]);
        assert_eq!(cached.fetched_at_ms, 2);
        assert_eq!(report.target, "catalog.json");
        assert!(
            report.facts.iter().any(
                |fact| fact.kind == crate::download::ExecutionDownloadFactKind::MetadataMissing
            )
        );
        assert!(
            report
                .facts
                .iter()
                .any(|fact| fact.kind == crate::download::ExecutionDownloadFactKind::Promoted)
        );
        assert!(fs::read_dir(&root).expect("read root").all(|entry| {
            !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn shared_promotion_replaces_existing_destination() {
        let root = temp_dir("promote-replaces-destination");
        fs::create_dir_all(&root).expect("create root");
        let tmp_path = root.join("catalog.tmp");
        let cache_path = root.join("catalog.json");
        fs::write(&cache_path, b"old").expect("write old cache");
        fs::write(&tmp_path, b"new").expect("write temp cache");

        crate::download::promote_launcher_managed_artifact_temp_once(&tmp_path, &cache_path)
            .await
            .expect("promote temp cache");

        assert_eq!(fs::read(&cache_path).expect("read promoted cache"), b"new");
        assert!(!tmp_path.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn shared_promotion_preserves_destination_when_temp_is_missing() {
        let root = temp_dir("promote-missing-temp");
        fs::create_dir_all(&root).expect("create root");
        let tmp_path = root.join("missing.tmp");
        let cache_path = root.join("catalog.json");
        fs::write(&cache_path, b"old").expect("write old cache");

        let error =
            crate::download::promote_launcher_managed_artifact_temp_once(&tmp_path, &cache_path)
                .await
                .expect_err("missing temp should fail");

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(fs::read(&cache_path).expect("read old cache"), b"old");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn write_cache_preserves_directory_destination_and_cleans_temp_on_promotion_failure() {
        let root = temp_dir("promote-directory-destination");
        fs::create_dir_all(&root).expect("create root");
        let cache_path = root.join("catalog.json");
        fs::create_dir_all(&cache_path).expect("create cache directory");
        let next = CachedCatalog {
            schema_version: LOADER_CATALOG_SCHEMA_VERSION,
            fetched_at_ms: 2,
            value: vec!["new".to_string()],
        };

        let result = write_cache(&cache_path, &next).await;

        assert!(result.is_err());
        assert!(cache_path.is_dir());
        assert!(fs::read_dir(&root).expect("read root").all(|entry| {
            !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));

        let _ = fs::remove_dir_all(root);
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

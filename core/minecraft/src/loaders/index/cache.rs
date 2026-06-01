use crate::loaders::types::{CachedCatalog, LoaderAvailability, LoaderCatalogState, LoaderError};
use chrono::Utc;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CATALOG_SCHEMA_VERSION: u32 = 6;
const CACHE_PERSIST_WARNING: &str =
    "Loader catalog is available, but the offline cache could not be updated.";

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
    let checked_at_ms = Utc::now().timestamp_millis();
    let cached = read_cache::<T>(&cache_path);
    if let Ok(cached) = &cached
        && is_cache_fresh(cached.fetched_at_ms, ttl)
    {
        return Ok((
            cached.value.clone(),
            LoaderCatalogState {
                availability: LoaderAvailability {
                    fresh: true,
                    stale: false,
                    cache_hit: true,
                    checked_at_ms,
                    last_success_at_ms: Some(cached.fetched_at_ms),
                    last_error: None,
                },
            },
        ));
    }

    match live_fetch().await {
        Ok(value) => {
            let cache_error = write_cache(&cache_path, &CachedCatalog::new(value.clone()))
                .err()
                .map(|_| CACHE_PERSIST_WARNING.to_string());
            Ok((
                value,
                LoaderCatalogState {
                    availability: LoaderAvailability {
                        fresh: true,
                        stale: false,
                        cache_hit: false,
                        checked_at_ms,
                        last_success_at_ms: Some(checked_at_ms),
                        last_error: cache_error,
                    },
                },
            ))
        }
        Err(error) => {
            if let Ok(cached) = cached {
                return Ok((
                    cached.value,
                    LoaderCatalogState {
                        availability: LoaderAvailability {
                            fresh: false,
                            stale: true,
                            cache_hit: true,
                            checked_at_ms,
                            last_success_at_ms: Some(cached.fetched_at_ms),
                            last_error: Some(error.to_string()),
                        },
                    },
                ));
            }
            Err(LoaderError::CatalogUnavailable(error.to_string()))
        }
    }
}

fn read_cache<T>(path: &Path) -> Result<CachedCatalog<T>, LoaderError>
where
    T: serde::de::DeserializeOwned,
{
    let data = fs::read(path)?;
    let cached = serde_json::from_slice::<CachedCatalog<T>>(&data)?;
    if cached.schema_version != CATALOG_SCHEMA_VERSION {
        return Err(LoaderError::Other(
            "catalog cache schema mismatch".to_string(),
        ));
    }
    Ok(cached)
}

fn write_cache<T>(path: &Path, value: &CachedCatalog<T>) -> Result<(), LoaderError>
where
    T: serde::Serialize,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(value)?;
    let tmp_path = cache_tmp_path(path);
    let result = (|| -> Result<(), LoaderError> {
        fs::write(&tmp_path, data)?;
        fs::rename(&tmp_path, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
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
    fn write_cache_uses_temp_file_before_replacement() {
        let root = temp_dir("atomic-cache-write");
        fs::create_dir_all(&root).expect("create root");
        let cache_path = root.join("catalog.json");
        let old = CachedCatalog {
            schema_version: CATALOG_SCHEMA_VERSION,
            fetched_at_ms: 1,
            value: vec!["old".to_string()],
        };
        write_cache(&cache_path, &old).expect("write old cache");

        let next = CachedCatalog {
            schema_version: CATALOG_SCHEMA_VERSION,
            fetched_at_ms: 2,
            value: vec!["new".to_string()],
        };
        write_cache(&cache_path, &next).expect("replace cache");

        let cached = read_cache::<Vec<String>>(&cache_path).expect("read replaced cache");
        assert_eq!(cached.value, vec!["new".to_string()]);
        assert_eq!(cached.fetched_at_ms, 2);
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
            "croopor-loader-cache-{prefix}-{}-{nanos:x}",
            std::process::id()
        ))
    }
}

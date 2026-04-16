use crate::loaders::types::{CachedCatalog, LoaderAvailability, LoaderCatalogState, LoaderError};
use chrono::Utc;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CATALOG_SCHEMA_VERSION: u32 = 6;

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
            write_cache(&cache_path, &CachedCatalog::new(value.clone()))?;
            Ok((
                value,
                LoaderCatalogState {
                    availability: LoaderAvailability {
                        fresh: true,
                        stale: false,
                        cache_hit: false,
                        checked_at_ms,
                        last_success_at_ms: Some(checked_at_ms),
                        last_error: None,
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
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

fn is_cache_fresh(fetched_at_ms: i64, ttl: Duration) -> bool {
    let now = Utc::now().timestamp_millis();
    now.saturating_sub(fetched_at_ms) <= ttl.as_millis() as i64
}

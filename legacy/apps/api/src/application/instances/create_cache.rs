use super::create::CreateStaticVersionRow;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

const CREATE_SOURCE_CACHE_TTL: Duration = Duration::from_secs(10 * 60);

static CREATE_VIEW_CACHE: LazyLock<Mutex<CreateViewCache>> =
    LazyLock::new(|| Mutex::new(CreateViewCache::default()));

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CreateSourceCacheKey {
    library_dir: PathBuf,
    source_id: String,
}

#[derive(Clone, Debug)]
struct CreateSourceCacheEntry {
    rows: Vec<CreateStaticVersionRow>,
    cached_at: Instant,
}

#[derive(Default)]
struct CreateViewCache {
    source_rows: HashMap<CreateSourceCacheKey, CreateSourceCacheEntry>,
}

pub(super) fn cached_source_rows(
    library_dir: &Path,
    source_id: &str,
) -> Option<Vec<CreateStaticVersionRow>> {
    let mut cache = CREATE_VIEW_CACHE.lock().ok()?;
    cache.prune_expired();
    cache
        .source_rows
        .get(&CreateSourceCacheKey {
            library_dir: library_dir.to_path_buf(),
            source_id: source_id.to_string(),
        })
        .map(|entry| entry.rows.clone())
}

pub(super) fn store_source_rows(
    library_dir: &Path,
    source_id: &str,
    rows: Vec<CreateStaticVersionRow>,
) {
    let Ok(mut cache) = CREATE_VIEW_CACHE.lock() else {
        return;
    };
    cache.prune_expired();
    cache.source_rows.insert(
        CreateSourceCacheKey {
            library_dir: library_dir.to_path_buf(),
            source_id: source_id.to_string(),
        },
        CreateSourceCacheEntry {
            rows,
            cached_at: Instant::now(),
        },
    );
}

pub(crate) fn invalidate_create_view_root(library_dir: &Path) {
    if let Ok(mut cache) = CREATE_VIEW_CACHE.lock() {
        cache
            .source_rows
            .retain(|key, _| key.library_dir.as_path() != library_dir);
    }
}

pub(crate) fn invalidate_create_view_source(library_dir: &Path, source_id: &str) {
    if let Ok(mut cache) = CREATE_VIEW_CACHE.lock() {
        cache.source_rows.remove(&CreateSourceCacheKey {
            library_dir: library_dir.to_path_buf(),
            source_id: source_id.to_string(),
        });
    }
}

#[cfg(test)]
pub(crate) fn seed_create_view_cache_for_tests(library_dir: &Path) {
    store_source_rows(library_dir, "test-source", Vec::new());
}

#[cfg(test)]
pub(crate) fn create_view_cache_contains_root_for_tests(library_dir: &Path) -> bool {
    CREATE_VIEW_CACHE
        .lock()
        .map(|cache| {
            cache
                .source_rows
                .keys()
                .any(|key| key.library_dir.as_path() == library_dir)
        })
        .unwrap_or_default()
}

impl CreateViewCache {
    fn prune_expired(&mut self) {
        let now = Instant::now();
        self.source_rows
            .retain(|_, entry| now.duration_since(entry.cached_at) <= CREATE_SOURCE_CACHE_TTL);
    }
}

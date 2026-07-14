use crate::artifact_path::ArtifactRelativePath;
use crate::loaders::types::LoaderError;
use crate::managed_component_publication::component_lane_name;
use crate::managed_component_table::ManagedComponentKind;
use crate::managed_fs::{ManagedBoundedFileReader, ManagedDir, ManagedFileGuard};
use std::io;
use std::path::Path;

#[derive(Clone)]
pub(crate) struct ManagedComponentExactCache {
    component_root: Option<ManagedDir>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum ManagedComponentExactCacheError {
    #[error("managed component exact-cache admission failed")]
    Admission,
    #[error("managed component exact-cache task stopped")]
    TaskStopped,
}

struct GuardedExactFile {
    directory: ManagedDir,
    name: String,
    guard: ManagedFileGuard,
}

impl ManagedComponentExactCache {
    pub(crate) async fn bind(
        managed_root: &Path,
        component: ManagedComponentKind,
    ) -> Result<Self, ManagedComponentExactCacheError> {
        let managed_root = managed_root.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let root = match ManagedDir::open_root(&managed_root) {
                Ok(root) => root,
                Err(LoaderError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(Self {
                        component_root: None,
                    });
                }
                Err(_) => return Err(ManagedComponentExactCacheError::Admission),
            };
            let component_name = component_lane_name(component);
            if !root
                .has_portably_exact_child_name(component_name)
                .map_err(|_| ManagedComponentExactCacheError::Admission)?
            {
                return Ok(Self {
                    component_root: None,
                });
            }
            let component_root = root
                .open_child(component_name)
                .map_err(|_| ManagedComponentExactCacheError::Admission)?;
            Ok(Self {
                component_root: Some(component_root),
            })
        })
        .await
        .map_err(|_| ManagedComponentExactCacheError::TaskStopped)?
    }

    pub(crate) async fn full_sha1(
        &self,
        relative_path: &ArtifactRelativePath,
        expected_size: u64,
    ) -> Result<Option<[u8; 20]>, ManagedComponentExactCacheError> {
        let Some(component_root) = self.component_root.clone() else {
            return Ok(None);
        };
        let relative_path = relative_path.clone();
        tokio::task::spawn_blocking(move || {
            let Some(source) = inspect_exact_file(component_root, &relative_path, expected_size)?
            else {
                return Ok(None);
            };
            source
                .directory
                .sha1_guarded_file_bytes(&source.name, &source.guard, expected_size)
                .map(Some)
                .map_err(|_| ManagedComponentExactCacheError::Admission)
        })
        .await
        .map_err(|_| ManagedComponentExactCacheError::TaskStopped)?
    }

    pub(crate) async fn bounded_reader(
        &self,
        relative_path: &ArtifactRelativePath,
        expected_size: u64,
    ) -> Result<Option<ManagedBoundedFileReader>, ManagedComponentExactCacheError> {
        let Some(component_root) = self.component_root.clone() else {
            return Ok(None);
        };
        let relative_path = relative_path.clone();
        tokio::task::spawn_blocking(move || {
            let Some(source) = inspect_exact_file(component_root, &relative_path, expected_size)?
            else {
                return Ok(None);
            };
            Ok(source.guard.into_bounded_reader(expected_size).ok())
        })
        .await
        .map_err(|_| ManagedComponentExactCacheError::TaskStopped)?
    }
}

fn inspect_exact_file(
    component_root: ManagedDir,
    relative_path: &ArtifactRelativePath,
    expected_size: u64,
) -> Result<Option<GuardedExactFile>, ManagedComponentExactCacheError> {
    let mut segments = relative_path.as_str().split('/').peekable();
    let mut directory = component_root;
    while let Some(segment) = segments.next() {
        if !directory
            .has_portably_exact_child_name(segment)
            .map_err(|_| ManagedComponentExactCacheError::Admission)?
        {
            return Ok(None);
        }
        if segments.peek().is_none() {
            let guard = directory
                .inspect_regular_file(segment)
                .map_err(|_| ManagedComponentExactCacheError::Admission)?
                .ok_or(ManagedComponentExactCacheError::Admission)?;
            if guard.size() != expected_size {
                return Ok(None);
            }
            return Ok(Some(GuardedExactFile {
                directory,
                name: segment.to_string(),
                guard,
            }));
        }
        directory = directory
            .open_child(segment)
            .map_err(|_| ManagedComponentExactCacheError::Admission)?;
    }
    Err(ManagedComponentExactCacheError::Admission)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha1::{Digest as _, Sha1};
    use std::io::Read as _;

    fn relative(path: &str) -> ArtifactRelativePath {
        ArtifactRelativePath::new(path).expect("managed component cache path")
    }

    fn sha1(bytes: &[u8]) -> [u8; 20] {
        Sha1::digest(bytes).into()
    }

    #[tokio::test]
    async fn missing_root_component_and_path_are_cache_misses() {
        let temporary = tempfile::tempdir().expect("cache test root");
        let missing_root = temporary.path().join("missing");
        let path = relative("objects/aa/aa01");

        let cache = ManagedComponentExactCache::bind(&missing_root, ManagedComponentKind::Assets)
            .await
            .expect("bind missing root");
        assert_eq!(cache.full_sha1(&path, 1).await.expect("missing root"), None);

        let cache =
            ManagedComponentExactCache::bind(temporary.path(), ManagedComponentKind::Assets)
                .await
                .expect("bind missing component");
        assert_eq!(
            cache.full_sha1(&path, 1).await.expect("missing component"),
            None
        );

        std::fs::create_dir_all(temporary.path().join("assets/objects/aa"))
            .expect("create component directories");
        let cache =
            ManagedComponentExactCache::bind(temporary.path(), ManagedComponentKind::Assets)
                .await
                .expect("bind component");
        assert_eq!(cache.full_sha1(&path, 1).await.expect("missing path"), None);
    }

    #[tokio::test]
    async fn full_sha1_proves_exact_corrupt_and_zero_files_under_guard() {
        let temporary = tempfile::tempdir().expect("cache test root");
        let directory = temporary.path().join("assets/objects/aa");
        std::fs::create_dir_all(&directory).expect("create object directory");
        let path = relative("objects/aa/aa01");
        let exact = b"exact-cache";
        std::fs::write(directory.join("aa01"), exact).expect("write exact object");
        let cache =
            ManagedComponentExactCache::bind(temporary.path(), ManagedComponentKind::Assets)
                .await
                .expect("bind component");

        assert_eq!(
            cache.full_sha1(&path, exact.len() as u64).await,
            Ok(Some(sha1(exact)))
        );
        assert_eq!(
            cache.full_sha1(&path, exact.len() as u64 + 1).await,
            Ok(None)
        );

        let corrupt = b"wrong-cache";
        assert_eq!(corrupt.len(), exact.len());
        std::fs::write(directory.join("aa01"), corrupt).expect("write corrupt object");
        assert_eq!(
            cache.full_sha1(&path, corrupt.len() as u64).await,
            Ok(Some(sha1(corrupt)))
        );

        let zero_path = relative("objects/aa/aa00");
        std::fs::write(directory.join("aa00"), []).expect("write zero object");
        assert_eq!(cache.full_sha1(&zero_path, 0).await, Ok(Some(sha1(&[]))));
        let mut reader = cache
            .bounded_reader(&zero_path, 0)
            .await
            .expect("inspect zero object")
            .expect("zero object reader");
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).expect("read zero object");
        assert!(bytes.is_empty());
    }

    #[tokio::test]
    async fn invalid_final_intermediate_and_alias_topology_fail_closed() {
        let final_directory = tempfile::tempdir().expect("final topology root");
        std::fs::create_dir_all(final_directory.path().join("assets/objects/aa/aa01"))
            .expect("create directory at final path");
        let cache =
            ManagedComponentExactCache::bind(final_directory.path(), ManagedComponentKind::Assets)
                .await
                .expect("bind final topology");
        assert_eq!(
            cache.full_sha1(&relative("objects/aa/aa01"), 1).await,
            Err(ManagedComponentExactCacheError::Admission)
        );

        let intermediate_file = tempfile::tempdir().expect("intermediate topology root");
        std::fs::create_dir_all(intermediate_file.path().join("assets"))
            .expect("create Assets root");
        std::fs::write(
            intermediate_file.path().join("assets/objects"),
            b"not a directory",
        )
        .expect("write intermediate file");
        let cache = ManagedComponentExactCache::bind(
            intermediate_file.path(),
            ManagedComponentKind::Assets,
        )
        .await
        .expect("bind intermediate topology");
        assert_eq!(
            cache.full_sha1(&relative("objects/aa/aa01"), 1).await,
            Err(ManagedComponentExactCacheError::Admission)
        );

        let alias = tempfile::tempdir().expect("alias topology root");
        std::fs::create_dir_all(alias.path().join("assets/Objects"))
            .expect("create portable alias");
        let cache = ManagedComponentExactCache::bind(alias.path(), ManagedComponentKind::Assets)
            .await
            .expect("bind alias topology");
        assert_eq!(
            cache.full_sha1(&relative("objects/aa/aa01"), 1).await,
            Err(ManagedComponentExactCacheError::Admission)
        );
    }
}

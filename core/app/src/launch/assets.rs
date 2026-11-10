//! Legacy asset indexes name trees that old clients read instead of the hashed object store.

use super::coordinator::LaunchError;
use super::libraries::{self, Retention};
use crate::files::ScopedDirectory;
use crate::install::artifacts::{InstalledVersionReceipt, LegacyAssetInputs};
use crate::tasks::CancellationToken;
use axial_fs::LeafName;
use axial_minecraft::LegacyAssetLayout;
use axial_minecraft::download::{DownloadError, transfer_cancellation_channel};
use axial_minecraft::managed_path::ManagedLibraryOperation;
use axial_resource::{PhysicalIoClass, PhysicalWorkRequest};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Verifies, and creates or repairs, the tree the installed index names.
/// Returns the directory passed to the game as its assets directory.
pub(super) async fn prepare(
    installed: &InstalledVersionReceipt,
    library: &ManagedLibraryOperation,
    library_dir: &Path,
    game_dir: &Path,
    retention: Arc<Retention>,
    cancellation: &CancellationToken,
) -> Result<Option<PathBuf>, LaunchError> {
    let Some(inputs) = installed
        .legacy_assets()
        .await
        .map_err(super::coordinator::install_read_error)?
    else {
        return Ok(None);
    };
    if cancellation.is_cancelled() {
        return Err(LaunchError::Cancelled);
    }
    match inputs.assets.layout {
        LegacyAssetLayout::Virtual => {
            let materialized = axial_minecraft::download::materialize_virtual_assets(
                library,
                &inputs.index_id,
                &inputs.assets,
            );
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(LaunchError::Cancelled),
                result = materialized => result.map_err(|error| {
                    let cause = match &error {
                        DownloadError::Integrity(_) => "integrity",
                        DownloadError::FileOperation(_) => "file_operation",
                        _ => "other",
                    };
                    tracing::warn!(stage = "virtual_assets", cause, "Launch preparation rejected.");
                    LaunchError::PreparationFailed
                })?,
            }
            Ok(Some(
                library_dir
                    .join("assets")
                    .join("virtual")
                    .join(&inputs.index_id),
            ))
        }
        LegacyAssetLayout::Resources => {
            prepare_resources(inputs, retention, cancellation).await?;
            Ok(Some(game_dir.join(RESOURCES)))
        }
    }
}

const RESOURCES: &str = "resources";

/// Pre-1.6 clients read `resources` in their game directory. Missing files are
/// created from verified objects; a differing file is refused, not replaced.
async fn prepare_resources(
    inputs: LegacyAssetInputs,
    retention: Arc<Retention>,
    cancellation: &CancellationToken,
) -> Result<(), LaunchError> {
    let admission = retention
        .work
        .admit(PhysicalWorkRequest::foreground(
            PhysicalIoClass::Write,
            libraries::SCRATCH_BYTES,
        ))
        .await
        .map_err(super::coordinator::physical_work_error)?;
    let (sender, cancelled) = transfer_cancellation_channel();
    let work = admission.run(move |_| {
        let mut directories = HashMap::<&str, ScopedDirectory>::new();
        let root = libraries::prepare_directory(
            &retention,
            &retention.game,
            &LeafName::new(RESOURCES).expect("fixed resources directory"),
        )?;
        directories.insert("", root);
        for file in &inputs.assets.files {
            if cancelled.is_cancelled() {
                return Err(LaunchError::Cancelled);
            }
            let path = file.path.as_str();
            let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
            let directory = prepare_parent(&retention, &mut directories, parent)?;
            let source = inputs
                .source(file)
                .map_err(super::coordinator::install_read_error)?;
            libraries::prepare_file(
                &source,
                &directory,
                LeafName::new(name).map_err(|_| LaunchError::InstallUnavailable)?,
                file.size,
                file.sha1,
                &retention,
                cancelled.clone(),
            )?;
        }
        Ok(())
    });
    tokio::pin!(work);
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => { sender.cancel(); work.await },
        result = &mut work => result,
    };
    // A stopped mutation worker cannot prove native settlement.
    result.expect("legacy resource copy worker was interrupted")
}

fn prepare_parent<'a>(
    retention: &Retention,
    directories: &mut HashMap<&'a str, ScopedDirectory>,
    path: &'a str,
) -> Result<ScopedDirectory, LaunchError> {
    if let Some(directory) = directories.get(path) {
        return Ok(directory.clone());
    }
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    let parent = prepare_parent(retention, directories, parent)?;
    let directory = libraries::prepare_directory(
        retention,
        &parent,
        &LeafName::new(name).map_err(|_| LaunchError::InstallUnavailable)?,
    )?;
    directories.insert(path, directory.clone());
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::artifacts::{ActivatedFile, ActivatedVersion};
    use crate::library::{GenerationPin, LibraryLifecycle, LibraryOpenOutcome};
    use sha1::{Digest, Sha1};

    const OBJECTS: [(&str, &[u8]); 3] = [
        ("sound/step.ogg", b"step"),
        ("newsound/step/grass.ogg", b"grass"),
        ("music/calm.ogg", b"step"),
    ];

    struct Fixture {
        temporary: tempfile::TempDir,
        _library: LibraryLifecycle,
        pin: GenerationPin,
        retention: Arc<Retention>,
        installed: InstalledVersionReceipt,
    }

    impl Fixture {
        async fn prepare(&self) -> Result<Option<PathBuf>, LaunchError> {
            let operation = self.pin.managed_library().unwrap();
            super::prepare(
                &self.installed,
                &operation,
                &self.pin.read_projection().unwrap(),
                &self.retention.game.read_projection().unwrap(),
                self.retention.clone(),
                &CancellationToken::new(),
            )
            .await
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.temporary.path().join(relative)
        }
    }

    /// A verified installation whose asset index carries `flag`.
    fn fixture(flag: &str) -> Fixture {
        let temporary =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        std::fs::create_dir(temporary.path().join("game")).unwrap();
        let version = serde_json::json!({
            "id": "1.6.4", "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": { "id": "legacy" }
        });
        let mut objects = serde_json::Map::new();
        let mut files = vec![
            (
                "versions/1.6.4/1.6.4.json".to_owned(),
                serde_json::to_vec(&version).unwrap(),
            ),
            ("versions/1.6.4/1.6.4.jar".to_owned(), b"client".to_vec()),
        ];
        for (name, bytes) in OBJECTS {
            let hash = hex::encode(Sha1::digest(bytes));
            objects.insert(
                name.into(),
                serde_json::json!({ "hash": hash, "size": bytes.len() }),
            );
            let object = format!("assets/objects/{}/{hash}", &hash[..2]);
            if !files.iter().any(|(path, _)| *path == object) {
                files.push((object, bytes.to_vec()));
            }
        }
        let mut index = serde_json::json!({ "objects": objects });
        index[flag] = true.into();
        files.push((
            "assets/indexes/legacy.json".into(),
            serde_json::to_vec(&index).unwrap(),
        ));
        let files = files
            .into_iter()
            .map(|(path, bytes)| {
                let target = temporary.path().join(&path);
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::fs::write(target, &bytes).unwrap();
                ActivatedFile {
                    path,
                    sha1: hex::encode(Sha1::digest(&bytes)),
                    size: bytes.len() as u64,
                }
            })
            .collect();
        let LibraryOpenOutcome::Ready(library) = LibraryLifecycle::open(temporary.path()) else {
            panic!("isolated library admission");
        };
        let pin = library.admit().unwrap();
        let installed = ActivatedVersion {
            version_id: "1.6.4".into(),
            contract_id: format!("managed-install-activation-v1.{}", "A".repeat(43)),
            files,
        }
        .verify(pin.clone())
        .unwrap();
        let game = ScopedDirectory::from_admitted(
            pin.directory()
                .unwrap()
                .open_directory(&LeafName::new("game").unwrap())
                .unwrap(),
            pin.clone(),
        )
        .unwrap();
        Fixture {
            temporary,
            _library: library,
            pin,
            retention: Arc::new(Retention::isolated_for_test(game)),
            installed,
        }
    }

    fn assert_tree(root: &Path) {
        for (name, bytes) in OBJECTS {
            assert_eq!(std::fs::read(root.join(name)).unwrap(), bytes, "{name}");
        }
    }

    #[tokio::test]
    async fn virtual_index_tree_is_created_and_repaired_in_the_library() {
        let fixture = fixture("virtual");
        let tree = fixture.path("assets/virtual/legacy");

        assert_eq!(fixture.prepare().await.unwrap(), Some(tree.clone()));
        assert_tree(&tree);
        assert!(!fixture.path("game/resources").exists());

        std::fs::write(tree.join("sound/step.ogg"), b"stop").unwrap();
        std::fs::remove_file(tree.join("newsound/step/grass.ogg")).unwrap();
        assert_eq!(fixture.prepare().await.unwrap(), Some(tree.clone()));
        assert_tree(&tree);
        fixture.installed.revalidate().unwrap();
    }

    #[tokio::test]
    async fn resources_index_tree_is_created_in_the_game_and_never_overwritten() {
        let fixture = fixture("map_to_resources");
        let tree = fixture.path("game/resources");
        std::fs::create_dir_all(tree.join("sound")).unwrap();
        std::fs::write(tree.join("sound/user.ogg"), b"user").unwrap();

        assert_eq!(fixture.prepare().await.unwrap(), Some(tree.clone()));
        assert_tree(&tree);
        assert!(!fixture.path("assets/virtual").exists());

        std::fs::remove_file(tree.join("newsound/step/grass.ogg")).unwrap();
        assert_eq!(fixture.prepare().await.unwrap(), Some(tree.clone()));
        assert_tree(&tree);

        std::fs::write(tree.join("music/calm.ogg"), b"changed").unwrap();
        assert_eq!(fixture.prepare().await, Err(LaunchError::PreparationFailed));
        assert_eq!(
            std::fs::read(tree.join("music/calm.ogg")).unwrap(),
            b"changed"
        );
        assert_eq!(std::fs::read(tree.join("sound/user.ogg")).unwrap(), b"user");
        fixture.installed.revalidate().unwrap();
    }
}

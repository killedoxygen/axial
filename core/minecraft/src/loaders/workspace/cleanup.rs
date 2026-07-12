use crate::artifact_path::ArtifactRelativePath;
use crate::loaders::managed_fs::{ManagedDir, ManagedTreeLimits, ManagedTreeSnapshot};
use crate::loaders::types::LoaderError;
use crate::loaders::validate_version_id;
use std::path::Path;

pub(crate) struct LoaderWorkspace {
    root: ManagedDir,
    directory: ManagedDir,
    target_version_id: String,
}

pub(crate) struct LoaderWorkspaceTemp {
    directory: ManagedDir,
}

pub(crate) struct ProcessorWorkspace {
    stage: ManagedDir,
    root: ManagedDir,
    libraries: ManagedDir,
    version: ManagedDir,
    processor_data: ManagedDir,
    home: ManagedDir,
    temp: ManagedDir,
}

impl LoaderWorkspace {
    pub(crate) fn target_version_id(&self) -> &str {
        &self.target_version_id
    }
    pub(crate) fn path(&self) -> &Path {
        self.directory.path()
    }

    pub(crate) async fn write_exact(&self, name: &str, bytes: &[u8]) -> Result<(), LoaderError> {
        self.directory.write_exact(name, bytes).await
    }

    pub(crate) fn revalidate(&self) -> Result<(), LoaderError> {
        self.root.revalidate()?;
        self.directory.revalidate()
    }

    pub(crate) fn read_live_library_authenticated(
        &self,
        relative: &ArtifactRelativePath,
        expected_size: Option<u64>,
        expected_sha1: &[u8; 20],
    ) -> Result<Vec<u8>, LoaderError> {
        self.root
            .open_child("libraries")?
            .read_relative_authenticated(relative, expected_size, expected_sha1)
    }

    pub(crate) fn read_base_client_authenticated(
        &self,
        version_id: &str,
        expected_size: Option<u64>,
        expected_sha1: Option<&str>,
    ) -> Result<Vec<u8>, LoaderError> {
        validate_version_id(version_id, "processor base version id")?;
        self.root
            .open_child("versions")?
            .open_child(version_id)?
            .read_authenticated(&format!("{version_id}.jar"), expected_size, expected_sha1)
    }

    pub(crate) fn create_temp(&self, name: &str) -> Result<LoaderWorkspaceTemp, LoaderError> {
        if let Some(stale) = self.directory.open_child_if_exists(name)? {
            stale.clear_owned_contents()?;
        }
        let directory = self.directory.open_or_create_child(name)?;
        Ok(LoaderWorkspaceTemp { directory })
    }

    pub(crate) fn prepare_processor_stage(
        &self,
        minecraft_version: &str,
    ) -> Result<ProcessorWorkspace, LoaderError> {
        validate_version_id(minecraft_version, "processor stage Minecraft version")?;
        if let Some(stale) = self.directory.open_child_if_exists("processor-stage")? {
            stale.clear_owned_contents()?;
        }
        let stage = self.directory.open_or_create_child("processor-stage")?;
        let root = stage.open_or_create_child("root")?;
        let libraries = root.open_or_create_child("libraries")?;
        let versions = root.open_or_create_child("versions")?;
        let version = versions.open_or_create_child(minecraft_version)?;
        let processor_data = root.open_or_create_child("processor-data")?;
        let home = stage.open_or_create_child("home")?;
        let temp = stage.open_or_create_child("tmp")?;
        let workspace = ProcessorWorkspace {
            stage,
            root,
            libraries,
            version,
            processor_data,
            home,
            temp,
        };
        workspace.revalidate()?;
        workspace.validate_fresh_layout(minecraft_version)?;
        Ok(workspace)
    }

    pub(crate) fn cleanup(self) -> Result<(), LoaderError> {
        self.directory.clear_owned_contents()
    }
}

impl LoaderWorkspaceTemp {
    pub(crate) async fn write_relative_exact(
        &self,
        relative: &crate::artifact_path::ArtifactRelativePath,
        bytes: &[u8],
    ) -> Result<(), LoaderError> {
        self.directory.write_relative_exact(relative, bytes).await
    }

    pub(crate) fn cleanup(self) -> Result<(), LoaderError> {
        self.directory.clear_owned_contents()
    }
}

impl ProcessorWorkspace {
    fn validate_fresh_layout(&self, minecraft_version: &str) -> Result<(), LoaderError> {
        let snapshot = self.snapshot_stage()?;
        let expected = [
            "home".to_string(),
            "root".to_string(),
            "root/libraries".to_string(),
            "root/processor-data".to_string(),
            "root/versions".to_string(),
            format!("root/versions/{minecraft_version}"),
            "tmp".to_string(),
        ]
        .into_iter()
        .map(|path| {
            ArtifactRelativePath::new(&path).map_err(|_| {
                LoaderError::Verify("processor stage layout is not canonical".to_string())
            })
        })
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
        let mutable_prefixes = [
            "home/",
            "root/libraries/",
            "root/processor-data/",
            "tmp/",
            &format!("root/versions/{minecraft_version}/"),
        ];
        if !snapshot.files().is_empty()
            || snapshot.directories().iter().any(|path| {
                !expected.contains(path)
                    && !mutable_prefixes
                        .iter()
                        .any(|prefix| path.as_str().starts_with(prefix))
            })
        {
            return Err(LoaderError::Verify(
                "processor stage contains an unexpected retained entry".to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn root_path(&self) -> &Path {
        self.root.path()
    }

    pub(crate) fn libraries_path(&self) -> &Path {
        self.libraries.path()
    }

    pub(crate) fn version_path(&self) -> &Path {
        self.version.path()
    }

    pub(crate) fn processor_data_path(&self) -> &Path {
        self.processor_data.path()
    }

    pub(crate) fn home_path(&self) -> &Path {
        self.home.path()
    }

    pub(crate) fn temp_path(&self) -> &Path {
        self.temp.path()
    }

    pub(crate) fn installer_path(&self) -> std::path::PathBuf {
        self.root.path().join("installer.jar")
    }

    pub(crate) async fn write_library_exact(
        &self,
        relative: &ArtifactRelativePath,
        bytes: &[u8],
    ) -> Result<(), LoaderError> {
        self.libraries.write_relative_exact(relative, bytes).await
    }

    pub(crate) fn ensure_library_parent(
        &self,
        relative: &ArtifactRelativePath,
    ) -> Result<(), LoaderError> {
        let _ = self.libraries.open_or_create_relative_parent(relative)?;
        self.libraries.revalidate()
    }

    pub(crate) async fn write_version_exact(
        &self,
        relative: &ArtifactRelativePath,
        bytes: &[u8],
    ) -> Result<(), LoaderError> {
        self.version.write_relative_exact(relative, bytes).await
    }

    pub(crate) async fn write_processor_data_exact(
        &self,
        relative: &ArtifactRelativePath,
        bytes: &[u8],
    ) -> Result<(), LoaderError> {
        self.processor_data
            .write_relative_exact(relative, bytes)
            .await
    }

    pub(crate) async fn write_installer_exact(&self, bytes: &[u8]) -> Result<(), LoaderError> {
        self.root.write_exact("installer.jar", bytes).await
    }

    pub(crate) fn read_library_authenticated(
        &self,
        relative: &ArtifactRelativePath,
        expected_size: Option<u64>,
        expected_sha1: &[u8; 20],
    ) -> Result<Vec<u8>, LoaderError> {
        self.libraries
            .read_relative_authenticated(relative, expected_size, expected_sha1)
    }

    pub(crate) fn read_version_authenticated(
        &self,
        relative: &ArtifactRelativePath,
        expected_size: Option<u64>,
        expected_sha1: &[u8; 20],
    ) -> Result<Vec<u8>, LoaderError> {
        self.version
            .read_relative_authenticated(relative, expected_size, expected_sha1)
    }

    pub(crate) fn read_processor_data_authenticated(
        &self,
        relative: &ArtifactRelativePath,
        expected_size: Option<u64>,
        expected_sha1: &[u8; 20],
    ) -> Result<Vec<u8>, LoaderError> {
        self.processor_data
            .read_relative_authenticated(relative, expected_size, expected_sha1)
    }

    pub(crate) fn read_installer_authenticated(
        &self,
        expected_size: Option<u64>,
        expected_sha1: &[u8; 20],
    ) -> Result<Vec<u8>, LoaderError> {
        self.root.read_relative_authenticated(
            &ArtifactRelativePath::new("installer.jar").map_err(|_| {
                LoaderError::Verify("processor installer path is invalid".to_string())
            })?,
            expected_size,
            expected_sha1,
        )
    }

    pub(crate) fn snapshot_root(&self) -> Result<ManagedTreeSnapshot, LoaderError> {
        self.root
            .snapshot_tree(ManagedTreeLimits::processor_stage())
    }

    pub(crate) fn snapshot_stage(&self) -> Result<ManagedTreeSnapshot, LoaderError> {
        self.stage
            .snapshot_tree(ManagedTreeLimits::processor_stage())
    }

    pub(crate) fn clear_scratch(&self) -> Result<(), LoaderError> {
        self.home.clone().clear_owned_contents()?;
        self.temp.clone().clear_owned_contents()?;
        self.home.revalidate()?;
        self.temp.revalidate()
    }

    pub(crate) fn revalidate(&self) -> Result<(), LoaderError> {
        self.stage.revalidate()?;
        self.root.revalidate()?;
        self.libraries.revalidate()?;
        self.version.revalidate()?;
        self.processor_data.revalidate()?;
        self.home.revalidate()?;
        self.temp.revalidate()
    }

    pub(crate) fn cleanup(self) -> Result<(), LoaderError> {
        self.stage.clear_owned_contents()
    }
}

pub(crate) fn prepare_fresh_work_dir(
    library_dir: &Path,
    version_id: &str,
) -> Result<LoaderWorkspace, LoaderError> {
    validate_version_id(version_id, "installer workspace version id")?;
    let root = ManagedDir::open_root(library_dir)?;
    let work = root
        .open_or_create_child("cache")?
        .open_or_create_child("loaders")?
        .open_or_create_child("work")?;
    if let Some(stale) = work.open_child_if_exists(version_id)? {
        stale.clear_owned_contents()?;
    }
    let directory = work.open_or_create_child(version_id)?;
    directory.revalidate()?;
    Ok(LoaderWorkspace {
        root,
        directory,
        target_version_id: version_id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::prepare_fresh_work_dir;
    use crate::artifact_path::ArtifactRelativePath;
    use sha1::{Digest as _, Sha1};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    #[test]
    fn fresh_workspace_rejects_symlinked_stage_without_outside_mutation() {
        let root = temp_dir("workspace-symlink-stage");
        let outside = temp_dir("workspace-symlink-outside");
        fs::create_dir_all(root.join("cache/loaders/work")).expect("work root");
        fs::create_dir_all(&outside).expect("outside root");
        let sentinel = outside.join("sentinel");
        fs::write(&sentinel, b"untouched").expect("sentinel");
        std::os::unix::fs::symlink(&outside, root.join("cache/loaders/work/version"))
            .expect("stage symlink");

        assert!(prepare_fresh_work_dir(&root, "version").is_err());
        assert_eq!(fs::read(&sentinel).expect("sentinel"), b"untouched");
        assert!(root.join("cache/loaders/work/version").is_symlink());
        assert_eq!(fs::read(&sentinel).expect("sentinel"), b"untouched");

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn fresh_workspace_reuses_cleared_admitted_shell() {
        let root = temp_dir("workspace-retained-shell");
        fs::create_dir_all(&root).expect("root");
        let workspace = prepare_fresh_work_dir(&root, "version").expect("fresh workspace");
        fs::write(workspace.path().join("stale"), b"stale").expect("stale artifact");

        workspace.cleanup().expect("clear workspace");

        let stage = root.join("cache/loaders/work/version");
        assert!(stage.is_dir());
        assert_eq!(fs::read_dir(&stage).expect("cleared stage").count(), 0);
        let reused = prepare_fresh_work_dir(&root, "version").expect("reused workspace");
        assert_eq!(reused.path(), stage);
        drop(reused);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn processor_stage_has_canonical_layout_and_typed_observation() {
        let root = temp_dir("processor-stage-layout");
        fs::create_dir_all(&root).expect("root");
        let workspace = prepare_fresh_work_dir(&root, "forge-version").expect("loader workspace");
        let processor = workspace
            .prepare_processor_stage("1.21.5")
            .expect("processor stage");
        let stage = workspace.path().join("processor-stage");

        assert_eq!(processor.root_path(), stage.join("root"));
        assert_eq!(processor.libraries_path(), stage.join("root/libraries"));
        assert_eq!(processor.version_path(), stage.join("root/versions/1.21.5"));
        assert_eq!(
            processor.processor_data_path(),
            stage.join("root/processor-data")
        );
        assert_eq!(processor.home_path(), stage.join("home"));
        assert_eq!(processor.temp_path(), stage.join("tmp"));
        assert_eq!(processor.installer_path(), stage.join("root/installer.jar"));

        let library = ArtifactRelativePath::new("example/library.jar").expect("library path");
        let version = ArtifactRelativePath::new("client.jar").expect("version path");
        let data = ArtifactRelativePath::new("patches/client.bin").expect("data path");
        processor
            .write_library_exact(&library, b"library")
            .await
            .expect("library write");
        processor
            .write_version_exact(&version, b"client")
            .await
            .expect("version write");
        processor
            .write_processor_data_exact(&data, b"patch")
            .await
            .expect("data write");
        processor
            .write_installer_exact(b"installer")
            .await
            .expect("installer write");
        fs::write(processor.home_path().join("home-state"), b"scratch").expect("home scratch");
        fs::write(processor.temp_path().join("temp-state"), b"scratch").expect("temp scratch");

        let library_sha1: [u8; 20] = Sha1::digest(b"library").into();
        assert_eq!(
            processor
                .read_library_authenticated(&library, Some(7), &library_sha1)
                .expect("authenticated library"),
            b"library"
        );
        let version_sha1: [u8; 20] = Sha1::digest(b"client").into();
        assert_eq!(
            processor
                .read_version_authenticated(&version, Some(6), &version_sha1)
                .expect("authenticated version"),
            b"client"
        );
        let root_snapshot = processor.snapshot_root().expect("root snapshot");
        assert!(
            root_snapshot
                .files()
                .contains_key(&ArtifactRelativePath::new("installer.jar").expect("installer path"))
        );
        let stage_snapshot = processor.snapshot_stage().expect("stage snapshot");
        assert!(
            stage_snapshot
                .files()
                .contains_key(&ArtifactRelativePath::new("home/home-state").expect("home path"))
        );
        assert!(
            stage_snapshot
                .files()
                .contains_key(&ArtifactRelativePath::new("tmp/temp-state").expect("temp path"))
        );

        processor.clear_scratch().expect("clear scratch");
        assert_eq!(
            fs::read_dir(processor.home_path())
                .expect("home directory")
                .count(),
            0
        );
        assert_eq!(
            fs::read_dir(processor.temp_path())
                .expect("temp directory")
                .count(),
            0
        );
        processor.revalidate().expect("revalidated stage");
        processor.cleanup().expect("processor cleanup");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn processor_stage_reuse_clears_stale_files_and_keeps_exact_shells() {
        let root = temp_dir("processor-stage-reuse");
        fs::create_dir_all(&root).expect("root");
        let workspace = prepare_fresh_work_dir(&root, "forge-version").expect("loader workspace");
        let first = workspace
            .prepare_processor_stage("1.20.1")
            .expect("first processor stage");
        fs::write(first.libraries_path().join("stale"), b"stale").expect("stale library");
        fs::write(first.home_path().join("stale"), b"stale").expect("stale home");
        drop(first);

        let reused = workspace
            .prepare_processor_stage("1.20.1")
            .expect("reused processor stage");
        assert!(!reused.libraries_path().join("stale").exists());
        assert!(!reused.home_path().join("stale").exists());
        let snapshot = reused.snapshot_stage().expect("reused snapshot");
        let directories = snapshot
            .directories()
            .iter()
            .map(|path| path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            directories,
            vec![
                "home",
                "root",
                "root/libraries",
                "root/processor-data",
                "root/versions",
                "root/versions/1.20.1",
                "tmp",
            ]
        );
        reused.cleanup().expect("reused cleanup");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn processor_stage_reuse_admits_empty_nested_mutable_shells() {
        let root = temp_dir("processor-stage-nested-reuse");
        fs::create_dir_all(&root).expect("root");
        let workspace = prepare_fresh_work_dir(&root, "forge-version").expect("loader workspace");
        let first = workspace
            .prepare_processor_stage("1.20.1")
            .expect("first processor stage");
        let library_shell = first.libraries_path().join("org/example");
        let data_shell = first.processor_data_path().join("patches/client");
        fs::create_dir_all(&library_shell).expect("library shell");
        fs::create_dir_all(&data_shell).expect("data shell");
        fs::write(library_shell.join("library.jar"), b"library").expect("nested library");
        fs::write(data_shell.join("patch.bin"), b"patch").expect("nested data");
        drop(first);

        let reused = workspace
            .prepare_processor_stage("1.20.1")
            .expect("nested shell reuse");
        assert!(library_shell.is_dir());
        assert!(data_shell.is_dir());
        assert_eq!(
            fs::read_dir(&library_shell)
                .expect("empty library shell")
                .count(),
            0
        );
        assert_eq!(
            fs::read_dir(&data_shell).expect("empty data shell").count(),
            0
        );
        reused.cleanup().expect("nested reuse cleanup");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn processor_stage_reuse_rejects_unexpected_retained_directories() {
        let root = temp_dir("processor-stage-unexpected-shell");
        fs::create_dir_all(&root).expect("root");
        let workspace = prepare_fresh_work_dir(&root, "forge-version").expect("loader workspace");
        let first = workspace
            .prepare_processor_stage("1.20.1")
            .expect("first processor stage");
        fs::create_dir(first.root_path().join("unexpected")).expect("unexpected shell");
        drop(first);

        assert!(workspace.prepare_processor_stage("1.20.1").is_err());
        let _ = fs::remove_dir_all(root);
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("axial-{prefix}-{nanos:x}"))
    }
}

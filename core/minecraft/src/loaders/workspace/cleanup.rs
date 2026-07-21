use crate::portable_path::PortableRelativePath;
use crate::loaders::types::LoaderError;
use crate::loaders::validate_version_id;
use crate::managed_fs::{ManagedDir, ManagedTreeLimits, ManagedTreeSnapshot};
use std::io::{Read, Seek};
use std::path::Path;
use tempfile::TempDir;

pub(crate) struct ProcessorWorkspaceOwner {
    temporary: TempDir,
    temporary_root: ManagedDir,
    workspace: ProcessorWorkspace,
    target_version_id: String,
}

pub(crate) struct ProcessorWorkspace {
    owner_root: ManagedDir,
    stage: ManagedDir,
    root: ManagedDir,
    libraries: ManagedDir,
    version: ManagedDir,
    processor_data: ManagedDir,
    home: ManagedDir,
    temp: ManagedDir,
}

impl ProcessorWorkspaceOwner {
    pub(crate) fn target_version_id(&self) -> &str {
        &self.target_version_id
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        self.temporary.path()
    }

    pub(crate) fn workspace(&self) -> &ProcessorWorkspace {
        &self.workspace
    }

    pub(crate) async fn materialize_runtime(
        &self,
        java_version: &crate::launch::JavaVersion,
        source: crate::runtime::RuntimeSourceReceipt,
    ) -> Result<crate::runtime::ProcessorRuntime, crate::runtime::JavaRuntimeLookupError> {
        self.temporary_root.revalidate().map_err(|_| {
            crate::runtime::JavaRuntimeLookupError::Install(
                "processor temporary root identity changed".to_string(),
            )
        })?;
        self.temporary_root
            .validate_exact_child_directories(&["processor-stage"])
            .map_err(|_| {
                crate::runtime::JavaRuntimeLookupError::Install(
                    "processor temporary root identity changed".to_string(),
                )
            })?;
        let usage = self
            .workspace
            .stage
            .validate_tree_usage_no_links(ManagedTreeLimits::processor_stage())
            .map_err(|_| {
                crate::runtime::JavaRuntimeLookupError::Install(
                    "processor temporary root exceeds its admitted bound".to_string(),
                )
            })?;
        let remaining_entries = 4096_usize
            .checked_sub(usage.entries().saturating_add(1))
            .ok_or_else(|| {
                crate::runtime::JavaRuntimeLookupError::Install(
                    "processor temporary root exceeds its entry bound".to_string(),
                )
            })?;
        let remaining_bytes = (512_u64 << 20).checked_sub(usage.bytes()).ok_or_else(|| {
            crate::runtime::JavaRuntimeLookupError::Install(
                "processor temporary root exceeds its byte bound".to_string(),
            )
        })?;
        let runtime_path = self.temporary.path().join("runtime");
        let runtime = crate::runtime::materialize_ephemeral_processor_runtime(
            java_version,
            source,
            &runtime_path,
            remaining_entries,
            remaining_bytes,
        )
        .await?;
        self.temporary_root.revalidate().map_err(|_| {
            crate::runtime::JavaRuntimeLookupError::Install(
                "processor temporary root identity changed".to_string(),
            )
        })?;
        self.workspace.validate_live_bounds().map_err(|_| {
            crate::runtime::JavaRuntimeLookupError::Install(
                "processor runtime destination identity changed".to_string(),
            )
        })?;
        Ok(runtime)
    }

    pub(crate) fn cleanup(self) -> Result<(), LoaderError> {
        let Self {
            temporary,
            temporary_root,
            workspace,
            target_version_id: _,
        } = self;
        let workspace_cleanup = workspace.cleanup();
        drop(temporary_root);
        let root_cleanup = temporary.close().map_err(LoaderError::Io);
        workspace_cleanup?;
        root_cleanup
    }

    pub(crate) fn quarantine(self) {
        let Self {
            temporary,
            temporary_root,
            workspace,
            target_version_id: _,
        } = self;
        drop(workspace);
        drop(temporary_root);
        let _ = temporary.keep();
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
            PortableRelativePath::new(&path).map_err(|_| {
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
        relative: &PortableRelativePath,
        bytes: &[u8],
    ) -> Result<(), LoaderError> {
        self.libraries.write_relative_exact(relative, bytes).await
    }

    pub(crate) async fn import_library_authenticated<R>(
        &self,
        relative: &PortableRelativePath,
        source: R,
        expected_size: u64,
        expected_sha1: [u8; 20],
    ) -> Result<(), LoaderError>
    where
        R: Read + Seek + Send + 'static,
    {
        self.libraries
            .import_relative_authenticated(relative, source, expected_size, expected_sha1)
            .await
    }

    pub(crate) fn ensure_library_parent(
        &self,
        relative: &PortableRelativePath,
    ) -> Result<(), LoaderError> {
        let _ = self.libraries.open_or_create_relative_parent(relative)?;
        self.libraries.revalidate()
    }

    pub(crate) async fn write_version_exact(
        &self,
        relative: &PortableRelativePath,
        bytes: &[u8],
    ) -> Result<(), LoaderError> {
        self.version.write_relative_exact(relative, bytes).await
    }

    pub(crate) async fn write_processor_data_exact(
        &self,
        relative: &PortableRelativePath,
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
        relative: &PortableRelativePath,
        expected_size: Option<u64>,
        expected_sha1: &[u8; 20],
    ) -> Result<Vec<u8>, LoaderError> {
        self.libraries
            .read_relative_authenticated(relative, expected_size, expected_sha1)
    }

    pub(crate) fn read_version_authenticated(
        &self,
        relative: &PortableRelativePath,
        expected_size: Option<u64>,
        expected_sha1: &[u8; 20],
    ) -> Result<Vec<u8>, LoaderError> {
        self.version
            .read_relative_authenticated(relative, expected_size, expected_sha1)
    }

    pub(crate) fn read_processor_data_authenticated(
        &self,
        relative: &PortableRelativePath,
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
            &PortableRelativePath::new("installer.jar").map_err(|_| {
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

    pub(crate) fn validate_live_bounds(&self) -> Result<(), LoaderError> {
        self.owner_root
            .validate_exact_child_directories(&["processor-stage", "runtime"])?;
        let limits = ManagedTreeLimits::processor_stage();
        let stage = self.stage.validate_tree_usage_no_links(limits)?;
        let runtime = self
            .owner_root
            .open_child("runtime")?
            .validate_tree_usage_allow_links(limits)?;
        if stage
            .entries()
            .saturating_add(runtime.entries())
            .saturating_add(2)
            > 4096
            || stage.bytes().saturating_add(runtime.bytes()) > (512_u64 << 20)
        {
            return Err(LoaderError::Verify(
                "processor temporary root exceeds its admitted bound".to_string(),
            ));
        }
        Ok(())
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

pub(crate) fn prepare_ephemeral_processor_workspace(
    version_id: &str,
    minecraft_version: &str,
) -> Result<ProcessorWorkspaceOwner, LoaderError> {
    validate_version_id(version_id, "installer workspace version id")?;
    validate_version_id(minecraft_version, "processor stage Minecraft version")?;
    let temporary = tempfile::Builder::new()
        .prefix("axial-loader-processor-")
        .tempdir()
        .map_err(LoaderError::Io)?;
    let temporary_root = ManagedDir::open_root(temporary.path())?;
    let stage = temporary_root.open_or_create_child("processor-stage")?;
    let root = stage.open_or_create_child("root")?;
    let libraries = root.open_or_create_child("libraries")?;
    let versions = root.open_or_create_child("versions")?;
    let version = versions.open_or_create_child(minecraft_version)?;
    let processor_data = root.open_or_create_child("processor-data")?;
    let home = stage.open_or_create_child("home")?;
    let temp = stage.open_or_create_child("tmp")?;
    let workspace = ProcessorWorkspace {
        owner_root: temporary_root.clone(),
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
    Ok(ProcessorWorkspaceOwner {
        temporary,
        temporary_root,
        workspace,
        target_version_id: version_id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::prepare_ephemeral_processor_workspace;
    use crate::portable_path::PortableRelativePath;
    use sha1::{Digest as _, Sha1};
    use std::fs;

    #[tokio::test]
    async fn ephemeral_processor_workspace_has_canonical_authenticated_layout() {
        let owner = prepare_ephemeral_processor_workspace("forge-version", "1.21.5")
            .expect("processor workspace");
        let temporary = owner.path().to_path_buf();
        let stage = temporary.join("processor-stage");
        let processor = owner.workspace();

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

        let library = PortableRelativePath::new("example/library.jar").expect("library path");
        let version = PortableRelativePath::new("client.jar").expect("version path");
        let data = PortableRelativePath::new("patches/client.bin").expect("data path");
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
                .contains_key(&PortableRelativePath::new("installer.jar").expect("installer path"))
        );
        let stage_snapshot = processor.snapshot_stage().expect("stage snapshot");
        assert!(
            stage_snapshot
                .files()
                .contains_key(&PortableRelativePath::new("home/home-state").expect("home path"))
        );
        assert!(
            stage_snapshot
                .files()
                .contains_key(&PortableRelativePath::new("tmp/temp-state").expect("temp path"))
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
        owner.workspace().revalidate().expect("revalidated owner");
        owner.cleanup().expect("ephemeral cleanup");
        assert!(!temporary.exists());
    }

    #[test]
    fn ephemeral_processor_workspace_starts_fresh_and_closes_completely() {
        let owner = prepare_ephemeral_processor_workspace("forge-version", "1.20.1")
            .expect("processor workspace");
        let temporary = owner.path().to_path_buf();
        let snapshot = owner.workspace().snapshot_stage().expect("fresh snapshot");
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
        owner.cleanup().expect("ephemeral cleanup");
        assert!(!temporary.exists());
    }
}

use crate::loaders::types::{LoaderArtifactKind, LoaderInstallSource};

pub fn install_source_kind(source: &LoaderInstallSource) -> LoaderArtifactKind {
    match source {
        LoaderInstallSource::ProfileJson { .. } => LoaderArtifactKind::ProfileJson,
        LoaderInstallSource::InstallerJar { .. } => LoaderArtifactKind::InstallerJar,
        LoaderInstallSource::LegacyArchive { .. } => LoaderArtifactKind::LegacyArchive,
    }
}

use crate::loaders::types::LoaderBuildRecord;

pub fn install_source_url(record: &LoaderBuildRecord) -> &str {
    match &record.install_source {
        crate::loaders::types::LoaderInstallSource::ProfileJson { url }
        | crate::loaders::types::LoaderInstallSource::InstallerJar { url }
        | crate::loaders::types::LoaderInstallSource::LegacyArchive { url } => url,
    }
}

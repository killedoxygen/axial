use crate::error::{ContentError, ContentResult};
use crate::manifest::{ContentManifest, ManifestEntry};
use crate::model::{CanonicalId, ContentDependency, ContentKind, FileRef, ProviderId};
use crate::transaction::{FileTransaction, StagingGuard, contained_path};
use axial_minecraft::download::{
    DownloadProgress, ExpectedIntegrity, download_file_with_client_report,
};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// A single resolved file the pipeline should download and record. Callers build
/// these from a resolution plan (selected content plus its dependencies).
#[derive(Debug, Clone)]
pub struct PlannedFile {
    pub canonical_id: CanonicalId,
    pub provider: ProviderId,
    pub project_id: String,
    pub version_id: String,
    pub kind: ContentKind,
    pub file: FileRef,
    pub dependencies: Vec<ContentDependency>,
    pub title: Option<String>,
}

/// Download and verify each planned file into its instance subdirectory, then
/// record it in the instance manifest. Files land through the shared verified
/// downloader (size + sha1 checked); a replaced file is removed only after its
/// replacement is in place. The manifest is saved once at the end.
pub async fn install_and_record<F>(
    client: &reqwest::Client,
    game_dir: &Path,
    files: &[PlannedFile],
    mut on_progress: F,
) -> ContentResult<ContentManifest>
where
    F: FnMut(DownloadProgress),
{
    let mut manifest = ContentManifest::load(game_dir)?;
    let total = files.len() as i32;
    let staging = StagingGuard::create(game_dir, "axial-content-stage")?;
    let mut relative_paths = Vec::with_capacity(files.len());

    for (index, planned) in files.iter().enumerate() {
        let Some(kind_dir) = planned.kind.install_subdir() else {
            return Err(ContentError::Invalid(format!(
                "{} is not installable as a single file",
                planned.kind.as_str()
            )));
        };
        let relative = format!("{kind_dir}/{}", planned.file.filename);
        let destination = contained_path(staging.path(), &relative)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }

        on_progress(progress(
            "download",
            index as i32,
            total,
            Some(planned.file.filename.clone()),
        ));

        let expected = ExpectedIntegrity {
            size: planned.file.size,
            sha1: planned.file.sha1.clone(),
        };
        download_file_with_client_report(client, &planned.file.url, &destination, &expected)
            .await
            .map_err(|error| ContentError::Download(error.into_download_error().to_string()))?;

        relative_paths.push(relative);
    }

    on_progress(progress("commit", total, total, None));
    let transaction = FileTransaction::apply(game_dir, staging.transfer(), &relative_paths)?;
    let mut stale_files = Vec::new();

    for planned in files {
        let kind_dir = planned.kind.install_subdir().ok_or_else(|| {
            ContentError::Invalid(format!(
                "{} is not installable as a single file",
                planned.kind.as_str()
            ))
        })?;
        let subdir = game_dir.join(kind_dir);
        let entry = ManifestEntry::managed(
            planned.canonical_id.clone(),
            planned.provider,
            planned.project_id.clone(),
            planned.version_id.clone(),
            planned.kind,
            &planned.file,
            planned.dependencies.clone(),
            planned.title.clone(),
        );
        if let Some(stale) = manifest.upsert(entry) {
            stale_files.push((subdir, stale));
        }
    }

    if let Err(error) = manifest.save(game_dir) {
        transaction.rollback();
        return Err(error);
    }
    transaction.commit();
    let installed_destinations: HashSet<PathBuf> = relative_paths
        .iter()
        .map(|relative| game_dir.join(relative))
        .collect();
    for (subdir, stale) in stale_files {
        remove_content_file_except(&subdir, &stale, &installed_destinations);
    }
    on_progress(done(total));
    Ok(manifest)
}

/// Remove a managed file (enabled or disabled variant) and drop its manifest
/// entry. Saves the manifest when an entry was actually removed.
pub fn uninstall(game_dir: &Path, canonical_id: &CanonicalId) -> ContentResult<bool> {
    let mut manifest = ContentManifest::load(game_dir)?;
    let Some(entry) = manifest.remove(canonical_id) else {
        return Ok(false);
    };
    if let Some(kind_dir) = entry.kind.install_subdir() {
        remove_content_file(&game_dir.join(kind_dir), &entry.filename);
    }
    manifest.save(game_dir)?;
    Ok(true)
}

fn remove_content_file(subdir: &Path, filename: &str) {
    let _ = fs::remove_file(subdir.join(filename));
    let _ = fs::remove_file(subdir.join(format!("{filename}.disabled")));
}

fn remove_content_file_except(subdir: &Path, filename: &str, protected: &HashSet<PathBuf>) {
    for candidate in [
        subdir.join(filename),
        subdir.join(format!("{filename}.disabled")),
    ] {
        if !protected.contains(&candidate) {
            let _ = fs::remove_file(candidate);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_cleanup_preserves_destinations_installed_by_the_same_batch() {
        let root = std::env::temp_dir().join(format!(
            "axial-content-stale-cleanup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        let mods = root.join("mods");
        fs::create_dir_all(&mods).expect("create mods directory");
        let installed = mods.join("common.jar");
        let stale_disabled = mods.join("common.jar.disabled");
        fs::write(&installed, b"new content").expect("write installed file");
        fs::write(&stale_disabled, b"stale content").expect("write stale disabled file");
        let protected = HashSet::from([installed.clone()]);

        remove_content_file_except(&mods, "common.jar", &protected);

        assert_eq!(
            fs::read(&installed).expect("installed file"),
            b"new content"
        );
        assert!(!stale_disabled.exists());
        let _ = fs::remove_dir_all(root);
    }
}

fn progress(phase: &str, current: i32, total: i32, file: Option<String>) -> DownloadProgress {
    DownloadProgress {
        phase: phase.to_string(),
        current,
        total,
        file,
        error: None,
        done: false,
        bytes_done: None,
        bytes_total: None,
    }
}

fn done(total: i32) -> DownloadProgress {
    DownloadProgress {
        phase: "done".to_string(),
        current: total,
        total,
        file: None,
        error: None,
        done: true,
        bytes_done: None,
        bytes_total: None,
    }
}

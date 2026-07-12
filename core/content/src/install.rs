use crate::error::{ContentError, ContentResult};
use crate::manifest::{ContentManifest, ManifestEntry};
use crate::model::{CanonicalId, ContentDependency, ContentKind, FileRef, ProviderId};
use axial_minecraft::download::{
    DownloadProgress, ExpectedIntegrity, download_file_with_client_report,
};
use std::fs;
use std::path::Path;

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

    for (index, planned) in files.iter().enumerate() {
        let Some(kind_dir) = planned.kind.install_subdir() else {
            return Err(ContentError::Invalid(format!(
                "{} is not installable as a single file",
                planned.kind.as_str()
            )));
        };
        let subdir = game_dir.join(kind_dir);
        fs::create_dir_all(&subdir)?;
        let destination = subdir.join(&planned.file.filename);

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
            remove_content_file(&subdir, &stale);
        }
    }

    manifest.save(game_dir)?;
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

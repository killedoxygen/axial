use crate::error::{ContentError, ContentResult};
use crate::manifest::{ContentManifest, ManifestEntry, entry_path_matches};
use crate::model::{CanonicalId, ContentDependency, ContentKind, FileRef, ProviderId};
use crate::transaction::{FileTransaction, StagingGuard, contained_path};
use axial_minecraft::download::{
    DownloadProgress, ExpectedIntegrity, download_file_with_client_report,
};
use std::collections::{HashMap, HashSet};
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
    let destinations = prepare_install_destinations(game_dir, &manifest, files)?;
    let staging = StagingGuard::create(game_dir, "axial-content-stage")?;

    for (index, (planned, planned_destination)) in files.iter().zip(&destinations).enumerate() {
        let destination = contained_path(staging.path(), &planned_destination.relative)?;
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
    }

    on_progress(progress("commit", total, total, None));
    let relative_paths: Vec<String> = destinations
        .iter()
        .map(|destination| destination.relative.clone())
        .collect();
    let must_be_absent: Vec<String> = destinations
        .iter()
        .filter(|destination| destination.must_be_absent)
        .map(|destination| destination.relative.clone())
        .collect();
    let mut transaction = FileTransaction::apply_preserving_absence(
        game_dir,
        staging.transfer(),
        &relative_paths,
        &must_be_absent,
    )?;
    let mut stale_files = Vec::new();

    for (planned, planned_destination) in files.iter().zip(&destinations) {
        let previous_filename = manifest
            .find(&planned.canonical_id)
            .map(|entry| entry.filename.clone());
        let mut entry = ManifestEntry::managed(
            planned.canonical_id.clone(),
            planned.provider,
            planned.project_id.clone(),
            planned.version_id.clone(),
            planned.kind,
            &planned.file,
            planned.dependencies.clone(),
            planned.title.clone(),
        );
        entry.enabled = planned_destination.enabled;
        manifest.upsert(entry);
        if let Some(previous_filename) = previous_filename {
            stale_files.extend(managed_file_variants(planned.kind, &previous_filename));
        }
    }

    stale_files.sort();
    stale_files.dedup();
    transaction.stage_removals(&stale_files)?;
    if let Err(error) = manifest.save(game_dir) {
        transaction.rollback();
        return Err(error);
    }
    transaction.commit();
    on_progress(done(total));
    Ok(manifest)
}

#[derive(Debug)]
struct InstallDestination {
    enabled: bool,
    relative: String,
    variants: Vec<String>,
    must_be_absent: bool,
}

/// Resolve and validate every final path before downloading. A manifest entry
/// owns both its enabled and disabled variants; an existing unmanaged path is
/// never implicitly adopted or overwritten.
fn prepare_install_destinations(
    game_dir: &Path,
    manifest: &ContentManifest,
    files: &[PlannedFile],
) -> ContentResult<Vec<InstallDestination>> {
    let mut seen_ids = HashSet::new();
    let mut batch_variants = HashSet::new();
    let mut variants_by_id: HashMap<CanonicalId, Vec<String>> = HashMap::new();
    let mut manifest_variant_owners: HashMap<String, Vec<&ManifestEntry>> = HashMap::new();
    for entry in &manifest.entries {
        for variant in managed_file_variants(entry.kind, &entry.filename) {
            manifest_variant_owners
                .entry(variant)
                .or_default()
                .push(entry);
        }
    }
    let mut destinations = Vec::with_capacity(files.len());

    for planned in files {
        if !seen_ids.insert(planned.canonical_id.clone()) {
            return Err(ContentError::Invalid(
                "the content plan contains the same project more than once".to_string(),
            ));
        }
        if planned.file.filename.is_empty() || planned.file.filename.ends_with(".disabled") {
            return Err(ContentError::Invalid(
                "the provider returned an invalid content filename".to_string(),
            ));
        }
        let variants = managed_file_variants(planned.kind, &planned.file.filename);
        if variants.len() != 2 {
            return Err(ContentError::Invalid(format!(
                "{} is not installable as a single file",
                planned.kind.as_str()
            )));
        }
        for variant in &variants {
            contained_path(game_dir, variant)?;
            if !batch_variants.insert(variant.clone()) {
                return Err(ContentError::Invalid(
                    "multiple content projects resolve to the same destination".to_string(),
                ));
            }
        }
        let enabled = preserved_enabled_state(game_dir, manifest, &planned.canonical_id);
        let relative = variants[usize::from(!enabled)].clone();
        variants_by_id.insert(planned.canonical_id.clone(), variants.clone());
        destinations.push(InstallDestination {
            enabled,
            relative,
            variants,
            must_be_absent: true,
        });
    }

    for (planned, destination) in files.iter().zip(&mut destinations) {
        let mut selected_destination_exists = false;
        for variant in &destination.variants {
            let owners = manifest_variant_owners
                .get(variant)
                .map(Vec::as_slice)
                .unwrap_or_default();
            for owner in owners {
                if owner.canonical_id == planned.canonical_id {
                    continue;
                }
                let owner_moves_away = variants_by_id
                    .get(&owner.canonical_id)
                    .is_some_and(|new_variants| !new_variants.contains(variant));
                if !owner_moves_away {
                    return Err(ContentError::Invalid(
                        "a content destination is already owned by another project".to_string(),
                    ));
                }
            }

            let path = contained_path(game_dir, variant)?;
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() && !owners.is_empty() => {
                    if owners.iter().any(|owner| !entry_path_matches(&path, owner)) {
                        return Err(ContentError::Invalid(
                            "a content destination is occupied by a file no longer owned by the manifest"
                                .to_string(),
                        ));
                    }
                    if variant == &destination.relative {
                        selected_destination_exists = true;
                    }
                }
                Ok(_) if owners.is_empty() => {
                    return Err(ContentError::Invalid(
                        "a content destination is already occupied by an unmanaged file"
                            .to_string(),
                    ));
                }
                Ok(_) => {
                    return Err(ContentError::Invalid(
                        "a managed content destination is not a regular file".to_string(),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(ContentError::Io(error)),
            }
        }
        destination.must_be_absent = !selected_destination_exists;
    }

    Ok(destinations)
}

/// Remove a managed file (enabled or disabled variant) and drop its manifest
/// entry. Saves the manifest when an entry was actually removed.
pub fn uninstall(game_dir: &Path, canonical_id: &CanonicalId) -> ContentResult<bool> {
    let mut manifest = ContentManifest::load(game_dir)?;
    let Some(entry) = manifest.remove(canonical_id) else {
        return Ok(false);
    };
    let mut transaction = FileTransaction::empty(game_dir)?;
    transaction.stage_removals(&managed_file_variants(entry.kind, &entry.filename))?;
    if let Err(error) = manifest.save(game_dir) {
        transaction.rollback();
        return Err(error);
    }
    transaction.commit();
    Ok(true)
}

/// Relative enabled and disabled paths owned by one manifest entry.
pub fn managed_file_variants(kind: ContentKind, filename: &str) -> Vec<String> {
    let Some(kind_dir) = kind.install_subdir() else {
        return Vec::new();
    };
    vec![
        format!("{kind_dir}/{filename}"),
        format!("{kind_dir}/{filename}.disabled"),
    ]
}

fn preserved_enabled_state(
    game_dir: &Path,
    manifest: &ContentManifest,
    canonical_id: &CanonicalId,
) -> bool {
    let Some(existing) = manifest.find(canonical_id) else {
        return true;
    };
    let Some(kind_dir) = existing.kind.install_subdir() else {
        return existing.enabled;
    };
    let subdir = game_dir.join(kind_dir);
    let enabled_path = subdir.join(&existing.filename);
    let disabled_path = subdir.join(format!("{}.disabled", existing.filename));
    if enabled_path.exists() {
        true
    } else if disabled_path.exists() {
        false
    } else {
        existing.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned(project: &str, filename: &str) -> PlannedFile {
        PlannedFile {
            canonical_id: CanonicalId::for_project(ProviderId::Modrinth, project),
            provider: ProviderId::Modrinth,
            project_id: project.to_string(),
            version_id: format!("{project}-version"),
            kind: ContentKind::Mod,
            file: FileRef {
                url: format!("https://example.invalid/{filename}"),
                filename: filename.to_string(),
                sha1: None,
                sha512: None,
                size: None,
                primary: true,
            },
            dependencies: Vec::new(),
            title: Some(project.to_string()),
        }
    }

    fn recorded(project: &str, filename: &str) -> ManifestEntry {
        let planned = planned(project, filename);
        ManifestEntry::managed(
            planned.canonical_id,
            planned.provider,
            planned.project_id,
            planned.version_id,
            planned.kind,
            &planned.file,
            Vec::new(),
            planned.title,
        )
    }

    fn test_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "axial-content-install-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn unmanaged_enabled_or_disabled_destinations_are_rejected() {
        for suffix in ["", ".disabled"] {
            let root = test_root(if suffix.is_empty() {
                "unmanaged-enabled"
            } else {
                "unmanaged-disabled"
            });
            fs::create_dir_all(root.join("mods")).expect("mods");
            fs::write(root.join(format!("mods/shared.jar{suffix}")), b"user file")
                .expect("unmanaged file");

            assert!(
                prepare_install_destinations(
                    &root,
                    &ContentManifest::default(),
                    &[planned("new", "shared.jar")],
                )
                .is_err()
            );
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn same_name_manual_replacement_is_not_treated_as_managed() {
        let root = test_root("same-name-replacement");
        fs::create_dir_all(root.join("mods")).expect("mods");
        let path = root.join("mods/shared.jar");
        fs::write(&path, b"launcher-owned bytes").expect("managed file");

        let mut entry = recorded("project", "shared.jar");
        entry.sha512 = Some(crate::manifest::sha512_file(&path).expect("managed hash"));
        entry.size = Some(b"launcher-owned bytes".len() as u64);
        let mut manifest = ContentManifest::default();
        manifest.upsert(entry);

        fs::write(&path, b"manually replaced user bytes").expect("user replacement");
        let result =
            prepare_install_destinations(&root, &manifest, &[planned("project", "shared.jar")]);

        assert!(result.is_err());
        assert_eq!(
            fs::read(&path).expect("preserved user replacement"),
            b"manually replaced user bytes"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn destination_owned_by_unselected_content_is_rejected() {
        let root = test_root("owned-collision");
        let mut manifest = ContentManifest::default();
        manifest.upsert(recorded("existing", "shared.jar"));

        assert!(
            prepare_install_destinations(&root, &manifest, &[planned("new", "shared.jar")],)
                .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn batch_can_reuse_a_path_only_when_its_owner_moves_away() {
        let root = test_root("owned-batch-move");
        let mut manifest = ContentManifest::default();
        manifest.upsert(recorded("first", "common.jar"));
        manifest.upsert(recorded("second", "second.jar"));

        let result = prepare_install_destinations(
            &root,
            &manifest,
            &[
                planned("first", "first.jar"),
                planned("second", "common.jar"),
            ],
        );

        assert!(result.is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn batch_projects_cannot_claim_the_same_destination() {
        let root = test_root("batch-collision");

        assert!(
            prepare_install_destinations(
                &root,
                &ContentManifest::default(),
                &[
                    planned("first", "shared.jar"),
                    planned("second", "shared.jar")
                ],
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn updates_preserve_the_existing_disabled_file_state() {
        let root = std::env::temp_dir().join(format!(
            "axial-content-disabled-update-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        let mods = root.join("mods");
        fs::create_dir_all(&mods).expect("create mods directory");
        fs::write(mods.join("old.jar.disabled"), b"disabled").expect("write disabled mod");
        let id = CanonicalId::for_project(ProviderId::Modrinth, "project");
        let mut manifest = ContentManifest::default();
        manifest.upsert(ManifestEntry::managed(
            id.clone(),
            ProviderId::Modrinth,
            "project".to_string(),
            "old-version".to_string(),
            ContentKind::Mod,
            &FileRef {
                url: "https://example.invalid/old.jar".to_string(),
                filename: "old.jar".to_string(),
                sha1: None,
                sha512: None,
                size: None,
                primary: true,
            },
            Vec::new(),
            None,
        ));

        assert!(!preserved_enabled_state(&root, &manifest, &id));
        fs::rename(mods.join("old.jar.disabled"), mods.join("old.jar"))
            .expect("enable existing mod");
        assert!(preserved_enabled_state(&root, &manifest, &id));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn uninstall_does_not_commit_when_the_managed_path_cannot_be_removed() {
        let root = std::env::temp_dir().join(format!(
            "axial-content-uninstall-failure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        fs::create_dir_all(root.join("mods/managed.jar")).expect("managed path fixture");
        let id = CanonicalId::for_project(ProviderId::Modrinth, "project");
        let mut manifest = ContentManifest::default();
        manifest.upsert(ManifestEntry::managed(
            id.clone(),
            ProviderId::Modrinth,
            "project".to_string(),
            "version".to_string(),
            ContentKind::Mod,
            &FileRef {
                url: "https://example.invalid/managed.jar".to_string(),
                filename: "managed.jar".to_string(),
                sha1: None,
                sha512: None,
                size: None,
                primary: true,
            },
            Vec::new(),
            None,
        ));
        manifest.save(&root).expect("save manifest");

        assert!(uninstall(&root, &id).is_err());
        let persisted = ContentManifest::load(&root).expect("reload manifest");
        assert!(persisted.find(&id).is_some());
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

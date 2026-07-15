use crate::error::{ContentError, ContentResult};
use crate::manifest::{ContentManifest, ManifestEntry, entry_file_present, entry_path_matches};
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
    let mut stale_entries = Vec::new();

    for (planned, planned_destination) in files.iter().zip(&destinations) {
        let previous = manifest.find(&planned.canonical_id).cloned();
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
        if manifest.upsert(entry).is_some()
            && let Some(previous) = previous
        {
            stale_entries.push(previous);
        }
    }

    let mut stale_files = Vec::new();
    for entry in &stale_entries {
        stale_files.extend(verified_removable_variants(
            game_dir,
            entry,
            &relative_paths,
        )?);
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
                    if owners.iter().any(|owner| {
                        !entry_has_checksum(owner) || !entry_path_matches(&path, owner)
                    }) {
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
    uninstall_many(game_dir, std::slice::from_ref(canonical_id)).map(|removed| removed > 0)
}

/// Remove a set of managed files in one transaction. Dependencies within the
/// selected set may be removed together, while live content outside the set
/// still protects anything it requires.
pub fn uninstall_many(game_dir: &Path, canonical_ids: &[CanonicalId]) -> ContentResult<usize> {
    let mut manifest = ContentManifest::load(game_dir)?;
    let requested = canonical_ids.iter().collect::<HashSet<_>>();
    let entries = manifest
        .entries
        .iter()
        .filter(|entry| requested.contains(&entry.canonical_id))
        .cloned()
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return Ok(0);
    }
    let selected = entries
        .iter()
        .map(|entry| &entry.canonical_id)
        .collect::<HashSet<_>>();
    if manifest.entries.iter().any(|dependent| {
        !selected.contains(&dependent.canonical_id)
            && entry_file_present(game_dir, dependent)
            && entries.iter().any(|entry| {
                dependent.dependencies.iter().any(|dependency| {
                    dependency.requires_project(&entry.project_id, &entry.version_id)
                })
            })
    }) {
        return Err(ContentError::Invalid(
            "content is required by another installed item".to_string(),
        ));
    }
    let mut removable = Vec::new();
    for entry in &entries {
        removable.extend(verified_removable_variants(game_dir, entry, &[])?);
    }
    removable.sort();
    removable.dedup();
    for entry in &entries {
        manifest.remove(&entry.canonical_id);
    }
    let mut transaction = FileTransaction::empty(game_dir)?;
    transaction.stage_removals(&removable)?;
    if let Err(error) = manifest.save(game_dir) {
        transaction.rollback();
        return Err(error);
    }
    transaction.commit();
    Ok(entries.len())
}

/// Return only manifest-owned variants that are still safe to remove. A live
/// path whose bytes no longer match provenance is user-owned and aborts the
/// whole cleanup. Destinations installed by the same transaction are protected
/// from stale cleanup without being compared against the superseded entry.
pub fn verified_removable_variants(
    game_dir: &Path,
    entry: &ManifestEntry,
    protected_paths: &[String],
) -> ContentResult<Vec<String>> {
    let mut removable = Vec::new();
    for relative in managed_file_variants(entry.kind, &entry.filename) {
        if protected_paths
            .iter()
            .any(|protected| protected == &relative)
        {
            continue;
        }
        let path = contained_path(game_dir, &relative)?;
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ContentError::Io(error)),
            Ok(metadata) if !metadata.is_file() => {
                return Err(ContentError::Invalid(
                    "a managed content path is no longer a regular file".to_string(),
                ));
            }
            Ok(_) if entry_has_checksum(entry) && entry_path_matches(&path, entry) => {
                removable.push(relative);
            }
            Ok(_) => {
                return Err(ContentError::Invalid(
                    "a managed content file changed outside the launcher".to_string(),
                ));
            }
        }
    }
    Ok(removable)
}

fn entry_has_checksum(entry: &ManifestEntry) -> bool {
    entry.sha512.is_some() || entry.sha1.is_some()
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

    #[test]
    fn uninstall_preserves_a_same_name_manual_replacement_and_provenance() {
        let root = test_root("uninstall-user-replacement");
        fs::create_dir_all(root.join("mods")).expect("mods");
        let path = root.join("mods/managed.jar");
        fs::write(&path, b"managed bytes").expect("managed file");
        let id = CanonicalId::for_project(ProviderId::Modrinth, "project");
        let mut entry = recorded("project", "managed.jar");
        entry.sha512 = Some(crate::manifest::sha512_file(&path).expect("managed hash"));
        entry.size = Some(b"managed bytes".len() as u64);
        let mut manifest = ContentManifest::default();
        manifest.upsert(entry);
        manifest.save(&root).expect("save manifest");

        fs::write(&path, b"user replacement").expect("replace managed file");
        assert!(uninstall(&root, &id).is_err());

        assert_eq!(
            fs::read(&path).expect("preserved replacement"),
            b"user replacement"
        );
        assert!(
            ContentManifest::load(&root)
                .expect("reload manifest")
                .find(&id)
                .is_some()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn uninstall_rejects_removing_a_live_required_dependency() {
        for version_only in [false, true] {
            let root = test_root(if version_only {
                "uninstall-required-version-only"
            } else {
                "uninstall-required-project"
            });
            fs::create_dir_all(root.join("mods")).expect("mods");
            fs::write(root.join("mods/dependency.jar"), b"dependency").expect("dependency file");
            fs::write(root.join("mods/dependent.jar"), b"dependent").expect("dependent file");

            let dependency = recorded("dependency", "dependency.jar");
            let dependency_id = dependency.canonical_id.clone();
            let mut dependent = recorded("dependent", "dependent.jar");
            dependent.dependencies.push(ContentDependency {
                project_id: (!version_only).then(|| dependency.project_id.clone()),
                version_id: Some(dependency.version_id.clone()),
                kind: crate::model::DependencyKind::Required,
            });
            let mut manifest = ContentManifest::default();
            manifest.upsert(dependency);
            manifest.upsert(dependent);
            manifest.save(&root).expect("save manifest");

            let error = uninstall(&root, &dependency_id)
                .expect_err("a live required dependency must not be removed");
            assert!(error.to_string().contains("required"));
            assert!(root.join("mods/dependency.jar").is_file());
            assert_eq!(
                ContentManifest::load(&root)
                    .expect("reload manifest")
                    .entries
                    .len(),
                2
            );
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn batch_uninstall_removes_dependents_and_dependencies_atomically() {
        let root = test_root("batch-uninstall-dependency-closure");
        fs::create_dir_all(root.join("mods")).expect("mods");
        fs::write(root.join("mods/dependency.jar"), b"dependency").expect("dependency file");
        fs::write(root.join("mods/dependent.jar"), b"dependent").expect("dependent file");

        let mut dependency = recorded("dependency", "dependency.jar");
        dependency.sha512 = Some(
            crate::manifest::sha512_file(&root.join("mods/dependency.jar"))
                .expect("dependency hash"),
        );
        let dependency_id = dependency.canonical_id.clone();
        let mut dependent = recorded("dependent", "dependent.jar");
        dependent.sha512 = Some(
            crate::manifest::sha512_file(&root.join("mods/dependent.jar")).expect("dependent hash"),
        );
        let dependent_id = dependent.canonical_id.clone();
        dependent.dependencies.push(ContentDependency {
            project_id: Some(dependency.project_id.clone()),
            version_id: Some(dependency.version_id.clone()),
            kind: crate::model::DependencyKind::Required,
        });
        let mut manifest = ContentManifest::default();
        manifest.upsert(dependency);
        manifest.upsert(dependent);
        manifest.save(&root).expect("save manifest");

        let removed = uninstall_many(&root, &[dependency_id, dependent_id])
            .expect("the selected dependency closure should be removable in any input order");

        assert_eq!(removed, 2);
        assert!(!root.join("mods/dependency.jar").exists());
        assert!(!root.join("mods/dependent.jar").exists());
        assert!(
            ContentManifest::load(&root)
                .expect("reload manifest")
                .entries
                .is_empty()
        );
        let _ = fs::remove_dir_all(root);
    }
}

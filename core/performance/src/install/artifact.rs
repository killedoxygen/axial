use super::manager::PerformanceManager;
use super::model::InstallError;
use super::mutation::ManagedMutationError;
use super::promotion::{promote_file_async, reconcile_managed_replace_backups};
use crate::MANAGED_ARTIFACT_MAX_BYTES;
use crate::modrinth::{ModrinthError, Version};
use crate::types::{
    CompositionPlan, CompositionState, InstalledMod, ManagedArtifactIntegrity,
    ManagedArtifactProvider, ManagedArtifactSource, ManagedMod, OwnershipClass,
};
use chrono::Utc;
use futures_util::{StreamExt, stream};
use sha2::{Digest, Sha512};
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;
use tracing::warn;

const MANAGED_ARTIFACT_INSTALL_CONCURRENCY: usize = 4;
pub(super) const MANAGED_ARTIFACT_INSTALL_FAILURE: &str = "managed artifact install failed";

impl PerformanceManager {
    async fn install_mod(
        &self,
        managed_mod: &crate::types::ManagedMod,
        game_version: &str,
        loader: &str,
        instance_mods_dir: &Path,
        previous_state: Option<&CompositionState>,
    ) -> Result<InstalledMod, ManagedMutationError> {
        let loaders = vec![loader.to_string()];

        let versions = self
            .resolve_managed_mod_versions(managed_mod, game_version, &loaders)
            .await
            .map_err(ManagedMutationError::definite)?;
        let version = versions.into_iter().next().ok_or_else(|| {
            ManagedMutationError::definite(InstallError::NoCompatibleVersion(
                managed_mod.project_id.clone(),
            ))
        })?;
        let file = version.primary_file().ok_or_else(|| {
            ManagedMutationError::definite(InstallError::NoPrimaryFile(
                managed_mod.project_id.clone(),
            ))
        })?;
        let filename =
            sanitize_mod_filename(&file.filename).map_err(ManagedMutationError::definite)?;
        let expected_sha = file.hashes.get("sha512").cloned().unwrap_or_default();
        if let Some(size) = file.size
            && size > MANAGED_ARTIFACT_MAX_BYTES
        {
            return Err(ManagedMutationError::definite(InstallError::Modrinth(
                ModrinthError::SizeExceeded {
                    expected: MANAGED_ARTIFACT_MAX_BYTES,
                    actual: size,
                },
            )));
        }
        let final_path = instance_mods_dir.join(&filename);
        let previously_tracked = tracked_artifact(previous_state, &filename);
        let was_previously_tracked = previously_tracked.is_some();
        let temp_path = managed_artifact_temp_path(&final_path, managed_mod);
        reconcile_managed_replace_backups(
            &final_path,
            previously_tracked.map(|installed| installed.integrity.sha512.as_str()),
        )
        .await
        .map_err(|error| ManagedMutationError::indeterminate("artifact_reconcile", error))?;

        if tokio::fs::try_exists(&final_path)
            .await
            .map_err(ManagedMutationError::definite)?
        {
            if let Some(previous) = previously_tracked
                && !file_matches_sha512(&final_path, &previous.integrity.sha512, None)
                    .await
                    .map_err(ManagedMutationError::definite)?
            {
                return Err(ManagedMutationError::definite(
                    InstallError::ManagedArtifactTargetExists(filename),
                ));
            }
            if !was_previously_tracked {
                return Err(ManagedMutationError::definite(
                    InstallError::ManagedArtifactTargetExists(filename),
                ));
            }
            if !expected_sha.trim().is_empty()
                && let Ok(true) = file_matches_sha512(&final_path, &expected_sha, file.size).await
            {
                reconcile_promoted_temp(&temp_path, &final_path, &expected_sha, file.size)
                    .await
                    .map_err(|error| {
                        ManagedMutationError::indeterminate("artifact_reconcile", error)
                    })?;
                return Ok(InstalledMod {
                    project_id: managed_mod.project_id.clone(),
                    version_id: version.id,
                    filename,
                    ownership_class: OwnershipClass::CompositionManaged,
                    source: modrinth_source(),
                    integrity: verified_sha512_integrity(expected_sha),
                });
            }
        }

        let download = self
            .modrinth
            .download_file_to_path(&file.url, &expected_sha, file.size, &temp_path)
            .await
            .map_err(ManagedMutationError::definite)?;
        let ownership_sha512 = download.sha512().to_string();
        if tokio::fs::try_exists(&final_path)
            .await
            .map_err(ManagedMutationError::definite)?
            && !was_previously_tracked
        {
            download
                .cleanup()
                .await
                .map_err(|error| ManagedMutationError::indeterminate("artifact_cleanup", error))?;
            return Err(ManagedMutationError::definite(
                InstallError::ManagedArtifactTargetExists(filename),
            ));
        }
        promote_file_async(
            download,
            &final_path,
            &filename,
            previously_tracked.map(|installed| installed.integrity.sha512.as_str()),
        )
        .await
        .map_err(|error| ManagedMutationError::indeterminate("artifact_promote", error))?;

        Ok(InstalledMod {
            project_id: managed_mod.project_id.clone(),
            version_id: version.id,
            filename,
            ownership_class: OwnershipClass::CompositionManaged,
            source: modrinth_source(),
            integrity: if expected_sha.trim().is_empty() {
                unverified_sha512_integrity(ownership_sha512)
            } else {
                verified_sha512_integrity(ownership_sha512)
            },
        })
    }

    pub(super) async fn attempt_install_plan(
        &self,
        plan: &CompositionPlan,
        game_version: &str,
        instance_mods_dir: &Path,
        previous_state: Option<&CompositionState>,
    ) -> Result<CompositionState, ManagedMutationError> {
        let mut state = CompositionState {
            composition_id: plan.composition_id.clone(),
            tier: plan.tier,
            installed_mods: Vec::with_capacity(plan.mods.len()),
            installed_at: Utc::now().to_rfc3339(),
            failure_count: 0,
            last_failure: String::new(),
        };

        let loader = plan.loader.clone();
        let game_version = game_version.to_string();
        let instance_mods_dir = instance_mods_dir.to_path_buf();
        let previous_state = previous_state.cloned();
        let managed_mods = plan.mods.iter().map(ManagedMod::clone);
        let mut installs = stream::iter(managed_mods.map(|managed_mod| {
            let loader = loader.clone();
            let game_version = game_version.clone();
            let instance_mods_dir = instance_mods_dir.clone();
            let previous_state = previous_state.clone();
            async move {
                let project_id = managed_mod.project_id.clone();
                let result = self
                    .install_mod(
                        &managed_mod,
                        &game_version,
                        &loader,
                        &instance_mods_dir,
                        previous_state.as_ref(),
                    )
                    .await;
                (project_id, result)
            }
        }))
        .buffer_unordered(managed_artifact_install_concurrency(plan.mods.len()));

        let mut indeterminate = None;
        while let Some((project_id, result)) = installs.next().await {
            match result {
                Ok(installed) => state.installed_mods.push(installed),
                Err(ManagedMutationError::Definite(error)) => {
                    state.failure_count += 1;
                    state.last_failure = managed_artifact_failure_evidence();
                    warn!("performance install failed for {project_id}: {error}");
                }
                Err(error @ ManagedMutationError::Indeterminate(_)) => {
                    warn!("performance install outcome is indeterminate for {project_id}: {error}");
                    if indeterminate.is_none() {
                        indeterminate = Some(error);
                    }
                }
            }
        }

        if let Some(error) = indeterminate {
            return Err(error);
        }

        state
            .installed_mods
            .sort_by(|left, right| left.project_id.cmp(&right.project_id));
        Ok(state)
    }
    pub(super) async fn resolve_managed_mod_versions(
        &self,
        managed_mod: &crate::types::ManagedMod,
        game_version: &str,
        loaders: &[String],
    ) -> Result<Vec<Version>, InstallError> {
        let project_result = self
            .list_versions_with_game_fallback(&managed_mod.project_id, game_version, loaders)
            .await;

        match project_result {
            Ok(versions) if !versions.is_empty() => Ok(versions),
            Ok(_) => self
                .list_versions_with_game_fallback(&managed_mod.slug, game_version, loaders)
                .await
                .map_err(InstallError::Modrinth),
            Err(ModrinthError::Http { status: 404, .. }) => self
                .list_versions_with_game_fallback(&managed_mod.slug, game_version, loaders)
                .await
                .map_err(InstallError::Modrinth),
            Err(error) => Err(InstallError::Modrinth(error)),
        }
    }

    async fn list_versions_with_game_fallback(
        &self,
        project_ref: &str,
        game_version: &str,
        loaders: &[String],
    ) -> Result<Vec<Version>, ModrinthError> {
        let game_versions = vec![game_version.to_string()];
        let mut versions = self
            .modrinth
            .list_versions(project_ref, &game_versions, loaders)
            .await?;
        if versions.is_empty()
            && let Some(parent_minor) = parent_minor_version(game_version)
            && parent_minor != game_version
        {
            versions = self
                .modrinth
                .list_versions(project_ref, &[parent_minor], loaders)
                .await?;
        }
        Ok(versions)
    }
}

pub(super) fn managed_artifact_install_concurrency(mod_count: usize) -> usize {
    mod_count.clamp(1, MANAGED_ARTIFACT_INSTALL_CONCURRENCY)
}

pub(super) fn managed_artifact_temp_path(final_path: &Path, managed_mod: &ManagedMod) -> PathBuf {
    managed_artifact_temp_path_for_project(final_path, &managed_mod.project_id)
}

pub(super) fn managed_artifact_temp_path_for_project(
    final_path: &Path,
    project_id: &str,
) -> PathBuf {
    let suffix = safe_temp_suffix(project_id);
    PathBuf::from(format!("{}.{}.tmp", final_path.display(), suffix))
}

fn safe_temp_suffix(value: &str) -> String {
    let suffix: String = value
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_'))
        .take(48)
        .map(char::from)
        .collect();
    if suffix.is_empty() {
        "managed".to_string()
    } else {
        suffix
    }
}
pub(super) fn modrinth_source() -> ManagedArtifactSource {
    ManagedArtifactSource {
        provider: ManagedArtifactProvider::Modrinth,
    }
}

fn verified_sha512_integrity(sha512: String) -> ManagedArtifactIntegrity {
    ManagedArtifactIntegrity {
        sha512,
        sha512_verified: true,
    }
}

fn unverified_sha512_integrity(sha512: String) -> ManagedArtifactIntegrity {
    ManagedArtifactIntegrity {
        sha512,
        sha512_verified: false,
    }
}

pub(super) async fn file_matches_sha512(
    path: &Path,
    expected_sha512: &str,
    expected_size: Option<u64>,
) -> Result<bool, std::io::Error> {
    let (actual_sha512, actual_size) = bounded_regular_file_sha512(path).await?;
    if expected_size.is_some_and(|expected_size| actual_size != expected_size) {
        return Ok(false);
    }
    Ok(actual_sha512.eq_ignore_ascii_case(expected_sha512))
}

async fn reconcile_promoted_temp(
    temp_path: &Path,
    final_path: &Path,
    expected_sha512: &str,
    expected_size: Option<u64>,
) -> Result<(), InstallError> {
    let temp_admission = match crate::file_identity::admit_async(temp_path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(InstallError::Io(error)),
        Ok(admission) => admission,
    };
    let temp_identity = temp_admission.identity();
    let temp_admitted_len = temp_admission.metadata().len();
    drop(temp_admission);
    let (temp_sha512, temp_size) = bounded_regular_file_sha512(temp_path).await?;
    crate::file_identity::revalidate_async(temp_path, temp_identity, temp_admitted_len).await?;
    let expected_matches =
        expected_sha512.trim().is_empty() || temp_sha512.eq_ignore_ascii_case(expected_sha512);
    if !expected_matches || expected_size.is_some_and(|expected| temp_size != expected) {
        return Err(InstallError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed download temp ownership cannot be proven",
        )));
    }
    match tokio::fs::symlink_metadata(final_path).await {
        Ok(metadata) if metadata.file_type().is_file() => {
            let (final_sha512, final_size) = bounded_regular_file_sha512(final_path).await?;
            if temp_sha512 != final_sha512 || temp_size != final_size {
                return Err(InstallError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "managed download temp conflicts with its published target",
                )));
            }
            remove_admitted_temp(temp_path, temp_identity).await
        }
        Ok(_) => Err(InstallError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed download target ownership cannot be proven",
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let current_temp = tokio::fs::symlink_metadata(temp_path).await?;
            let final_still_absent = matches!(
                tokio::fs::symlink_metadata(final_path).await,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            );
            if !current_temp.file_type().is_file()
                || crate::file_identity::revalidate_async(
                    temp_path,
                    temp_identity,
                    temp_admitted_len,
                )
                .await
                .is_err()
                || !final_still_absent
            {
                return Err(InstallError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "managed download temp changed before recovery promotion",
                )));
            }
            tokio::fs::hard_link(temp_path, final_path).await?;
            let final_metadata = tokio::fs::symlink_metadata(final_path).await?;
            let current_temp = tokio::fs::symlink_metadata(temp_path).await?;
            if !final_metadata.file_type().is_file()
                || !current_temp.file_type().is_file()
                || crate::file_identity::revalidate_async(
                    temp_path,
                    temp_identity,
                    temp_admitted_len,
                )
                .await
                .is_err()
                || crate::file_identity::revalidate_async(
                    final_path,
                    temp_identity,
                    temp_admitted_len,
                )
                .await
                .is_err()
                || !file_matches_sha512(final_path, expected_sha512, expected_size).await?
            {
                return Err(InstallError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "managed download temp promotion ownership cannot be proven",
                )));
            }
            remove_admitted_temp(temp_path, temp_identity).await
        }
        Err(error) => Err(InstallError::Io(error)),
    }
}

pub(super) async fn reconcile_managed_artifact_obligations(
    instance_mods_dir: &Path,
    state: Option<&CompositionState>,
) -> Result<(), InstallError> {
    for installed in state
        .into_iter()
        .flat_map(|state| state.installed_mods.iter())
    {
        let final_path = instance_mods_dir.join(&installed.filename);
        reconcile_managed_replace_backups(&final_path, Some(&installed.integrity.sha512)).await?;
        reconcile_promoted_temp(
            &managed_artifact_temp_path_for_project(&final_path, &installed.project_id),
            &final_path,
            &installed.integrity.sha512,
            None,
        )
        .await?;
    }
    super::promotion::settle_empty_managed_replace_root(instance_mods_dir).await?;
    prove_no_managed_download_temps(instance_mods_dir).await
}

async fn remove_admitted_temp(
    temp_path: &Path,
    admitted: crate::file_identity::FileIdentity,
) -> Result<(), InstallError> {
    let current = tokio::fs::symlink_metadata(temp_path).await?;
    if !current.file_type().is_file()
        || crate::file_identity::revalidate_async(temp_path, admitted, current.len())
            .await
            .is_err()
    {
        return Err(InstallError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed download temp identity changed during recovery",
        )));
    }
    tokio::fs::remove_file(temp_path).await?;
    Ok(())
}

async fn prove_no_managed_download_temps(instance_mods_dir: &Path) -> Result<(), InstallError> {
    let mut entries = match tokio::fs::read_dir(instance_mods_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(InstallError::Io(error)),
    };
    let mut count = 0_usize;
    while let Some(entry) = entries.next_entry().await? {
        count = count.saturating_add(1);
        if count > crate::state::RECOVERY_ENTRY_LIMIT {
            return Err(InstallError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "managed artifact recovery entries exceed the limit",
            )));
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if is_managed_download_temp_name(&name) {
            return Err(InstallError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "managed download temp remains without exact published ownership proof",
            )));
        }
    }
    Ok(())
}

fn is_managed_download_temp_name(name: &str) -> bool {
    name.strip_suffix(".tmp")
        .and_then(|stem| stem.rsplit_once('.'))
        .is_some_and(|(final_name, suffix)| {
            !final_name.is_empty()
                && !suffix.is_empty()
                && suffix.len() <= 48
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
}

async fn bounded_regular_file_sha512(path: &Path) -> Result<(String, u64), std::io::Error> {
    let admitted = crate::file_identity::admit_async(path).await?;
    if admitted.metadata().len() > MANAGED_ARTIFACT_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed artifact is not a bounded regular file",
        ));
    }
    let opened_len = admitted.metadata().len();
    let identity = admitted.identity();
    crate::file_identity::revalidate_async(path, identity, opened_len).await?;
    let mut file = tokio::fs::File::from_std(admitted.into_file());
    let mut hasher = Sha512::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut actual_size = 0_u64;
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        actual_size = actual_size.saturating_add(read as u64);
        if actual_size > MANAGED_ARTIFACT_MAX_BYTES || actual_size > opened_len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "managed artifact changed while reading",
            ));
        }
        hasher.update(&buffer[..read]);
    }
    if actual_size != opened_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "managed artifact changed while reading",
        ));
    }
    crate::file_identity::revalidate_async(path, identity, opened_len).await?;
    Ok((hex::encode(hasher.finalize()), actual_size))
}

fn parent_minor_version(game_version: &str) -> Option<String> {
    let mut parts = game_version.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    Some(format!("{major}.{minor}"))
}

fn sanitize_mod_filename(name: &str) -> Result<String, InstallError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(InstallError::InvalidFilename(name.to_string()));
    }
    let base = Path::new(trimmed)
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| InstallError::InvalidFilename(name.to_string()))?;
    if base != trimmed {
        return Err(InstallError::InvalidFilename(name.to_string()));
    }
    Ok(base.to_string())
}

fn tracked_artifact<'a>(
    state: Option<&'a CompositionState>,
    filename: &str,
) -> Option<&'a InstalledMod> {
    state.and_then(|state| {
        state
            .installed_mods
            .iter()
            .find(|installed| installed.filename == filename)
    })
}

fn managed_artifact_failure_evidence() -> String {
    MANAGED_ARTIFACT_INSTALL_FAILURE.to_string()
}

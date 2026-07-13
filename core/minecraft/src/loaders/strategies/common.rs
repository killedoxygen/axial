use crate::download::{
    AuthenticatedSelectedArtifactSource, DownloadProgress, Downloader, ExactLibraryDownloadProof,
    MaterializedLibraryIdentity,
    download_installer_libraries_with_declarations_and_facts_and_descriptors,
    download_profile_libraries_with_declarations_and_facts_and_descriptors,
    reconstruct_installer_library_declarations, reconstruct_profile_library_declarations,
};
use crate::known_good::{
    KnownGoodInstallReceipt, KnownGoodReconstructionReceipt, reconstructed_effective_version,
    seal_reconstructed_installer_source, seal_reconstructed_legacy_archive_source,
    seal_reconstructed_profile_source,
};
use crate::known_good_libraries::{
    PendingExactLibraryDeclarations, PendingStreamedLibraryDeclarations,
    RetainedInstallerLibrarySource, seal_profile_exact_library_declarations,
};
use crate::loaders::api::validate_loader_build_record_identity;
use crate::loaders::bound_processors::{
    AuthenticatedProcessorSources, spawn_bound_processor_execution,
    spawn_reconstruction_processor_execution,
};
use crate::loaders::compose::{
    LoaderProfileFragment, cleanup_incomplete_version, compose_loader_version,
    create_managed_version_dir, finalize_version_install, write_composed_version,
};
use crate::loaders::forge_installer::{
    AuthenticatedForgeInstallerPlan, AuthenticatedInstallerReconstructionInput,
    BoundForgeInstallExecution, PendingForgeInstallExecution, PendingForgeNetworkInstall,
    VerifiedInstallerClientBytes, bind_authenticated_installer_plan, plan_authenticated_installer,
};
#[cfg(not(test))]
use crate::loaders::http::fetch_bytes;
#[cfg(test)]
use crate::loaders::http::fetch_bytes_for_test as fetch_bytes;
use crate::loaders::managed_fs::ManagedDir;
use crate::loaders::providers::{self, ProfileInstallProof};
use crate::loaders::source::{VerifiedLoaderSource, fetch_sha1_verified_source};
use crate::loaders::types::{
    LoaderArtifactKind, LoaderBuildRecord, LoaderComponentId, LoaderError, LoaderInstallPlan,
    LoaderInstallSource, LoaderInstallStrategy,
};
use crate::loaders::{validate_provider_version_id, validate_version_id};
use crate::paths::versions_dir;
use crate::runtime::{ManagedRuntimeCache, acquire_preferred_runtime_source};
use sha1::{Digest as _, Sha1};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use tokio::fs as async_fs;
use zip::ZipArchive;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

const MAX_LOADER_SOURCE_BYTES: u64 = 50 << 20;
const MAX_LEGACY_OVERLAY_ENTRIES: usize = 65_536;
const MAX_LEGACY_OVERLAY_ENTRY_BYTES: u64 = 64 << 20;
const MAX_LEGACY_OVERLAY_PAYLOAD_BYTES: u64 = 256 << 20;
const MAX_LEGACY_OVERLAY_NAME_BYTES: usize = 16 << 20;
const MAX_LEGACY_OVERLAY_OVERHEAD_BYTES: usize = 16 << 20;
const MAX_LEGACY_OVERLAY_OUTPUT_BYTES: usize = 272 << 20;

pub(crate) struct AuthenticatedLegacyOverlayAuthority {
    base: KnownGoodReconstructionReceipt,
    base_client_source: AuthenticatedSelectedArtifactSource,
    archive_source: VerifiedLoaderSource,
    record: LoaderBuildRecord,
    resolved_version: crate::launch::VersionJson,
    version_bytes: Vec<u8>,
    child_client_bytes: Vec<u8>,
}

pub(crate) struct AuthenticatedInstallerReconstructionAuthority {
    base: KnownGoodReconstructionReceipt,
    base_client_source: AuthenticatedSelectedArtifactSource,
    record: LoaderBuildRecord,
    input: AuthenticatedInstallerReconstructionInput,
    resolved_version: crate::launch::VersionJson,
    version_bytes: Vec<u8>,
    child_client: VerifiedInstallerClientBytes,
}

impl AuthenticatedInstallerReconstructionAuthority {
    fn new(
        base: KnownGoodReconstructionReceipt,
        base_client_source: AuthenticatedSelectedArtifactSource,
        record: LoaderBuildRecord,
        input: AuthenticatedInstallerReconstructionInput,
        resolved_version: crate::launch::VersionJson,
        version_bytes: Vec<u8>,
        child_client: VerifiedInstallerClientBytes,
    ) -> Self {
        Self {
            base,
            base_client_source,
            record,
            input,
            resolved_version,
            version_bytes,
            child_client,
        }
    }

    pub(crate) fn consume_for_sealing(
        self,
    ) -> (
        KnownGoodReconstructionReceipt,
        AuthenticatedSelectedArtifactSource,
        LoaderBuildRecord,
        AuthenticatedInstallerReconstructionInput,
        crate::launch::VersionJson,
        Vec<u8>,
        VerifiedInstallerClientBytes,
    ) {
        (
            self.base,
            self.base_client_source,
            self.record,
            self.input,
            self.resolved_version,
            self.version_bytes,
            self.child_client,
        )
    }
}

impl AuthenticatedLegacyOverlayAuthority {
    pub(crate) fn consume_for_sealing(
        self,
    ) -> (
        KnownGoodReconstructionReceipt,
        AuthenticatedSelectedArtifactSource,
        VerifiedLoaderSource,
        LoaderBuildRecord,
        crate::launch::VersionJson,
        Vec<u8>,
        Vec<u8>,
    ) {
        (
            self.base,
            self.base_client_source,
            self.archive_source,
            self.record,
            self.resolved_version,
            self.version_bytes,
            self.child_client_bytes,
        )
    }
}

struct AuthenticatedProfileSource {
    bytes: Vec<u8>,
    provider_url: String,
    logical_identity: String,
}

impl AuthenticatedProfileSource {
    fn into_bytes_for(
        self,
        provider_url: &str,
        logical_identity: &str,
    ) -> Result<Vec<u8>, LoaderError> {
        if self.provider_url != provider_url || self.logical_identity != logical_identity {
            return Err(LoaderError::Verify(
                "authenticated loader profile does not match its selected contract".to_string(),
            ));
        }
        Ok(self.bytes)
    }
}

async fn acquire_profile_source(
    provider_url: &str,
    logical_identity: &str,
) -> Result<AuthenticatedProfileSource, LoaderError> {
    Ok(AuthenticatedProfileSource {
        bytes: fetch_bytes(provider_url, MAX_LOADER_SOURCE_BYTES).await?,
        provider_url: provider_url.to_string(),
        logical_identity: logical_identity.to_string(),
    })
}

pub(super) async fn reconstruct_from_profile_source(
    plan: &LoaderInstallPlan,
) -> Result<KnownGoodReconstructionReceipt, LoaderError> {
    let downloader = Downloader::source_only();
    let proof = providers::fetch_profile_install_proof(&plan.record).await?;
    Box::pin(reconstruct_profile_with_downloader(
        plan,
        &downloader,
        proof,
    ))
    .await
}

async fn reconstruct_profile_with_downloader(
    plan: &LoaderInstallPlan,
    downloader: &Downloader,
    proof: ProfileInstallProof,
) -> Result<KnownGoodReconstructionReceipt, LoaderError> {
    let LoaderInstallSource::ProfileJson { url } = &plan.record.install_source else {
        return Err(LoaderError::InvalidProfile(
            "profile reconstruction requires a fixed profile source".to_string(),
        ));
    };
    let base = downloader
        .reconstruct_version(&plan.record.minecraft_version)
        .await
        .map_err(|error| LoaderError::Verify(format!("reconstruct vanilla base: {error}")))?;
    let profile_source = acquire_profile_source(url, &plan.record.version_id).await?;
    reconstruct_profile_after_sources(plan, base, profile_source, proof).await
}

#[cfg(test)]
async fn reconstruct_profile_with_test_sources(
    plan: &LoaderInstallPlan,
    downloader: &Downloader,
    proof_url: &str,
) -> Result<KnownGoodReconstructionReceipt, LoaderError> {
    let proof =
        providers::fetch_profile_install_proof_from_url_for_test(&plan.record, proof_url).await?;
    Box::pin(reconstruct_profile_with_downloader(plan, downloader, proof)).await
}

async fn reconstruct_profile_after_sources(
    plan: &LoaderInstallPlan,
    base: KnownGoodReconstructionReceipt,
    profile_source: AuthenticatedProfileSource,
    proof: ProfileInstallProof,
) -> Result<KnownGoodReconstructionReceipt, LoaderError> {
    if proof.provider_url().trim().is_empty() {
        return Err(LoaderError::InvalidProfile(
            "loader profile proof has no provider identity".to_string(),
        ));
    }
    let LoaderInstallSource::ProfileJson { url } = &plan.record.install_source else {
        return Err(LoaderError::InvalidProfile(
            "profile reconstruction requires a fixed profile source".to_string(),
        ));
    };
    let profile_bytes = profile_source.into_bytes_for(url, &plan.record.version_id)?;
    let fragment = parse_profile_json(&profile_bytes, &plan.record.component_name)?;
    validate_profile_source_structure(&fragment, &plan.record, &proof)?;
    let declarations = seal_profile_exact_library_declarations(
        fragment,
        proof,
        plan.record.component_id,
        &crate::rules::default_environment(),
    )
    .map_err(|error| {
        LoaderError::Verify(format!("derive profile library declarations: {error:?}"))
    })?;
    let declarations = reconstruct_profile_library_declarations(declarations)
        .await
        .map_err(|error| LoaderError::Verify(error.to_string()))?;
    let (fragment, _) = declarations
        .profile_contract()
        .ok_or_else(|| LoaderError::Verify("profile library contract is missing".to_string()))?;
    let version = compose_loader_version(
        reconstructed_effective_version(&base),
        &plan.record.minecraft_version,
        &plan.record.version_id,
        fragment,
    )?;
    let version_bytes = serde_json::to_vec_pretty(&version)?;
    seal_reconstructed_profile_source(base, &plan.record, version, &version_bytes, declarations)
        .map_err(|error| LoaderError::Verify(format!("derive loader authority: {error:?}")))
}

pub(super) async fn reconstruct_from_legacy_archive(
    plan: &LoaderInstallPlan,
) -> Result<KnownGoodReconstructionReceipt, LoaderError> {
    let downloader = Downloader::source_only();
    reconstruct_legacy_with_downloader(plan, &downloader).await
}

async fn reconstruct_legacy_with_downloader(
    plan: &LoaderInstallPlan,
    downloader: &Downloader,
) -> Result<KnownGoodReconstructionReceipt, LoaderError> {
    Box::pin(reconstruct_legacy_with_downloader_inner(plan, downloader)).await
}

async fn reconstruct_legacy_with_downloader_inner(
    plan: &LoaderInstallPlan,
    downloader: &Downloader,
) -> Result<KnownGoodReconstructionReceipt, LoaderError> {
    let LoaderInstallSource::LegacyArchive { url } = &plan.record.install_source else {
        return Err(LoaderError::InvalidProfile(
            "earliest Forge reconstruction requires a fixed archive source".to_string(),
        ));
    };
    let base = downloader
        .reconstruct_version_with_client_source(&plan.record.minecraft_version)
        .await
        .map_err(|error| LoaderError::Verify(format!("reconstruct vanilla base: {error}")))?;
    let (base, base_client_source) = base.consume_for_overlay();
    let archive_source = fetch_sha1_verified_source(
        url,
        MAX_LOADER_SOURCE_BYTES,
        "legacy Forge archive",
        &plan.record.version_id,
    )
    .await?;
    let (resolved_version, version_bytes, child_client_bytes) = derive_legacy_archive_inputs(
        reconstructed_effective_version(&base),
        &plan.record,
        base_client_source.bytes().to_vec(),
        archive_source.bytes().to_vec(),
    )
    .await?;
    seal_reconstructed_legacy_archive_source(AuthenticatedLegacyOverlayAuthority {
        base,
        base_client_source,
        archive_source,
        record: plan.record.clone(),
        resolved_version,
        version_bytes,
        child_client_bytes,
    })
    .map_err(|error| LoaderError::Verify(format!("derive loader authority: {error:?}")))
}

async fn derive_legacy_archive_inputs(
    base_version: &crate::launch::VersionJson,
    record: &LoaderBuildRecord,
    base_client_bytes: Vec<u8>,
    archive_bytes: Vec<u8>,
) -> Result<(crate::launch::VersionJson, Vec<u8>, Vec<u8>), LoaderError> {
    let child_client_bytes =
        overlay_legacy_archive_bytes_blocking(base_client_bytes, archive_bytes).await?;
    let mut version = base_version.clone();
    version.id = record.version_id.clone();
    version.inherits_from = record.minecraft_version.clone();
    version.materialized = true;
    let client = version.downloads.client.as_mut().ok_or_else(|| {
        LoaderError::Verify("authenticated base version has no client download".to_string())
    })?;
    client.sha1 = format!("{:x}", Sha1::digest(&child_client_bytes));
    client.size = i64::try_from(child_client_bytes.len())
        .map_err(|_| LoaderError::Verify("legacy client is too large".to_string()))?;
    client.url.clear();
    let version_bytes = serde_json::to_vec_pretty(&version)?;
    Ok((version, version_bytes, child_client_bytes))
}

pub(super) async fn reconstruct_from_installer_source(
    plan: &LoaderInstallPlan,
) -> Result<KnownGoodReconstructionReceipt, LoaderError> {
    let downloader = Downloader::source_only();
    reconstruct_installer_with_downloader(plan, &downloader).await
}

async fn reconstruct_installer_with_downloader(
    plan: &LoaderInstallPlan,
    downloader: &Downloader,
) -> Result<KnownGoodReconstructionReceipt, LoaderError> {
    let installer_url = validate_installer_record_authority(&plan.record)?;
    let installer_source = fetch_sha1_verified_source(
        installer_url,
        MAX_LOADER_SOURCE_BYTES,
        "loader installer",
        &plan.record.version_id,
    )
    .await?;
    let authenticated =
        extract_installer_blocking(installer_source, plan.record.component_name.clone()).await?;
    let installer_plan = bind_authenticated_installer_plan(authenticated, &plan.record)
        .map_err(|error| installer_extract_error(&plan.record.component_name, error))?;
    let execution = installer_plan
        .into_install_execution()
        .map_err(|error| installer_extract_error(&plan.record.component_name, error))?;
    let (execution, processor_required) = match execution {
        BoundForgeInstallExecution::Run(execution) => {
            match execution.into_declared_reconstruction() {
                Ok(continuation) => (
                    BoundForgeInstallExecution::Continue(Box::new(continuation)),
                    false,
                ),
                Err(execution) => (BoundForgeInstallExecution::Run(execution), true),
            }
        }
        BoundForgeInstallExecution::Continue(continuation) => {
            (BoundForgeInstallExecution::Continue(continuation), false)
        }
        BoundForgeInstallExecution::UnsupportedMissingOutputs => {
            return Err(LoaderError::InvalidProfile(
                "loader installer processors do not expose authenticated client outputs"
                    .to_string(),
            ));
        }
    };
    let sources = execution
        .into_reconstruction_sources()
        .map_err(|error| installer_extract_error(&plan.record.component_name, error))?;
    let (base, base_client_source, input) = if processor_required {
        let base = downloader
            .reconstruct_version_for_processor(&plan.record.minecraft_version)
            .await
            .map_err(|error| LoaderError::Verify(format!("reconstruct vanilla base: {error}")))?;
        let (pending_base, base_client_source, runtime_source) = base.into_parts();
        let processor_sources = AuthenticatedProcessorSources::from_reconstructed(
            pending_base.version().clone(),
            base_client_source,
            runtime_source,
        )
        .map_err(|error| LoaderError::ProcessorFailed(error.to_string()))?;
        let result = spawn_reconstruction_processor_execution(
            sources,
            plan.record.version_id.clone(),
            plan.record.minecraft_version.clone(),
            processor_sources,
        )
        .finish(|_| {})
        .await
        .map_err(|error| LoaderError::ProcessorFailed(error.to_string()))?;
        let (base_client_source, runtime_source) = result
            .sources
            .into_reconstructed_parts()
            .map_err(|error| LoaderError::ProcessorFailed(error.to_string()))?;
        let base = pending_base
            .complete(runtime_source)
            .map_err(|error| LoaderError::Verify(format!("reconstruct vanilla base: {error}")))?;
        let input = result
            .continuation
            .into_observed_reconstruction_receipt_input(result.outputs)
            .map_err(|error| installer_extract_error(&plan.record.component_name, error))?;
        (base, base_client_source, input)
    } else {
        let execution = reconstruct_installer_library_declarations(sources)
            .await
            .map_err(|error| LoaderError::Verify(error.to_string()))?;
        let BoundForgeInstallExecution::Continue(continuation) = execution else {
            return Err(LoaderError::InvalidProfile(
                "declarative reconstruction did not settle its processors".to_string(),
            ));
        };
        let input = continuation
            .into_reconstruction_receipt_input()
            .map_err(|error| installer_extract_error(&plan.record.component_name, error))?;
        let base = downloader
            .reconstruct_version_with_client_source(&plan.record.minecraft_version)
            .await
            .map_err(|error| LoaderError::Verify(format!("reconstruct vanilla base: {error}")))?;
        let (base, base_client_source) = base.consume_for_overlay();
        (base, base_client_source, input)
    };
    let mut version = compose_loader_version(
        reconstructed_effective_version(&base),
        &plan.record.minecraft_version,
        &plan.record.version_id,
        input.version(),
    )?;
    let child_client = input
        .derive_child_client_bytes(base_client_source.bytes())
        .map_err(|error| installer_extract_error(&plan.record.component_name, error))?;
    let client = version.downloads.client.as_mut().ok_or_else(|| {
        LoaderError::Verify("authenticated base version has no client download".to_string())
    })?;
    client.sha1 = format!("{:x}", Sha1::digest(child_client.bytes()));
    client.size = i64::try_from(child_client.bytes().len())
        .map_err(|_| LoaderError::Verify("loader client is too large".to_string()))?;
    client.url.clear();
    let version_bytes = serde_json::to_vec_pretty(&version)?;
    seal_reconstructed_installer_source(AuthenticatedInstallerReconstructionAuthority::new(
        base,
        base_client_source,
        plan.record.clone(),
        input,
        version,
        version_bytes,
        child_client,
    ))
    .map_err(|error| LoaderError::Verify(format!("derive loader authority: {error:?}")))
}

// Profile-source loaders ship a ready version JSON and then download its libraries.
pub async fn install_from_profile_source<F>(
    library_dir: &Path,
    runtime_cache: &ManagedRuntimeCache,
    plan: &LoaderInstallPlan,
    send: &mut F,
) -> Result<KnownGoodInstallReceipt, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    let base_receipt = Box::pin(ensure_base_version(
        library_dir,
        runtime_cache,
        &plan.record.minecraft_version,
        send,
    ))
    .await?;
    let source_proof = providers::fetch_profile_install_proof(&plan.record).await?;
    Box::pin(install_profile_source_after_authenticated_base(
        library_dir,
        plan,
        &base_receipt,
        source_proof,
        send,
    ))
    .await
}

async fn install_profile_source_after_authenticated_base<F>(
    library_dir: &Path,
    plan: &LoaderInstallPlan,
    base_receipt: &KnownGoodInstallReceipt,
    source_proof: ProfileInstallProof,
    send: &mut F,
) -> Result<KnownGoodInstallReceipt, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    let LoaderInstallSource::ProfileJson { url: profile_url } = &plan.record.install_source else {
        return Err(LoaderError::InvalidProfile(
            "profile loader build requires a profile json source".to_string(),
        ));
    };
    if profile_url.is_empty() {
        return Err(LoaderError::InvalidProfile(
            "profile loader source URL is empty".to_string(),
        ));
    }
    send(progress(
        "profile",
        0,
        1,
        Some("Fetching loader profile...".to_string()),
    ));
    let profile_bytes = acquire_profile_source(profile_url, &plan.record.version_id)
        .await?
        .into_bytes_for(profile_url, &plan.record.version_id)?;
    let fragment = parse_profile_json(&profile_bytes, &plan.record.component_name)?;
    validate_profile_source_structure(&fragment, &plan.record, &source_proof)?;
    let library_declarations = seal_profile_exact_library_declarations(
        fragment,
        source_proof,
        plan.record.component_id,
        &crate::rules::default_environment(),
    )
    .map_err(|error| {
        LoaderError::Verify(format!("derive profile library declarations: {error:?}"))
    })?;
    let installed_version_id = plan.record.version_id.clone();
    validate_version_id(&installed_version_id, "installed loader version id")?;

    let library_download_result = Box::pin(download_profile_loader_libraries_with_evidence(
        library_dir,
        library_declarations,
        "loader_libraries",
        &mut *send,
    ))
    .await;
    let (library_declarations, library_proofs) =
        cleanup_on_error(library_download_result, library_dir, &installed_version_id)?;
    let library_declarations = cleanup_on_error(
        library_declarations
            .seal_streamed(library_proofs)
            .map_err(|error| {
                LoaderError::Verify(format!("complete profile library declarations: {error:?}"))
            }),
        library_dir,
        &installed_version_id,
    )?;
    let (fragment, _) = library_declarations
        .profile_contract()
        .ok_or_else(|| LoaderError::Verify("profile library contract is missing".to_string()))?;
    let version = cleanup_on_error(
        compose_loader_version(
            base_receipt.effective_version(),
            &plan.record.minecraft_version,
            &installed_version_id,
            fragment,
        ),
        library_dir,
        &installed_version_id,
    )?;
    let version_bytes = serde_json::to_vec_pretty(&version)?;
    let receipt = cleanup_on_error(
        KnownGoodInstallReceipt::from_verified_profile_source(
            base_receipt,
            &plan.record,
            version.clone(),
            &version_bytes,
            library_declarations,
        )
        .map_err(|error| LoaderError::Verify(format!("derive loader authority: {error:?}"))),
        library_dir,
        &installed_version_id,
    )?;
    let authenticated_client = base_receipt
        .authenticated_client_integrity()
        .map_err(|error| LoaderError::Verify(format!("authenticate base client: {error:?}")))?;
    cleanup_on_error(
        write_composed_version(
            library_dir,
            &installed_version_id,
            &version,
            &version_bytes,
            &plan.record.minecraft_version,
            &authenticated_client,
        )
        .await,
        library_dir,
        &installed_version_id,
    )?;
    cleanup_on_error(
        finalize_version_install(library_dir, &installed_version_id),
        library_dir,
        &installed_version_id,
    )?;
    send(done());
    Ok(receipt)
}

// Installer-source loaders require extracting metadata and Maven entries from the installer jar.
pub async fn install_from_installer_source<F>(
    library_dir: &Path,
    runtime_cache: &ManagedRuntimeCache,
    plan: &LoaderInstallPlan,
    send: &mut F,
) -> Result<KnownGoodInstallReceipt, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    let installer_url = validate_installer_record_authority(&plan.record)?;
    send(progress(
        "artifacts",
        0,
        1,
        Some(format!(
            "Downloading {} installer...",
            plan.record.component_name
        )),
    ));
    let installer_source = fetch_sha1_verified_source(
        installer_url,
        MAX_LOADER_SOURCE_BYTES,
        "loader installer",
        &plan.record.version_id,
    )
    .await?;
    let authenticated =
        extract_installer_blocking(installer_source, plan.record.component_name.clone()).await?;
    let installer_plan = bind_authenticated_installer_plan(authenticated, &plan.record)
        .map_err(|error| installer_extract_error(&plan.record.component_name, error))?;
    let execution = installer_plan
        .into_install_execution()
        .map_err(|error| installer_extract_error(&plan.record.component_name, error))?;
    if matches!(
        &execution,
        BoundForgeInstallExecution::UnsupportedMissingOutputs
    ) {
        return Err(LoaderError::InvalidProfile(
            "loader installer processors do not expose authenticated client outputs".to_string(),
        ));
    }
    let network_install = execution
        .into_network_install(&crate::paths::libraries_dir(library_dir))
        .map_err(|error| installer_extract_error(&plan.record.component_name, error))?;
    let base_receipt = Box::pin(ensure_base_version(
        library_dir,
        runtime_cache,
        &plan.record.minecraft_version,
        send,
    ))
    .await?;
    let (pending_execution, materialized_network) =
        Box::pin(download_installer_libraries_with_evidence(
            library_dir,
            network_install,
            "loader_libraries",
            &mut *send,
        ))
        .await?;
    let execution = pending_execution
        .complete_network(materialized_network)
        .map_err(|error| installer_extract_error(&plan.record.component_name, error))?;
    Box::pin(finish_supported_installer_install(
        library_dir,
        plan,
        execution,
        base_receipt,
        send,
    ))
    .await
}

async fn finish_supported_installer_install<F>(
    library_dir: &Path,
    plan: &LoaderInstallPlan,
    execution: BoundForgeInstallExecution,
    base_receipt: KnownGoodInstallReceipt,
    send: &mut F,
) -> Result<KnownGoodInstallReceipt, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    let installed_version_id = plan.record.version_id.clone();
    validate_version_id(&installed_version_id, "installed loader version id")?;
    let (base_client_bytes, receipt_input) = match execution {
        BoundForgeInstallExecution::Run(execution) => {
            let base_client_bytes = read_installed_base_client(library_dir, &base_receipt)?;
            let runtime_source =
                acquire_preferred_runtime_source(&base_receipt.effective_version().java_version)
                    .await
                    .map_err(|error| LoaderError::ProcessorFailed(error.to_string()))?;
            let installed_root = ManagedDir::open_root(library_dir)?;
            let processor_sources = AuthenticatedProcessorSources::from_installed(
                base_receipt.effective_version().clone(),
                base_client_bytes,
                runtime_source,
                installed_root,
            )
            .map_err(|error| LoaderError::ProcessorFailed(error.to_string()))?;
            send(progress(
                "processors",
                0,
                1,
                Some("Running processors...".to_string()),
            ));
            let result = spawn_bound_processor_execution(
                *execution,
                installed_version_id.clone(),
                plan.record.minecraft_version.clone(),
                processor_sources,
            )
            .finish(|update| {
                send(DownloadProgress {
                    phase: "processors".to_string(),
                    current: update.current as i32,
                    total: update.total as i32,
                    file: Some("Running processors...".to_string()),
                    error: None,
                    done: false,
                    bytes_done: None,
                    bytes_total: None,
                });
            })
            .await
            .map_err(|error| LoaderError::ProcessorFailed(error.to_string()))?;
            let (base_client_bytes, _runtime_source) = result
                .sources
                .into_installed_parts()
                .map_err(|error| LoaderError::ProcessorFailed(error.to_string()))?;
            let receipt_input = result
                .continuation
                .into_observed_receipt_input(result.outputs)
                .map_err(|error| installer_extract_error(&plan.record.component_name, error))?;
            (base_client_bytes, receipt_input)
        }
        BoundForgeInstallExecution::Continue(continuation) => {
            let receipt_input = continuation
                .into_receipt_input()
                .map_err(|error| installer_extract_error(&plan.record.component_name, error))?;
            (
                read_installed_base_client(library_dir, &base_receipt)?,
                receipt_input,
            )
        }
        BoundForgeInstallExecution::UnsupportedMissingOutputs => unreachable!(),
    };
    let mut version = compose_loader_version(
        base_receipt.effective_version(),
        &plan.record.minecraft_version,
        &installed_version_id,
        receipt_input.version(),
    )?;
    let child_client = receipt_input
        .derive_child_client_bytes(&base_client_bytes)
        .map_err(|error| installer_extract_error(&plan.record.component_name, error))?;
    let client = version.downloads.client.as_mut().ok_or_else(|| {
        LoaderError::Verify("authenticated base version has no client download".to_string())
    })?;
    client.sha1 = format!("{:x}", Sha1::digest(child_client.bytes()));
    client.size = i64::try_from(child_client.bytes().len())
        .map_err(|_| LoaderError::Verify("loader client is too large".to_string()))?;
    client.url.clear();
    let version_bytes = serde_json::to_vec_pretty(&version)?;
    let pending_receipt = KnownGoodInstallReceipt::from_verified_installer_source(
        base_receipt,
        &plan.record,
        receipt_input,
        version,
        &version_bytes,
        &base_client_bytes,
        &child_client,
    )
    .map_err(|error| LoaderError::Verify(format!("derive loader authority: {error:?}")))?;

    if child_client.bytes() != base_client_bytes {
        send(progress(
            "client_jar",
            0,
            1,
            Some(format!("{installed_version_id}.jar")),
        ));
    }
    let child_client_bytes = child_client.into_bytes();
    let (pending_receipt, retained_sources) = pending_receipt.into_publications();
    let version_dir = create_managed_version_dir(library_dir, &installed_version_id)?;
    cleanup_on_error(
        write_loader_install_effects(
            &version_dir,
            &installed_version_id,
            &version_bytes,
            &child_client_bytes,
        )
        .await,
        library_dir,
        &installed_version_id,
    )?;
    let materialized = cleanup_on_error(
        materialize_retained_installer_libraries(library_dir, retained_sources).await,
        library_dir,
        &installed_version_id,
    )?;
    let receipt = pending_receipt.complete(materialized).map_err(|error| {
        LoaderError::Verify(format!("complete installer materializations: {error:?}"))
    })?;
    if child_client_bytes != base_client_bytes {
        send(progress(
            "client_jar",
            1,
            1,
            Some(format!("{installed_version_id}.jar")),
        ));
    }

    cleanup_on_error(
        finalize_version_install(library_dir, &installed_version_id),
        library_dir,
        &installed_version_id,
    )?;
    send(done());
    Ok(receipt)
}

fn read_installed_base_client(
    library_dir: &Path,
    receipt: &KnownGoodInstallReceipt,
) -> Result<Vec<u8>, LoaderError> {
    let integrity = receipt
        .authenticated_client_integrity()
        .map_err(|error| LoaderError::Verify(format!("authenticate base client: {error:?}")))?;
    let bytes = ManagedDir::open_root(library_dir)?
        .open_child("versions")?
        .open_child(receipt.version_id())?
        .read_authenticated(
            &format!("{}.jar", receipt.version_id()),
            integrity.size,
            integrity.sha1.as_deref(),
        )?;
    receipt
        .authenticate_client_bytes(&bytes)
        .map_err(|error| LoaderError::Verify(format!("authenticate base client: {error:?}")))?;
    Ok(bytes)
}

fn validate_installer_record_authority(record: &LoaderBuildRecord) -> Result<&str, LoaderError> {
    validate_loader_build_record_identity(record)?;
    let expected_strategy = match record.component_id {
        LoaderComponentId::Forge => matches!(
            record.strategy,
            LoaderInstallStrategy::ForgeModern | LoaderInstallStrategy::ForgeLegacyInstaller
        ),
        LoaderComponentId::NeoForge => record.strategy == LoaderInstallStrategy::NeoForgeModern,
        LoaderComponentId::Fabric | LoaderComponentId::Quilt => false,
    };
    let exact_source = matches!(
        &record.install_source,
        LoaderInstallSource::InstallerJar { url } if !url.is_empty()
    );
    if !expected_strategy
        || record.component_name != record.component_id.display_name()
        || record.artifact_kind != LoaderArtifactKind::InstallerJar
        || !exact_source
    {
        return Err(LoaderError::InvalidProfile(
            "loader installer authority does not match the live build record".to_string(),
        ));
    }
    let LoaderInstallSource::InstallerJar { url } = &record.install_source else {
        unreachable!("validated installer source")
    };
    Ok(url)
}

fn legacy_archive_source_url(record: &LoaderBuildRecord) -> Result<&str, LoaderError> {
    validate_loader_build_record_identity(record)?;
    let exact_authority = record.component_id == LoaderComponentId::Forge
        && record.component_name == record.component_id.display_name()
        && record.strategy == LoaderInstallStrategy::ForgeEarliestLegacy
        && record.artifact_kind == LoaderArtifactKind::LegacyArchive;
    let LoaderInstallSource::LegacyArchive { url } = &record.install_source else {
        return Err(LoaderError::InvalidProfile(
            "earliest Forge build requires a legacy archive source".to_string(),
        ));
    };
    if !exact_authority || url.is_empty() {
        return Err(LoaderError::InvalidProfile(
            "legacy archive authority does not match the live build record".to_string(),
        ));
    }
    Ok(url)
}

// Legacy archive loaders carry Maven entries in provider-specific zip layouts.
pub async fn install_from_legacy_archive<F>(
    library_dir: &Path,
    runtime_cache: &ManagedRuntimeCache,
    plan: &LoaderInstallPlan,
    send: &mut F,
) -> Result<KnownGoodInstallReceipt, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    let archive_url = legacy_archive_source_url(&plan.record)?;
    send(progress(
        "artifacts",
        0,
        1,
        Some(format!(
            "Downloading {} archive...",
            plan.record.component_name
        )),
    ));
    let archive_source = fetch_sha1_verified_source(
        archive_url,
        MAX_LOADER_SOURCE_BYTES,
        "legacy Forge archive",
        &plan.record.version_id,
    )
    .await?;
    let base_receipt = Box::pin(ensure_base_version(
        library_dir,
        runtime_cache,
        &plan.record.minecraft_version,
        send,
    ))
    .await?;
    Box::pin(install_legacy_archive_after_authenticated_base(
        library_dir,
        plan,
        archive_source,
        &base_receipt,
        send,
    ))
    .await
}

async fn install_legacy_archive_after_authenticated_base<F>(
    library_dir: &Path,
    plan: &LoaderInstallPlan,
    archive_source: VerifiedLoaderSource,
    base_receipt: &KnownGoodInstallReceipt,
    send: &mut F,
) -> Result<KnownGoodInstallReceipt, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    validate_version_id(&plan.record.version_id, "installed loader version id")?;
    let archive_url = legacy_archive_source_url(&plan.record)?;

    let base_client_path = versions_dir(library_dir)
        .join(&plan.record.minecraft_version)
        .join(format!("{}.jar", plan.record.minecraft_version));
    let base_client_bytes = async_fs::read(&base_client_path).await?;
    base_receipt
        .authenticate_client_bytes(&base_client_bytes)
        .map_err(|error| LoaderError::Verify(format!("authenticate base client: {error:?}")))?;
    let archive_bytes = archive_source.into_bytes_for(archive_url, &plan.record.version_id)?;
    let (version, version_bytes, child_client_bytes) = derive_legacy_archive_inputs(
        base_receipt.effective_version(),
        &plan.record,
        base_client_bytes,
        archive_bytes,
    )
    .await?;
    let receipt = KnownGoodInstallReceipt::from_verified_legacy_archive_source(
        base_receipt,
        &plan.record,
        version,
        &version_bytes,
        &child_client_bytes,
    )
    .map_err(|error| LoaderError::Verify(format!("derive loader authority: {error:?}")))?;

    let version_dir = create_managed_version_dir(library_dir, &plan.record.version_id)?;
    cleanup_on_error(
        write_loader_install_effects(
            &version_dir,
            &plan.record.version_id,
            &version_bytes,
            &child_client_bytes,
        )
        .await,
        library_dir,
        &plan.record.version_id,
    )?;

    cleanup_on_error(
        finalize_version_install(library_dir, &plan.record.version_id),
        library_dir,
        &plan.record.version_id,
    )?;
    send(done());
    Ok(receipt)
}

async fn ensure_base_version<F>(
    library_dir: &Path,
    runtime_cache: &ManagedRuntimeCache,
    version_id: &str,
    send: &mut F,
) -> Result<KnownGoodInstallReceipt, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    let install_lock = base_version_install_lock(library_dir, version_id);
    let _guard = install_lock.lock().await;

    let downloader = Downloader::new(library_dir.to_path_buf(), runtime_cache.clone());
    let mut facts = Vec::new();
    let mut descriptors = Vec::new();
    let result = Box::pin(downloader.install_version_with_facts_and_descriptors(
        version_id,
        |progress| {
            if !progress.done {
                send(progress);
            }
        },
        |fact| facts.push(fact),
        |descriptor| descriptors.push(descriptor),
    ))
    .await;
    match result {
        Ok(receipt) => Ok(receipt),
        Err(error) => Err(LoaderError::BaseInstallFailed {
            error: Box::new(error),
            facts,
            descriptors,
        }),
    }
}

fn base_version_install_lock(library_dir: &Path, version_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mutex = LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    base_version_install_lock_from_map(mutex, library_dir, version_id)
}

fn base_version_install_lock_from_map(
    mutex: &std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    library_dir: &Path,
    version_id: &str,
) -> Arc<tokio::sync::Mutex<()>> {
    let key = format!("{}\n{}", library_dir.to_string_lossy(), version_id.trim());
    let mut guard = match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard
        .entry(key)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

async fn download_profile_loader_libraries_with_evidence<F>(
    library_dir: &Path,
    declarations: PendingExactLibraryDeclarations,
    phase: &str,
    send: &mut F,
) -> Result<
    (
        PendingStreamedLibraryDeclarations,
        Vec<ExactLibraryDownloadProof>,
    ),
    LoaderError,
>
where
    F: FnMut(DownloadProgress),
{
    let mut facts = Vec::new();
    let mut descriptors = Vec::new();
    download_profile_libraries_with_declarations_and_facts_and_descriptors(
        library_dir,
        declarations,
        phase,
        &mut *send,
        |fact| facts.push(fact),
        |descriptor| descriptors.push(descriptor),
    )
    .await
    .map_err(|_| LoaderError::ArtifactDownloadFailed { facts, descriptors })
}

async fn download_installer_libraries_with_evidence<F>(
    library_dir: &Path,
    install: PendingForgeNetworkInstall,
    phase: &str,
    send: &mut F,
) -> Result<
    (
        PendingForgeInstallExecution,
        Vec<MaterializedLibraryIdentity>,
    ),
    LoaderError,
>
where
    F: FnMut(DownloadProgress),
{
    let mut facts = Vec::new();
    let mut descriptors = Vec::new();
    download_installer_libraries_with_declarations_and_facts_and_descriptors(
        library_dir,
        install,
        phase,
        &mut *send,
        |fact| facts.push(fact),
        |descriptor| descriptors.push(descriptor),
    )
    .await
    .map_err(|_| LoaderError::ArtifactDownloadFailed { facts, descriptors })
}

fn cleanup_on_error<T, E>(
    result: Result<T, E>,
    library_dir: &Path,
    version_id: &str,
) -> Result<T, E> {
    result.inspect_err(|_| cleanup_incomplete_version(library_dir, version_id))
}

fn parse_profile_json(
    bytes: &[u8],
    component_name: &str,
) -> Result<LoaderProfileFragment, LoaderError> {
    serde_json::from_slice::<LoaderProfileFragment>(bytes)
        .map_err(|error| LoaderError::InvalidProfile(format!("{component_name} profile: {error}")))
}

fn validate_profile_source_structure(
    fragment: &LoaderProfileFragment,
    record: &LoaderBuildRecord,
    proof: &ProfileInstallProof,
) -> Result<(), LoaderError> {
    validate_provider_version_id(&fragment.id, "upstream loader profile version id")?;
    let (canonical_profile_id, inherits_from, client_main_class) = proof.identity();
    if canonical_profile_id != fragment.id
        || inherits_from != fragment.inherits_from
        || fragment.inherits_from != record.minecraft_version
        || client_main_class != fragment.main_class
        || fragment.main_class.trim().is_empty()
    {
        return Err(LoaderError::InvalidProfile(
            "loader profile identity does not match its live provider proof".to_string(),
        ));
    }
    if (!fragment.kind.is_empty() && fragment.kind != "release")
        || fragment.asset_index.is_some()
        || !fragment.assets.is_empty()
        || fragment.downloads.is_some()
        || fragment.java_version.is_some()
        || fragment.logging.is_some()
    {
        return Err(LoaderError::InvalidProfile(
            "loader profile overrides authenticated base-owned metadata".to_string(),
        ));
    }

    Ok(())
}

async fn extract_installer_blocking(
    installer_source: VerifiedLoaderSource,
    component_name: String,
) -> Result<AuthenticatedForgeInstallerPlan, LoaderError> {
    tokio::task::spawn_blocking(move || {
        plan_authenticated_installer(installer_source)
            .map_err(|error| installer_extract_error(&component_name, error))
    })
    .await
    .map_err(|error| LoaderError::InstallExecutionFailed(error.to_string()))?
}

async fn materialize_retained_installer_libraries(
    library_dir: &Path,
    sources: Vec<RetainedInstallerLibrarySource>,
) -> Result<Vec<MaterializedLibraryIdentity>, LoaderError> {
    let root = ManagedDir::open_root(library_dir)?.open_or_create_child("libraries")?;
    let mut materialized = Vec::with_capacity(sources.len());
    for source in sources {
        let publication = root.materialize_installer_library(source).await?;
        materialized.push(MaterializedLibraryIdentity::from_installer_publication(
            publication,
        ));
    }
    root.revalidate()?;
    Ok(materialized)
}

async fn overlay_legacy_archive_bytes_blocking(
    base_client_bytes: Vec<u8>,
    archive_data: Vec<u8>,
) -> Result<Vec<u8>, LoaderError> {
    tokio::task::spawn_blocking(move || {
        overlay_legacy_archive_bytes(&base_client_bytes, &archive_data)
    })
    .await
    .map_err(|error| LoaderError::InstallExecutionFailed(error.to_string()))?
}

async fn write_loader_install_effects(
    version_dir: &ManagedDir,
    version_id: &str,
    version_bytes: &[u8],
    child_client_bytes: &[u8],
) -> Result<(), LoaderError> {
    validate_version_id(version_id, "installed loader version id")?;
    version_dir
        .write_exact(".incomplete", b"installing")
        .await?;
    version_dir
        .write_exact(&format!("{version_id}.json"), version_bytes)
        .await?;
    version_dir
        .write_exact(&format!("{version_id}.jar"), child_client_bytes)
        .await?;
    version_dir.revalidate()
}

fn overlay_legacy_archive_bytes(
    base_client_bytes: &[u8],
    archive_data: &[u8],
) -> Result<Vec<u8>, LoaderError> {
    let mut base_archive = ZipArchive::new(std::io::Cursor::new(base_client_bytes))
        .map_err(|error| legacy_archive_error("base Minecraft", error))?;
    let mut forge_archive = ZipArchive::new(std::io::Cursor::new(archive_data))
        .map_err(|error| legacy_archive_error("Forge", error))?;
    let forge_names = legacy_overlay_entry_names(&mut forge_archive)?;
    let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let mut budget = LegacyOverlayBudget::default();

    copy_legacy_overlay_entries(
        &mut base_archive,
        &mut writer,
        Some(&forge_names),
        &mut budget,
    )?;
    let mut forge_archive = ZipArchive::new(std::io::Cursor::new(archive_data))
        .map_err(|error| legacy_archive_error("Forge", error))?;
    copy_legacy_overlay_entries(&mut forge_archive, &mut writer, None, &mut budget)?;
    let output = writer
        .finish()
        .map(|cursor| cursor.into_inner())
        .map_err(|error| legacy_archive_error("legacy Forge overlay", error))?;
    if output.len() > MAX_LEGACY_OVERLAY_OUTPUT_BYTES {
        return Err(legacy_overlay_limit_error());
    }
    Ok(output)
}

fn legacy_overlay_entry_names<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<HashSet<String>, LoaderError> {
    let mut names = HashSet::new();
    let mut name_bytes = 0usize;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| legacy_archive_error("Forge", error))?;
        if index >= MAX_LEGACY_OVERLAY_ENTRIES {
            return Err(legacy_overlay_limit_error());
        }
        name_bytes = name_bytes
            .checked_add(entry.name().len())
            .ok_or_else(legacy_overlay_limit_error)?;
        if name_bytes > MAX_LEGACY_OVERLAY_NAME_BYTES {
            return Err(legacy_overlay_limit_error());
        }
        if legacy_archive_entry_is_skipped(entry.name()) {
            continue;
        }
        names.insert(entry.name().to_string());
    }
    Ok(names)
}

#[derive(Default)]
struct LegacyOverlayBudget {
    entries: usize,
    payload_bytes: u64,
    name_bytes: usize,
    output_overhead_bytes: usize,
}

impl LegacyOverlayBudget {
    fn reserve(&mut self, name: &str, size: u64) -> Result<(), LoaderError> {
        if size > MAX_LEGACY_OVERLAY_ENTRY_BYTES {
            return Err(legacy_overlay_limit_error());
        }
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(legacy_overlay_limit_error)?;
        self.payload_bytes = self
            .payload_bytes
            .checked_add(size)
            .ok_or_else(legacy_overlay_limit_error)?;
        self.name_bytes = self
            .name_bytes
            .checked_add(name.len())
            .ok_or_else(legacy_overlay_limit_error)?;
        self.output_overhead_bytes = self
            .output_overhead_bytes
            .checked_add(
                name.len()
                    .checked_mul(2)
                    .and_then(|bytes| bytes.checked_add(256))
                    .ok_or_else(legacy_overlay_limit_error)?,
            )
            .ok_or_else(legacy_overlay_limit_error)?;
        if self.entries > MAX_LEGACY_OVERLAY_ENTRIES
            || self.payload_bytes > MAX_LEGACY_OVERLAY_PAYLOAD_BYTES
            || self.name_bytes > MAX_LEGACY_OVERLAY_NAME_BYTES
            || self.output_overhead_bytes > MAX_LEGACY_OVERLAY_OVERHEAD_BYTES
        {
            return Err(legacy_overlay_limit_error());
        }
        Ok(())
    }
}

fn copy_legacy_overlay_entries<
    R: std::io::Read + std::io::Seek,
    W: std::io::Write + std::io::Seek,
>(
    archive: &mut ZipArchive<R>,
    writer: &mut ZipWriter<W>,
    replaced_names: Option<&HashSet<String>>,
    budget: &mut LegacyOverlayBudget,
) -> Result<(), LoaderError> {
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| legacy_archive_error("legacy Forge", error))?;
        let name = entry.name().to_string();
        if legacy_archive_entry_is_skipped(&name)
            || replaced_names.is_some_and(|names| names.contains(&name))
        {
            continue;
        }
        budget.reserve(&name, entry.size())?;
        if entry.is_dir() || name.ends_with('/') {
            writer
                .add_directory(&name, SimpleFileOptions::default())
                .map_err(|error| legacy_archive_error("legacy Forge overlay", error))?;
            continue;
        }

        let expected_size = entry.size();
        let capacity = usize::try_from(expected_size).map_err(|_| legacy_overlay_limit_error())?;
        let mut bytes = Vec::with_capacity(capacity);
        entry
            .by_ref()
            .take(expected_size.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(LoaderError::Io)?;
        if bytes.len() as u64 != expected_size {
            return Err(legacy_overlay_limit_error());
        }
        writer
            .start_file(&name, SimpleFileOptions::default())
            .map_err(|error| legacy_archive_error("legacy Forge overlay", error))?;
        writer.write_all(&bytes).map_err(LoaderError::Io)?;
    }
    Ok(())
}

fn legacy_overlay_limit_error() -> LoaderError {
    LoaderError::InvalidProfile("legacy Forge overlay exceeds bounded output limits".to_string())
}

fn legacy_archive_entry_is_skipped(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper == "META-INF/MANIFEST.MF"
        || upper.ends_with(".SF")
        || upper.ends_with(".RSA")
        || upper.ends_with(".DSA")
}

fn installer_extract_error(component_name: &str, error: impl std::fmt::Display) -> LoaderError {
    LoaderError::InvalidProfile(format!("extracting {component_name} installer: {error}"))
}

fn legacy_archive_error(component_name: &str, error: impl std::fmt::Display) -> LoaderError {
    LoaderError::InvalidProfile(format!(
        "validating {component_name} legacy archive: {error}"
    ))
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

fn done() -> DownloadProgress {
    DownloadProgress {
        phase: "done".to_string(),
        current: 1,
        total: 1,
        file: None,
        error: None,
        done: true,
        bytes_done: None,
        bytes_total: None,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::{
        AuthenticatedProcessorSources, materialize_retained_installer_libraries,
        read_installed_base_client, spawn_bound_processor_execution,
    };
    use super::{
        base_version_install_lock_from_map, cleanup_on_error,
        download_installer_libraries_with_evidence, ensure_base_version,
        fetch_sha1_verified_source, finish_supported_installer_install,
        install_from_installer_source, install_from_legacy_archive, install_from_profile_source,
        install_legacy_archive_after_authenticated_base,
        install_profile_source_after_authenticated_base, overlay_legacy_archive_bytes,
        reconstruct_installer_with_downloader, reconstruct_legacy_with_downloader,
        reconstruct_profile_with_test_sources, validate_installer_record_authority,
        validate_profile_source_structure,
    };
    use crate::download::{DownloadProgress, Downloader, ExpectedIntegrity};
    use crate::known_good::{KnownGoodArtifactKind, KnownGoodInstallReceipt, KnownGoodIntegrity};
    use crate::launch::{
        AssetIndex, Downloads, JavaVersion, Library, LoggingConf, resolve_version,
    };
    use crate::loaders::compose::LoaderProfileFragment;
    use crate::loaders::forge_installer::{
        BoundForgeInstallExecution, BoundForgeInstallerPlan, bind_authenticated_installer_plan,
        plan_authenticated_installer,
    };
    #[cfg(unix)]
    use crate::loaders::managed_fs::ManagedDir;
    use crate::loaders::providers::{ProfileInstallProof, ProfileLibraryProof};
    use crate::loaders::source::VerifiedLoaderSource;
    use crate::loaders::types::LoaderError;
    use crate::loaders::types::{
        LoaderArtifactKind, LoaderBuildMetadata, LoaderBuildRecord, LoaderBuildSubjectKind,
        LoaderComponentId, LoaderInstallPlan, LoaderInstallSource, LoaderInstallStrategy,
        LoaderInstallability,
    };
    use crate::loaders::{build_id_for, installed_version_id_for, validate_version_id};
    use crate::manifest::VersionManifest;
    use crate::paths::versions_dir;
    use crate::rules::default_environment;
    use crate::runtime::ManagedRuntimeCache;
    #[cfg(unix)]
    use crate::runtime::{RuntimeId, TestRuntimeSourceDescriptor, acquire_test_runtime_source};
    use sha1::{Digest as _, Sha1};
    use std::collections::{BTreeMap, HashMap};
    use std::fs;
    use std::io::{ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    const TEST_PROCESSOR_COORDINATE: &str = "x:p:1";
    const TEST_PROCESSOR_TERMINAL_BYTES: &[u8] = b"processor-terminal";

    #[tokio::test]
    async fn profile_reconstruction_matches_install_and_leaves_all_managed_state_untouched() {
        for component in [LoaderComponentId::Fabric, LoaderComponentId::Quilt] {
            let root = temp_dir(match component {
                LoaderComponentId::Fabric => "fabric-reconstruction-parity",
                LoaderComponentId::Quilt => "quilt-reconstruction-parity",
                _ => unreachable!(),
            });
            let base_id = "1.21.5";
            let base_client = zip_entries(&[("net/minecraft/client/Main.class", b"base")]);
            let exact_library = b"exact-profile-library".to_vec();
            let client_server = TestByteServer::start(base_client.clone());
            let exact_server = TestByteServer::start(exact_library.clone());
            let version_bytes = vanilla_version_bytes_with_exact_library(
                base_id,
                &client_server.url,
                &base_client,
                &exact_server.url,
                &exact_library,
            );
            let version_server = TestByteServer::start(version_bytes.clone());
            let manifest = test_install_manifest(base_id, &version_server.url, &version_bytes);
            let incomplete_server = TestByteServer::start(zip_entries(&[(
                "org/example/Incomplete.class",
                b"incomplete",
            )]));
            let native_server =
                TestByteServer::start(zip_entries(&[("org/example/Native.class", b"native")]));
            let extra_server =
                TestByteServer::start(zip_entries(&[("org/example/Extra.class", b"extra")]));

            let mut record = profile_record();
            if component == LoaderComponentId::Quilt {
                record.component_id = component;
                record.component_name = component.display_name().to_string();
                record.loader_version = "0.29.2".to_string();
                record.strategy = LoaderInstallStrategy::QuiltProfile;
                canonicalize_record_identity(&mut record);
            }
            let (profile_bytes, proof_bytes, expected_fresh) = profile_reconstruction_sources(
                &record,
                &incomplete_server.url,
                &exact_server.url,
                &exact_library,
                &native_server.url,
                &extra_server.url,
            );
            let profile_server = TestByteServer::start(profile_bytes);
            let proof_server = TestByteServer::start(proof_bytes);
            record.install_source = LoaderInstallSource::ProfileJson {
                url: profile_server.url.clone(),
            };
            let plan = LoaderInstallPlan {
                record: record.clone(),
            };

            let install_downloader =
                Downloader::with_test_install_manifest(&root, manifest.clone());
            let base_receipt = install_downloader
                .install_version(base_id, |_| {})
                .await
                .expect("install authenticated vanilla base");
            let install_proof =
                crate::loaders::providers::fetch_profile_install_proof_from_url_for_test(
                    &record,
                    &proof_server.url,
                )
                .await
                .expect("install profile proof");
            let install_receipt = install_profile_source_after_authenticated_base(
                &root,
                &plan,
                &base_receipt,
                install_proof,
                &mut |_| {},
            )
            .await
            .expect("install profile loader");
            seed_reconstruction_sentinels(&root);
            let before = snapshot_tree(&root);
            let request_counts = (
                version_server.request_count(),
                client_server.request_count(),
                profile_server.request_count(),
                proof_server.request_count(),
                exact_server.request_count(),
                incomplete_server.request_count(),
                native_server.request_count(),
                extra_server.request_count(),
            );

            let reconstruction_downloader = Downloader::with_test_install_manifest(&root, manifest);
            let reconstructed = reconstruct_profile_with_test_sources(
                &plan,
                &reconstruction_downloader,
                &proof_server.url,
            )
            .await
            .expect("reconstruct profile loader");

            assert_eq!(snapshot_tree(&root), before);
            assert_eq!(version_server.request_count(), request_counts.0 + 1);
            assert_eq!(client_server.request_count(), request_counts.1);
            assert_eq!(profile_server.request_count(), request_counts.2 + 1);
            assert_eq!(proof_server.request_count(), request_counts.3 + 1);
            assert_eq!(exact_server.request_count(), request_counts.4);
            assert_eq!(
                incomplete_server.request_count(),
                request_counts.5 + expected_fresh.0
            );
            assert_eq!(
                native_server.request_count(),
                request_counts.6 + expected_fresh.1
            );
            assert_eq!(
                extra_server.request_count(),
                request_counts.7 + expected_fresh.2
            );
            assert_eq!(
                install_receipt.into_activation_source().into_parts(),
                reconstructed.into_activation_source().into_parts()
            );

            for server in [
                client_server,
                version_server,
                exact_server,
                incomplete_server,
                native_server,
                extra_server,
                profile_server,
                proof_server,
            ] {
                server.stop();
            }
            let _ = fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn modern_installer_reconstruction_matches_install_and_streams_only_fresh_sources() {
        let root = temp_dir("modern-installer-reconstruction-parity");
        let base_id = "1.21.5";
        let base_client = zip_entries(&[("net/minecraft/client/Main.class", b"base")]);
        let vanilla_exact = b"vanilla-exact".to_vec();
        let installer_exact =
            zip_entries(&[("example/Exact.class", b"installer-exact".as_slice())]);
        let installer_fresh =
            zip_entries(&[("example/Fresh.class", b"installer-fresh".as_slice())]);
        let client_server = TestByteServer::start(base_client.clone());
        let vanilla_exact_server = TestByteServer::start(vanilla_exact.clone());
        let version_bytes = vanilla_version_bytes_with_exact_library(
            base_id,
            &client_server.url,
            &base_client,
            &vanilla_exact_server.url,
            &vanilla_exact,
        );
        let version_server = TestByteServer::start(version_bytes.clone());
        let manifest = test_install_manifest(base_id, &version_server.url, &version_bytes);
        let installer_exact_server = TestByteServer::start(installer_exact.clone());
        let installer_fresh_server = TestByteServer::start(installer_fresh);
        let mut record = installer_record();
        let installer = declarative_modern_forge_installer_jar(
            &record,
            &installer_exact_server.url,
            &installer_exact,
            &installer_fresh_server.url,
        );
        let installer_server = TestByteServer::start_with_sha1(installer);
        record.install_source = LoaderInstallSource::InstallerJar {
            url: installer_server.url.clone(),
        };
        let plan = LoaderInstallPlan {
            record: record.clone(),
        };

        let install_downloader = Downloader::with_test_install_manifest(&root, manifest.clone());
        let base_receipt = install_downloader
            .install_version(base_id, |_| {})
            .await
            .expect("install authenticated vanilla base");
        let installer_source = verified_test_source_for(
            &installer_server.url,
            "loader installer",
            &record.version_id,
        )
        .await;
        let installer_plan = bind_test_installer(installer_source, &record);
        let execution = materialize_test_installer_network(
            &root,
            installer_plan,
            &mut |_progress: DownloadProgress| {},
        )
        .await;
        let install_receipt = finish_supported_installer_install(
            &root,
            &plan,
            execution,
            base_receipt,
            &mut |_progress: DownloadProgress| {},
        )
        .await
        .expect("install declarative Forge installer");
        seed_reconstruction_sentinels(&root);
        let before = snapshot_tree(&root);
        let installer_path = reqwest::Url::parse(&installer_server.url)
            .expect("installer URL")
            .path()
            .to_string();
        let installer_sidecar_path = format!("{installer_path}.sha1");
        let counts = (
            version_server.request_count(),
            client_server.request_count(),
            vanilla_exact_server.request_count(),
            installer_server.request_count_for(&installer_path),
            installer_server.request_count_for(&installer_sidecar_path),
            installer_exact_server.request_count(),
            installer_fresh_server.request_count(),
        );

        let reconstruction_downloader = Downloader::with_test_install_manifest(&root, manifest);
        let reconstructed =
            reconstruct_installer_with_downloader(&plan, &reconstruction_downloader)
                .await
                .expect("reconstruct declarative Forge installer");

        assert_eq!(snapshot_tree(&root), before);
        assert_eq!(version_server.request_count(), counts.0 + 1);
        assert_eq!(client_server.request_count(), counts.1 + 1);
        assert_eq!(vanilla_exact_server.request_count(), counts.2);
        assert_eq!(
            installer_server.request_count_for(&installer_path),
            counts.3 + 1
        );
        assert_eq!(
            installer_server.request_count_for(&installer_sidecar_path),
            counts.4 + 1
        );
        assert_eq!(installer_exact_server.request_count(), counts.5);
        assert_eq!(installer_fresh_server.request_count(), counts.6 + 1);
        assert_eq!(
            install_receipt.into_activation_source().into_parts(),
            reconstructed.into_activation_source().into_parts()
        );

        for server in [
            client_server,
            vanilla_exact_server,
            version_server,
            installer_exact_server,
            installer_fresh_server,
            installer_server,
        ] {
            server.stop();
        }
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn true_legacy_installer_reconstruction_matches_install_without_effects() {
        let root = temp_dir("legacy-installer-reconstruction-parity");
        let mut record = installer_record();
        record.minecraft_version = "1.5.2".to_string();
        record.loader_version = "7.8.1.738".to_string();
        record.strategy = LoaderInstallStrategy::ForgeLegacyInstaller;
        canonicalize_record_identity(&mut record);
        let base_client = zip_entries(&[
            ("META-INF/MANIFEST.MF", b"signed manifest".as_slice()),
            ("META-INF/MOJANG_C.SF", b"signature".as_slice()),
            ("META-INF/MOJANG_C.RSA", b"signature".as_slice()),
            (
                "net/minecraft/client/Minecraft.class",
                b"base client".as_slice(),
            ),
        ]);
        let client_server = TestByteServer::start(base_client.clone());
        let version_bytes =
            vanilla_version_bytes(&record.minecraft_version, &client_server.url, &base_client);
        let version_server = TestByteServer::start(version_bytes.clone());
        let manifest = test_install_manifest(
            &record.minecraft_version,
            &version_server.url,
            &version_bytes,
        );
        let installer_server =
            TestByteServer::start_with_sha1(true_legacy_forge_installer_jar(&record, true));
        record.install_source = LoaderInstallSource::InstallerJar {
            url: installer_server.url.clone(),
        };
        let plan = LoaderInstallPlan {
            record: record.clone(),
        };

        let install_downloader = Downloader::with_test_install_manifest(&root, manifest.clone());
        let base_receipt = install_downloader
            .install_version(&record.minecraft_version, |_| {})
            .await
            .expect("install authenticated legacy vanilla base");
        let installer_source = verified_test_source_for(
            &installer_server.url,
            "loader installer",
            &record.version_id,
        )
        .await;
        let installer_plan = bind_test_installer(installer_source, &record);
        let execution = materialize_test_installer_network(
            &root,
            installer_plan,
            &mut |_progress: DownloadProgress| {},
        )
        .await;
        let install_receipt = finish_supported_installer_install(
            &root,
            &plan,
            execution,
            base_receipt,
            &mut |_progress: DownloadProgress| {},
        )
        .await
        .expect("install true-legacy Forge installer");
        seed_reconstruction_sentinels(&root);
        let before = snapshot_tree(&root);
        let installer_path = reqwest::Url::parse(&installer_server.url)
            .expect("installer URL")
            .path()
            .to_string();
        let installer_sidecar_path = format!("{installer_path}.sha1");
        let counts = (
            version_server.request_count(),
            client_server.request_count(),
            installer_server.request_count_for(&installer_path),
            installer_server.request_count_for(&installer_sidecar_path),
        );

        let reconstruction_downloader = Downloader::with_test_install_manifest(&root, manifest);
        let reconstructed =
            reconstruct_installer_with_downloader(&plan, &reconstruction_downloader)
                .await
                .expect("reconstruct true-legacy Forge installer");

        assert_eq!(snapshot_tree(&root), before);
        assert_eq!(version_server.request_count(), counts.0 + 1);
        assert_eq!(client_server.request_count(), counts.1 + 1);
        assert_eq!(
            installer_server.request_count_for(&installer_path),
            counts.2 + 1
        );
        assert_eq!(
            installer_server.request_count_for(&installer_sidecar_path),
            counts.3 + 1
        );
        assert_eq!(
            install_receipt.into_activation_source().into_parts(),
            reconstructed.into_activation_source().into_parts()
        );

        for server in [client_server, version_server, installer_server] {
            server.stop();
        }
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_size_processor_reconstruction_cancels_descendants_then_retries_cleanly() {
        let root = temp_dir("processor-required-installer-reconstruction");
        seed_reconstruction_sentinels(&root);
        let before = snapshot_tree(&root);
        let processor_state = root.join("processor-state");
        let cancelled_leader = root.join("cancelled-leader.pid");
        let cancelled_descendant = root.join("cancelled-descendant.pid");
        let cancelled_workspace = root.join("cancelled-workspace");
        let successful_descendant = root.join("successful-descendant.pid");
        let successful_workspace = root.join("successful-workspace");
        let mut record = installer_record();
        let base_client = zip_entries(&[("net/minecraft/client/Main.class", b"base")]);
        let client_server = TestByteServer::start(base_client.clone());
        let mut version: serde_json::Value = serde_json::from_slice(&vanilla_version_bytes(
            &record.minecraft_version,
            &client_server.url,
            &base_client,
        ))
        .expect("base version");
        version["javaVersion"] = serde_json::json!({
            "component": "java-runtime-delta",
            "majorVersion": 17
        });
        let version_bytes = serde_json::to_vec(&version).expect("base version bytes");
        let version_server = TestByteServer::start(version_bytes.clone());
        let manifest = test_install_manifest(
            &record.minecraft_version,
            &version_server.url,
            &version_bytes,
        );
        let fresh_library = zip_entries(&[("example/Fresh.class", b"processor-fresh".as_slice())]);
        let fresh_server = TestByteServer::start(fresh_library);
        let installer_server =
            TestByteServer::start_with_sha1(single_step_processor_installer_jar_with_libraries(
                &record,
                vec![serde_json::json!({
                    "name": "example:processor-fresh:1.0",
                    "downloads": {"artifact": {
                        "path": "example/processor-fresh/1.0/processor-fresh-1.0.jar",
                        "url": fresh_server.url.clone()
                    }}
                })],
            ));
        record.install_source = LoaderInstallSource::InstallerJar {
            url: installer_server.url.clone(),
        };
        let plan = LoaderInstallPlan { record };
        let fake_java = format!(
            r#"#!/bin/sh
case "$*" in
  *-version*) printf '%s\n' 'openjdk version "17.0.1"' >&2; exit 0 ;;
esac
for last do :; done
if [ ! -e {processor_state} ]; then
  : > {processor_state}
  printf '%s\n' "$$" > {cancelled_leader}
  printf '%s\n' "$PWD" > {cancelled_workspace}
  sleep 30 &
  printf '%s\n' "$!" > {cancelled_descendant}
  wait
  exit 1
fi
printf '%s\n' "$PWD" > {successful_workspace}
sleep 30 &
printf '%s\n' "$!" > {successful_descendant}
printf '%s' 'processor-terminal' > "$last"
"#,
            processor_state = shell_quote_path(&processor_state),
            cancelled_leader = shell_quote_path(&cancelled_leader),
            cancelled_descendant = shell_quote_path(&cancelled_descendant),
            cancelled_workspace = shell_quote_path(&cancelled_workspace),
            successful_descendant = shell_quote_path(&successful_descendant),
            successful_workspace = shell_quote_path(&successful_workspace),
        )
        .into_bytes();
        let runtime_file_server = TestByteServer::start(fake_java.clone());
        let runtime_manifest_bytes = serde_json::to_vec(&serde_json::json!({
            "files": {
                "bin": {"type": "directory"},
                "bin/java": {
                    "type": "file",
                    "executable": true,
                    "downloads": {"raw": {
                        "url": runtime_file_server.url.clone(),
                        "sha1": sha1_hex(&fake_java),
                        "size": fake_java.len()
                    }}
                }
            }
        }))
        .expect("runtime manifest");
        let runtime_manifest_server = TestByteServer::start(runtime_manifest_bytes.clone());
        let runtime_source = TestRuntimeSourceDescriptor {
            component: RuntimeId::from("java-runtime-delta"),
            url: runtime_manifest_server.url.clone(),
            sha1: sha1_hex(&runtime_manifest_bytes),
            size: runtime_manifest_bytes.len() as u64,
        };
        let installer_path = reqwest::Url::parse(&installer_server.url)
            .expect("installer URL")
            .path()
            .to_string();
        let installer_sidecar_path = format!("{installer_path}.sha1");

        let cancelled_root = root.clone();
        let cancelled_plan = plan.clone();
        let cancelled_manifest = manifest.clone();
        let cancelled_runtime_source = runtime_source.clone();
        let reconstruction = tokio::spawn(async move {
            let downloader =
                Downloader::with_test_install_manifest(&cancelled_root, cancelled_manifest)
                    .with_test_runtime_source(cancelled_runtime_source);
            reconstruct_installer_with_downloader(&cancelled_plan, &downloader).await
        });
        wait_for_test_file(&cancelled_descendant).await;
        let cancelled_leader_pid = read_test_pid(&cancelled_leader);
        let cancelled_descendant_pid = read_test_pid(&cancelled_descendant);
        let cancelled_workspace_root = read_processor_workspace_root(&cancelled_workspace);
        reconstruction.abort();
        match reconstruction.await {
            Err(error) => assert!(
                error.is_cancelled(),
                "the caller cancellation must drop the in-flight reconstruction future"
            ),
            Ok(_) => panic!("cancelled reconstruction task completed"),
        }
        wait_for_process_and_workspace_cleanup(
            &[cancelled_leader_pid, cancelled_descendant_pid],
            &cancelled_workspace_root,
        )
        .await;

        let downloader = Downloader::with_test_install_manifest(&root, manifest)
            .with_test_runtime_source(runtime_source);
        let reconstructed = reconstruct_installer_with_downloader(&plan, &downloader)
            .await
            .expect("processor reconstruction retry");
        wait_for_test_file(&successful_descendant).await;
        let successful_descendant_pid = read_test_pid(&successful_descendant);
        let successful_workspace_root = read_processor_workspace_root(&successful_workspace);
        wait_for_process_and_workspace_cleanup(
            &[successful_descendant_pid],
            &successful_workspace_root,
        )
        .await;

        let mut after = snapshot_tree(&root);
        for marker in [
            &processor_state,
            &cancelled_leader,
            &cancelled_descendant,
            &cancelled_workspace,
            &successful_descendant,
            &successful_workspace,
        ] {
            after.remove(marker.strip_prefix(&root).expect("marker below test root"));
        }
        assert_eq!(after, before);
        assert_eq!(version_server.request_count(), 2);
        assert_eq!(client_server.request_count(), 2);
        assert_eq!(fresh_server.request_count(), 2);
        assert_eq!(installer_server.request_count_for(&installer_path), 2);
        assert_eq!(
            installer_server.request_count_for(&installer_sidecar_path),
            2
        );
        assert_eq!(runtime_manifest_server.request_count(), 2);
        assert_eq!(runtime_file_server.request_count(), 2);
        let (_, inventory) = reconstructed.into_activation_source().into_parts();
        let terminal = inventory
            .entries()
            .iter()
            .find(|entry| entry.path().as_str().ends_with("-client.jar"))
            .expect("observed terminal artifact");
        assert!(matches!(
            terminal.integrity(),
            KnownGoodIntegrity::Sha1 { digest, size }
                if digest.as_str() == sha1_hex(TEST_PROCESSOR_TERMINAL_BYTES)
                    && *size == TEST_PROCESSOR_TERMINAL_BYTES.len() as u64
        ));

        for server in [
            client_server,
            version_server,
            fresh_server,
            installer_server,
            runtime_manifest_server,
            runtime_file_server,
        ] {
            server.stop();
        }
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_size_processor_reconstruction_matches_install_for_supported_shapes() {
        for shape in [
            ProcessorFixtureShape::ForgeSpecZero,
            ProcessorFixtureShape::ForgeModern,
            ProcessorFixtureShape::NeoModern,
        ] {
            assert_processor_reconstruction_parity(shape).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn processor_reconstruction_executes_two_step_source_union_exactly() {
        assert_two_step_processor_source_union().await;
    }

    #[tokio::test]
    async fn outputless_neoforge_reconstruction_fails_before_vanilla_sources() {
        let root = temp_dir("outputless-neoforge-reconstruction");
        seed_reconstruction_sentinels(&root);
        let before = snapshot_tree(&root);
        let mut record = installer_record();
        record.component_id = LoaderComponentId::NeoForge;
        record.component_name = record.component_id.display_name().to_string();
        record.loader_version = "21.5.74".to_string();
        record.strategy = LoaderInstallStrategy::NeoForgeModern;
        canonicalize_record_identity(&mut record);
        let base_client = zip_entries(&[("net/minecraft/client/Main.class", b"base")]);
        let client_server = TestByteServer::start(base_client.clone());
        let version_bytes =
            vanilla_version_bytes(&record.minecraft_version, &client_server.url, &base_client);
        let version_server = TestByteServer::start(version_bytes.clone());
        let manifest = test_install_manifest(
            &record.minecraft_version,
            &version_server.url,
            &version_bytes,
        );
        let installer_server =
            TestByteServer::start_with_sha1(unsupported_neoforge_installer_jar(&record));
        record.install_source = LoaderInstallSource::InstallerJar {
            url: installer_server.url.clone(),
        };
        let plan = LoaderInstallPlan { record };
        let downloader = Downloader::with_test_install_manifest(&root, manifest);

        let error = match reconstruct_installer_with_downloader(&plan, &downloader).await {
            Ok(_) => panic!("outputless NeoForge processor must stay unsupported"),
            Err(error) => error,
        };

        assert!(matches!(error, LoaderError::InvalidProfile(_)));
        assert_eq!(snapshot_tree(&root), before);
        assert_eq!(version_server.request_count(), 0);
        assert_eq!(client_server.request_count(), 0);
        assert_eq!(installer_server.request_count(), 2);

        for server in [client_server, version_server, installer_server] {
            server.stop();
        }
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn profile_reconstruction_rejects_fresh_404_despite_installed_and_cached_evidence() {
        let root = temp_dir("profile-reconstruction-fresh-404");
        let base_id = "1.21.5";
        let base_client = zip_entries(&[("net/minecraft/client/Main.class", b"base")]);
        let client_server = TestByteServer::start(base_client.clone());
        let version_bytes = vanilla_version_bytes(base_id, &client_server.url, &base_client);
        let version_server = TestByteServer::start(version_bytes.clone());
        let manifest = test_install_manifest(base_id, &version_server.url, &version_bytes);
        let exact_library = b"unused-exact-library".to_vec();
        let incomplete_server = TestByteServer::start(zip_entries(&[(
            "org/example/Incomplete.class",
            b"incomplete",
        )]));
        let exact_server = TestByteServer::start(exact_library.clone());
        let native_server =
            TestByteServer::start(zip_entries(&[("org/example/Native.class", b"native")]));
        let extra_server =
            TestByteServer::start(zip_entries(&[("org/example/Extra.class", b"extra")]));
        let mut record = profile_record();
        let (profile_bytes, proof_bytes, _) = profile_reconstruction_sources(
            &record,
            &incomplete_server.url,
            &exact_server.url,
            &exact_library,
            &native_server.url,
            &extra_server.url,
        );
        let profile_server = TestByteServer::start(profile_bytes);
        let proof_server = TestByteServer::start(proof_bytes);
        record.install_source = LoaderInstallSource::ProfileJson {
            url: profile_server.url.clone(),
        };
        let install_plan = LoaderInstallPlan {
            record: record.clone(),
        };
        let downloader = Downloader::with_test_install_manifest(&root, manifest.clone());
        let base_receipt = downloader
            .install_version(base_id, |_| {})
            .await
            .expect("install authenticated vanilla base");
        let install_proof =
            crate::loaders::providers::fetch_profile_install_proof_from_url_for_test(
                &record,
                &proof_server.url,
            )
            .await
            .expect("install profile proof");
        install_profile_source_after_authenticated_base(
            &root,
            &install_plan,
            &base_receipt,
            install_proof,
            &mut |_| {},
        )
        .await
        .expect("install profile loader");

        seed_reconstruction_sentinels(&root);
        let before = snapshot_tree(&root);
        let missing_server = TestByteServer::start_not_found();
        let mut missing_plan = install_plan;
        missing_plan.record.install_source = LoaderInstallSource::ProfileJson {
            url: missing_server.url.clone(),
        };
        let version_count = version_server.request_count();
        let missing_count = missing_server.request_count();
        let reconstruction_downloader = Downloader::with_test_install_manifest(&root, manifest);

        let error = match reconstruct_profile_with_test_sources(
            &missing_plan,
            &reconstruction_downloader,
            &proof_server.url,
        )
        .await
        {
            Ok(_) => panic!("fresh profile 404 must reject installed evidence"),
            Err(error) => error,
        };

        assert!(matches!(error, LoaderError::ArtifactMissing(_)));
        assert_eq!(version_server.request_count(), version_count + 1);
        assert_eq!(missing_server.request_count(), missing_count + 1);
        assert_eq!(snapshot_tree(&root), before);

        for server in [
            client_server,
            version_server,
            incomplete_server,
            exact_server,
            native_server,
            extra_server,
            profile_server,
            proof_server,
            missing_server,
        ] {
            server.stop();
        }
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn earliest_archive_reconstruction_matches_install_uses_fresh_sources_once_and_is_effect_free()
     {
        for (minecraft_version, loader_version, label) in [
            ("1.2.5", "3.4.9.171", "earliest-client-parity"),
            ("1.4.7", "6.6.2.534", "earliest-universal-parity"),
        ] {
            let root = temp_dir(label);
            let mut record = legacy_archive_record();
            record.minecraft_version = minecraft_version.to_string();
            record.loader_version = loader_version.to_string();
            canonicalize_record_identity(&mut record);
            let base_client = zip_entries(&[
                ("net/minecraft/client/Minecraft.class", b"base"),
                ("META-INF/MANIFEST.MF", b"manifest"),
            ]);
            let archive = zip_entries(&[("net/minecraftforge/Forge.class", b"forge")]);
            let client_server = TestByteServer::start(base_client.clone());
            let version_bytes =
                vanilla_version_bytes(&record.minecraft_version, &client_server.url, &base_client);
            let version_server = TestByteServer::start(version_bytes.clone());
            let manifest = test_install_manifest(
                &record.minecraft_version,
                &version_server.url,
                &version_bytes,
            );
            let archive_server = TestByteServer::start_with_sha1(archive);
            record.install_source = LoaderInstallSource::LegacyArchive {
                url: archive_server.url.clone(),
            };
            let plan = LoaderInstallPlan {
                record: record.clone(),
            };
            let install_downloader =
                Downloader::with_test_install_manifest(&root, manifest.clone());
            let base_receipt = install_downloader
                .install_version(&record.minecraft_version, |_| {})
                .await
                .expect("install authenticated vanilla base");
            let archive_source = verified_test_source_for(
                &archive_server.url,
                "legacy Forge archive",
                &record.version_id,
            )
            .await;
            let install_receipt = install_legacy_archive_after_authenticated_base(
                &root,
                &plan,
                archive_source,
                &base_receipt,
                &mut |_| {},
            )
            .await
            .expect("install earliest Forge archive");
            seed_reconstruction_sentinels(&root);
            let before = snapshot_tree(&root);
            let archive_path = reqwest::Url::parse(&archive_server.url)
                .expect("archive URL")
                .path()
                .to_string();
            let sidecar_path = format!("{archive_path}.sha1");
            let counts = (
                version_server.request_count(),
                client_server.request_count(),
                archive_server.request_count(),
                archive_server.request_count_for(&archive_path),
                archive_server.request_count_for(&sidecar_path),
            );

            let reconstruction_downloader = Downloader::with_test_install_manifest(&root, manifest);
            let reconstructed =
                reconstruct_legacy_with_downloader(&plan, &reconstruction_downloader)
                    .await
                    .expect("reconstruct earliest Forge archive");

            assert_eq!(snapshot_tree(&root), before);
            assert_eq!(version_server.request_count(), counts.0 + 1);
            assert_eq!(client_server.request_count(), counts.1 + 1);
            assert_eq!(archive_server.request_count(), counts.2 + 2);
            assert_eq!(
                archive_server.request_count_for(&archive_path),
                counts.3 + 1
            );
            assert_eq!(
                archive_server.request_count_for(&sidecar_path),
                counts.4 + 1
            );
            assert_eq!(
                install_receipt.into_activation_source().into_parts(),
                reconstructed.into_activation_source().into_parts()
            );

            for server in [client_server, version_server, archive_server] {
                server.stop();
            }
            let _ = fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn earliest_archive_reconstruction_rejects_malformed_sidecar_without_effects() {
        let root = temp_dir("earliest-reconstruction-malformed-sidecar");
        let mut record = legacy_archive_record();
        let base_client = zip_entries(&[("net/minecraft/client/Minecraft.class", b"base")]);
        let client_server = TestByteServer::start(base_client.clone());
        let version_bytes =
            vanilla_version_bytes(&record.minecraft_version, &client_server.url, &base_client);
        let version_server = TestByteServer::start(version_bytes.clone());
        let manifest = test_install_manifest(
            &record.minecraft_version,
            &version_server.url,
            &version_bytes,
        );
        let archive_server = TestByteServer::start_with_sha1_proof(
            zip_entries(&[("net/minecraftforge/Forge.class", b"forge")]),
            b"not-a-strict-sha1 archive.zip".to_vec(),
        );
        record.install_source = LoaderInstallSource::LegacyArchive {
            url: archive_server.url.clone(),
        };
        let plan = LoaderInstallPlan { record };
        let install_downloader = Downloader::with_test_install_manifest(&root, manifest.clone());
        install_downloader
            .install_version(&plan.record.minecraft_version, |_| {})
            .await
            .expect("install authenticated vanilla evidence");
        seed_reconstruction_sentinels(&root);
        let before = snapshot_tree(&root);
        let counts = (
            version_server.request_count(),
            client_server.request_count(),
            archive_server.request_count(),
        );
        let reconstruction_downloader = Downloader::with_test_install_manifest(&root, manifest);

        let error =
            match reconstruct_legacy_with_downloader(&plan, &reconstruction_downloader).await {
                Ok(_) => panic!("malformed archive proof must reject reconstruction"),
                Err(error) => error,
            };

        assert!(
            matches!(error, LoaderError::InvalidProfile(message) if message.contains("exactly one 40-hex digest"))
        );
        assert_eq!(version_server.request_count(), counts.0 + 1);
        assert_eq!(client_server.request_count(), counts.1 + 1);
        assert_eq!(archive_server.request_count(), counts.2 + 2);
        assert_eq!(snapshot_tree(&root), before);

        for server in [client_server, version_server, archive_server] {
            server.stop();
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_on_error_clears_incomplete_version_dir() {
        let root = temp_dir("cleanup-on-error");
        let version_dir = versions_dir(&root).join("broken-loader");
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::write(version_dir.join(".incomplete"), b"installing").expect("marker");

        let result = cleanup_on_error::<(), _>(
            Err(LoaderError::Verify("broken".to_string())),
            &root,
            "broken-loader",
        );

        assert!(result.is_err());
        assert!(version_dir.is_dir());
        assert_eq!(
            fs::read_dir(&version_dir)
                .expect("retained version shell")
                .count(),
            0
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn profile_source_validation_does_not_enrich_profile() {
        let record = profile_record();
        let proof = fabric_profile_proof(&record);
        let fragment = fabric_profile_fragment(&record);

        validate_profile_source_structure(&fragment, &record, &proof)
            .expect("exact live profile proof");
        let loader = fragment
            .libraries
            .iter()
            .find(|library| library.name.starts_with("net.fabricmc:fabric-loader:"))
            .expect("loader library");
        assert!(loader.sha1.is_empty());
        assert_eq!(loader.size, 0);
    }

    #[test]
    fn profile_source_rejects_identity_drift_and_base_owned_overrides() {
        let record = profile_record();
        let proof = fabric_profile_proof(&record);

        let mut variants = Vec::new();
        let mut fragment = fabric_profile_fragment(&record);
        fragment.id.push_str("-wrong");
        variants.push(fragment);
        let mut fragment = fabric_profile_fragment(&record);
        fragment.inherits_from = "1.21.4".to_string();
        variants.push(fragment);
        let mut fragment = fabric_profile_fragment(&record);
        fragment.main_class = "wrong.Main".to_string();
        variants.push(fragment);
        let mut fragment = fabric_profile_fragment(&record);
        fragment.kind = "snapshot".to_string();
        variants.push(fragment);
        let mut fragment = fabric_profile_fragment(&record);
        fragment.asset_index = Some(AssetIndex::default());
        variants.push(fragment);
        let mut fragment = fabric_profile_fragment(&record);
        fragment.assets = "legacy".to_string();
        variants.push(fragment);
        let mut fragment = fabric_profile_fragment(&record);
        fragment.downloads = Some(Downloads::default());
        variants.push(fragment);
        let mut fragment = fabric_profile_fragment(&record);
        fragment.java_version = Some(JavaVersion::default());
        variants.push(fragment);
        let mut fragment = fabric_profile_fragment(&record);
        fragment.logging = Some(LoggingConf::default());
        variants.push(fragment);

        for fragment in variants {
            assert!(
                validate_profile_source_structure(&fragment, &record, &proof).is_err(),
                "identity drift or base-owned override must fail"
            );
        }
    }

    #[test]
    fn loader_install_futures_stay_small_enough_for_tokio_workers() {
        let root = PathBuf::from("/tmp/axial-loader-future-size");
        let profile_plan = LoaderInstallPlan {
            record: profile_record(),
        };
        let installer_plan = LoaderInstallPlan {
            record: installer_record(),
        };
        let legacy_plan = LoaderInstallPlan {
            record: legacy_archive_record(),
        };
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");

        let mut send = |_progress: DownloadProgress| {};
        assert!(
            std::mem::size_of_val(&ensure_base_version(
                &root,
                &runtime_cache,
                "1.21.5",
                &mut send,
            )) < 4096,
            "loader base-version future should not embed the full vanilla install future"
        );

        let mut send = |_progress: DownloadProgress| {};
        assert!(
            std::mem::size_of_val(&install_from_profile_source(
                &root,
                &runtime_cache,
                &profile_plan,
                &mut send,
            )) < 4096,
            "profile-backed loader install future should stay small"
        );

        let mut send = |_progress: DownloadProgress| {};
        assert!(
            std::mem::size_of_val(&install_from_installer_source(
                &root,
                &runtime_cache,
                &installer_plan,
                &mut send,
            )) < 4096,
            "installer-backed loader install future should stay small"
        );

        let mut send = |_progress: DownloadProgress| {};
        assert!(
            std::mem::size_of_val(&install_from_legacy_archive(
                &root,
                &runtime_cache,
                &legacy_plan,
                &mut send,
            )) < 4096,
            "legacy archive loader install future should stay small"
        );

        assert!(
            std::mem::size_of_val(&super::super::install_build(
                &root,
                &runtime_cache,
                &installer_plan,
                |_| {},
            )) < 4096,
            "loader strategy dispatcher future should not embed the largest strategy branch"
        );

        assert!(
            std::mem::size_of_val(&crate::loaders::install_build(
                &root,
                runtime_cache.clone(),
                installer_plan.record.clone(),
                |_| {}
            )) < 4096,
            "public loader install future should not embed the strategy dispatcher"
        );

        assert!(
            std::mem::size_of_val(&reconstruct_profile_with_test_sources(
                &profile_plan,
                &Downloader::with_test_install_manifest(
                    &root,
                    test_install_manifest(
                        "1.21.5",
                        "https://example.test/version.json",
                        b"version"
                    )
                ),
                "https://example.test/profile-proof.json",
            )) < 4096,
            "profile reconstruction future should stay small"
        );
        assert!(
            std::mem::size_of_val(&reconstruct_legacy_with_downloader(
                &legacy_plan,
                &Downloader::with_test_install_manifest(
                    &root,
                    test_install_manifest("1.2.5", "https://example.test/version.json", b"version")
                ),
            )) < 4096,
            "archive reconstruction future should stay small"
        );
        assert!(
            std::mem::size_of_val(&super::super::reconstruct_build(&profile_plan)) < 4096,
            "loader reconstruction dispatcher future should stay small"
        );
        assert!(
            std::mem::size_of_val(&crate::loaders::reconstruct_build(
                &profile_plan.record.version_id
            )) < 4096,
            "public loader reconstruction future should stay small"
        );
    }

    #[tokio::test]
    async fn base_version_install_lock_serializes_same_library_version() {
        let locks = std::sync::Mutex::new(HashMap::new());
        let root = PathBuf::from("/tmp/axial-loader-base-lock");
        let first = base_version_install_lock_from_map(&locks, &root, "1.21.5");
        let second = base_version_install_lock_from_map(&locks, &root, "1.21.5");
        let other_version = base_version_install_lock_from_map(&locks, &root, "1.21.4");
        let other_library =
            base_version_install_lock_from_map(&locks, &root.join("other"), "1.21.5");

        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &other_version));
        assert!(!Arc::ptr_eq(&first, &other_library));

        let first_guard = first.lock().await;
        let second_wait = tokio::spawn(async move {
            let _guard = second.lock().await;
        });
        tokio::task::yield_now().await;
        assert!(!second_wait.is_finished());

        drop(first_guard);
        tokio::time::timeout(Duration::from_secs(1), second_wait)
            .await
            .expect("second waiter should acquire after first guard drops")
            .expect("second waiter should not panic");
    }

    #[test]
    fn base_version_install_lock_recovers_from_poisoned_map_lock() {
        let locks = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let root = PathBuf::from("/tmp/axial-loader-poisoned-base-lock");
        let seeded_install_lock = Arc::new(tokio::sync::Mutex::new(()));
        let poison_target = Arc::clone(&locks);
        let poison_seed = Arc::clone(&seeded_install_lock);
        let poison_root = root.clone();

        let _ = std::thread::spawn(move || {
            let key = format!("{}\n{}", poison_root.to_string_lossy(), "1.21.5");
            let mut guard = poison_target.lock().unwrap();
            guard.insert(key, poison_seed);
            panic!("poison base install lock map");
        })
        .join();

        assert!(locks.is_poisoned());
        let recovered_lock = base_version_install_lock_from_map(&locks, &root, "1.21.5");

        assert!(Arc::ptr_eq(&recovered_lock, &seeded_install_lock));
    }

    #[tokio::test]
    async fn fabric_install_ignores_bogus_profile_integrity_and_streams_fresh_bytes() {
        let root = temp_dir("fabric-profile-bogus-integrity");
        fs::create_dir_all(&root).expect("create root");
        let mut record = profile_record();
        let coordinate = format!("net.fabricmc:fabric-loader:{}", record.loader_version);
        let artifact_path = format!(
            "net/fabricmc/fabric-loader/{0}/fabric-loader-{0}.jar",
            record.loader_version
        );
        let stale = zip_entries(&[("example/Stale.class", b"stale")]);
        let fresh = zip_entries(&[(
            "net/fabricmc/loader/impl/launch/knot/KnotClient.class",
            b"fresh",
        )]);
        let destination = root.join("libraries").join(&artifact_path);
        fs::create_dir_all(destination.parent().expect("artifact parent"))
            .expect("artifact parent");
        fs::write(&destination, stale).expect("stale profile library");
        let library_server = TestByteServer::start(fresh.clone());
        let profile_id = format!(
            "fabric-loader-{}-{}",
            record.loader_version, record.minecraft_version
        );
        let bogus_sha1 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let profile_bytes = serde_json::to_vec(&serde_json::json!({
            "id": profile_id.clone(),
            "inheritsFrom": record.minecraft_version.clone(),
            "type": "release",
            "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
            "libraries": [{
                "name": coordinate.clone(),
                "sha1": bogus_sha1,
                "sha256": "untrusted-sha256",
                "checksums": [bogus_sha1],
                "size": 1,
                "downloads": {"artifact": {
                    "path": artifact_path.clone(),
                    "url": library_server.url.clone(),
                    "sha1": bogus_sha1,
                    "size": 1
                }}
            }]
        }))
        .expect("profile json");
        let profile_server = TestByteServer::start(profile_bytes);
        record.install_source = LoaderInstallSource::ProfileJson {
            url: profile_server.url.clone(),
        };
        let plan = LoaderInstallPlan {
            record: record.clone(),
        };
        let proof = ProfileInstallProof::from_test(
            profile_id,
            record.minecraft_version.clone(),
            "net.fabricmc.loader.impl.launch.knot.KnotClient".to_string(),
            vec![ProfileLibraryProof::from_test(
                coordinate.clone(),
                None,
                None,
            )],
        );
        write_base_version(&root, &record.minecraft_version);
        let base = test_authenticated_receipt(&root, &record.minecraft_version);

        let receipt = install_profile_source_after_authenticated_base(
            &root,
            &plan,
            &base,
            proof,
            &mut |_progress| {},
        )
        .await
        .expect("Fabric profile install");
        assert_eq!(fs::read(&destination).expect("fresh library"), fresh);
        let version_path = versions_dir(&root)
            .join(&record.version_id)
            .join(format!("{}.json", record.version_id));
        let written: crate::launch::VersionJson =
            serde_json::from_slice(&fs::read(version_path).expect("written version json"))
                .expect("parse written version");
        let library = written
            .libraries
            .iter()
            .find(|library| library.name == coordinate)
            .expect("written Fabric library");
        let digest = sha1_hex(&fresh);
        assert_ne!(digest, bogus_sha1);
        assert_eq!(library.sha1, digest);
        assert_eq!(library.size, fresh.len() as i64);
        assert!(library.sha256.is_empty());
        assert!(library.checksums.is_empty());
        let artifact = library
            .downloads
            .as_ref()
            .and_then(|downloads| downloads.artifact.as_ref())
            .expect("written artifact");
        assert_eq!(artifact.sha1, digest);
        assert_eq!(artifact.size, fresh.len() as i64);
        assert!(
            receipt
                .into_activation_source()
                .into_parts()
                .1
                .entries()
                .iter()
                .any(|entry| {
                    entry.path().as_str() == artifact_path
                        && matches!(
                            entry.integrity(),
                            KnownGoodIntegrity::Sha1 { digest: receipt_digest, size }
                                if receipt_digest.as_str() == digest && *size == fresh.len() as u64
                        )
                })
        );
        assert_eq!(library_server.request_count(), 1);
        profile_server.stop();
        library_server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn checksumless_quilt_install_writes_sealed_metadata_and_returns_sha1_receipt() {
        let root = temp_dir("quilt-profile-sealed-write");
        fs::create_dir_all(&root).expect("create root");
        let mut record = profile_record();
        record.component_id = LoaderComponentId::Quilt;
        record.component_name = "Quilt".to_string();
        record.loader_version = "0.29.2".to_string();
        canonicalize_record_identity(&mut record);
        record.strategy = LoaderInstallStrategy::QuiltProfile;
        let coordinate = format!("org.quiltmc:quilt-loader:{}", record.loader_version);
        let artifact_path = format!(
            "org/quiltmc/quilt-loader/{0}/quilt-loader-{0}.jar",
            record.loader_version
        );
        let library_bytes =
            zip_entries(&[("org/quiltmc/loader/impl/QuiltLoader.class", b"loader")]);
        let library_server = TestByteServer::start(library_bytes.clone());
        let profile_id = format!(
            "quilt-loader-{}-{}",
            record.loader_version, record.minecraft_version
        );
        let profile_bytes = serde_json::to_vec(&serde_json::json!({
            "id": profile_id.clone(),
            "inheritsFrom": record.minecraft_version.clone(),
            "type": "release",
            "mainClass": "org.quiltmc.loader.impl.launch.knot.KnotClient",
            "libraries": [{
                "name": coordinate.clone(),
                "downloads": {"artifact": {
                    "path": artifact_path.clone(),
                    "url": library_server.url.clone()
                }}
            }]
        }))
        .expect("profile json");
        let profile_server = TestByteServer::start(profile_bytes);
        record.install_source = LoaderInstallSource::ProfileJson {
            url: profile_server.url.clone(),
        };
        let plan = LoaderInstallPlan {
            record: record.clone(),
        };
        let proof = ProfileInstallProof::from_test(
            profile_id,
            record.minecraft_version.clone(),
            "org.quiltmc.loader.impl.launch.knot.KnotClient".to_string(),
            vec![ProfileLibraryProof::from_test(
                coordinate.clone(),
                None,
                None,
            )],
        );
        write_base_version(&root, &record.minecraft_version);
        let base = test_authenticated_receipt(&root, &record.minecraft_version);

        let receipt = install_profile_source_after_authenticated_base(
            &root,
            &plan,
            &base,
            proof,
            &mut |_progress| {},
        )
        .await
        .expect("quilt profile install");
        let version_path = versions_dir(&root)
            .join(&record.version_id)
            .join(format!("{}.json", record.version_id));
        let written: crate::launch::VersionJson =
            serde_json::from_slice(&fs::read(version_path).expect("written version json"))
                .expect("parse written version");
        let library = written
            .libraries
            .iter()
            .find(|library| library.name == coordinate)
            .expect("written quilt library");
        let digest = sha1_hex(&library_bytes);
        assert_eq!(library.sha1, digest);
        assert_eq!(library.size, library_bytes.len() as i64);
        let artifact = library
            .downloads
            .as_ref()
            .and_then(|downloads| downloads.artifact.as_ref())
            .expect("written artifact");
        assert_eq!(artifact.sha1, digest);
        assert_eq!(artifact.size, library_bytes.len() as i64);
        let inventory = receipt.into_activation_source().into_parts().1;
        assert!(inventory.entries().iter().any(|entry| {
            entry.path().as_str() == artifact_path
                && matches!(
                    entry.integrity(),
                    KnownGoodIntegrity::Sha1 { digest: receipt_digest, size }
                        if receipt_digest.as_str() == digest && *size == library_bytes.len() as u64
                )
        }));
        assert_eq!(library_server.request_count(), 1);
        profile_server.stop();
        library_server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_whitespace_only_installed_version_id() {
        let error = validate_version_id(" \n ", "installed loader version id").expect_err("error");
        assert!(matches!(
            error,
            LoaderError::InstallExecutionFailed(message)
                if message == "installed loader version id is empty"
        ));
    }

    #[test]
    fn rejects_whitespace_padded_installed_version_id() {
        let error =
            validate_version_id(" loader-id ", "installed loader version id").expect_err("error");
        assert!(matches!(
            error,
            LoaderError::InstallExecutionFailed(message)
                if message == "installed loader version id contains surrounding whitespace"
        ));
    }

    #[tokio::test]
    async fn every_installer_strategy_rejects_sha1_mismatch_before_base_effects() {
        for (component, strategy) in [
            (
                LoaderComponentId::Forge,
                LoaderInstallStrategy::ForgeLegacyInstaller,
            ),
            (LoaderComponentId::Forge, LoaderInstallStrategy::ForgeModern),
            (
                LoaderComponentId::NeoForge,
                LoaderInstallStrategy::NeoForgeModern,
            ),
        ] {
            let root = temp_dir("installer-source-sha1-mismatch");
            let server = TestByteServer::start_with_sha1_proof(
                installer_jar("upstream-installer-id"),
                vec![b'0'; 40],
            );
            let mut record = installer_record();
            record.component_id = component;
            record.component_name = component.display_name().to_string();
            record.strategy = strategy;
            record.install_source = LoaderInstallSource::InstallerJar {
                url: server.url.clone(),
            };
            canonicalize_record_identity(&mut record);
            let plan = LoaderInstallPlan { record };
            let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");

            let error = install_from_installer_source(&root, &runtime_cache, &plan, &mut |_| {})
                .await
                .expect_err("mismatched proof must fail");

            assert!(
                matches!(error, LoaderError::Verify(message) if message.contains("live sha1 proof"))
            );
            assert!(!root.exists());
            assert_eq!(server.request_count(), 2);
            server.stop();
        }
    }

    #[tokio::test]
    async fn installer_source_rejects_malformed_sha1_proof_before_base_effects() {
        let root = temp_dir("installer-source-sha1-malformed");
        let server = TestByteServer::start_with_sha1_proof(
            installer_jar("upstream-installer-id"),
            b"not-a-digest installer.jar".to_vec(),
        );
        let mut record = installer_record();
        record.install_source = LoaderInstallSource::InstallerJar {
            url: server.url.clone(),
        };
        let plan = LoaderInstallPlan { record };
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");

        let error = install_from_installer_source(&root, &runtime_cache, &plan, &mut |_| {})
            .await
            .expect_err("malformed proof must fail");

        assert!(
            matches!(error, LoaderError::InvalidProfile(message) if message.contains("exactly one 40-hex digest"))
        );
        assert!(!root.exists());
        assert_eq!(server.request_count(), 2);
        server.stop();
    }

    #[test]
    fn installer_record_authority_rejects_every_envelope_drift() {
        let record = installer_record();
        validate_installer_record_authority(&record).expect("canonical installer authority");

        let mut variants = Vec::new();
        let mut drift = record.clone();
        drift.build_id.push('x');
        variants.push(drift);
        let mut drift = record.clone();
        drift.component_name = "NeoForge".to_string();
        variants.push(drift);
        let mut drift = record.clone();
        drift.strategy = LoaderInstallStrategy::NeoForgeModern;
        variants.push(drift);
        let mut drift = record.clone();
        drift.artifact_kind = LoaderArtifactKind::ProfileJson;
        variants.push(drift);
        let mut drift = record.clone();
        drift.install_source = LoaderInstallSource::ProfileJson {
            url: "https://example.test/profile.json".to_string(),
        };
        variants.push(drift);
        let mut drift = record;
        drift.install_source = LoaderInstallSource::InstallerJar { url: String::new() };
        variants.push(drift);

        for record in variants {
            assert!(validate_installer_record_authority(&record).is_err());
        }
    }

    #[tokio::test]
    async fn semantic_installer_drift_is_rejected_before_base_effects() {
        let root = temp_dir("installer-semantic-drift");
        let mut record = installer_record();
        let server = TestByteServer::start_with_sha1(modern_forge_installer_jar_with_parent(
            &record, "1.21.4", None,
        ));
        record.install_source = LoaderInstallSource::InstallerJar {
            url: server.url.clone(),
        };
        let plan = LoaderInstallPlan { record };
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");

        let error = install_from_installer_source(&root, &runtime_cache, &plan, &mut |_| {})
            .await
            .expect_err("semantic drift must fail before base acquisition");

        assert!(matches!(error, LoaderError::InvalidProfile(_)));
        assert!(!root.exists());
        assert_eq!(server.request_count(), 2);
        server.stop();
    }

    #[tokio::test]
    async fn unsupported_neoforge_processors_are_rejected_without_install_effects() {
        let root = temp_dir("unsupported-neoforge-no-effects");
        let mut record = installer_record();
        record.component_id = LoaderComponentId::NeoForge;
        record.component_name = "NeoForge".to_string();
        record.loader_version = "21.5.74".to_string();
        canonicalize_record_identity(&mut record);
        record.strategy = LoaderInstallStrategy::NeoForgeModern;
        let server = TestByteServer::start_with_sha1(unsupported_neoforge_installer_jar(&record));
        record.install_source = LoaderInstallSource::InstallerJar {
            url: server.url.clone(),
        };
        let plan = LoaderInstallPlan { record };
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");

        let error = install_from_installer_source(&root, &runtime_cache, &plan, &mut |_| {})
            .await
            .expect_err("unsupported NeoForge processors");

        assert!(matches!(error, LoaderError::InvalidProfile(_)));
        assert!(!root.exists());
        assert_eq!(server.request_count(), 2);
        server.stop();
    }

    #[tokio::test]
    async fn authenticated_installer_identity_installs_to_backend_version_id() {
        let root = temp_dir("installer-bound-identity");
        write_base_version(&root, "1.21.5");
        let record = installer_record();
        let installer_server =
            TestByteServer::start_with_sha1(modern_forge_installer_jar(&record, None));
        let plan = LoaderInstallPlan {
            record: record.clone(),
        };
        let mut progress = |_progress: DownloadProgress| {};
        let installer_source =
            verified_test_source(&installer_server.url, "loader installer").await;
        let installer_plan = bind_test_installer(installer_source, &record);

        let receipt = finish_test_installer(&root, &plan, installer_plan, &mut progress).await;

        assert_eq!(receipt.version_id(), record.version_id);
        assert_backend_version_was_written(
            &root,
            &record.version_id,
            &format!("1.21.5-forge-{}", record.loader_version),
        );
        let processor_path = "example/processor-only/1.0/processor-only-1.0.jar";
        let installed_version: serde_json::Value = serde_json::from_slice(
            &fs::read(
                versions_dir(&root)
                    .join(&record.version_id)
                    .join(format!("{}.json", record.version_id)),
            )
            .expect("installed version bytes"),
        )
        .expect("installed version json");
        assert!(
            !installed_version["libraries"]
                .as_array()
                .expect("installed libraries")
                .iter()
                .any(|library| library["name"] == "example:processor-only:1.0")
        );
        assert!(root.join("libraries").join(processor_path).is_file());
        assert!(
            receipt
                .into_activation_source()
                .into_parts()
                .1
                .entries()
                .iter()
                .any(|entry| entry.path().as_str() == processor_path)
        );
        assert_eq!(installer_server.request_count(), 2);
        installer_server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn continue_receipt_failure_precedes_writes_and_cleans_workspace() {
        let root = temp_dir("installer-continue-receipt-before-write");
        write_base_version(&root, "1.21.5");
        write_base_version(&root, "1.21.4");
        let record = installer_record();
        let installer_server =
            TestByteServer::start_with_sha1(modern_forge_installer_jar(&record, None));
        let plan = LoaderInstallPlan {
            record: record.clone(),
        };
        let installer_source =
            verified_test_source(&installer_server.url, "loader installer").await;
        let execution = materialize_test_installer_network(
            &root,
            bind_test_installer(installer_source, &record),
            &mut |_| {},
        )
        .await;

        let error = finish_supported_installer_install(
            &root,
            &plan,
            execution,
            test_authenticated_receipt(&root, "1.21.4"),
            &mut |_| {},
        )
        .await
        .expect_err("mismatched base receipt");

        assert!(matches!(error, LoaderError::InvalidProfile(_)));
        assert!(!versions_dir(&root).join(&record.version_id).exists());
        installer_server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn run_failure_precedes_writes_and_cleans_workspace() {
        let root = temp_dir("installer-run-receipt-before-write");
        write_base_version(&root, "1.21.5");
        write_base_version(&root, "1.21.4");
        let record = installer_record();
        let installer_server =
            TestByteServer::start_with_sha1(runnable_forge_installer_jar(&record));
        let plan = LoaderInstallPlan {
            record: record.clone(),
        };
        let installer_source =
            verified_test_source(&installer_server.url, "loader installer").await;
        let execution = materialize_test_installer_network(
            &root,
            bind_test_installer(installer_source, &record),
            &mut |_| {},
        )
        .await;

        let error = finish_supported_installer_install(
            &root,
            &plan,
            execution,
            test_authenticated_receipt(&root, "1.21.4"),
            &mut |_| {},
        )
        .await
        .expect_err("mismatched processor base receipt");

        assert!(matches!(error, LoaderError::ProcessorFailed(_)));
        assert!(!versions_dir(&root).join(&record.version_id).exists());
        installer_server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn installer_source_allows_checksumless_authenticated_root_library() {
        let root = temp_dir("installer-checksumless-root-library");
        let minecraft_version = "1.21.5";
        write_base_version(&root, minecraft_version);
        let library_body = zip_entries(&[("net/minecraftforge/Forge.class", b"forge")]);
        let base_library_body = zip_entries(&[("example/Base.class", b"base")]);
        let coordinate = "net.minecraftforge:forge:1.21.5-55.0.0:universal";
        let relative_path =
            "net/minecraftforge/forge/1.21.5-55.0.0/forge-1.21.5-55.0.0-universal.jar";
        let base_version_path = versions_dir(&root)
            .join(minecraft_version)
            .join(format!("{minecraft_version}.json"));
        let mut base_version: serde_json::Value =
            serde_json::from_slice(&fs::read(&base_version_path).expect("base version bytes"))
                .expect("base version json");
        base_version["libraries"] = serde_json::json!([{
            "name": coordinate,
            "sha1": sha1_hex(&base_library_body),
            "size": base_library_body.len()
        }]);
        fs::write(
            &base_version_path,
            serde_json::to_vec_pretty(&base_version).expect("serialize base version"),
        )
        .expect("write shadowing base version");
        let library_path = root.join("libraries").join(relative_path);
        fs::create_dir_all(library_path.parent().expect("library parent"))
            .expect("create library parent");
        fs::write(&library_path, &base_library_body).expect("write base-selected library");
        let library_server = TestByteServer::start(library_body);
        let record = installer_record();
        let installer_server = TestByteServer::start_with_sha1(modern_forge_installer_jar(
            &record,
            Some(&library_server.url),
        ));
        let plan = LoaderInstallPlan {
            record: record.clone(),
        };
        let installer_source =
            verified_test_source(&installer_server.url, "loader installer").await;
        let installer_plan = bind_test_installer(installer_source, &record);

        let receipt = finish_test_installer(&root, &plan, installer_plan, &mut |_| {}).await;

        assert_eq!(receipt.version_id(), record.version_id);
        assert_eq!(installer_server.request_count(), 2);
        assert_eq!(library_server.request_count(), 1);
        assert!(zip_contains(
            &library_path,
            "net/minecraftforge/Forge.class"
        ));
        assert!(!zip_contains(&library_path, "example/Base.class"));
        let version_json = fs::read(
            versions_dir(&root)
                .join(&record.version_id)
                .join(format!("{}.json", record.version_id)),
        )
        .expect("read installer profile");
        let version: serde_json::Value =
            serde_json::from_slice(&version_json).expect("parse installer profile");
        let libraries = version["libraries"].as_array().expect("libraries");
        assert!(libraries.iter().any(|library| {
            library["name"] == "net.minecraftforge:forge:1.21.5-55.0.0:universal"
                && library.get("axialChecksumlessAllowed").is_none()
        }));
        let inventory = receipt.into_activation_source().into_parts().1;
        let entry = inventory
            .entries()
            .iter()
            .find(|entry| entry.path().as_str() == relative_path)
            .expect("loader-shadowed receipt entry");
        assert!(matches!(
            entry.integrity(),
            KnownGoodIntegrity::Sha1 { digest, size }
                if digest.as_str() == sha1_hex(&fs::read(&library_path).expect("final library"))
                    && *size == fs::metadata(&library_path).expect("library metadata").len()
        ));

        installer_server.stop();
        library_server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn strip_meta_legacy_installer_install_strips_child_not_base_client() {
        let root = temp_dir("installer-strip-meta-install");
        let minecraft_version = "1.5.2";
        let loader_version = "7.8.1.738";
        let version_id =
            installed_version_id_for(LoaderComponentId::Forge, minecraft_version, loader_version)
                .expect("canonical installed version id");
        let base_dir = versions_dir(&root).join(minecraft_version);
        fs::create_dir_all(&base_dir).expect("base version dir");
        let signed_client = zip_entries(&[
            ("META-INF/MANIFEST.MF", b"signed manifest".as_slice()),
            ("META-INF/MOJANG_C.SF", b"signature".as_slice()),
            ("META-INF/MOJANG_C.RSA", b"signature".as_slice()),
            ("net/minecraft/client/Minecraft.class", b"class".as_slice()),
        ]);
        fs::write(
            base_dir.join(format!("{minecraft_version}.jar")),
            &signed_client,
        )
        .expect("write signed base client");
        fs::write(
            base_dir.join(format!("{minecraft_version}.json")),
            format!(
                r#"{{
                    "id":"{minecraft_version}",
                    "type":"release",
                    "mainClass":"net.minecraft.client.Minecraft",
                    "assets":"base-assets",
                    "assetIndex":{{"id":"legacy","url":"","sha1":"","size":0,"totalSize":0}},
                    "javaVersion":{{"component":"jre-legacy","majorVersion":8}},
                    "logging":{{
                        "client":{{
                            "argument":"base-logging",
                            "file":{{"id":"base-log.xml","url":"","sha1":"","size":0}}
                        }}
                    }},
                    "downloads":{{
                        "client":{{
                            "url":"https://example.invalid/{minecraft_version}.jar",
                            "sha1":"{}",
                            "size":{}
                        }}
                    }},
                    "libraries":[]
                }}"#,
                sha1_hex(&signed_client),
                signed_client.len()
            ),
        )
        .expect("write base version json");
        let install_profile = br#"{
            "versionInfo": {
                "id": "1.5.2-Forge7.8.1.738",
                "mainClass": "net.minecraft.launchwrapper.Launch",
                "minecraftArguments": "${auth_player_name} ${auth_session}",
                "libraries": [
                    { "name": "net.minecraftforge:minecraftforge:7.8.1.738" }
                ]
            },
            "install": {
                "path": "net.minecraftforge:minecraftforge:7.8.1.738",
                "filePath": "minecraftforge-universal-1.5.2-7.8.1.738.jar",
                "target": "1.5.2-Forge7.8.1.738",
                "minecraft": "1.5.2",
                "stripMeta": true
            }
        }"#;
        let forge_jar = zip_entries(&[
            ("META-INF/MANIFEST.MF", b"forge manifest".as_slice()),
            ("META-INF/FORGE.SF", b"signature".as_slice()),
            ("net/minecraftforge/Forge.class", b"forge".as_slice()),
        ]);
        let installer = zip_entries(&[
            ("install_profile.json", install_profile.as_slice()),
            (
                "minecraftforge-universal-1.5.2-7.8.1.738.jar",
                forge_jar.as_slice(),
            ),
        ]);
        let mut record = installer_record();
        record.minecraft_version = minecraft_version.to_string();
        record.loader_version = loader_version.to_string();
        canonicalize_record_identity(&mut record);
        assert_eq!(record.version_id, version_id);
        record.strategy = LoaderInstallStrategy::ForgeLegacyInstaller;
        let installer_server = TestByteServer::start_with_sha1(installer);
        let plan = LoaderInstallPlan {
            record: record.clone(),
        };
        let installer_source =
            verified_test_source(&installer_server.url, "loader installer").await;
        let installer_plan = bind_test_installer(installer_source, &record);

        let receipt = finish_test_installer(&root, &plan, installer_plan, &mut |_| {}).await;
        assert_eq!(receipt.version_id(), record.version_id);

        let child_jar = versions_dir(&root)
            .join(&version_id)
            .join(format!("{version_id}.jar"));
        assert!(zip_contains(
            &child_jar,
            "net/minecraft/client/Minecraft.class"
        ));
        assert!(!zip_contains(&child_jar, "META-INF/MANIFEST.MF"));
        assert!(!zip_contains(&child_jar, "META-INF/MOJANG_C.SF"));
        assert!(!zip_contains(&child_jar, "META-INF/MOJANG_C.RSA"));
        let base_jar = base_dir.join(format!("{minecraft_version}.jar"));
        assert!(zip_contains(&base_jar, "META-INF/MANIFEST.MF"));
        assert!(zip_contains(&base_jar, "META-INF/MOJANG_C.SF"));
        assert!(zip_contains(&base_jar, "META-INF/MOJANG_C.RSA"));
        let forge_artifact = root
            .join("libraries")
            .join("net")
            .join("minecraftforge")
            .join("forge")
            .join("1.5.2-7.8.1.738")
            .join("forge-1.5.2-7.8.1.738-universal.jar");
        assert!(zip_contains(
            &forge_artifact,
            "net/minecraftforge/Forge.class"
        ));
        assert!(!zip_contains(&forge_artifact, "META-INF/MANIFEST.MF"));
        assert!(!zip_contains(&forge_artifact, "META-INF/FORGE.SF"));

        let child_jar_bytes = fs::read(&child_jar).expect("read child jar");
        let version_json = fs::read_to_string(
            versions_dir(&root)
                .join(&version_id)
                .join(format!("{version_id}.json")),
        )
        .expect("read version json");
        let version: serde_json::Value =
            serde_json::from_str(&version_json).expect("parse version json");
        assert_eq!(
            version["downloads"]["client"]["sha1"],
            sha1_hex(&child_jar_bytes)
        );
        assert_eq!(
            version["downloads"]["client"]["size"],
            child_jar_bytes.len() as i64
        );
        assert_eq!(version["downloads"]["client"]["url"], "");
        assert_eq!(version["assets"], "base-assets");
        assert_eq!(version["assetIndex"]["id"], "legacy");
        assert_eq!(version["javaVersion"]["component"], "jre-legacy");
        assert_eq!(version["javaVersion"]["majorVersion"], 8);
        assert_eq!(version["logging"]["client"]["argument"], "base-logging");
        assert_eq!(version["logging"]["client"]["file"]["id"], "base-log.xml");

        assert_eq!(installer_server.request_count(), 2);
        installer_server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_archive_overlays_base_client_without_mutating_base() {
        let root = temp_dir("legacy-archive-overlay");
        let base_version_id = "1.2.5";
        let base_dir = versions_dir(&root).join(base_version_id);
        fs::create_dir_all(&base_dir).expect("base version dir");
        fs::write(
            base_dir.join(format!("{base_version_id}.json")),
            format!(
                r#"{{
                    "id":"{base_version_id}",
                    "type":"release",
                    "mainClass":"net.minecraft.client.Minecraft",
                    "assetIndex":{{"id":"legacy","url":"","sha1":"","size":0,"totalSize":0}},
                    "libraries":[]
                }}"#
            ),
        )
        .expect("base json");
        fs::write(
            base_dir.join(format!("{base_version_id}.jar")),
            zip_entries(&[
                ("net/minecraft/client/Minecraft.class", b"base".as_slice()),
                ("com/example/Replaced.class", b"base".as_slice()),
            ]),
        )
        .expect("base jar");
        let forge_archive = zip_entries(&[
            ("net/minecraftforge/Forge.class", b"forge".as_slice()),
            ("com/example/Replaced.class", b"forge".as_slice()),
            ("META-INF/TEST.SF", b"signature".as_slice()),
        ]);
        let server = TestByteServer::start_with_sha1(forge_archive.clone());
        let mut record = legacy_archive_record();
        record.minecraft_version = base_version_id.to_string();
        canonicalize_record_identity(&mut record);
        record.install_source = LoaderInstallSource::LegacyArchive {
            url: server.url.clone(),
        };
        let plan = LoaderInstallPlan {
            record: record.clone(),
        };

        let archive_source =
            verified_test_source_for(&server.url, "legacy Forge archive", &record.version_id).await;
        let base_receipt = test_authenticated_receipt(&root, &record.minecraft_version);
        let receipt = install_legacy_archive_after_authenticated_base(
            &root,
            &plan,
            archive_source,
            &base_receipt,
            &mut |_progress| {},
        )
        .await
        .expect("install legacy archive");

        assert_eq!(receipt.version_id(), record.version_id);
        let installed_jar = versions_dir(&root)
            .join(&record.version_id)
            .join(format!("{}.jar", record.version_id));
        assert!(zip_contains(
            &installed_jar,
            "net/minecraft/client/Minecraft.class"
        ));
        assert!(zip_contains(
            &installed_jar,
            "net/minecraftforge/Forge.class"
        ));
        assert_eq!(
            zip_entry_bytes(&installed_jar, "com/example/Replaced.class"),
            b"forge"
        );
        assert!(!zip_contains(&installed_jar, "META-INF/TEST.SF"));
        let installed_jar_bytes = fs::read(&installed_jar).expect("read installed jar");
        let expected_child_bytes = overlay_legacy_archive_bytes(
            &fs::read(base_dir.join(format!("{base_version_id}.jar")))
                .expect("read authenticated base source"),
            &forge_archive,
        )
        .expect("derive expected child source");
        assert_eq!(installed_jar_bytes, expected_child_bytes);
        let installed_jar_receipt = receipt
            .into_activation_source()
            .into_parts()
            .1
            .entries()
            .iter()
            .find(|entry| entry.kind() == KnownGoodArtifactKind::ClientJar)
            .expect("client jar receipt")
            .integrity()
            .clone();
        let KnownGoodIntegrity::Sha1 { digest, size } = installed_jar_receipt else {
            panic!("client jar receipt must retain canonical source integrity");
        };
        assert_eq!(digest.as_str(), sha1_hex(&installed_jar_bytes));
        assert_eq!(size, installed_jar_bytes.len() as u64);
        let installed_version_json = fs::read_to_string(
            versions_dir(&root)
                .join(&record.version_id)
                .join(format!("{}.json", record.version_id)),
        )
        .expect("read installed version json");
        let installed_version: serde_json::Value =
            serde_json::from_str(&installed_version_json).expect("parse installed version json");
        assert_eq!(
            installed_version["downloads"]["client"]["sha1"],
            sha1_hex(&installed_jar_bytes)
        );
        assert_eq!(
            installed_version["downloads"]["client"]["size"],
            installed_jar_bytes.len() as i64
        );
        assert_eq!(installed_version["downloads"]["client"]["url"], "");

        let base_jar = base_dir.join(format!("{base_version_id}.jar"));
        assert!(zip_contains(
            &base_jar,
            "net/minecraft/client/Minecraft.class"
        ));
        assert!(!zip_contains(&base_jar, "net/minecraftforge/Forge.class"));
        assert_eq!(
            zip_entry_bytes(&base_jar, "com/example/Replaced.class"),
            b"base"
        );

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_archive_rejects_corrupt_authenticated_base_client() {
        let root = temp_dir("legacy-archive-corrupt-base");
        let base_version_id = "1.2.5";
        write_base_version(&root, base_version_id);
        let base_receipt = test_authenticated_receipt(&root, base_version_id);
        fs::write(
            versions_dir(&root)
                .join(base_version_id)
                .join(format!("{base_version_id}.jar")),
            b"corrupt base client",
        )
        .expect("corrupt base client");
        let server = TestByteServer::start_with_sha1(zip_entries(&[(
            "net/minecraftforge/Forge.class",
            b"forge".as_slice(),
        )]));
        let mut record = legacy_archive_record();
        record.install_source = LoaderInstallSource::LegacyArchive {
            url: server.url.clone(),
        };
        let plan = LoaderInstallPlan {
            record: record.clone(),
        };
        let archive_source =
            verified_test_source_for(&server.url, "legacy Forge archive", &record.version_id).await;

        let error = install_legacy_archive_after_authenticated_base(
            &root,
            &plan,
            archive_source,
            &base_receipt,
            &mut |_| {},
        )
        .await
        .expect_err("corrupt base must fail");

        assert!(
            matches!(error, LoaderError::Verify(message) if message.contains("authenticate base client"))
        );
        assert!(!versions_dir(&root).join(record.version_id).exists());
        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_archive_rejects_mismatched_live_sha1_proof() {
        let root = temp_dir("legacy-archive-sha1-mismatch");
        let archive = zip_entries(&[("net/minecraftforge/Forge.class", b"forge".as_slice())]);
        let server = TestByteServer::start_with_sha1_proof(archive, vec![b'0'; 40]);
        let mut record = legacy_archive_record();
        record.install_source = LoaderInstallSource::LegacyArchive {
            url: server.url.clone(),
        };
        let plan = LoaderInstallPlan {
            record: record.clone(),
        };
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let error = install_from_legacy_archive(&root, &runtime_cache, &plan, &mut |_| {})
            .await
            .expect_err("mismatched proof must fail");

        assert!(
            matches!(error, LoaderError::Verify(message) if message.contains("live sha1 proof"))
        );
        assert!(!root.exists());
        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_archive_rejects_malformed_live_sha1_proof() {
        let root = temp_dir("legacy-archive-sha1-malformed");
        let archive = zip_entries(&[("net/minecraftforge/Forge.class", b"forge".as_slice())]);
        let server = TestByteServer::start_with_sha1_proof(
            archive,
            b"not-a-strict-sha1 artifact.jar".to_vec(),
        );
        let mut record = legacy_archive_record();
        record.install_source = LoaderInstallSource::LegacyArchive {
            url: server.url.clone(),
        };
        let plan = LoaderInstallPlan {
            record: record.clone(),
        };
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");

        let error = install_from_legacy_archive(&root, &runtime_cache, &plan, &mut |_| {})
            .await
            .expect_err("malformed proof must fail");

        assert!(
            matches!(error, LoaderError::InvalidProfile(message) if message.contains("exactly one 40-hex digest"))
        );
        assert!(!root.exists());
        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn legacy_archive_rejects_symlinked_child_version_without_outside_write() {
        let root = temp_dir("legacy-archive-symlink-child");
        let outside = temp_dir("legacy-archive-symlink-outside");
        let sentinel = outside.join("sentinel");
        fs::create_dir_all(&outside).expect("outside dir");
        fs::write(&sentinel, b"untouched").expect("outside sentinel");
        let base_version_id = "1.2.5";
        write_base_version(&root, base_version_id);
        fs::write(
            versions_dir(&root)
                .join(base_version_id)
                .join(format!("{base_version_id}.jar")),
            zip_entries(&[("net/minecraft/client/Minecraft.class", b"base".as_slice())]),
        )
        .expect("valid base client");
        let base_receipt = test_authenticated_receipt(&root, base_version_id);
        let mut record = legacy_archive_record();
        let child_path = versions_dir(&root).join(&record.version_id);
        std::os::unix::fs::symlink(&outside, &child_path).expect("symlink child version");
        let server = TestByteServer::start_with_sha1(zip_entries(&[(
            "net/minecraftforge/Forge.class",
            b"forge".as_slice(),
        )]));
        record.install_source = LoaderInstallSource::LegacyArchive {
            url: server.url.clone(),
        };
        let plan = LoaderInstallPlan {
            record: record.clone(),
        };
        let archive_source =
            verified_test_source_for(&server.url, "legacy Forge archive", &record.version_id).await;

        let error = install_legacy_archive_after_authenticated_base(
            &root,
            &plan,
            archive_source,
            &base_receipt,
            &mut |_| {},
        )
        .await
        .expect_err("symlinked child must fail");

        assert!(matches!(error, LoaderError::Io(_) | LoaderError::Verify(_)));
        assert_eq!(fs::read(&sentinel).expect("read sentinel"), b"untouched");
        assert_eq!(fs::read_dir(&outside).expect("outside dir").count(), 1);
        server.stop();
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn legacy_overlay_rejects_declared_entry_expansion_over_limit() {
        let base_client =
            zip_entries(&[("net/minecraft/client/Minecraft.class", b"base".as_slice())]);
        let mut archive = zip_entries(&[("oversized.class", b"".as_slice())]);
        set_first_zip_entry_declared_size(
            &mut archive,
            u32::try_from(super::MAX_LEGACY_OVERLAY_ENTRY_BYTES + 1)
                .expect("test limit fits zip32"),
        );

        let error = overlay_legacy_archive_bytes(&base_client, &archive)
            .expect_err("declared expansion must be rejected before decompression");

        assert!(
            matches!(error, LoaderError::InvalidProfile(message) if message.contains("bounded output limits"))
        );
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("axial-{prefix}-{nanos:x}"))
    }

    #[cfg(unix)]
    fn shell_quote_path(path: &Path) -> String {
        format!(
            "'{}'",
            path.to_str()
                .expect("test path must be UTF-8")
                .replace('\'', "'\"'\"'")
        )
    }

    #[cfg(unix)]
    async fn wait_for_test_file(path: &Path) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !path.is_file() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("processor lifecycle marker");
    }

    #[cfg(unix)]
    fn read_test_pid(path: &Path) -> i32 {
        fs::read_to_string(path)
            .expect("read processor PID marker")
            .trim()
            .parse()
            .expect("parse processor PID marker")
    }

    #[cfg(unix)]
    fn read_processor_workspace_root(path: &Path) -> PathBuf {
        let root = PathBuf::from(
            fs::read_to_string(path)
                .expect("read processor workspace marker")
                .trim(),
        );
        root.parent()
            .and_then(Path::parent)
            .expect("processor stage root")
            .to_path_buf()
    }

    #[cfg(unix)]
    fn process_exists(raw_pid: i32) -> bool {
        let pid = rustix::process::Pid::from_raw(raw_pid).expect("positive processor PID");
        match rustix::process::test_kill_process(pid) {
            Ok(()) => true,
            Err(rustix::io::Errno::SRCH) => false,
            Err(error) => panic!("inspect processor PID: {error}"),
        }
    }

    #[cfg(unix)]
    async fn wait_for_process_and_workspace_cleanup(raw_pids: &[i32], workspace_root: &Path) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while raw_pids.iter().copied().any(process_exists) || workspace_root.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("processor tree and workspace cleanup");
    }

    fn installer_jar(version_id: &str) -> Vec<u8> {
        installer_jar_with_profile_json(&profile_json(version_id))
    }

    fn true_legacy_forge_installer_jar(record: &LoaderBuildRecord, strip_meta: bool) -> Vec<u8> {
        let upstream_id = format!(
            "{}-Forge{}",
            record.minecraft_version, record.loader_version
        );
        let coordinate = format!(
            "net.minecraftforge:minecraftforge:{}",
            record.loader_version
        );
        let file_name = format!(
            "minecraftforge-universal-{}-{}.jar",
            record.minecraft_version, record.loader_version
        );
        let install_profile = serde_json::to_vec(&serde_json::json!({
            "versionInfo": {
                "id": upstream_id,
                "mainClass": "net.minecraft.launchwrapper.Launch",
                "minecraftArguments": "${auth_player_name} ${auth_session}",
                "libraries": [{"name": coordinate}]
            },
            "install": {
                "path": coordinate,
                "filePath": file_name,
                "target": upstream_id,
                "minecraft": record.minecraft_version,
                "stripMeta": strip_meta
            }
        }))
        .expect("serialize true-legacy Forge install profile");
        let forge_jar = zip_entries(&[
            ("META-INF/MANIFEST.MF", b"forge manifest".as_slice()),
            ("META-INF/FORGE.SF", b"signature".as_slice()),
            ("net/minecraftforge/Forge.class", b"forge".as_slice()),
        ]);
        zip_entries(&[
            ("install_profile.json", install_profile.as_slice()),
            (file_name.as_str(), forge_jar.as_slice()),
        ])
    }

    fn modern_forge_installer_jar(
        record: &LoaderBuildRecord,
        library_url: Option<&str>,
    ) -> Vec<u8> {
        modern_forge_installer_jar_with_parent(record, &record.minecraft_version, library_url)
    }

    fn declarative_modern_forge_installer_jar(
        record: &LoaderBuildRecord,
        exact_url: &str,
        exact_bytes: &[u8],
        fresh_url: &str,
    ) -> Vec<u8> {
        use zip::write::SimpleFileOptions;

        let forge_version = format!("{}-{}", record.minecraft_version, record.loader_version);
        let root_coordinate = format!("net.minecraftforge:forge:{forge_version}:universal");
        let root_path = format!(
            "maven/net/minecraftforge/forge/{0}/forge-{0}-universal.jar",
            forge_version
        );
        let root = zip_entries(&[("net/minecraftforge/Forge.class", b"forge")]);
        let processor_only = zip_entries(&[("example/Processor.class", b"processor")]);
        let version_json = serde_json::to_vec(&serde_json::json!({
            "id": format!("{}-forge-{}", record.minecraft_version, record.loader_version),
            "inheritsFrom": record.minecraft_version,
            "type": "release",
            "mainClass": "cpw.mods.bootstraplauncher.BootstrapLauncher",
            "logging": {},
            "libraries": [
                {"name": root_coordinate},
                {
                    "name": "example:exact:1.0",
                    "downloads": {"artifact": {
                        "path": "example/exact/1.0/exact-1.0.jar",
                        "url": exact_url,
                        "sha1": sha1_hex(exact_bytes),
                        "size": exact_bytes.len()
                    }}
                },
                {
                    "name": "example:fresh:1.0",
                    "downloads": {"artifact": {
                        "path": "example/fresh/1.0/fresh-1.0.jar",
                        "url": fresh_url
                    }}
                }
            ]
        }))
        .expect("serialize declarative Forge version profile");
        let install_profile = serde_json::to_vec(&serde_json::json!({
            "spec": 1,
            "profile": "forge",
            "version": format!("{}-forge-{}", record.minecraft_version, record.loader_version),
            "path": format!("net.minecraftforge:forge:{forge_version}:shim"),
            "minecraft": record.minecraft_version,
            "processors": [],
            "libraries": [{
                "name": "example:processor-only:1.0",
                "sha1": sha1_hex(&processor_only),
                "size": processor_only.len()
            }]
        }))
        .expect("serialize declarative Forge install profile");
        let mut cursor = std::io::Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(&mut cursor);
        for (name, bytes) in [
            ("version.json".to_string(), version_json),
            ("install_profile.json".to_string(), install_profile),
            (root_path, root),
            (
                "maven/example/processor-only/1.0/processor-only-1.0.jar".to_string(),
                processor_only,
            ),
        ] {
            archive
                .start_file(name, SimpleFileOptions::default())
                .expect("start declarative Forge entry");
            archive
                .write_all(&bytes)
                .expect("write declarative Forge entry");
        }
        archive
            .finish()
            .expect("finish declarative Forge installer");
        cursor.into_inner()
    }

    fn modern_forge_installer_jar_with_parent(
        record: &LoaderBuildRecord,
        parent: &str,
        library_url: Option<&str>,
    ) -> Vec<u8> {
        use zip::write::SimpleFileOptions;

        let root_coordinate = format!(
            "net.minecraftforge:forge:{}-{}:universal",
            record.minecraft_version, record.loader_version
        );
        let mut library = serde_json::json!({"name": root_coordinate});
        if let Some(url) = library_url {
            library["url"] = serde_json::Value::String(url.to_string());
        }
        let version_json = serde_json::to_vec(&serde_json::json!({
            "id": format!("{}-forge-{}", record.minecraft_version, record.loader_version),
            "inheritsFrom": parent,
            "type": "release",
            "mainClass": "cpw.mods.bootstraplauncher.BootstrapLauncher",
            "logging": {},
            "libraries": [library]
        }))
        .expect("serialize Forge version profile");
        let processor_only = zip_entries(&[("example/Processor.class", b"processor")]);
        let install_profile = serde_json::to_vec(&serde_json::json!({
            "spec": 1,
            "profile": "forge",
            "version": format!("{}-forge-{}", record.minecraft_version, record.loader_version),
            "path": format!(
                "net.minecraftforge:forge:{}-{}:shim",
                record.minecraft_version, record.loader_version
            ),
            "minecraft": record.minecraft_version,
            "processors": [],
            "libraries": [{
                "name": "example:processor-only:1.0",
                "sha1": sha1_hex(&processor_only),
                "size": processor_only.len()
            }]
        }))
        .expect("serialize Forge install profile");

        let mut cursor = std::io::Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(&mut cursor);
        archive
            .start_file("version.json", SimpleFileOptions::default())
            .expect("start version profile");
        archive
            .write_all(&version_json)
            .expect("write version profile");
        archive
            .start_file("install_profile.json", SimpleFileOptions::default())
            .expect("start install profile");
        archive
            .write_all(&install_profile)
            .expect("write install profile");
        archive
            .start_file(
                "maven/example/processor-only/1.0/processor-only-1.0.jar",
                SimpleFileOptions::default(),
            )
            .expect("start embedded processor-only library");
        archive
            .write_all(&processor_only)
            .expect("write embedded processor-only library");
        if library_url.is_none() {
            let embedded = zip_entries(&[("net/minecraftforge/Forge.class", b"forge")]);
            archive
                .start_file(
                    format!(
                        "maven/net/minecraftforge/forge/{0}-{1}/forge-{0}-{1}-universal.jar",
                        record.minecraft_version, record.loader_version
                    ),
                    SimpleFileOptions::default(),
                )
                .expect("start embedded Forge root");
            archive
                .write_all(&embedded)
                .expect("write embedded Forge root");
        }
        archive.finish().expect("finish installer jar");
        cursor.into_inner()
    }

    fn unsupported_neoforge_installer_jar(record: &LoaderBuildRecord) -> Vec<u8> {
        let version_id = format!("neoforge-{}", record.loader_version);
        let version_json = serde_json::to_vec(&serde_json::json!({
            "id": version_id,
            "inheritsFrom": record.minecraft_version,
            "type": "release",
            "mainClass": "cpw.mods.bootstraplauncher.BootstrapLauncher",
            "logging": {},
            "libraries": [{
                "name": format!("net.neoforged:neoforge:{}:universal", record.loader_version)
            }]
        }))
        .expect("serialize NeoForge version profile");
        let install_profile = serde_json::to_vec(&serde_json::json!({
            "spec": 1,
            "profile": "NeoForge",
            "version": version_id,
            "minecraft": record.minecraft_version,
            "processors": [{
                "jar": "net.neoforged.installertools:installertools:2.1.3"
            }],
            "libraries": []
        }))
        .expect("serialize NeoForge install profile");
        zip_entries(&[
            ("version.json", version_json.as_slice()),
            ("install_profile.json", install_profile.as_slice()),
        ])
    }

    #[cfg(unix)]
    #[derive(Clone, Copy, Debug)]
    enum ProcessorFixtureShape {
        ForgeSpecZero,
        ForgeModern,
        NeoModern,
    }

    struct ProcessorFixtureLayout {
        version_id: String,
        profile: &'static str,
        spec: i32,
        root_coordinate: String,
        root_entry: String,
        root_bytes: Vec<u8>,
        terminal_coordinate: String,
        install_path: Option<String>,
        additional_install_library: Option<(String, String, Vec<u8>)>,
    }

    #[cfg(unix)]
    fn processor_fixture_record(shape: ProcessorFixtureShape) -> LoaderBuildRecord {
        let mut record = installer_record();
        match shape {
            ProcessorFixtureShape::ForgeSpecZero => {
                record.minecraft_version = "1.12.2".to_string();
                record.loader_version = "14.23.5.2859".to_string();
                record.strategy = LoaderInstallStrategy::ForgeLegacyInstaller;
            }
            ProcessorFixtureShape::ForgeModern => {}
            ProcessorFixtureShape::NeoModern => {
                record.component_id = LoaderComponentId::NeoForge;
                record.component_name = record.component_id.display_name().to_string();
                record.loader_version = "21.5.74".to_string();
                record.strategy = LoaderInstallStrategy::NeoForgeModern;
            }
        }
        canonicalize_record_identity(&mut record);
        record
    }

    fn processor_fixture_layout(record: &LoaderBuildRecord) -> ProcessorFixtureLayout {
        let root_bytes = zip_entries(&[("example/Root.class", b"root")]);
        match (record.component_id, record.strategy) {
            (LoaderComponentId::Forge, LoaderInstallStrategy::ForgeModern) => {
                let forge_version =
                    format!("{}-{}", record.minecraft_version, record.loader_version);
                let root_coordinate = format!("net.minecraftforge:forge:{forge_version}:universal");
                let shim_coordinate = format!("net.minecraftforge:forge:{forge_version}:shim");
                ProcessorFixtureLayout {
                    version_id: format!(
                        "{}-forge-{}",
                        record.minecraft_version, record.loader_version
                    ),
                    profile: "forge",
                    spec: 1,
                    root_coordinate,
                    root_entry: format!(
                        "maven/net/minecraftforge/forge/{forge_version}/forge-{forge_version}-universal.jar"
                    ),
                    root_bytes,
                    terminal_coordinate: format!("net.minecraftforge:forge:{forge_version}:client"),
                    install_path: Some(shim_coordinate.clone()),
                    additional_install_library: Some((
                        shim_coordinate,
                        format!(
                            "maven/net/minecraftforge/forge/{forge_version}/forge-{forge_version}-shim.jar"
                        ),
                        zip_entries(&[("example/Shim.class", b"shim")]),
                    )),
                }
            }
            (LoaderComponentId::Forge, LoaderInstallStrategy::ForgeLegacyInstaller) => {
                let forge_version =
                    format!("{}-{}", record.minecraft_version, record.loader_version);
                let root_coordinate = format!("net.minecraftforge:forge:{forge_version}");
                ProcessorFixtureLayout {
                    version_id: format!(
                        "{}-forge-{}",
                        record.minecraft_version, record.loader_version
                    ),
                    profile: "forge",
                    spec: 0,
                    root_coordinate: root_coordinate.clone(),
                    root_entry: format!(
                        "maven/net/minecraftforge/forge/{forge_version}/forge-{forge_version}.jar"
                    ),
                    root_bytes,
                    terminal_coordinate: format!("net.minecraftforge:forge:{forge_version}:client"),
                    install_path: Some(root_coordinate),
                    additional_install_library: None,
                }
            }
            (LoaderComponentId::NeoForge, LoaderInstallStrategy::NeoForgeModern) => {
                let root_coordinate =
                    format!("net.neoforged:neoforge:{}:universal", record.loader_version);
                ProcessorFixtureLayout {
                    version_id: format!("neoforge-{}", record.loader_version),
                    profile: "NeoForge",
                    spec: 1,
                    root_coordinate,
                    root_entry: format!(
                        "maven/net/neoforged/neoforge/{0}/neoforge-{0}-universal.jar",
                        record.loader_version
                    ),
                    root_bytes,
                    terminal_coordinate: format!(
                        "net.neoforged:neoforge:{}:client",
                        record.loader_version
                    ),
                    install_path: None,
                    additional_install_library: None,
                }
            }
            _ => panic!("unsupported processor fixture shape"),
        }
    }

    fn runnable_forge_installer_jar(record: &LoaderBuildRecord) -> Vec<u8> {
        single_step_processor_installer_jar(record)
    }

    fn single_step_processor_installer_jar(record: &LoaderBuildRecord) -> Vec<u8> {
        single_step_processor_installer_jar_with_libraries(record, Vec::new())
    }

    fn single_step_processor_installer_jar_with_libraries(
        record: &LoaderBuildRecord,
        extra_version_libraries: Vec<serde_json::Value>,
    ) -> Vec<u8> {
        let layout = processor_fixture_layout(record);
        processor_installer_jar(
            record,
            &layout,
            sha1_hex(TEST_PROCESSOR_TERMINAL_BYTES),
            extra_version_libraries,
            serde_json::json!({
                "PATCHED": {"client": format!("[{}]", layout.terminal_coordinate)},
                "PATCHED_SHA": {
                    "client": format!("'{}'", sha1_hex(TEST_PROCESSOR_TERMINAL_BYTES))
                }
            }),
            serde_json::json!([{
                "jar": TEST_PROCESSOR_COORDINATE,
                "args": ["single", "{PATCHED}"],
                "sides": ["client"],
                "outputs": {"{PATCHED}": "{PATCHED_SHA}"}
            }]),
        )
    }

    fn processor_installer_jar(
        record: &LoaderBuildRecord,
        layout: &ProcessorFixtureLayout,
        terminal_sha1: String,
        extra_version_libraries: Vec<serde_json::Value>,
        data: serde_json::Value,
        processors: serde_json::Value,
    ) -> Vec<u8> {
        use zip::write::SimpleFileOptions;

        let processor = zip_entries(&[
            ("META-INF/MANIFEST.MF", b"Main-Class: example.Processor\n\n"),
            ("example/Processor.class", b"processor"),
        ]);
        let mut version_libraries = vec![
            serde_json::json!({
                "name": layout.root_coordinate.clone(),
                "sha1": sha1_hex(&layout.root_bytes),
                "size": layout.root_bytes.len()
            }),
            serde_json::json!({
            "name": layout.terminal_coordinate.clone(),
            "sha1": terminal_sha1
            }),
        ];
        version_libraries.extend(extra_version_libraries);
        let version_json = serde_json::to_vec(&serde_json::json!({
            "id": layout.version_id.clone(),
            "inheritsFrom": record.minecraft_version,
            "type": "release",
            "mainClass": "cpw.mods.bootstraplauncher.BootstrapLauncher",
            "logging": {},
            "libraries": version_libraries
        }))
        .expect("serialize processor version profile");
        let mut install_libraries = vec![
            serde_json::json!({
                "name": layout.root_coordinate.clone(),
                "sha1": sha1_hex(&layout.root_bytes),
                "size": layout.root_bytes.len()
            }),
            serde_json::json!({
                "name": TEST_PROCESSOR_COORDINATE,
                "sha1": sha1_hex(&processor),
                "size": processor.len()
            }),
        ];
        if let Some((coordinate, _, bytes)) = &layout.additional_install_library {
            install_libraries.push(serde_json::json!({
                "name": coordinate,
                "sha1": sha1_hex(bytes),
                "size": bytes.len()
            }));
        }
        let mut install_profile = serde_json::json!({
            "spec": layout.spec,
            "profile": layout.profile,
            "version": layout.version_id.clone(),
            "minecraft": record.minecraft_version,
            "libraries": install_libraries,
            "data": data,
            "processors": processors
        });
        if let Some(path) = &layout.install_path {
            install_profile["path"] = path.clone().into();
        }
        let install_profile =
            serde_json::to_vec(&install_profile).expect("serialize processor install profile");
        let mut cursor = std::io::Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(&mut cursor);
        let mut entries = vec![
            ("version.json".to_string(), version_json),
            ("install_profile.json".to_string(), install_profile),
            (layout.root_entry.clone(), layout.root_bytes.clone()),
            ("maven/x/p/1/p-1.jar".to_string(), processor),
        ];
        if let Some((_, path, bytes)) = &layout.additional_install_library {
            entries.push((path.clone(), bytes.clone()));
        }
        for (name, bytes) in entries {
            archive
                .start_file(name, SimpleFileOptions::default())
                .expect("start processor fixture entry");
            archive
                .write_all(&bytes)
                .expect("write processor fixture entry");
        }
        archive.finish().expect("finish processor installer");
        cursor.into_inner()
    }

    #[cfg(unix)]
    struct TestProcessorRuntime {
        descriptor: TestRuntimeSourceDescriptor,
        manifest_server: TestByteServer,
        file_server: TestByteServer,
    }

    #[cfg(unix)]
    impl TestProcessorRuntime {
        fn start() -> Self {
            let fake_java = br#"#!/bin/sh
case "$*" in
  *-version*) printf '%s\n' 'openjdk version "17.0.1"' >&2; exit 0 ;;
esac
case "$4" in
  single) printf '%s' 'processor-terminal' > "$5" ;;
  step-one) cat "$5" "$6" > "$7" ;;
  step-two) cat "$5" > "$6" ;;
  *) exit 9 ;;
esac
"#
            .to_vec();
            let file_server = TestByteServer::start(fake_java.clone());
            let manifest_bytes = serde_json::to_vec(&serde_json::json!({
                "files": {
                    "bin": {"type": "directory"},
                    "bin/java": {
                        "type": "file",
                        "executable": true,
                        "downloads": {"raw": {
                            "url": file_server.url.clone(),
                            "sha1": sha1_hex(&fake_java),
                            "size": fake_java.len()
                        }}
                    }
                }
            }))
            .expect("runtime manifest");
            let manifest_server = TestByteServer::start(manifest_bytes.clone());
            let descriptor = TestRuntimeSourceDescriptor {
                component: RuntimeId::from("java-runtime-delta"),
                url: manifest_server.url.clone(),
                sha1: sha1_hex(&manifest_bytes),
                size: manifest_bytes.len() as u64,
            };
            Self {
                descriptor,
                manifest_server,
                file_server,
            }
        }

        fn stop(self) {
            self.manifest_server.stop();
            self.file_server.stop();
        }
    }

    #[cfg(unix)]
    async fn install_test_processor_base(
        root: &Path,
        record: &LoaderBuildRecord,
        runtime: &TestRuntimeSourceDescriptor,
    ) -> (
        KnownGoodInstallReceipt,
        VersionManifest,
        TestByteServer,
        TestByteServer,
    ) {
        let base_client = zip_entries(&[("net/minecraft/client/Main.class", b"base")]);
        let client_server = TestByteServer::start(base_client.clone());
        let mut version: serde_json::Value = serde_json::from_slice(&vanilla_version_bytes(
            &record.minecraft_version,
            &client_server.url,
            &base_client,
        ))
        .expect("base version");
        version["javaVersion"] = serde_json::json!({
            "component": "java-runtime-delta",
            "majorVersion": 17
        });
        let version_bytes = serde_json::to_vec(&version).expect("base version bytes");
        let version_server = TestByteServer::start(version_bytes.clone());
        let manifest = test_install_manifest(
            &record.minecraft_version,
            &version_server.url,
            &version_bytes,
        );
        let receipt = Downloader::with_test_install_manifest(root, manifest.clone())
            .with_test_runtime_source(runtime.clone())
            .install_version(&record.minecraft_version, |_| {})
            .await
            .expect("install processor fixture base");
        (receipt, manifest, client_server, version_server)
    }

    #[cfg(unix)]
    async fn finish_test_processor_installer_with_runtime(
        root: &Path,
        plan: &LoaderInstallPlan,
        installer_plan: BoundForgeInstallerPlan,
        base_receipt: KnownGoodInstallReceipt,
        runtime: &TestRuntimeSourceDescriptor,
    ) -> KnownGoodInstallReceipt {
        let execution = materialize_test_installer_network(
            root,
            installer_plan,
            &mut |_progress: DownloadProgress| {},
        )
        .await;
        let BoundForgeInstallExecution::Run(execution) = execution else {
            panic!("processor fixture must retain executable work");
        };
        let base_client_bytes =
            read_installed_base_client(root, &base_receipt).expect("authenticated base client");
        let runtime_source =
            acquire_test_runtime_source(&base_receipt.effective_version().java_version, runtime)
                .await
                .expect("authenticated processor runtime source");
        let processor_sources = AuthenticatedProcessorSources::from_installed(
            base_receipt.effective_version().clone(),
            base_client_bytes,
            runtime_source,
            ManagedDir::open_root(root).expect("managed install root"),
        )
        .expect("authenticated installed processor sources");
        let result = spawn_bound_processor_execution(
            *execution,
            plan.record.version_id.clone(),
            plan.record.minecraft_version.clone(),
            processor_sources,
        )
        .finish(|_| {})
        .await
        .expect("execute installed processor graph");
        let (base_client_bytes, _runtime_source) = result
            .sources
            .into_installed_parts()
            .expect("recover installed processor sources");
        let receipt_input = result
            .continuation
            .into_observed_receipt_input(result.outputs)
            .expect("seal installed processor outputs");
        let mut version = super::compose_loader_version(
            base_receipt.effective_version(),
            &plan.record.minecraft_version,
            &plan.record.version_id,
            receipt_input.version(),
        )
        .expect("compose installed processor version");
        let child_client = receipt_input
            .derive_child_client_bytes(&base_client_bytes)
            .expect("derive installed processor client");
        let client = version
            .downloads
            .client
            .as_mut()
            .expect("installed processor client declaration");
        client.sha1 = sha1_hex(child_client.bytes());
        client.size =
            i64::try_from(child_client.bytes().len()).expect("installed processor client size");
        client.url.clear();
        let version_bytes =
            serde_json::to_vec_pretty(&version).expect("serialize installed processor version");
        let pending = KnownGoodInstallReceipt::from_verified_installer_source(
            base_receipt,
            &plan.record,
            receipt_input,
            version,
            &version_bytes,
            &base_client_bytes,
            &child_client,
        )
        .expect("derive installed processor receipt");
        let (pending, retained_sources) = pending.into_publications();
        let materialized = materialize_retained_installer_libraries(root, retained_sources)
            .await
            .expect("publish installed processor outputs");
        pending
            .complete(materialized)
            .expect("complete installed processor receipt")
    }

    #[cfg(unix)]
    async fn assert_processor_reconstruction_parity(shape: ProcessorFixtureShape) {
        let root = temp_dir(&format!("processor-parity-{shape:?}"));
        let runtime = TestProcessorRuntime::start();
        let mut record = processor_fixture_record(shape);
        let (base_receipt, manifest, client_server, version_server) =
            install_test_processor_base(&root, &record, &runtime.descriptor).await;
        let installer_server =
            TestByteServer::start_with_sha1(single_step_processor_installer_jar(&record));
        record.install_source = LoaderInstallSource::InstallerJar {
            url: installer_server.url.clone(),
        };
        let plan = LoaderInstallPlan {
            record: record.clone(),
        };
        let installer_source = verified_test_source_for(
            &installer_server.url,
            "loader installer",
            &record.version_id,
        )
        .await;
        let install_receipt = finish_test_processor_installer_with_runtime(
            &root,
            &plan,
            bind_test_installer(installer_source, &record),
            base_receipt,
            &runtime.descriptor,
        )
        .await;
        seed_reconstruction_sentinels(&root);
        let before = snapshot_tree(&root);
        let installer_path = reqwest::Url::parse(&installer_server.url)
            .expect("installer URL")
            .path()
            .to_string();
        let installer_sidecar_path = format!("{installer_path}.sha1");
        let counts = (
            version_server.request_count(),
            client_server.request_count(),
            installer_server.request_count_for(&installer_path),
            installer_server.request_count_for(&installer_sidecar_path),
            runtime.manifest_server.request_count(),
            runtime.file_server.request_count(),
        );
        let reconstructed = reconstruct_installer_with_downloader(
            &plan,
            &Downloader::with_test_install_manifest(&root, manifest)
                .with_test_runtime_source(runtime.descriptor.clone()),
        )
        .await
        .expect("reconstruct processor fixture");

        assert_eq!(snapshot_tree(&root), before);
        assert_eq!(version_server.request_count(), counts.0 + 1);
        assert_eq!(client_server.request_count(), counts.1 + 1);
        assert_eq!(
            installer_server.request_count_for(&installer_path),
            counts.2 + 1
        );
        assert_eq!(
            installer_server.request_count_for(&installer_sidecar_path),
            counts.3 + 1
        );
        assert_eq!(runtime.manifest_server.request_count(), counts.4 + 1);
        assert_eq!(runtime.file_server.request_count(), counts.5 + 1);
        let installed = install_receipt.into_activation_source().into_parts();
        let reconstructed = reconstructed.into_activation_source().into_parts();
        assert_eq!(installed, reconstructed);
        let terminal_path =
            crate::launch::maven_to_path(&processor_fixture_layout(&record).terminal_coordinate)
                .to_string_lossy()
                .replace('\\', "/");
        let terminal = reconstructed
            .1
            .entries()
            .iter()
            .find(|entry| entry.path().as_str() == terminal_path)
            .expect("observed processor terminal");
        assert!(matches!(
            terminal.integrity(),
            KnownGoodIntegrity::Sha1 { digest, size }
                if digest.as_str() == sha1_hex(TEST_PROCESSOR_TERMINAL_BYTES)
                    && *size == TEST_PROCESSOR_TERMINAL_BYTES.len() as u64
        ));

        client_server.stop();
        version_server.stop();
        installer_server.stop();
        runtime.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    async fn assert_two_step_processor_source_union() {
        let root = temp_dir("processor-two-step-source-union");
        let runtime = TestProcessorRuntime::start();
        let mut record = processor_fixture_record(ProcessorFixtureShape::ForgeModern);
        let (base_receipt, manifest, client_server, version_server) =
            install_test_processor_base(&root, &record, &runtime.descriptor).await;
        let exact_input = zip_entries(&[("x/ExactInput.class", b"exact-input")]);
        let exact_non_input = zip_entries(&[("x/ExactNonInput.class", b"exact-non-input")]);
        let fresh_input = zip_entries(&[("x/FreshInput.class", b"fresh-input")]);
        let fresh_non_input = zip_entries(&[("x/FreshNonInput.class", b"fresh-non-input")]);
        let terminal_bytes = [exact_input.as_slice(), fresh_input.as_slice()].concat();
        let intermediate_sha1 = sha1_hex(&terminal_bytes);
        let terminal_sha1 = intermediate_sha1.clone();
        let exact_input_server = TestByteServer::start(exact_input.clone());
        let exact_non_input_server = TestByteServer::start(exact_non_input.clone());
        let fresh_input_server = TestByteServer::start(fresh_input.clone());
        let fresh_non_input_server = TestByteServer::start(fresh_non_input.clone());
        let mut layout = processor_fixture_layout(&record);
        layout.terminal_coordinate = "x:t:1".to_string();
        let installer = processor_installer_jar(
            &record,
            &layout,
            terminal_sha1.clone(),
            vec![
                serde_json::json!({
                    "name": "x:e:1",
                    "url": exact_input_server.url.clone(),
                    "sha1": sha1_hex(&exact_input),
                    "size": exact_input.len()
                }),
                serde_json::json!({
                    "name": "x:n:1",
                    "url": exact_non_input_server.url.clone(),
                    "sha1": sha1_hex(&exact_non_input),
                    "size": exact_non_input.len()
                }),
                serde_json::json!({
                    "name": "x:f:1",
                    "url": fresh_input_server.url.clone(),
                    "sha1": sha1_hex(&fresh_input)
                }),
                serde_json::json!({
                    "name": "x:g:1",
                    "url": fresh_non_input_server.url.clone()
                }),
            ],
            serde_json::json!({}),
            serde_json::json!([
                {
                    "jar": TEST_PROCESSOR_COORDINATE,
                    "args": [
                        "step-one",
                        "[x:e:1]",
                        "[x:f:1]",
                        "[x:i:1]"
                    ],
                    "outputs": {"[x:i:1]": format!("'{intermediate_sha1}'")}
                },
                {
                    "jar": TEST_PROCESSOR_COORDINATE,
                    "args": ["step-two", "[x:i:1]", "[x:t:1]"],
                    "outputs": {"[x:t:1]": format!("'{terminal_sha1}'")}
                }
            ]),
        );
        let installer_server = TestByteServer::start_with_sha1(installer);
        record.install_source = LoaderInstallSource::InstallerJar {
            url: installer_server.url.clone(),
        };
        let plan = LoaderInstallPlan {
            record: record.clone(),
        };
        let installer_source = verified_test_source_for(
            &installer_server.url,
            "loader installer",
            &record.version_id,
        )
        .await;
        let install_receipt = finish_test_processor_installer_with_runtime(
            &root,
            &plan,
            bind_test_installer(installer_source, &record),
            base_receipt,
            &runtime.descriptor,
        )
        .await;
        assert_eq!(exact_input_server.request_count(), 1);
        assert_eq!(exact_non_input_server.request_count(), 1);
        assert_eq!(fresh_input_server.request_count(), 1);
        assert_eq!(fresh_non_input_server.request_count(), 1);
        seed_reconstruction_sentinels(&root);
        let before = snapshot_tree(&root);
        let installer_path = reqwest::Url::parse(&installer_server.url)
            .expect("installer URL")
            .path()
            .to_string();
        let installer_sidecar_path = format!("{installer_path}.sha1");
        let fixed_counts = (
            version_server.request_count(),
            client_server.request_count(),
            installer_server.request_count_for(&installer_path),
            installer_server.request_count_for(&installer_sidecar_path),
            runtime.manifest_server.request_count(),
            runtime.file_server.request_count(),
        );
        let reconstructed = reconstruct_installer_with_downloader(
            &plan,
            &Downloader::with_test_install_manifest(&root, manifest)
                .with_test_runtime_source(runtime.descriptor.clone()),
        )
        .await
        .expect("reconstruct two-step processor graph");

        assert_eq!(snapshot_tree(&root), before);
        assert_eq!(exact_input_server.request_count(), 2);
        assert_eq!(exact_non_input_server.request_count(), 1);
        assert_eq!(fresh_input_server.request_count(), 2);
        assert_eq!(fresh_non_input_server.request_count(), 2);
        assert_eq!(version_server.request_count(), fixed_counts.0 + 1);
        assert_eq!(client_server.request_count(), fixed_counts.1 + 1);
        assert_eq!(
            installer_server.request_count_for(&installer_path),
            fixed_counts.2 + 1
        );
        assert_eq!(
            installer_server.request_count_for(&installer_sidecar_path),
            fixed_counts.3 + 1
        );
        assert_eq!(runtime.manifest_server.request_count(), fixed_counts.4 + 1);
        assert_eq!(runtime.file_server.request_count(), fixed_counts.5 + 1);
        let installed = install_receipt.into_activation_source().into_parts();
        let reconstructed = reconstructed.into_activation_source().into_parts();
        assert_eq!(installed, reconstructed);
        let terminal_path = crate::launch::maven_to_path(&layout.terminal_coordinate)
            .to_string_lossy()
            .replace('\\', "/");
        let intermediate_path = crate::launch::maven_to_path("x:i:1")
            .to_string_lossy()
            .replace('\\', "/");
        assert!(
            reconstructed
                .1
                .entries()
                .iter()
                .all(|entry| entry.path().as_str() != intermediate_path)
        );
        let terminal = reconstructed
            .1
            .entries()
            .iter()
            .find(|entry| entry.path().as_str() == terminal_path)
            .expect("two-step terminal output");
        assert!(matches!(
            terminal.integrity(),
            KnownGoodIntegrity::Sha1 { digest, size }
                if digest.as_str() == terminal_sha1 && *size == terminal_bytes.len() as u64
        ));

        for server in [
            client_server,
            version_server,
            installer_server,
            exact_input_server,
            exact_non_input_server,
            fresh_input_server,
            fresh_non_input_server,
        ] {
            server.stop();
        }
        runtime.stop();
        let _ = fs::remove_dir_all(root);
    }

    fn bind_test_installer(
        source: VerifiedLoaderSource,
        record: &LoaderBuildRecord,
    ) -> BoundForgeInstallerPlan {
        let authenticated =
            plan_authenticated_installer(source).expect("authenticated installer plan");
        bind_authenticated_installer_plan(authenticated, record).expect("bound installer plan")
    }

    async fn finish_test_installer(
        root: &std::path::Path,
        plan: &LoaderInstallPlan,
        installer_plan: BoundForgeInstallerPlan,
        send: &mut impl FnMut(DownloadProgress),
    ) -> KnownGoodInstallReceipt {
        let execution = materialize_test_installer_network(root, installer_plan, send).await;
        finish_supported_installer_install(
            root,
            plan,
            execution,
            test_authenticated_receipt(root, &plan.record.minecraft_version),
            send,
        )
        .await
        .expect("finish installer install")
    }

    async fn materialize_test_installer_network(
        root: &std::path::Path,
        installer_plan: BoundForgeInstallerPlan,
        send: &mut impl FnMut(DownloadProgress),
    ) -> BoundForgeInstallExecution {
        let execution = installer_plan
            .into_install_execution()
            .expect("installer execution");
        let network_install = execution
            .into_network_install(&crate::paths::libraries_dir(root))
            .expect("classified installer network");
        let (pending, materialized) = download_installer_libraries_with_evidence(
            root,
            network_install,
            "loader_libraries",
            send,
        )
        .await
        .expect("materialized installer network");
        pending
            .complete_network(materialized)
            .expect("completed installer network")
    }

    fn installer_jar_with_profile_json(profile_json: &[u8]) -> Vec<u8> {
        use zip::write::SimpleFileOptions;

        let mut cursor = std::io::Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(&mut cursor);
        archive
            .start_file("version.json", SimpleFileOptions::default())
            .expect("start version.json");
        archive.write_all(profile_json).expect("write version.json");
        archive.finish().expect("finish installer jar");
        cursor.into_inner()
    }

    fn zip_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use zip::write::SimpleFileOptions;

        let mut cursor = std::io::Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(&mut cursor);
        for (name, bytes) in entries {
            archive
                .start_file(name, SimpleFileOptions::default())
                .expect("start zip entry");
            archive.write_all(bytes).expect("write zip entry");
        }
        archive.finish().expect("finish zip");
        cursor.into_inner()
    }

    fn set_first_zip_entry_declared_size(bytes: &mut [u8], size: u32) {
        let local = bytes
            .windows(4)
            .position(|window| window == [0x50, 0x4b, 0x03, 0x04])
            .expect("local zip header");
        let central = bytes
            .windows(4)
            .position(|window| window == [0x50, 0x4b, 0x01, 0x02])
            .expect("central zip header");
        bytes[local + 22..local + 26].copy_from_slice(&size.to_le_bytes());
        bytes[central + 24..central + 28].copy_from_slice(&size.to_le_bytes());
    }

    fn zip_contains(path: &std::path::Path, name: &str) -> bool {
        let file = fs::File::open(path).expect("open zip");
        let mut archive = zip::ZipArchive::new(file).expect("read zip");
        archive.by_name(name).is_ok()
    }

    fn zip_entry_bytes(path: &std::path::Path, name: &str) -> Vec<u8> {
        let file = fs::File::open(path).expect("open zip");
        let mut archive = zip::ZipArchive::new(file).expect("read zip");
        let mut entry = archive.by_name(name).expect("zip entry");
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).expect("read zip entry");
        bytes
    }

    fn profile_json(id: &str) -> Vec<u8> {
        format!(r#"{{"id":"{id}","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient"}}"#)
            .into_bytes()
    }

    fn write_base_version(root: &std::path::Path, version_id: &str) {
        let version_dir = versions_dir(root).join(version_id);
        fs::create_dir_all(&version_dir).expect("create base version dir");
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            format!(
                r#"{{
                    "id":"{version_id}",
                    "type":"release",
                    "mainClass":"net.minecraft.client.main.Main",
                    "assetIndex":{{"id":"{version_id}","url":"","sha1":"","size":0,"totalSize":0}},
                    "libraries":[]
                }}"#
            ),
        )
        .expect("write base version json");
        fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
            .expect("write base jar");
    }

    fn test_client_integrity(root: &std::path::Path, version_id: &str) -> ExpectedIntegrity {
        let client = fs::read(
            versions_dir(root)
                .join(version_id)
                .join(format!("{version_id}.jar")),
        )
        .expect("read test base client");
        ExpectedIntegrity {
            size: Some(client.len() as u64),
            sha1: Some(sha1_hex(&client)),
        }
    }

    async fn verified_test_source(url: &str, label: &'static str) -> VerifiedLoaderSource {
        fetch_sha1_verified_source(url, super::MAX_LOADER_SOURCE_BYTES, label, label)
            .await
            .expect("verified test source")
    }

    async fn verified_test_source_for(
        url: &str,
        label: &'static str,
        logical_identity: &str,
    ) -> VerifiedLoaderSource {
        fetch_sha1_verified_source(url, super::MAX_LOADER_SOURCE_BYTES, label, logical_identity)
            .await
            .expect("verified test source")
    }

    fn test_authenticated_receipt(
        root: &std::path::Path,
        version_id: &str,
    ) -> KnownGoodInstallReceipt {
        let integrity = test_client_integrity(root, version_id);
        let mut version = resolve_version(root, version_id).expect("resolve test base version");
        let client = version.downloads.client.get_or_insert_default();
        client.size = integrity.size.expect("test client size") as i64;
        client.sha1 = integrity.sha1.expect("test client sha1");
        client.url = "https://example.invalid/client.jar".to_string();
        KnownGoodInstallReceipt::from_test_authenticated_version(version, default_environment())
    }

    fn assert_backend_version_was_written(
        root: &std::path::Path,
        backend_version_id: &str,
        upstream_version_id: &str,
    ) {
        let backend_dir = versions_dir(root).join(backend_version_id);
        let upstream_dir = versions_dir(root).join(upstream_version_id);
        assert!(backend_dir.is_dir());
        assert!(!upstream_dir.exists());
        let version_json = fs::read(backend_dir.join(format!("{backend_version_id}.json")))
            .expect("read backend version json");
        let version: serde_json::Value =
            serde_json::from_slice(&version_json).expect("parse backend version json");
        assert_eq!(
            version.get("id").and_then(serde_json::Value::as_str),
            Some(backend_version_id)
        );
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha1::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    fn vanilla_version_bytes(id: &str, client_url: &str, client_bytes: &[u8]) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": id,
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "downloads": {
                "client": {
                    "url": client_url,
                    "sha1": sha1_hex(client_bytes),
                    "size": client_bytes.len()
                }
            },
            "libraries": []
        }))
        .expect("serialize vanilla version")
    }

    fn vanilla_version_bytes_with_exact_library(
        id: &str,
        client_url: &str,
        client_bytes: &[u8],
        library_url: &str,
        library_bytes: &[u8],
    ) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": id,
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "downloads": {
                "client": {
                    "url": client_url,
                    "sha1": sha1_hex(client_bytes),
                    "size": client_bytes.len()
                }
            },
            "libraries": [{
                "name": "org.example:vanilla-exact:1.0",
                "downloads": {"artifact": {
                    "path": "org/example/vanilla-exact/1.0/vanilla-exact-1.0.jar",
                    "url": library_url,
                    "sha1": sha1_hex(library_bytes),
                    "size": library_bytes.len()
                }}
            }]
        }))
        .expect("serialize vanilla version with exact library")
    }

    fn test_install_manifest(id: &str, version_url: &str, version_bytes: &[u8]) -> VersionManifest {
        serde_json::from_value(serde_json::json!({
            "latest": { "release": id, "snapshot": id },
            "versions": [{
                "id": id,
                "type": "release",
                "url": version_url,
                "sha1": sha1_hex(version_bytes),
                "complianceLevel": 1
            }]
        }))
        .expect("valid test install manifest")
    }

    fn profile_reconstruction_sources(
        record: &LoaderBuildRecord,
        incomplete_url: &str,
        exact_url: &str,
        exact_bytes: &[u8],
        native_url: &str,
        extra_url: &str,
    ) -> (Vec<u8>, Vec<u8>, (usize, usize, usize)) {
        let intermediary = format!("net.fabricmc:intermediary:{}", record.minecraft_version);
        let intermediary_path = format!(
            "net/fabricmc/intermediary/{0}/intermediary-{0}.jar",
            record.minecraft_version
        );
        let environment = default_environment();
        let native_classifier = match environment.os_arch.as_str() {
            "x86_64" => "natives-64".to_string(),
            "x86" => "natives-32".to_string(),
            "arm64" => "natives-arm64".to_string(),
            other => format!("natives-{other}"),
        };
        let native_path =
            format!("org/example/profile-native/1.0/profile-native-1.0-{native_classifier}.jar");
        let native_library = serde_json::json!({
            "name": "org.example:profile-native:1.0",
            "natives": {environment.os_name: "natives-${arch}"},
            "downloads": {"classifiers": {
                native_classifier: {
                    "path": native_path,
                    "url": native_url
                }
            }}
        });
        let extra_library = serde_json::json!({
            "name": "org.example:profile-extra:1.0",
            "downloads": {"artifact": {
                "path": "org/example/profile-extra/1.0/profile-extra-1.0.jar",
                "url": extra_url
            }}
        });
        match record.component_id {
            LoaderComponentId::Fabric => {
                let loader = format!("net.fabricmc:fabric-loader:{}", record.loader_version);
                let profile = serde_json::json!({
                    "id": format!(
                        "fabric-loader-{}-{}",
                        record.loader_version, record.minecraft_version
                    ),
                    "inheritsFrom": record.minecraft_version,
                    "type": "release",
                    "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
                    "libraries": [
                        {
                            "name": loader,
                            "downloads": {"artifact": {
                                "path": format!(
                                    "net/fabricmc/fabric-loader/{0}/fabric-loader-{0}.jar",
                                    record.loader_version
                                ),
                                "url": incomplete_url
                            }}
                        },
                        {
                            "name": intermediary,
                            "downloads": {"artifact": {
                                "path": intermediary_path,
                                "url": incomplete_url
                            }}
                        },
                        native_library,
                        extra_library
                    ]
                });
                let proof = serde_json::json!({
                    "loader": {
                        "version": record.loader_version,
                        "maven": format!(
                            "net.fabricmc:fabric-loader:{}",
                            record.loader_version
                        )
                    },
                    "intermediary": {
                        "version": record.minecraft_version,
                        "maven": format!(
                            "net.fabricmc:intermediary:{}",
                            record.minecraft_version
                        )
                    },
                    "launcherMeta": {"mainClass": {
                        "client": "net.fabricmc.loader.impl.launch.knot.KnotClient"
                    }}
                });
                (
                    serde_json::to_vec(&profile).expect("Fabric profile"),
                    serde_json::to_vec(&proof).expect("Fabric proof"),
                    (2, 1, 1),
                )
            }
            LoaderComponentId::Quilt => {
                let loader = format!("org.quiltmc:quilt-loader:{}", record.loader_version);
                let hashed = format!("org.quiltmc:hashed:{}", record.minecraft_version);
                let exact_sha1 = sha1_hex(exact_bytes);
                let profile = serde_json::json!({
                    "id": format!(
                        "quilt-loader-{}-{}",
                        record.loader_version, record.minecraft_version
                    ),
                    "inheritsFrom": record.minecraft_version,
                    "type": "release",
                    "mainClass": "org.quiltmc.loader.impl.launch.knot.KnotClient",
                    "libraries": [
                        {
                            "name": loader,
                            "downloads": {"artifact": {
                                "path": format!(
                                    "org/quiltmc/quilt-loader/{0}/quilt-loader-{0}.jar",
                                    record.loader_version
                                ),
                                "url": exact_url
                            }}
                        },
                        {
                            "name": hashed,
                            "downloads": {"artifact": {
                                "path": format!(
                                    "org/quiltmc/hashed/{0}/hashed-{0}.jar",
                                    record.minecraft_version
                                ),
                                "url": exact_url
                            }}
                        },
                        {
                            "name": intermediary,
                            "downloads": {"artifact": {
                                "path": intermediary_path,
                                "url": incomplete_url
                            }}
                        },
                        native_library,
                        extra_library
                    ]
                });
                let proof = serde_json::json!({
                    "loader": {
                        "version": record.loader_version,
                        "maven": format!(
                            "org.quiltmc:quilt-loader:{}",
                            record.loader_version
                        ),
                        "file_size": exact_bytes.len(),
                        "hashes": {"sha1": exact_sha1}
                    },
                    "hashed": {
                        "version": record.minecraft_version,
                        "maven": format!(
                            "org.quiltmc:hashed:{}",
                            record.minecraft_version
                        ),
                        "file_size": exact_bytes.len(),
                        "hashes": {"sha1": sha1_hex(exact_bytes)}
                    },
                    "intermediary": {
                        "version": record.minecraft_version,
                        "maven": format!(
                            "net.fabricmc:intermediary:{}",
                            record.minecraft_version
                        )
                    },
                    "launcherMeta": {"mainClass": {
                        "client": "org.quiltmc.loader.impl.launch.knot.KnotClient"
                    }}
                });
                (
                    serde_json::to_vec(&profile).expect("Quilt profile"),
                    serde_json::to_vec(&proof).expect("Quilt proof"),
                    (1, 1, 1),
                )
            }
            _ => panic!("profile fixture requires Fabric or Quilt"),
        }
    }

    fn seed_reconstruction_sentinels(root: &Path) {
        for (relative, bytes) in [
            (
                "assets/indexes/reconstruction-sentinel.json",
                b"asset-index".as_slice(),
            ),
            (
                "assets/objects/aa/reconstruction-sentinel",
                b"asset-object".as_slice(),
            ),
            (
                "assets/log_configs/reconstruction.xml",
                b"log-config".as_slice(),
            ),
            (
                "libraries/reconstruction/exact.jar",
                b"exact-library".as_slice(),
            ),
            (
                "libraries/reconstruction/fresh.jar",
                b"fresh-library".as_slice(),
            ),
            (
                "libraries/reconstruction/native.jar",
                b"native-library".as_slice(),
            ),
            (
                "libraries/reconstruction/extra.jar",
                b"extra-library".as_slice(),
            ),
            (
                "runtime/reconstruction/.axial-ready",
                b"runtime-ready".as_slice(),
            ),
            (
                "runtime/reconstruction/manifest.json",
                b"runtime-manifest".as_slice(),
            ),
            (
                "runtime/reconstruction/bin/java",
                b"runtime-executable".as_slice(),
            ),
            (
                "runtime/reconstruction/lib/runtime.bin",
                b"runtime-file".as_slice(),
            ),
            (
                "cache/version_manifest_v2.json",
                b"manifest-cache".as_slice(),
            ),
            (
                "cache/loaders/catalog/reconstruction.json",
                b"catalog".as_slice(),
            ),
            (
                "state/known-good/reconstruction.json",
                b"known-good".as_slice(),
            ),
            (
                "versions/reconstruction-sentinel/.incomplete",
                b"marker".as_slice(),
            ),
            ("launcher_profiles.json", b"launcher-profile".as_slice()),
        ] {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().expect("sentinel parent"))
                .expect("create sentinel parent");
            fs::write(path, bytes).expect("write reconstruction sentinel");
        }
        #[cfg(unix)]
        {
            let link = root.join("runtime/reconstruction/lib/runtime-link");
            std::os::unix::fs::symlink("runtime.bin", link).expect("runtime sentinel symlink");
        }
        #[cfg(windows)]
        {
            let link = root.join("runtime/reconstruction/lib/runtime-link");
            fs::write(link, b"runtime-link-surrogate").expect("runtime link sentinel");
        }
    }

    fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(root: &Path, path: &Path, snapshot: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let metadata = fs::symlink_metadata(path).expect("snapshot metadata");
            let relative = path.strip_prefix(root).expect("snapshot relative path");
            if !relative.as_os_str().is_empty() {
                let entry = if metadata.is_dir() {
                    vec![b'd']
                } else if metadata.is_file() {
                    let mut entry = vec![b'f'];
                    entry.extend(fs::read(path).expect("snapshot file"));
                    entry
                } else if metadata.file_type().is_symlink() {
                    let mut entry = vec![b'l'];
                    entry.extend(
                        fs::read_link(path)
                            .expect("snapshot symlink")
                            .to_string_lossy()
                            .as_bytes(),
                    );
                    entry
                } else {
                    vec![b'o']
                };
                snapshot.insert(relative.to_path_buf(), entry);
            }
            if metadata.is_dir() {
                let mut children = fs::read_dir(path)
                    .expect("snapshot directory")
                    .map(|entry| entry.expect("snapshot entry").path())
                    .collect::<Vec<_>>();
                children.sort();
                for child in children {
                    visit(root, &child, snapshot);
                }
            }
        }

        let mut snapshot = BTreeMap::new();
        if root.exists() {
            visit(root, root, &mut snapshot);
        }
        snapshot
    }

    fn profile_record() -> LoaderBuildRecord {
        let component_id = LoaderComponentId::Fabric;
        let minecraft_version = "1.21.5";
        let loader_version = "0.16.14";
        LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id,
            component_name: "Fabric".to_string(),
            build_id: build_id_for(component_id, minecraft_version, loader_version),
            minecraft_version: minecraft_version.to_string(),
            loader_version: loader_version.to_string(),
            version_id: installed_version_id_for(component_id, minecraft_version, loader_version)
                .expect("canonical installed version id"),
            build_meta: LoaderBuildMetadata::default(),
            strategy: LoaderInstallStrategy::FabricProfile,
            artifact_kind: LoaderArtifactKind::ProfileJson,
            installability: LoaderInstallability::Installable,
            install_source: LoaderInstallSource::ProfileJson {
                url: "https://meta.fabricmc.net/profile/json".to_string(),
            },
        }
    }

    fn fabric_profile_proof(record: &LoaderBuildRecord) -> ProfileInstallProof {
        ProfileInstallProof::from_test(
            format!(
                "fabric-loader-{}-{}",
                record.loader_version, record.minecraft_version
            ),
            record.minecraft_version.clone(),
            "net.fabricmc.loader.impl.launch.knot.KnotClient".to_string(),
            vec![
                ProfileLibraryProof::from_test(
                    format!("net.fabricmc:fabric-loader:{}", record.loader_version),
                    None,
                    None,
                ),
                ProfileLibraryProof::from_test(
                    format!("net.fabricmc:intermediary:{}", record.minecraft_version),
                    None,
                    None,
                ),
            ],
        )
    }

    fn fabric_profile_fragment(record: &LoaderBuildRecord) -> LoaderProfileFragment {
        LoaderProfileFragment {
            id: format!(
                "fabric-loader-{}-{}",
                record.loader_version, record.minecraft_version
            ),
            inherits_from: record.minecraft_version.clone(),
            kind: "release".to_string(),
            main_class: "net.fabricmc.loader.impl.launch.knot.KnotClient".to_string(),
            libraries: vec![
                Library {
                    name: format!("net.fabricmc:fabric-loader:{}", record.loader_version),
                    ..Library::default()
                },
                Library {
                    name: format!("net.fabricmc:intermediary:{}", record.minecraft_version),
                    ..Library::default()
                },
            ],
            ..LoaderProfileFragment::default()
        }
    }

    fn installer_record() -> LoaderBuildRecord {
        let mut record = profile_record();
        record.component_id = LoaderComponentId::Forge;
        record.component_name = "Forge".to_string();
        record.loader_version = "55.0.0".to_string();
        canonicalize_record_identity(&mut record);
        record.strategy = LoaderInstallStrategy::ForgeModern;
        record.artifact_kind = LoaderArtifactKind::InstallerJar;
        record.install_source = LoaderInstallSource::InstallerJar {
            url: "https://example.test/installer.jar".to_string(),
        };
        record
    }

    fn legacy_archive_record() -> LoaderBuildRecord {
        let mut record = profile_record();
        record.component_id = LoaderComponentId::Forge;
        record.component_name = "Forge".to_string();
        record.minecraft_version = "1.2.5".to_string();
        record.loader_version = "3.4.9.171".to_string();
        canonicalize_record_identity(&mut record);
        record.strategy = LoaderInstallStrategy::ForgeEarliestLegacy;
        record.artifact_kind = LoaderArtifactKind::LegacyArchive;
        record.install_source = LoaderInstallSource::LegacyArchive {
            url: "https://example.test/legacy.jar".to_string(),
        };
        record
    }

    fn canonicalize_record_identity(record: &mut LoaderBuildRecord) {
        record.build_id = build_id_for(
            record.component_id,
            &record.minecraft_version,
            &record.loader_version,
        );
        record.version_id = installed_version_id_for(
            record.component_id,
            &record.minecraft_version,
            &record.loader_version,
        )
        .expect("canonical installed version id");
    }

    struct TestByteServer {
        url: String,
        request_count: Arc<AtomicUsize>,
        request_paths: Arc<Mutex<Vec<String>>>,
        stop_server: mpsc::Sender<()>,
        server: thread::JoinHandle<()>,
    }

    impl TestByteServer {
        fn start(body: Vec<u8>) -> Self {
            Self::start_with_optional_sha1(body, None)
        }

        fn start_with_sha1(body: Vec<u8>) -> Self {
            let proof = sha1_hex(&body).into_bytes();
            Self::start_with_optional_sha1(body, Some(proof))
        }

        fn start_with_sha1_proof(body: Vec<u8>, proof: Vec<u8>) -> Self {
            Self::start_with_optional_sha1(body, Some(proof))
        }

        fn start_with_optional_sha1(body: Vec<u8>, sha1_proof: Option<Vec<u8>>) -> Self {
            Self::start_with_status(body, sha1_proof, "200 OK")
        }

        fn start_not_found() -> Self {
            Self::start_with_status(b"missing".to_vec(), None, "404 Not Found")
        }

        fn start_with_status(
            body: Vec<u8>,
            sha1_proof: Option<Vec<u8>>,
            status: &'static str,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            listener
                .set_nonblocking(true)
                .expect("set test server nonblocking");
            let url = format!(
                "http://{}/legacy-client.zip",
                listener.local_addr().expect("server addr")
            );
            let request_count = Arc::new(AtomicUsize::new(0));
            let server_request_count = Arc::clone(&request_count);
            let request_paths = Arc::new(Mutex::new(Vec::new()));
            let server_request_paths = Arc::clone(&request_paths);
            let (stop_server, server_stopped) = mpsc::channel();
            let server = thread::spawn(move || {
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            server_request_count.fetch_add(1, Ordering::SeqCst);
                            let path = respond(stream, status, &body, sha1_proof.as_deref());
                            server_request_paths
                                .lock()
                                .expect("record request path")
                                .push(path);
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            if server_stopped.try_recv().is_ok() {
                                break;
                            }
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept connection: {error}"),
                    }
                }
            });

            Self {
                url,
                request_count,
                request_paths,
                stop_server,
                server,
            }
        }

        fn request_count(&self) -> usize {
            self.request_count.load(Ordering::SeqCst)
        }

        fn request_count_for(&self, path: &str) -> usize {
            self.request_paths
                .lock()
                .expect("read request paths")
                .iter()
                .filter(|requested| requested.as_str() == path)
                .count()
        }

        fn stop(self) {
            self.stop_server.send(()).expect("stop test server");
            self.server.join().expect("server thread");
        }
    }

    fn respond(
        mut stream: TcpStream,
        status: &str,
        body: &[u8],
        sha1_proof: Option<&[u8]>,
    ) -> String {
        let mut buffer = [0_u8; 1024];
        let read = stream.read(&mut buffer).unwrap_or_default();
        let request = String::from_utf8_lossy(&buffer[..read]);
        let request_path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_string();
        let body = if request_path.ends_with(".sha1") {
            sha1_proof.unwrap_or(body)
        } else {
            body
        };
        let header = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(header.as_bytes())
            .expect("write response header");
        stream.write_all(body).expect("write response body");
        request_path
    }
}

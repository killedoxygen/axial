use super::{
    InstanceWriteOperation,
    create_cache::{cached_source_rows, invalidate_create_view_source, store_source_rows},
    create_policy::{
        LoaderBuildSelectionError, evaluate_create_view_loader_version_policies,
        loader_build_is_known_incompatible_default, loader_build_is_unstable_default,
        loader_catalog_is_stale, loader_version_policy_inputs, no_compatible_stable_loader_message,
        preferred_loader_build, select_preferred_loader_build,
        stale_loader_version_catalog_message,
    },
    enrich_instance_for_scan, instance_write_error_response,
};
use crate::application::install::InstallQueueInstallItemViewModel;
use crate::application::timing::{
    CreateInstanceTiming, CreateViewTiming, trace_create_instance, trace_create_view,
};
use crate::application::version::{
    VERSION_SCAN_DEGRADED_MESSAGE, installed_versions_scan, version_scan_degraded_response,
};
use crate::application::{
    CommandResult, CommandResultCarriers, CreateInstancePayload, InstallQueueRequest,
    InstallQueueStateResponse, enqueue_install_owned, loader_error_response,
};
use crate::guardian::{
    GuardianJvmPresetOption, GuardianJvmPresetResolution, guardian_jvm_preset_options,
    normalize_create_jvm_preset,
};
use crate::observability::telemetry::TelemetryEvent;
use crate::state::contracts::{CommandKind, OperationId, OperationStatus};
use crate::state::{
    AppState, InstalledVersionsLookup, ProducerLease, RequestProducerHandoff, new_instance,
};
use axial_config::{EnrichedInstance, Instance, generate_instance_id};
use axial_launcher::{
    GuardianMode, LaunchReadinessReasonId, LaunchReadinessRequest, LaunchReadinessSeverity,
    inspect_launch_readiness,
};
use axial_minecraft::{
    LifecycleChannel, LifecycleMeta, LoaderAvailability, LoaderBuildRecord, LoaderCatalogState,
    LoaderComponentId, LoaderInstallability, MinecraftVersionMeta, VersionEntry,
    analyze_minecraft_version, fetch_builds, fetch_components, fetch_supported_versions,
    fetch_version_manifest_cached, manifest_release_references, parse_build_id,
};
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    ops::Deref,
    path::Path,
    time::{Duration, Instant},
};

#[derive(Debug, Default, Deserialize)]
pub(crate) struct CreateInstanceRequest {
    pub name: String,
    #[serde(default)]
    pub selection_id: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub accent: String,
    #[serde(default)]
    pub art_seed: Option<u32>,
    #[serde(default)]
    pub max_memory_mb: Option<i32>,
    #[serde(default)]
    pub min_memory_mb: Option<i32>,
    #[serde(default)]
    pub window_width: Option<i32>,
    #[serde(default)]
    pub window_height: Option<i32>,
    #[serde(default)]
    pub jvm_preset_id: Option<String>,
    #[serde(default)]
    pub auto_optimize: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CreateInstanceSourceOptionViewModel {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CreateInstanceVersionRowViewModel {
    pub source_id: String,
    pub selection_id: String,
    pub minecraft_version_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader_build: Option<CreateLoaderBuildIdentityViewModel>,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    pub channel: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<CreateVersionTagViewModel>,
    pub download_state: String,
    pub create_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CreateVersionTagViewModel {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CreateLoaderBuildIdentityViewModel {
    pub component_id: LoaderComponentId,
    pub build_id: String,
    pub target_version_id: String,
    pub minecraft_version_id: String,
    pub loader_version: String,
    pub installability: LoaderInstallability,
    pub availability: LoaderAvailability,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CreateInstanceDefaultsViewModel {
    pub source_id: String,
    pub channel_id: String,
    pub jvm_preset_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_mb: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_width: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_height: Option<i32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CreateInstanceNoticeViewModel {
    pub state_id: String,
    pub tone: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CreateOptimizeOptionViewModel {
    pub id: String,
    pub label: String,
    pub detail: String,
    pub default_enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CreateLoaderAutoOptionViewModel {
    pub selection_id: String,
    pub label: String,
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CreateLoaderBuildOptionViewModel {
    pub selection_id: String,
    pub build_id: String,
    pub label: String,
    pub channel_id: String,
    pub channel_label: String,
    pub recommended: bool,
    pub installed: bool,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CreateLoaderBuildsViewResponse {
    pub source_id: String,
    pub minecraft_version_id: String,
    pub auto: CreateLoaderAutoOptionViewModel,
    pub builds: Vec<CreateLoaderBuildOptionViewModel>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CreateInstanceViewResponse {
    pub sources: Vec<CreateInstanceSourceOptionViewModel>,
    pub channels: Vec<CreateInstanceSourceOptionViewModel>,
    pub versions: Vec<CreateInstanceVersionRowViewModel>,
    pub preset_options: Vec<GuardianJvmPresetOption>,
    pub optimize_option: CreateOptimizeOptionViewModel,
    pub defaults: CreateInstanceDefaultsViewModel,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<CreateInstanceNoticeViewModel>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CreateStaticVersionRow {
    source_id: String,
    selection_id: String,
    minecraft_version_id: String,
    loader_build: Option<CreateLoaderBuildIdentityViewModel>,
    display_name: String,
    hint: Option<String>,
    channel: String,
    tags: Vec<CreateVersionTagViewModel>,
    disabled_reason: Option<String>,
    fresh_catalog_required: bool,
}

#[derive(Debug)]
struct CreateVersionRowsResult {
    rows: Vec<CreateInstanceVersionRowViewModel>,
    scan_elapsed: Duration,
    catalog_elapsed: Duration,
    policy_elapsed: Duration,
    source_cache_hit: bool,
    scan_source: &'static str,
    refresh_count: u32,
}

impl CreateVersionRowsResult {
    fn empty() -> Self {
        Self {
            rows: Vec::new(),
            scan_elapsed: Duration::ZERO,
            catalog_elapsed: Duration::ZERO,
            policy_elapsed: Duration::ZERO,
            source_cache_hit: false,
            scan_source: "none",
            refresh_count: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CreateInstanceResultViewModel {
    pub state_id: String,
    pub tone: String,
    pub title: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CreateQueuedInstallSummary {
    pub state_id: String,
    pub kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CreateGuardianNotice {
    pub state_id: String,
    pub tone: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CreateInstanceResponse {
    #[serde(flatten)]
    pub instance: EnrichedInstance,
    pub result: CommandResult<CreateInstancePayload>,
    pub view_model: CreateInstanceResultViewModel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_queue: Option<InstallQueueStateResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_install: Option<CreateQueuedInstallSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guardian_notice: Option<CreateGuardianNotice>,
}

impl Deref for CreateInstanceResponse {
    type Target = EnrichedInstance;

    fn deref(&self) -> &Self::Target {
        &self.instance
    }
}

pub(crate) async fn handle_create_instance_view(
    state: &AppState,
    producer: &ProducerLease,
    source_id: Option<&str>,
) -> CreateInstanceViewResponse {
    let started_at = Instant::now();
    let mut notices = Vec::new();
    if state.library_dir().is_none() {
        notices.push(CreateInstanceNoticeViewModel {
            state_id: "library_unconfigured".to_string(),
            tone: "warn".to_string(),
            message: "Library is not configured".to_string(),
            detail: Some(
                "Choose a library folder before creating instances that need downloads."
                    .to_string(),
            ),
        });
    }
    let requested_source = normalize_create_view_source(source_id);
    let row_result =
        create_version_rows(state, producer, requested_source.as_deref(), &mut notices).await;
    let versions = row_result.rows;
    let unavailable_sources = unavailable_source_ids(&notices);
    trace_create_view(CreateViewTiming {
        source_id: requested_source.as_deref().unwrap_or("vanilla"),
        total: started_at.elapsed(),
        scan: row_result.scan_elapsed,
        catalog: row_result.catalog_elapsed,
        policy: row_result.policy_elapsed,
        version_count: versions.len(),
        source_cache_hit: row_result.source_cache_hit,
        scan_source: row_result.scan_source,
        refresh_count: row_result.refresh_count,
    });

    CreateInstanceViewResponse {
        sources: create_source_options(&unavailable_sources),
        channels: create_channel_options(),
        versions,
        preset_options: guardian_jvm_preset_options(),
        optimize_option: create_optimize_option(),
        defaults: CreateInstanceDefaultsViewModel {
            source_id: "vanilla".to_string(),
            channel_id: "release".to_string(),
            jvm_preset_id: String::new(),
            max_memory_mb: None,
            window_width: None,
            window_height: None,
        },
        notices,
    }
}

fn create_optimize_option() -> CreateOptimizeOptionViewModel {
    CreateOptimizeOptionViewModel {
        id: "auto_optimize".to_string(),
        label: "Auto-optimize".to_string(),
        detail: "Axial tunes this instance's performance while you play.".to_string(),
        default_enabled: true,
    }
}

pub(crate) async fn handle_create_loader_builds_view(
    state: &AppState,
    producer: &ProducerLease,
    source_id: &str,
    minecraft_version: &str,
) -> Result<CreateLoaderBuildsViewResponse, (StatusCode, Json<serde_json::Value>)> {
    let component_id = LoaderComponentId::parse(source_id.trim()).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "unknown loader component" })),
        )
    })?;
    let minecraft_version = minecraft_version.trim();
    if minecraft_version.is_empty() {
        return Err(bad_create_request("minecraft_version is required"));
    }
    let installed_lookup = state
        .installed_versions_snapshot(producer)
        .await
        .ok_or_else(library_not_configured_response)?;
    let library_dir = installed_lookup.library_dir().to_path_buf();
    let (builds, catalog) = fetch_builds(library_dir.as_path(), component_id, minecraft_version)
        .await
        .map_err(loader_error_response)?;
    let installed_scan = installed_versions_scan(&installed_lookup.snapshot);
    if installed_scan.is_degraded() {
        return Err(version_scan_degraded_response());
    }
    let installed_keys = installed_loader_build_keys(&installed_scan.versions);
    let recommended_build_id = preferred_loader_build(builds.clone()).map(|build| build.build_id);
    let catalog_stale = loader_catalog_is_stale(&catalog);

    let build_options = builds
        .into_iter()
        .filter(|build| build.component_id == component_id)
        .map(|build| {
            let installed = installed_keys.contains(&loader_build_install_key(&build));
            let disabled_reason = if loader_build_is_known_incompatible_default(&build) {
                Some(format!(
                    "This {} build is known to be incompatible with Minecraft {}.",
                    component_id.display_name(),
                    build.minecraft_version
                ))
            } else if catalog_stale && !installed {
                Some(stale_loader_version_catalog_message())
            } else {
                None
            };
            let beta = loader_build_is_unstable_default(&build);
            let recommended = recommended_build_id.as_deref() == Some(build.build_id.as_str());
            CreateLoaderBuildOptionViewModel {
                selection_id: format!("loader_build|{}|{}", component_id.as_str(), build.build_id),
                build_id: build.build_id,
                label: build.loader_version,
                channel_id: if beta { "beta" } else { "stable" }.to_string(),
                channel_label: if beta { "Beta" } else { "Stable" }.to_string(),
                recommended,
                installed,
                enabled: disabled_reason.is_none(),
                disabled_reason,
            }
        })
        .collect();

    Ok(CreateLoaderBuildsViewResponse {
        source_id: component_id.as_str().to_string(),
        minecraft_version_id: minecraft_version.to_string(),
        auto: CreateLoaderAutoOptionViewModel {
            selection_id: format!(
                "loader_version|{}|{}",
                component_id.as_str(),
                minecraft_version
            ),
            label: "Automatic".to_string(),
            detail: format!(
                "Axial picks the newest stable {} build.",
                component_id.display_name()
            ),
        },
        builds: build_options,
    })
}

fn normalize_create_view_source(source_id: Option<&str>) -> Option<String> {
    let source_id = source_id.map(str::trim).filter(|value| !value.is_empty())?;
    if source_id == "vanilla" {
        return Some("vanilla".to_string());
    }
    let component_id = LoaderComponentId::parse(source_id)?;
    Some(component_id.as_str().to_string())
}

pub(crate) async fn handle_create_instance_owned(
    state: &AppState,
    payload: CreateInstanceRequest,
    handoff: RequestProducerHandoff,
) -> Result<CreateInstanceResponse, (StatusCode, Json<serde_json::Value>)> {
    let started_at = Instant::now();
    let producer = handoff
        .try_claim()
        .map_err(create_shutdown_error_response)?;
    let installed_lookup = state
        .installed_versions_snapshot(&producer)
        .await
        .ok_or_else(library_not_configured_response)?;
    let installed_scan = installed_versions_scan(&installed_lookup.snapshot);
    if installed_scan.is_degraded() {
        return Err(version_scan_degraded_response());
    }
    let selection =
        resolve_create_selection(&installed_lookup, &installed_scan.versions, &payload).await?;
    let preset = normalize_create_jvm_preset(payload.jvm_preset_id.as_deref());
    let mc_dir = Some(installed_lookup.library_dir().to_path_buf());
    let install_request = create_install_queue_request_if_needed(
        state,
        &installed_lookup,
        &installed_scan,
        &selection,
    )?;
    let queued_install_request = install_request.clone();
    let instance = build_created_instance(&payload, &selection, &preset)?;
    let instance = state
        .create_instance(instance, mc_dir)
        .await
        .map_err(|error| instance_write_error_response(InstanceWriteOperation::Create, error))?;
    let created_instance_id = instance.id.clone();
    let install_queue = queue_create_install_or_rollback_owned(
        state,
        &created_instance_id,
        install_request,
        handoff,
    )
    .await?;
    let enriched = enrich_instance_for_scan(
        state,
        instance,
        &installed_scan,
        Some(installed_lookup.library_dir()),
    );
    state
        .telemetry()
        .emit(TelemetryEvent::instance_created(Some(
            enriched.version_display.loader_key.clone(),
        )));
    let queued_install = install_queue.as_ref().and_then(|response| {
        queued_install_request
            .as_ref()
            .and_then(|request| create_queued_install_summary(response, request))
    });
    trace_create_instance(CreateInstanceTiming {
        total: started_at.elapsed(),
        version_count: installed_scan.versions.len(),
        scan_source: installed_lookup.source.as_str(),
        refresh_count: installed_lookup.refresh_count,
        queued_install: queued_install.is_some(),
    });

    Ok(create_instance_response(
        enriched,
        install_queue,
        queued_install,
        create_guardian_notice(&preset),
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CreateSelection {
    Vanilla {
        version_id: String,
    },
    Loader {
        component_id: LoaderComponentId,
        build_id: String,
        target_version_id: String,
    },
}

impl CreateSelection {
    fn target_version_id(&self) -> &str {
        match self {
            Self::Vanilla { version_id } => version_id,
            Self::Loader {
                target_version_id, ..
            } => target_version_id,
        }
    }

    fn install_queue_request(&self) -> Option<InstallQueueRequest> {
        match self {
            Self::Vanilla { version_id } => Some(InstallQueueRequest {
                kind: "vanilla".to_string(),
                version_id: version_id.clone(),
                manifest_url: String::new(),
                component_id: String::new(),
                build_id: String::new(),
            }),
            Self::Loader {
                component_id,
                build_id,
                ..
            } => Some(InstallQueueRequest {
                kind: "loader".to_string(),
                version_id: String::new(),
                manifest_url: String::new(),
                component_id: component_id.as_str().to_string(),
                build_id: build_id.clone(),
            }),
        }
    }
}

async fn resolve_create_selection(
    installed_lookup: &InstalledVersionsLookup,
    installed_versions: &[VersionEntry],
    payload: &CreateInstanceRequest,
) -> Result<CreateSelection, (StatusCode, Json<serde_json::Value>)> {
    let selection_id = payload.selection_id.trim();
    if selection_id.is_empty() {
        return Err(bad_create_request("selection_id is required"));
    }

    let mut parts = selection_id.splitn(3, '|');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("vanilla"), Some(version_id), None) if !version_id.trim().is_empty() => {
            resolve_vanilla_create_selection(installed_lookup, version_id.trim()).await
        }
        (Some("loader_version"), Some(component_id), Some(minecraft_version))
            if !minecraft_version.trim().is_empty() =>
        {
            let component_id = LoaderComponentId::parse(component_id.trim()).ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "unknown loader component" })),
                )
            })?;
            resolve_loader_create_selection(
                installed_lookup,
                installed_versions,
                component_id,
                LoaderCreateRequest::PreferredForMinecraft(minecraft_version.trim()),
            )
            .await
        }
        (Some("loader_build"), Some(component_id), Some(build_id))
            if !build_id.trim().is_empty() =>
        {
            let component_id = LoaderComponentId::parse(component_id.trim()).ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "unknown loader component" })),
                )
            })?;
            resolve_loader_create_selection(
                installed_lookup,
                installed_versions,
                component_id,
                LoaderCreateRequest::ExactBuild(build_id.trim()),
            )
            .await
        }
        _ => Err(bad_create_request("invalid create selection")),
    }
}

enum LoaderCreateRequest<'a> {
    PreferredForMinecraft(&'a str),
    ExactBuild(&'a str),
}

async fn resolve_loader_create_selection(
    installed_lookup: &InstalledVersionsLookup,
    installed_versions: &[VersionEntry],
    component_id: LoaderComponentId,
    request: LoaderCreateRequest<'_>,
) -> Result<CreateSelection, (StatusCode, Json<serde_json::Value>)> {
    let (minecraft_version, exact_build_id) = match request {
        LoaderCreateRequest::PreferredForMinecraft(minecraft_version) => {
            (minecraft_version.to_string(), None)
        }
        LoaderCreateRequest::ExactBuild(build_id) => {
            let Some((parsed_component_id, minecraft_version, _loader_version)) =
                parse_build_id(build_id)
            else {
                return Err(bad_create_request("invalid create selection"));
            };
            if parsed_component_id != component_id {
                return Err(bad_create_request("invalid create selection"));
            }
            (minecraft_version, Some(build_id))
        }
    };
    let library_dir = installed_lookup.library_dir();
    let (builds, catalog) = fetch_builds(library_dir, component_id, &minecraft_version)
        .await
        .map_err(loader_error_response)?;
    invalidate_create_view_source(library_dir, component_id.as_str());

    if let Some(build_id) = exact_build_id {
        return resolve_loader_create_selection_from_build_catalog(
            component_id,
            build_id,
            builds,
            &catalog,
            installed_versions,
        );
    }

    let build = select_preferred_loader_build(component_id, builds).map_err(|error| match error {
        LoaderBuildSelectionError::NoBuildAvailable => (
            StatusCode::NOT_FOUND,
            Json(
                serde_json::json!({ "error": "No stable loader build is available for this Minecraft version." }),
            ),
        ),
        LoaderBuildSelectionError::NoCompatibleDefault { component_id } => {
            no_compatible_stable_loader_response(component_id)
        }
    })?;
    if loader_build_is_known_incompatible_default(&build) {
        return Err(no_compatible_stable_loader_response(component_id));
    }
    let exact_installed = exact_loader_build_is_installed(installed_versions, &build);
    if loader_catalog_is_stale(&catalog) && !exact_installed {
        return Err(stale_loader_catalog_response());
    }

    Ok(CreateSelection::Loader {
        component_id: build.component_id,
        build_id: build.build_id,
        target_version_id: build.version_id,
    })
}

pub(super) fn resolve_loader_create_selection_from_build_catalog(
    component_id: LoaderComponentId,
    build_id: &str,
    builds: Vec<LoaderBuildRecord>,
    catalog: &LoaderCatalogState,
    installed_versions: &[VersionEntry],
) -> Result<CreateSelection, (StatusCode, Json<serde_json::Value>)> {
    let build = builds
        .into_iter()
        .find(|build| build.component_id == component_id && build.build_id == build_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Selected loader build is not available." })),
            )
        })?;
    let exact_installed = exact_loader_build_is_installed(installed_versions, &build);
    if loader_build_is_known_incompatible_default(&build) {
        return Err(no_compatible_stable_loader_response(component_id));
    }
    if loader_catalog_is_stale(catalog) && !exact_installed {
        return Err(stale_loader_catalog_response());
    }

    Ok(CreateSelection::Loader {
        component_id: build.component_id,
        build_id: build.build_id,
        target_version_id: build.version_id,
    })
}

async fn resolve_vanilla_create_selection(
    installed_lookup: &InstalledVersionsLookup,
    version_id: &str,
) -> Result<CreateSelection, (StatusCode, Json<serde_json::Value>)> {
    let manifest = fetch_version_manifest_cached(installed_lookup.library_dir())
        .await
        .map_err(|_| minecraft_versions_unavailable_response())?;
    let Some(version) = manifest
        .versions
        .into_iter()
        .find(|version| version.id == version_id)
    else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Minecraft version is unavailable" })),
        ));
    };

    Ok(CreateSelection::Vanilla {
        version_id: version.id,
    })
}

fn build_created_instance(
    payload: &CreateInstanceRequest,
    selection: &CreateSelection,
    preset: &GuardianJvmPresetResolution,
) -> Result<Instance, (StatusCode, Json<serde_json::Value>)> {
    let name = payload.name.trim();
    if name.is_empty() {
        return Err(bad_create_request("instance name is required"));
    }
    let mut instance = new_instance(
        generate_instance_id(),
        name.to_string(),
        selection.target_version_id().to_string(),
        payload.icon.clone(),
        payload.accent.clone(),
    );
    if let Some(art_seed) = payload.art_seed {
        instance.art_seed = art_seed;
    }
    if let Some(max_memory_mb) = payload.max_memory_mb {
        instance.max_memory_mb = max_memory_mb.max(0);
    }
    if let Some(min_memory_mb) = payload.min_memory_mb {
        instance.min_memory_mb = min_memory_mb.max(0);
    }
    if let Some(window_width) = payload.window_width {
        instance.window_width = window_width.max(0);
    }
    if let Some(window_height) = payload.window_height {
        instance.window_height = window_height.max(0);
    }
    instance.jvm_preset = preset.stored_preset.clone();
    if let Some(auto_optimize) = payload.auto_optimize {
        instance.auto_optimize = auto_optimize;
    }
    Ok(instance)
}

fn create_install_queue_request_if_needed(
    state: &AppState,
    installed_lookup: &InstalledVersionsLookup,
    installed_scan: &crate::application::version::InstalledVersionsScan,
    selection: &CreateSelection,
) -> Result<Option<InstallQueueRequest>, (StatusCode, Json<serde_json::Value>)> {
    let Some(request) = selection.install_queue_request() else {
        return Ok(None);
    };
    if version_is_launch_ready_or_user_blocked(
        state,
        installed_lookup,
        installed_scan,
        selection.target_version_id(),
    )? {
        return Ok(None);
    }

    Ok(Some(request))
}

async fn queue_create_install_request(
    state: &AppState,
    request: Option<InstallQueueRequest>,
    handoff: RequestProducerHandoff,
) -> Result<Option<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    let Some(request) = request else {
        return Ok(None);
    };

    enqueue_install_owned(state, request, handoff)
        .await
        .map(Some)
}

pub(super) async fn queue_create_install_or_rollback_owned(
    state: &AppState,
    instance_id: &str,
    request: Option<InstallQueueRequest>,
    handoff: RequestProducerHandoff,
) -> Result<Option<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    match queue_create_install_request(state, request, handoff).await {
        Ok(install_queue) => Ok(install_queue),
        Err(error) => {
            rollback_created_instance(state, instance_id)
                .await
                .map_err(|rollback_error| {
                    instance_write_error_response(InstanceWriteOperation::Create, rollback_error)
                })?;
            Err(error)
        }
    }
}

#[cfg(test)]
pub(crate) async fn handle_create_instance(
    state: &AppState,
    payload: CreateInstanceRequest,
) -> Result<CreateInstanceResponse, (StatusCode, Json<serde_json::Value>)> {
    let request = state
        .try_admit_request()
        .expect("admit test create request");
    handle_create_instance_owned(state, payload, request.producer_handoff()).await
}

#[cfg(test)]
pub(super) async fn queue_create_install_or_rollback(
    state: &AppState,
    instance_id: &str,
    request: Option<InstallQueueRequest>,
) -> Result<Option<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    let admitted = state
        .try_admit_request()
        .expect("admit test create request");
    queue_create_install_or_rollback_owned(state, instance_id, request, admitted.producer_handoff())
        .await
}

fn version_is_launch_ready_or_user_blocked(
    state: &AppState,
    installed_lookup: &InstalledVersionsLookup,
    installed_scan: &crate::application::version::InstalledVersionsScan,
    version_id: &str,
) -> Result<bool, (StatusCode, Json<serde_json::Value>)> {
    if installed_scan.is_degraded() {
        return Err(version_scan_degraded_response());
    }
    let config = state.config().current();
    let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
        library_dir: installed_lookup.library_dir().to_path_buf(),
        requested_java: config.java_path_override.trim().to_string(),
        version_id: version_id.to_string(),
        guardian_mode: GuardianMode::from_config(&config.guardian_mode),
    });
    if readiness.launchable {
        return Ok(true);
    }
    Ok(readiness
        .reasons
        .iter()
        .filter(|reason| reason.severity == LaunchReadinessSeverity::Blocking)
        .all(|reason| reason.id == LaunchReadinessReasonId::JavaOverrideMissing))
}

fn create_shutdown_error_response(
    _error: crate::state::LifecycleAdmissionError,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": "application shutdown is in progress" })),
    )
}

async fn rollback_created_instance(
    state: &AppState,
    instance_id: &str,
) -> Result<(), axial_config::InstanceStoreError> {
    state.delete_instance(instance_id.to_string(), true).await
}

fn create_queued_install_summary(
    response: &InstallQueueStateResponse,
    request: &InstallQueueRequest,
) -> Option<CreateQueuedInstallSummary> {
    if let Some(active) = response
        .active
        .as_ref()
        .filter(|active| install_queue_item_matches_request(&active.install_item, request))
    {
        return Some(CreateQueuedInstallSummary {
            state_id: "install_active".to_string(),
            kind: active.kind.clone(),
            label: active.label.clone(),
            queue_id: Some(active.queue_id.clone()),
            install_id: active.install_id.clone(),
            operation_id: active.operation_id.clone(),
        });
    }

    response
        .items
        .iter()
        .find(|item| install_queue_item_matches_request(&item.install_item, request))
        .map(|item| CreateQueuedInstallSummary {
            state_id: "install_queued".to_string(),
            kind: item.kind.clone(),
            label: item.label.clone(),
            queue_id: Some(item.queue_id.clone()),
            install_id: None,
            operation_id: None,
        })
}

fn install_queue_item_matches_request(
    item: &InstallQueueInstallItemViewModel,
    request: &InstallQueueRequest,
) -> bool {
    match request.kind.trim() {
        "vanilla" | "minecraft" => item.loader.is_none() && item.version_id == request.version_id,
        "loader" => item.loader.as_ref().is_some_and(|loader| {
            loader.component_id == request.component_id && loader.build_id == request.build_id
        }),
        _ => false,
    }
}

fn create_instance_response(
    instance: EnrichedInstance,
    install_queue: Option<InstallQueueStateResponse>,
    queued_install: Option<CreateQueuedInstallSummary>,
    guardian_notice: Option<CreateGuardianNotice>,
) -> CreateInstanceResponse {
    let payload = CreateInstancePayload {
        instance_id: Some(instance.id.clone()),
        queue_id: queued_install
            .as_ref()
            .and_then(|summary| summary.queue_id.clone()),
        install_id: queued_install
            .as_ref()
            .and_then(|summary| summary.install_id.clone()),
        operation_id: queued_install
            .as_ref()
            .and_then(|summary| summary.operation_id.clone()),
    };
    let operation_id = payload.operation_id.clone();
    let summary = queued_install
        .as_ref()
        .map(|queued| format!("Created {}; {} queued.", instance.name, queued.label))
        .unwrap_or_else(|| format!("Created {}", instance.name));

    CreateInstanceResponse {
        instance,
        result: CommandResult {
            command: CommandKind::CreateInstance,
            operation_id,
            status: OperationStatus::Succeeded,
            safety: None,
            carriers: CommandResultCarriers::default(),
            payload,
            view_model: None,
        },
        view_model: CreateInstanceResultViewModel {
            state_id: if queued_install.is_some() {
                "created_install_queued".to_string()
            } else {
                "created".to_string()
            },
            tone: if guardian_notice.is_some() {
                "warn".to_string()
            } else {
                "success".to_string()
            },
            title: "Instance created".to_string(),
            summary,
            detail: guardian_notice
                .as_ref()
                .and_then(|notice| notice.detail.clone()),
        },
        install_queue,
        queued_install,
        guardian_notice,
    }
}

fn create_guardian_notice(preset: &GuardianJvmPresetResolution) -> Option<CreateGuardianNotice> {
    preset.warning.then(|| CreateGuardianNotice {
        state_id: preset.state_id.clone(),
        tone: "warn".to_string(),
        message: "Guardian adjusted the JVM preset".to_string(),
        detail: preset.detail.clone(),
    })
}

fn create_source_options(
    unavailable_sources: &HashSet<String>,
) -> Vec<CreateInstanceSourceOptionViewModel> {
    let mut sources = vec![CreateInstanceSourceOptionViewModel {
        id: "vanilla".to_string(),
        label: "Vanilla".to_string(),
        enabled: true,
        disabled_reason: None,
    }];
    sources.extend(fetch_components().into_iter().map(|component| {
        let id = component.id.as_str().to_string();
        let unavailable = unavailable_sources.contains(&id);
        CreateInstanceSourceOptionViewModel {
            id,
            label: component.name,
            enabled: !unavailable,
            disabled_reason: unavailable
                .then(|| "Provider catalog is unavailable right now.".to_string()),
        }
    }));
    sources
}

fn create_channel_options() -> Vec<CreateInstanceSourceOptionViewModel> {
    ["release", "snapshot", "legacy"]
        .into_iter()
        .map(|id| CreateInstanceSourceOptionViewModel {
            id: id.to_string(),
            label: match id {
                "release" => "Release",
                "snapshot" => "Snapshot",
                "legacy" => "Legacy",
                _ => id,
            }
            .to_string(),
            enabled: true,
            disabled_reason: None,
        })
        .collect()
}

async fn create_version_rows(
    state: &AppState,
    producer: &ProducerLease,
    source_id: Option<&str>,
    notices: &mut Vec<CreateInstanceNoticeViewModel>,
) -> CreateVersionRowsResult {
    let scan_started = Instant::now();
    let Some(installed_lookup) = state.installed_versions_snapshot(producer).await else {
        return CreateVersionRowsResult::empty();
    };
    let library_dir = installed_lookup.library_dir().to_path_buf();
    let scan_source = installed_lookup.source.as_str();
    let refresh_count = installed_lookup.refresh_count;
    let installed_scan = installed_versions_scan(&installed_lookup.snapshot);
    let scan_elapsed = scan_started.elapsed();
    let scan_baseline = || CreateVersionRowsResult {
        scan_elapsed,
        scan_source,
        refresh_count,
        ..CreateVersionRowsResult::empty()
    };
    if installed_scan.is_degraded() {
        notices.push(CreateInstanceNoticeViewModel {
            state_id: "library_scan_degraded".to_string(),
            tone: "warn".to_string(),
            message: "Installed versions are unavailable".to_string(),
            detail: Some(VERSION_SCAN_DEGRADED_MESSAGE.to_string()),
        });
        return CreateVersionRowsResult { ..scan_baseline() };
    }

    let installed_versions = installed_scan.versions;
    let installed = installed_launchable_version_ids(&installed_versions);
    let loader_build_installed = installed_loader_build_keys(&installed_versions);
    let loader_installed = installed_loader_minecraft_keys(&installed_versions);
    let source_id = source_id.unwrap_or("vanilla");

    if let Some(cached_rows) = cacheable_source_rows(&library_dir, source_id) {
        return CreateVersionRowsResult {
            rows: materialize_version_rows(
                &cached_rows,
                &installed,
                &loader_build_installed,
                &loader_installed,
            ),
            source_cache_hit: true,
            ..scan_baseline()
        };
    }

    let mut static_rows = Vec::new();
    let mut catalog_elapsed = Duration::ZERO;
    let mut policy_elapsed = Duration::ZERO;

    if source_id == "vanilla" {
        let catalog_started = Instant::now();
        let manifest_result = fetch_version_manifest_cached(&library_dir).await;
        catalog_elapsed += catalog_started.elapsed();
        let manifest = match manifest_result {
            Ok(manifest) => manifest,
            Err(_) => {
                notices.push(CreateInstanceNoticeViewModel {
                    state_id: "catalog_unavailable".to_string(),
                    tone: "warn".to_string(),
                    message: "Minecraft versions are unavailable".to_string(),
                    detail: Some("Check your connection and try again.".to_string()),
                });
                return CreateVersionRowsResult {
                    catalog_elapsed,
                    policy_elapsed,
                    ..scan_baseline()
                };
            }
        };
        let releases = manifest_release_references(&manifest);
        for version in &manifest.versions {
            let analysis = analyze_minecraft_version(
                &version.id,
                &version.kind,
                &version.release_time,
                None,
                &releases,
            );
            static_rows.push(create_static_version_row(
                "vanilla",
                format!("vanilla|{}", version.id),
                &version.id,
                None,
                &analysis.minecraft_meta,
                &analysis.lifecycle,
                Vec::new(),
                None,
                false,
            ));
        }
        store_cacheable_source_rows(&library_dir, source_id, &static_rows);
        return CreateVersionRowsResult {
            rows: materialize_version_rows(
                &static_rows,
                &installed,
                &loader_build_installed,
                &loader_installed,
            ),
            catalog_elapsed,
            policy_elapsed,
            ..scan_baseline()
        };
    }

    for component in fetch_components() {
        if source_id != component.id.as_str() {
            continue;
        }
        let catalog_started = Instant::now();
        let supported_versions_result = fetch_supported_versions(&library_dir, component.id).await;
        catalog_elapsed += catalog_started.elapsed();
        match supported_versions_result {
            Ok((versions, versions_catalog)) => {
                let policy_inputs = loader_version_policy_inputs(&versions);
                let policy_started = Instant::now();
                let policy_decisions = evaluate_create_view_loader_version_policies(
                    &library_dir,
                    component.id,
                    &versions_catalog,
                    &policy_inputs,
                );
                policy_elapsed += policy_started.elapsed();

                for (version, decision) in versions.into_iter().zip(policy_decisions) {
                    static_rows.push(create_static_version_row(
                        component.id.as_str(),
                        format!("loader_version|{}|{}", component.id.as_str(), version.id),
                        &version.id,
                        None,
                        &version.minecraft_meta,
                        &version.lifecycle,
                        decision.tags,
                        decision.disabled_reason,
                        decision.fresh_catalog_required,
                    ));
                }
            }
            Err(_) => {
                notices.push(CreateInstanceNoticeViewModel {
                    state_id: format!("source_unavailable_{}", component.id.short_key()),
                    tone: "warn".to_string(),
                    message: format!("{} is unavailable", component.id.display_name()),
                    detail: Some("Check your connection and try again.".to_string()),
                });
                return CreateVersionRowsResult {
                    catalog_elapsed,
                    policy_elapsed,
                    ..scan_baseline()
                };
            }
        }
    }

    store_cacheable_source_rows(&library_dir, source_id, &static_rows);
    CreateVersionRowsResult {
        rows: materialize_version_rows(
            &static_rows,
            &installed,
            &loader_build_installed,
            &loader_installed,
        ),
        catalog_elapsed,
        policy_elapsed,
        ..scan_baseline()
    }
}

fn cacheable_source_rows(
    library_dir: &Path,
    source_id: &str,
) -> Option<Vec<CreateStaticVersionRow>> {
    let rows = cached_source_rows(library_dir, source_id)?;
    if source_rows_are_cacheable(&rows) {
        return Some(rows);
    }
    invalidate_create_view_source(library_dir, source_id);
    None
}

fn store_cacheable_source_rows(
    library_dir: &Path,
    source_id: &str,
    rows: &[CreateStaticVersionRow],
) {
    if source_rows_are_cacheable(rows) {
        store_source_rows(library_dir, source_id, rows.to_vec());
    }
}

fn source_rows_are_cacheable(rows: &[CreateStaticVersionRow]) -> bool {
    rows.iter().all(|row| !row.fresh_catalog_required)
}

#[allow(clippy::too_many_arguments)]
fn create_static_version_row(
    source_id: &str,
    selection_id: String,
    minecraft_version_id: &str,
    loader_build: Option<CreateLoaderBuildIdentityViewModel>,
    minecraft_meta: &MinecraftVersionMeta,
    lifecycle: &LifecycleMeta,
    tags: Vec<CreateVersionTagViewModel>,
    disabled_reason: Option<String>,
    fresh_catalog_required: bool,
) -> CreateStaticVersionRow {
    CreateStaticVersionRow {
        source_id: source_id.to_string(),
        selection_id,
        minecraft_version_id: minecraft_version_id.to_string(),
        loader_build,
        display_name: if minecraft_meta.display_name.is_empty() {
            minecraft_version_id.to_string()
        } else {
            minecraft_meta.display_name.clone()
        },
        hint: (!minecraft_meta.display_hint.is_empty())
            .then(|| minecraft_meta.display_hint.clone()),
        channel: create_channel_id(lifecycle.channel).to_string(),
        tags,
        disabled_reason,
        fresh_catalog_required,
    }
}

fn materialize_version_rows(
    rows: &[CreateStaticVersionRow],
    installed: &HashSet<String>,
    loader_build_installed: &HashSet<LoaderBuildInstallKey>,
    loader_installed: &HashSet<LoaderMinecraftInstallKey>,
) -> Vec<CreateInstanceVersionRowViewModel> {
    rows.iter()
        .map(|row| {
            let full_installed =
                row_full_installed(row, installed, loader_build_installed, loader_installed);
            let download_state = download_state(
                installed.contains(&row.minecraft_version_id),
                full_installed,
            );
            let disabled_reason = materialized_row_disabled_reason(row, full_installed);
            CreateInstanceVersionRowViewModel {
                source_id: row.source_id.clone(),
                selection_id: row.selection_id.clone(),
                minecraft_version_id: row.minecraft_version_id.clone(),
                loader_build: row.loader_build.clone(),
                display_name: row.display_name.clone(),
                hint: row.hint.clone(),
                channel: row.channel.clone(),
                tags: row.tags.clone(),
                download_state: download_state.to_string(),
                create_enabled: disabled_reason.is_none(),
                disabled_reason,
            }
        })
        .collect()
}

fn materialized_row_disabled_reason(
    row: &CreateStaticVersionRow,
    full_installed: bool,
) -> Option<String> {
    if let Some(reason) = &row.disabled_reason {
        return Some(reason.clone());
    }
    if row.fresh_catalog_required && !full_installed {
        return Some(stale_loader_version_catalog_message());
    }
    None
}

fn row_full_installed(
    row: &CreateStaticVersionRow,
    installed: &HashSet<String>,
    loader_build_installed: &HashSet<LoaderBuildInstallKey>,
    loader_installed: &HashSet<LoaderMinecraftInstallKey>,
) -> bool {
    if row.source_id == "vanilla" {
        return installed.contains(&row.minecraft_version_id);
    }
    if let Some(loader_build) = &row.loader_build {
        return loader_build_installed.contains(&LoaderBuildInstallKey {
            component_id: loader_build.component_id,
            build_id: loader_build.build_id.clone(),
            version_id: loader_build.target_version_id.clone(),
        });
    }
    LoaderComponentId::parse(&row.source_id).is_some_and(|component_id| {
        loader_installed.contains(&LoaderMinecraftInstallKey {
            component_id,
            version_id: row.minecraft_version_id.clone(),
        })
    })
}

fn installed_launchable_version_ids(versions: &[VersionEntry]) -> HashSet<String> {
    versions
        .iter()
        .filter(|version| version.installed && version.launchable)
        .map(|version| version.id.clone())
        .collect()
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct LoaderBuildInstallKey {
    component_id: LoaderComponentId,
    build_id: String,
    version_id: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct LoaderMinecraftInstallKey {
    component_id: LoaderComponentId,
    version_id: String,
}

fn installed_loader_build_keys(versions: &[VersionEntry]) -> HashSet<LoaderBuildInstallKey> {
    let mut installed = HashSet::new();
    for version in versions {
        if !version.installed || !version.launchable {
            continue;
        }
        let Some(loader) = &version.loader else {
            continue;
        };
        installed.insert(LoaderBuildInstallKey {
            component_id: loader.component_id,
            build_id: loader.build_id.clone(),
            version_id: version.id.clone(),
        });
    }
    installed
}

fn installed_loader_minecraft_keys(
    versions: &[VersionEntry],
) -> HashSet<LoaderMinecraftInstallKey> {
    let mut installed = HashSet::new();
    for version in versions {
        if !version.installed || !version.launchable {
            continue;
        }
        let Some(loader) = &version.loader else {
            continue;
        };
        let minecraft_version = if version.inherits_from.trim().is_empty() {
            parse_build_id(&loader.build_id)
                .map(|(_component_id, minecraft_version, _)| minecraft_version.trim().to_string())
        } else {
            Some(version.inherits_from.trim().to_string())
        };
        let Some(version_id) = minecraft_version.filter(|value| !value.is_empty()) else {
            continue;
        };
        installed.insert(LoaderMinecraftInstallKey {
            component_id: loader.component_id,
            version_id,
        });
    }
    installed
}

fn exact_loader_build_is_installed(versions: &[VersionEntry], build: &LoaderBuildRecord) -> bool {
    installed_loader_build_keys(versions).contains(&loader_build_install_key(build))
}

fn loader_build_install_key(build: &LoaderBuildRecord) -> LoaderBuildInstallKey {
    LoaderBuildInstallKey {
        component_id: build.component_id,
        build_id: build.build_id.clone(),
        version_id: build.version_id.clone(),
    }
}

fn download_state(base_installed: bool, full_installed: bool) -> &'static str {
    if full_installed {
        "full"
    } else if base_installed {
        "base"
    } else {
        "none"
    }
}

fn unavailable_source_ids(notices: &[CreateInstanceNoticeViewModel]) -> HashSet<String> {
    notices
        .iter()
        .filter_map(|notice| {
            notice
                .state_id
                .strip_prefix("source_unavailable_")
                .and_then(source_id_from_short_key)
                .map(str::to_string)
        })
        .collect()
}

fn source_id_from_short_key(short_key: &str) -> Option<&'static str> {
    fetch_components()
        .into_iter()
        .find(|component| component.id.short_key() == short_key)
        .map(|component| component.id.as_str())
}

fn no_compatible_stable_loader_response(
    component_id: LoaderComponentId,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": no_compatible_stable_loader_message(component_id)
        })),
    )
}

fn create_channel_id(channel: LifecycleChannel) -> &'static str {
    match channel {
        LifecycleChannel::Stable => "release",
        LifecycleChannel::Preview | LifecycleChannel::Experimental => "snapshot",
        LifecycleChannel::Legacy => "legacy",
        LifecycleChannel::Unknown => "unknown",
    }
}

fn bad_create_request(message: &'static str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": message })),
    )
}

fn library_not_configured_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::PRECONDITION_FAILED,
        Json(serde_json::json!({ "error": "Axial library is not configured" })),
    )
}

fn minecraft_versions_unavailable_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({ "error": "Minecraft versions are unavailable" })),
    )
}

fn stale_loader_catalog_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::PRECONDITION_FAILED,
        Json(serde_json::json!({
            "error": "Loader catalog needs a fresh provider check before this build can be installed."
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_loader_catalog_requirement_uses_current_installed_state() {
        let component_id = LoaderComponentId::Fabric;
        let row = stale_required_loader_row(component_id, "1.21.1");
        let installed = HashSet::new();
        let loader_build_installed = HashSet::new();
        let loader_installed = HashSet::new();

        let uninstalled = materialize_version_rows(
            std::slice::from_ref(&row),
            &installed,
            &loader_build_installed,
            &loader_installed,
        );
        assert!(!uninstalled[0].create_enabled);
        assert_eq!(
            uninstalled[0].disabled_reason.as_deref(),
            Some(
                "Loader catalog needs a fresh provider check before this version can be installed."
            )
        );

        let loader_installed = HashSet::from([LoaderMinecraftInstallKey {
            component_id,
            version_id: "1.21.1".to_string(),
        }]);
        let installed = materialize_version_rows(
            &[row],
            &installed,
            &loader_build_installed,
            &loader_installed,
        );
        assert!(installed[0].create_enabled);
        assert_eq!(installed[0].disabled_reason, None);
    }

    #[test]
    fn stale_loader_catalog_rows_are_not_cached_or_reused() {
        super::super::create_cache::reset_create_view_cache_for_tests();
        let component_id = LoaderComponentId::Fabric;
        let library_dir = std::env::temp_dir().join(format!(
            "axial-create-source-cache-stale-{}",
            std::process::id()
        ));
        let source_id = component_id.as_str();
        let row = stale_required_loader_row(component_id, "1.21.1");

        store_cacheable_source_rows(&library_dir, source_id, std::slice::from_ref(&row));
        assert!(cached_source_rows(&library_dir, source_id).is_none());

        store_source_rows(&library_dir, source_id, vec![row]);
        assert!(cacheable_source_rows(&library_dir, source_id).is_none());
        assert!(cached_source_rows(&library_dir, source_id).is_none());

        super::super::create_cache::reset_create_view_cache_for_tests();
    }

    fn stale_required_loader_row(
        component_id: LoaderComponentId,
        minecraft_version_id: &str,
    ) -> CreateStaticVersionRow {
        CreateStaticVersionRow {
            source_id: component_id.as_str().to_string(),
            selection_id: format!(
                "loader_version|{}|{}",
                component_id.as_str(),
                minecraft_version_id
            ),
            minecraft_version_id: minecraft_version_id.to_string(),
            loader_build: None,
            display_name: minecraft_version_id.to_string(),
            hint: None,
            channel: "release".to_string(),
            tags: Vec::new(),
            disabled_reason: None,
            fresh_catalog_required: true,
        }
    }
}

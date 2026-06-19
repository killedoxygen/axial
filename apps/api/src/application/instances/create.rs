use super::{
    InstanceWriteOperation, enrich_instance_for_state, instance_error_kind,
    instance_write_error_response, scan_current_versions,
};
use crate::application::install::InstallQueueInstallItemViewModel;
use crate::application::version::{VERSION_SCAN_DEGRADED_MESSAGE, version_scan_degraded_response};
use crate::application::{
    CommandResult, CommandResultCarriers, CreateInstancePayload, InstallQueueRequest,
    InstallQueueStateResponse, enqueue_install, loader_error_response,
};
use crate::guardian::{
    GuardianJvmPresetOption, GuardianJvmPresetResolution, guardian_jvm_preset_options,
    normalize_create_jvm_preset,
};
use crate::state::AppState;
use crate::state::contracts::{CommandKind, OperationId, OperationStatus};
use axum::{Json, http::StatusCode};
use croopor_config::{EnrichedInstance, Instance};
use croopor_launcher::{
    GuardianMode, LaunchReadinessReasonId, LaunchReadinessRequest, LaunchReadinessSeverity,
    inspect_launch_readiness,
};
use croopor_minecraft::{
    LifecycleChannel, LifecycleMeta, LoaderAvailability, LoaderBuildRecord, LoaderCatalogState,
    LoaderComponentId, LoaderInstallability, LoaderSelectionReason, MinecraftVersionMeta,
    VersionEntry, analyze_minecraft_version, compare_version_like, fetch_builds, fetch_components,
    fetch_supported_versions, fetch_version_manifest_cached, manifest_release_references,
    parse_build_id,
};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, collections::HashSet, ops::Deref, path::PathBuf};

const MAX_CREATE_NAME_COLLISION_RETRIES: usize = 9;

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
    pub download_state: String,
    pub create_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
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
pub(crate) struct CreateInstanceViewResponse {
    pub sources: Vec<CreateInstanceSourceOptionViewModel>,
    pub channels: Vec<CreateInstanceSourceOptionViewModel>,
    pub versions: Vec<CreateInstanceVersionRowViewModel>,
    pub preset_options: Vec<GuardianJvmPresetOption>,
    pub defaults: CreateInstanceDefaultsViewModel,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<CreateInstanceNoticeViewModel>,
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
    source_id: Option<&str>,
) -> CreateInstanceViewResponse {
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
    let versions = create_version_rows(state, requested_source.as_deref(), &mut notices).await;
    let unavailable_sources = unavailable_source_ids(&notices);

    CreateInstanceViewResponse {
        sources: create_source_options(&unavailable_sources),
        channels: create_channel_options(),
        versions,
        preset_options: guardian_jvm_preset_options(),
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

fn normalize_create_view_source(source_id: Option<&str>) -> Option<String> {
    let source_id = source_id.map(str::trim).filter(|value| !value.is_empty())?;
    if source_id == "vanilla" {
        return Some("vanilla".to_string());
    }
    let component_id = LoaderComponentId::parse(source_id)?;
    Some(component_id.as_str().to_string())
}

pub(crate) async fn handle_create_instance(
    state: &AppState,
    payload: CreateInstanceRequest,
) -> Result<CreateInstanceResponse, (StatusCode, Json<serde_json::Value>)> {
    let selection = resolve_create_selection(state, &payload).await?;
    let preset = normalize_create_jvm_preset(payload.jvm_preset_id.as_deref());
    let mc_dir = state.library_dir().map(PathBuf::from);
    let install_request = create_install_queue_request_if_needed(state, &selection)?;
    let queued_install_request = install_request.clone();
    let instance =
        create_instance_with_unique_name(state, &payload, &selection, mc_dir.as_deref())?;
    let created_instance_id = instance.id.clone();
    let instance = match apply_create_initial_settings(state, instance, &payload, &preset) {
        Ok(instance) => instance,
        Err(error) => {
            rollback_created_instance(state, &created_instance_id);
            return Err(error);
        }
    };
    let install_queue =
        queue_create_install_or_rollback(state, &created_instance_id, install_request).await?;
    let enriched = enrich_instance_for_state(state, instance);
    let queued_install = install_queue.as_ref().and_then(|response| {
        queued_install_request
            .as_ref()
            .and_then(|request| create_queued_install_summary(response, request))
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
    state: &AppState,
    payload: &CreateInstanceRequest,
) -> Result<CreateSelection, (StatusCode, Json<serde_json::Value>)> {
    let selection_id = payload.selection_id.trim();
    if selection_id.is_empty() {
        return Err(bad_create_request("selection_id is required"));
    }

    let mut parts = selection_id.splitn(3, '|');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("vanilla"), Some(version_id), None) if !version_id.trim().is_empty() => {
            resolve_vanilla_create_selection(state, version_id.trim()).await
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
            resolve_loader_version_create_selection(state, component_id, minecraft_version.trim())
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
            resolve_loader_create_selection(state, component_id, build_id.trim()).await
        }
        _ => Err(bad_create_request("invalid create selection")),
    }
}

async fn resolve_loader_version_create_selection(
    state: &AppState,
    component_id: LoaderComponentId,
    minecraft_version: &str,
) -> Result<CreateSelection, (StatusCode, Json<serde_json::Value>)> {
    let library_dir = state
        .library_dir()
        .ok_or_else(library_not_configured_response)?;
    let (builds, catalog) = fetch_builds(
        PathBuf::from(library_dir).as_path(),
        component_id,
        minecraft_version,
    )
    .await
    .map_err(loader_error_response)?;
    let installed_scan = scan_current_versions(state);
    if installed_scan.is_degraded() {
        return Err(version_scan_degraded_response());
    }
    let build = preferred_loader_build(builds).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(
                serde_json::json!({ "error": "No stable loader build is available for this Minecraft version." }),
            ),
        )
    })?;
    if loader_build_is_known_incompatible_default(&build) {
        return Err(no_compatible_stable_loader_response(component_id));
    }
    let exact_installed = exact_loader_build_is_installed(&installed_scan.versions, &build);
    if loader_catalog_is_stale(&catalog) && !exact_installed {
        return Err(stale_loader_catalog_response());
    }

    Ok(CreateSelection::Loader {
        component_id: build.component_id,
        build_id: build.build_id,
        target_version_id: build.version_id,
    })
}

async fn resolve_loader_create_selection(
    state: &AppState,
    component_id: LoaderComponentId,
    build_id: &str,
) -> Result<CreateSelection, (StatusCode, Json<serde_json::Value>)> {
    let Some((parsed_component_id, minecraft_version, _loader_version)) = parse_build_id(build_id)
    else {
        return Err(bad_create_request("invalid create selection"));
    };
    if parsed_component_id != component_id {
        return Err(bad_create_request("invalid create selection"));
    }

    let library_dir = state
        .library_dir()
        .ok_or_else(library_not_configured_response)?;
    let (builds, catalog) = fetch_builds(
        PathBuf::from(library_dir).as_path(),
        component_id,
        &minecraft_version,
    )
    .await
    .map_err(loader_error_response)?;
    let installed_scan = scan_current_versions(state);
    if installed_scan.is_degraded() {
        return Err(version_scan_degraded_response());
    }
    resolve_loader_create_selection_from_build_catalog(
        component_id,
        build_id,
        builds,
        &catalog,
        &installed_scan.versions,
    )
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
    state: &AppState,
    version_id: &str,
) -> Result<CreateSelection, (StatusCode, Json<serde_json::Value>)> {
    let library_dir = state
        .library_dir()
        .ok_or_else(library_not_configured_response)?;
    let library_dir = PathBuf::from(library_dir);
    let manifest = fetch_version_manifest_cached(&library_dir)
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

pub(super) fn preferred_loader_build(builds: Vec<LoaderBuildRecord>) -> Option<LoaderBuildRecord> {
    builds.into_iter().find(loader_build_is_stable_default)
}

fn loader_build_is_stable_default(build: &LoaderBuildRecord) -> bool {
    matches!(
        build.build_meta.selection.reason,
        LoaderSelectionReason::Recommended
            | LoaderSelectionReason::LatestStable
            | LoaderSelectionReason::Stable
            | LoaderSelectionReason::Unlabeled
    )
}

fn create_instance_with_unique_name(
    state: &AppState,
    payload: &CreateInstanceRequest,
    selection: &CreateSelection,
    mc_dir: Option<&std::path::Path>,
) -> Result<Instance, (StatusCode, Json<serde_json::Value>)> {
    let base_name = payload.name.trim();
    if base_name.is_empty() {
        return Err(bad_create_request("instance name is required"));
    }

    for attempt in 0..=MAX_CREATE_NAME_COLLISION_RETRIES {
        let name = if attempt == 0 {
            base_name.to_string()
        } else {
            format!("{base_name} ({attempt})")
        };
        match state.instances().add(
            name,
            selection.target_version_id().to_string(),
            payload.icon.clone(),
            payload.accent.clone(),
            mc_dir,
        ) {
            Ok(instance) => return Ok(instance),
            Err(error)
                if attempt < MAX_CREATE_NAME_COLLISION_RETRIES
                    && instance_error_kind(&error) == Some(std::io::ErrorKind::AlreadyExists) =>
            {
                continue;
            }
            Err(error) => {
                return Err(instance_write_error_response(
                    InstanceWriteOperation::Create,
                    error,
                ));
            }
        }
    }

    Err((
        StatusCode::CONFLICT,
        Json(serde_json::json!({ "error": "an instance with this name already exists" })),
    ))
}

fn apply_create_initial_settings(
    state: &AppState,
    mut instance: Instance,
    payload: &CreateInstanceRequest,
    preset: &GuardianJvmPresetResolution,
) -> Result<Instance, (StatusCode, Json<serde_json::Value>)> {
    let mut changed = false;
    if let Some(art_seed) = payload.art_seed {
        instance.art_seed = art_seed;
        changed = true;
    }
    if let Some(max_memory_mb) = payload.max_memory_mb {
        instance.max_memory_mb = max_memory_mb.max(0);
        changed = true;
    }
    if let Some(min_memory_mb) = payload.min_memory_mb {
        instance.min_memory_mb = min_memory_mb.max(0);
        changed = true;
    }
    if let Some(window_width) = payload.window_width {
        instance.window_width = window_width.max(0);
        changed = true;
    }
    if let Some(window_height) = payload.window_height {
        instance.window_height = window_height.max(0);
        changed = true;
    }
    if instance.jvm_preset != preset.stored_preset {
        instance.jvm_preset = preset.stored_preset.clone();
        changed = true;
    }

    if !changed {
        return Ok(instance);
    }

    state
        .instances()
        .update(instance)
        .map_err(|error| instance_write_error_response(InstanceWriteOperation::Create, error))
}

fn create_install_queue_request_if_needed(
    state: &AppState,
    selection: &CreateSelection,
) -> Result<Option<InstallQueueRequest>, (StatusCode, Json<serde_json::Value>)> {
    let Some(request) = selection.install_queue_request() else {
        return Ok(None);
    };
    if version_is_launch_ready_or_user_blocked(state, selection.target_version_id())? {
        return Ok(None);
    }

    Ok(Some(request))
}

async fn queue_create_install_request(
    state: &AppState,
    request: Option<InstallQueueRequest>,
) -> Result<Option<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    let Some(request) = request else {
        return Ok(None);
    };

    enqueue_install(state, request).await.map(Some)
}

pub(super) async fn queue_create_install_or_rollback(
    state: &AppState,
    instance_id: &str,
    request: Option<InstallQueueRequest>,
) -> Result<Option<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    match queue_create_install_request(state, request).await {
        Ok(install_queue) => Ok(install_queue),
        Err(error) => {
            rollback_created_instance(state, instance_id);
            Err(error)
        }
    }
}

fn version_is_launch_ready_or_user_blocked(
    state: &AppState,
    version_id: &str,
) -> Result<bool, (StatusCode, Json<serde_json::Value>)> {
    let scan = scan_current_versions(state);
    if scan.is_degraded() {
        return Err(version_scan_degraded_response());
    }
    let Some(library_dir) = state.library_dir().map(PathBuf::from) else {
        return Ok(false);
    };
    let config = state.config().current();
    let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
        library_dir,
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

fn rollback_created_instance(state: &AppState, instance_id: &str) {
    let _ = state.instances().remove(instance_id, true);
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
    source_id: Option<&str>,
    notices: &mut Vec<CreateInstanceNoticeViewModel>,
) -> Vec<CreateInstanceVersionRowViewModel> {
    let Some(library_dir) = state.library_dir().map(PathBuf::from) else {
        return Vec::new();
    };

    let installed_scan = scan_current_versions(state);
    if installed_scan.is_degraded() {
        notices.push(CreateInstanceNoticeViewModel {
            state_id: "library_scan_degraded".to_string(),
            tone: "warn".to_string(),
            message: "Installed versions are unavailable".to_string(),
            detail: Some(VERSION_SCAN_DEGRADED_MESSAGE.to_string()),
        });
        return Vec::new();
    }
    let mut rows = Vec::new();

    let installed_versions = installed_scan.versions;
    let installed = installed_launchable_version_ids(&installed_versions);
    let loader_installed = installed_loader_minecraft_keys(&installed_versions);

    if source_id.unwrap_or("vanilla") == "vanilla" {
        let manifest = match fetch_version_manifest_cached(&library_dir).await {
            Ok(manifest) => manifest,
            Err(_) => {
                notices.push(CreateInstanceNoticeViewModel {
                    state_id: "catalog_unavailable".to_string(),
                    tone: "warn".to_string(),
                    message: "Minecraft versions are unavailable".to_string(),
                    detail: Some("Check your connection and try again.".to_string()),
                });
                return Vec::new();
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
            rows.push(create_version_row(
                "vanilla",
                format!("vanilla|{}", version.id),
                &version.id,
                None,
                &analysis.minecraft_meta,
                &analysis.lifecycle,
                download_state(
                    installed.contains(&version.id),
                    installed.contains(&version.id),
                ),
                None,
            ));
        }
        return rows;
    }

    for component in fetch_components() {
        if source_id.is_some_and(|source_id| source_id != component.id.as_str()) {
            continue;
        }
        match fetch_supported_versions(&library_dir, component.id).await {
            Ok((versions, versions_catalog)) => {
                let disabled_reasons = join_all(versions.iter().map(|version| {
                    let exact_installed = loader_installed.contains(&LoaderMinecraftInstallKey {
                        component_id: component.id,
                        version_id: version.id.clone(),
                    });
                    loader_version_disabled_reason(
                        &library_dir,
                        component.id,
                        &version.id,
                        &versions_catalog,
                        exact_installed,
                        version.stable_hint,
                    )
                }))
                .await;

                for (version, disabled_reason) in versions.into_iter().zip(disabled_reasons) {
                    let exact_installed = loader_installed.contains(&LoaderMinecraftInstallKey {
                        component_id: component.id,
                        version_id: version.id.clone(),
                    });
                    rows.push(create_version_row(
                        component.id.as_str(),
                        format!("loader_version|{}|{}", component.id.as_str(), version.id),
                        &version.id,
                        None,
                        &version.minecraft_meta,
                        &version.lifecycle,
                        download_state(installed.contains(&version.id), exact_installed),
                        disabled_reason,
                    ));
                }
            }
            Err(_) => notices.push(CreateInstanceNoticeViewModel {
                state_id: format!("source_unavailable_{}", component.id.short_key()),
                tone: "warn".to_string(),
                message: format!("{} is unavailable", component.id.display_name()),
                detail: Some("Check your connection and try again.".to_string()),
            }),
        }
    }

    rows
}

fn create_version_row(
    source_id: &str,
    selection_id: String,
    minecraft_version_id: &str,
    loader_build: Option<CreateLoaderBuildIdentityViewModel>,
    minecraft_meta: &MinecraftVersionMeta,
    lifecycle: &LifecycleMeta,
    download_state: &'static str,
    disabled_reason: Option<String>,
) -> CreateInstanceVersionRowViewModel {
    CreateInstanceVersionRowViewModel {
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
        download_state: download_state.to_string(),
        create_enabled: disabled_reason.is_none(),
        disabled_reason,
    }
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

async fn loader_version_disabled_reason(
    library_dir: &std::path::Path,
    component_id: LoaderComponentId,
    minecraft_version: &str,
    versions_catalog: &LoaderCatalogState,
    exact_installed: bool,
    stable_hint: Option<bool>,
) -> Option<String> {
    if matches!(
        component_id,
        LoaderComponentId::Forge | LoaderComponentId::NeoForge
    ) && stable_hint == Some(false)
    {
        return Some(
            "Only beta loader builds are available for this Minecraft version. Pick an exact beta build explicitly if you want to test it."
                .to_string(),
        );
    }
    if let Some(reason) =
        known_loader_minecraft_version_disabled_reason(library_dir, component_id, minecraft_version)
            .await
    {
        return Some(reason);
    }
    if exact_installed {
        return None;
    }
    if loader_catalog_is_stale(versions_catalog) {
        return Some(
            "Loader catalog needs a fresh provider check before this version can be installed."
                .to_string(),
        );
    }
    None
}

async fn known_loader_minecraft_version_disabled_reason(
    library_dir: &std::path::Path,
    component_id: LoaderComponentId,
    minecraft_version: &str,
) -> Option<String> {
    if component_id != LoaderComponentId::Quilt
        || !quilt_java25_minecraft_version(minecraft_version)
    {
        return None;
    }

    let Ok((builds, _catalog)) = fetch_builds(library_dir, component_id, minecraft_version).await
    else {
        return None;
    };
    let Some(build) = preferred_loader_build(builds) else {
        return Some(no_compatible_stable_loader_message(component_id).to_string());
    };
    if loader_build_is_known_incompatible_default(&build) {
        Some(no_compatible_stable_loader_message(component_id).to_string())
    } else {
        None
    }
}

fn loader_build_is_known_incompatible_default(build: &LoaderBuildRecord) -> bool {
    build.component_id == LoaderComponentId::Quilt
        && quilt_java25_minecraft_version(&build.minecraft_version)
        && quilt_loader_version_is_before_java25_support(&build.loader_version)
}

fn quilt_java25_minecraft_version(minecraft_version: &str) -> bool {
    let value = minecraft_version.trim();
    value == "26" || value.starts_with("26.")
}

fn quilt_loader_version_is_before_java25_support(loader_version: &str) -> bool {
    let value = loader_version.trim();
    compare_version_like(value, "0.30.0") == Ordering::Less && !value.starts_with("0.30.")
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

fn no_compatible_stable_loader_message(component_id: LoaderComponentId) -> String {
    format!(
        "No stable compatible {} loader is available for this Minecraft version.",
        component_id.display_name()
    )
}

fn loader_catalog_is_stale(catalog: &LoaderCatalogState) -> bool {
    catalog.availability.stale || !catalog.availability.fresh
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
        Json(serde_json::json!({ "error": "Croopor library is not configured" })),
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

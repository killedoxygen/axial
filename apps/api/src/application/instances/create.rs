use super::{
    InstanceWriteOperation, enrich_instance_for_state, instance_error_kind,
    instance_write_error_response, scan_current_versions,
};
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
use croopor_minecraft::{
    LifecycleChannel, LifecycleMeta, LoaderBuildRecord, LoaderComponentId, MinecraftVersionMeta,
    VersionEntry, analyze_minecraft_version, fetch_builds, fetch_components,
    fetch_supported_versions, fetch_version_manifest_cached, manifest_release_references,
    scan_versions,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    ops::Deref,
    path::PathBuf,
};

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

pub(crate) async fn handle_create_instance_view(state: &AppState) -> CreateInstanceViewResponse {
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
    let versions = create_version_rows(state, &mut notices).await;

    CreateInstanceViewResponse {
        sources: create_source_options(),
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

pub(crate) async fn handle_create_instance(
    state: &AppState,
    payload: CreateInstanceRequest,
) -> Result<CreateInstanceResponse, (StatusCode, Json<serde_json::Value>)> {
    let selection = resolve_create_selection(state, &payload).await?;
    let preset = normalize_create_jvm_preset(payload.jvm_preset_id.as_deref());
    let mc_dir = state.library_dir().map(PathBuf::from);
    let instance =
        create_instance_with_unique_name(state, &payload, &selection, mc_dir.as_deref())?;
    let instance = apply_create_initial_settings(state, instance, &payload, &preset)?;
    let enriched = enrich_instance_for_state(state, instance);
    let install_queue = queue_create_install_if_needed(state, &selection).await?;
    let queued_install = install_queue
        .as_ref()
        .and_then(create_queued_install_summary);

    Ok(create_instance_response(
        enriched,
        install_queue,
        queued_install,
        create_guardian_notice(&preset),
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CreateSelection {
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
        (Some("loader_minecraft"), Some(component_id), Some(minecraft_version))
            if !minecraft_version.trim().is_empty() =>
        {
            let component_id = LoaderComponentId::parse(component_id.trim()).ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "unknown loader component" })),
                )
            })?;
            let library_dir = state
                .library_dir()
                .ok_or_else(library_not_configured_response)?;
            let (builds, _) = fetch_builds(
                PathBuf::from(library_dir).as_path(),
                component_id,
                minecraft_version.trim(),
            )
            .await
            .map_err(loader_error_response)?;
            let build = preferred_loader_build(builds).ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "loader build not found" })),
                )
            })?;

            Ok(CreateSelection::Loader {
                component_id: build.component_id,
                build_id: build.build_id,
                target_version_id: build.version_id,
            })
        }
        _ => Err(bad_create_request("invalid create selection")),
    }
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

fn preferred_loader_build(builds: Vec<LoaderBuildRecord>) -> Option<LoaderBuildRecord> {
    builds
        .into_iter()
        .max_by_key(|build| build.build_meta.selection.default_rank)
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

async fn queue_create_install_if_needed(
    state: &AppState,
    selection: &CreateSelection,
) -> Result<Option<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    let Some(request) = selection.install_queue_request() else {
        return Ok(None);
    };
    if version_is_installed(state, selection.target_version_id()) {
        return Ok(None);
    }

    enqueue_install(state, request).await.map(Some)
}

fn version_is_installed(state: &AppState, version_id: &str) -> bool {
    scan_current_versions(state)
        .into_iter()
        .any(|version| version.id == version_id && version.installed && version.launchable)
}

fn create_queued_install_summary(
    response: &InstallQueueStateResponse,
) -> Option<CreateQueuedInstallSummary> {
    if let Some(active) = response.active.as_ref() {
        return Some(CreateQueuedInstallSummary {
            state_id: "install_active".to_string(),
            kind: active.kind.clone(),
            label: active.label.clone(),
            queue_id: Some(active.queue_id.clone()),
            install_id: Some(active.install_id.clone()),
            operation_id: Some(active.operation_id.clone()),
        });
    }

    response
        .items
        .first()
        .map(|item| CreateQueuedInstallSummary {
            state_id: "install_queued".to_string(),
            kind: item.kind.clone(),
            label: item.label.clone(),
            queue_id: Some(item.queue_id.clone()),
            install_id: None,
            operation_id: None,
        })
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

fn create_source_options() -> Vec<CreateInstanceSourceOptionViewModel> {
    let mut sources = vec![CreateInstanceSourceOptionViewModel {
        id: "vanilla".to_string(),
        label: "Vanilla".to_string(),
        enabled: true,
        disabled_reason: None,
    }];
    sources.extend(fetch_components().into_iter().map(|component| {
        CreateInstanceSourceOptionViewModel {
            id: component.id.as_str().to_string(),
            label: component.name,
            enabled: true,
            disabled_reason: None,
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
    notices: &mut Vec<CreateInstanceNoticeViewModel>,
) -> Vec<CreateInstanceVersionRowViewModel> {
    let Some(library_dir) = state.library_dir().map(PathBuf::from) else {
        return Vec::new();
    };

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

    let installed_versions = scan_versions(&library_dir).unwrap_or_default();
    let installed = installed_launchable_version_ids(&installed_versions);
    let loader_installed = full_loader_installed_minecraft_versions(&installed_versions);
    let releases = manifest_release_references(&manifest);
    let manifest_versions = manifest.versions;
    let mut rows = Vec::new();

    for version in &manifest_versions {
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
            &analysis.minecraft_meta,
            &analysis.lifecycle,
            download_state(
                installed.contains(&version.id),
                installed.contains(&version.id),
            ),
        ));
    }

    for component in fetch_components() {
        match fetch_supported_versions(&library_dir, component.id).await {
            Ok((versions, _catalog)) => {
                let full_installed = loader_installed.get(&component.id);
                for version in versions {
                    rows.push(create_version_row(
                        component.id.as_str(),
                        format!("loader_minecraft|{}|{}", component.id.as_str(), version.id),
                        &version.id,
                        &version.minecraft_meta,
                        &version.lifecycle,
                        download_state(
                            installed.contains(&version.id),
                            full_installed.is_some_and(|set| set.contains(&version.id)),
                        ),
                    ));
                }
            }
            Err(_) => notices.push(CreateInstanceNoticeViewModel {
                state_id: format!("loader_versions_unavailable_{}", component.id.short_key()),
                tone: "warn".to_string(),
                message: format!("{} versions are unavailable", component.id.display_name()),
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
    minecraft_meta: &MinecraftVersionMeta,
    lifecycle: &LifecycleMeta,
    download_state: &'static str,
) -> CreateInstanceVersionRowViewModel {
    CreateInstanceVersionRowViewModel {
        source_id: source_id.to_string(),
        selection_id,
        minecraft_version_id: minecraft_version_id.to_string(),
        display_name: if minecraft_meta.display_name.is_empty() {
            minecraft_version_id.to_string()
        } else {
            minecraft_meta.display_name.clone()
        },
        hint: (!minecraft_meta.display_hint.is_empty())
            .then(|| minecraft_meta.display_hint.clone()),
        channel: create_channel_id(lifecycle.channel).to_string(),
        download_state: download_state.to_string(),
        create_enabled: true,
        disabled_reason: None,
    }
}

fn installed_launchable_version_ids(versions: &[VersionEntry]) -> HashSet<String> {
    versions
        .iter()
        .filter(|version| version.installed && version.launchable)
        .map(|version| version.id.clone())
        .collect()
}

fn full_loader_installed_minecraft_versions(
    versions: &[VersionEntry],
) -> HashMap<LoaderComponentId, HashSet<String>> {
    let mut installed = HashMap::<LoaderComponentId, HashSet<String>>::new();
    for version in versions {
        if !version.installed || !version.launchable {
            continue;
        }
        let Some(loader) = &version.loader else {
            continue;
        };
        let minecraft_version = if version.inherits_from.trim().is_empty() {
            minecraft_version_from_loader_build_id(&loader.build_id)
        } else {
            Some(version.inherits_from.clone())
        };
        if let Some(minecraft_version) = minecraft_version {
            installed
                .entry(loader.component_id)
                .or_default()
                .insert(minecraft_version);
        }
    }
    installed
}

fn minecraft_version_from_loader_build_id(build_id: &str) -> Option<String> {
    let mut parts = build_id.split(':');
    let _component = parts.next()?;
    let minecraft_version = parts.next()?.trim();
    if minecraft_version.is_empty() {
        None
    } else {
        Some(minecraft_version.to_string())
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

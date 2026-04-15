use super::policy;
use super::runner::trace_launch_event;
use crate::logging::timestamp_utc;
use crate::state::{AppState, LaunchSessionRecord};
use axum::{Json, http::StatusCode};
use croopor_config::{AppConfig, Instance};
use croopor_launcher::{GuardianSummary, LaunchGuardianContext, LaunchIntent, LaunchState};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub(super) struct LaunchRequest {
    pub instance_id: String,
    pub username: Option<String>,
    pub max_memory_mb: Option<i32>,
    pub min_memory_mb: Option<i32>,
    pub client_started_at_ms: Option<i64>,
}

pub(super) struct LaunchSessionTask {
    pub instance: Instance,
    pub config: AppConfig,
    pub intent: LaunchIntent,
    pub launched_at: String,
}

pub(super) struct PreparedLaunch {
    pub task: LaunchSessionTask,
}

pub(super) async fn prepare_launch_session(
    state: &AppState,
    payload: LaunchRequest,
) -> Result<PreparedLaunch, (StatusCode, Json<serde_json::Value>)> {
    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({ "error": "Croopor library is not configured" })),
        )
    })?;
    let library_dir = PathBuf::from(library_dir);

    let instance = state.instances().get(&payload.instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "instance not found" })),
        )
    })?;
    if state.sessions().has_active_instance(&instance.id).await {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": "instance already has an active session" })),
        ));
    }

    let config = state.config().current();
    let username = payload
        .username
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(config.username.as_str())
        .to_string();
    let max_memory_mb = policy::effective_max_memory(&instance, &config, payload.max_memory_mb);
    let min_memory_mb =
        policy::effective_min_memory(&instance, &config, payload.min_memory_mb, max_memory_mb);
    let requested_java = policy::selected_java_override(&instance, &config);
    let requested_preset = policy::selected_jvm_preset(&instance, &config);
    let launched_at = timestamp_utc();
    let session_id = policy::generate_session_id();
    let guardian = LaunchGuardianContext {
        mode: policy::selected_guardian_mode(&config),
        java_override_origin: policy::java_override_origin(&instance, &config),
        preset_override_origin: policy::preset_override_origin(&instance, &config),
        raw_jvm_args_origin: policy::raw_jvm_args_origin(&instance),
    };

    let intent = LaunchIntent {
        session_id: session_id.0.clone(),
        library_dir: library_dir.clone(),
        instance_id: instance.id.clone(),
        version_id: instance.version_id.clone(),
        username: username.clone(),
        requested_java: requested_java.clone(),
        requested_preset: requested_preset.clone(),
        extra_jvm_args: policy::split_jvm_args(&instance.extra_jvm_args),
        max_memory_mb,
        min_memory_mb,
        resolution: policy::selected_resolution(&instance, &config),
        launcher_name: "croopor".to_string(),
        launcher_version: state.version().to_string(),
        game_dir: Some(state.instances().game_dir(&instance.id)),
        guardian: guardian.clone(),
        performance_mode: policy::selected_performance_mode(&instance, &config),
    };

    state
        .sessions()
        .insert(LaunchSessionRecord {
            session_id: session_id.clone(),
            instance_id: instance.id.clone(),
            version_id: instance.version_id.clone(),
            state: LaunchState::Queued,
            pid: None,
            exit_code: None,
            command: Vec::new(),
            java_path: None,
            natives_dir: None,
            failure: None,
            healing: None,
            guardian: serde_json::to_value(GuardianSummary::new(guardian.mode)).ok(),
        })
        .await;
    trace_launch_event(
        &session_id.0,
        &format!(
            "launch requested for instance {} version {} client_started_at_ms={:?}",
            instance.id, instance.version_id, payload.client_started_at_ms
        ),
    );

    Ok(PreparedLaunch {
        task: LaunchSessionTask {
            instance: instance.clone(),
            config: config.clone(),
            intent,
            launched_at,
        },
    })
}

use std::sync::Arc;

use axial_app::{
    settings::{FlagOverridePatch, FlagsResponse, SettingsError, SettingsStore},
    tasks::TaskOwner,
    telemetry::Telemetry,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    routing::{get, put},
};

use super::config::{ApiError, json_error, report_save_failure, settings_error};

#[derive(Clone)]
struct FlagRouteState {
    settings: Arc<SettingsStore>,
    telemetry: Arc<Telemetry>,
    tasks: TaskOwner,
}

pub fn router(settings: Arc<SettingsStore>, telemetry: Arc<Telemetry>, tasks: TaskOwner) -> Router {
    Router::new()
        .route("/api/v1/flags", get(list_flags))
        .route("/api/v1/flags/{key}", put(update_flag))
        .layer(DefaultBodyLimit::max(4096))
        .with_state(FlagRouteState {
            settings,
            telemetry,
            tasks,
        })
}

async fn list_flags(State(state): State<FlagRouteState>) -> Result<Json<FlagsResponse>, ApiError> {
    tokio::task::spawn_blocking(move || state.settings.list_flags())
        .await
        .map_err(|_| settings_error(SettingsError::Unavailable))?
        .map(Json)
        .map_err(settings_error)
}

async fn update_flag(
    State(state): State<FlagRouteState>,
    Path(key): Path<String>,
    request: Result<Json<FlagOverridePatch>, JsonRejection>,
) -> Result<Json<FlagsResponse>, ApiError> {
    let Json(patch) = request.map_err(json_error)?;
    let tasks = state.tasks.clone();
    let handle = tasks
        .try_spawn((), move |cancellation| async move {
            if cancellation.is_cancelled() {
                return Err(SettingsError::Unavailable);
            }
            tokio::task::spawn_blocking(move || {
                let result = state.settings.update_flag(&key, patch);
                if let Err(error) = &result {
                    report_save_failure(&state.telemetry, error);
                }
                result
            })
            .await
            .expect("feature flag persistence task panicked")
        })
        .map_err(|_| settings_error(SettingsError::Unavailable))?;
    handle
        .join()
        .await
        .map_err(|_| settings_error(SettingsError::Unavailable))?
        .map(Json)
        .map_err(settings_error)
}

pub mod accounts;
pub mod auth;
pub mod benchmarks;
pub mod content;
pub(crate) mod error;
pub mod install;
pub mod instances;
pub mod java;
pub mod launch;
pub mod music;
pub mod performance;
pub mod resources;
pub mod settings;
pub mod skin;
pub mod system;
pub mod telemetry;
pub mod update;
pub mod versions;

use axial_app::{
    library::{AdmissionState, LibraryLifecycle, LibraryMode},
    settings::SettingsStore,
};
use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use error::{ApiError, ApiResult};
use serde::Serialize;
use std::sync::Arc;

/// A command acknowledgement without further payload.
#[derive(Serialize)]
pub(crate) struct Status {
    status: &'static str,
}

impl Status {
    pub(crate) const OK: Self = Self::new("ok");

    pub(crate) const fn new(status: &'static str) -> Self {
        Self { status }
    }
}

/// Media bodies are never sniffed; `cache` states their reuse policy.
pub(crate) fn media(content_type: &'static str, cache: &'static str, bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, cache),
        ],
        bytes,
    )
        .into_response()
}

pub(crate) fn status_router(settings: Arc<SettingsStore>, library: LibraryLifecycle) -> Router {
    Router::new()
        .route("/api/v1/status", get(status))
        .with_state((settings, library))
}

#[derive(Serialize)]
struct ApplicationStatus {
    dev_mode: bool,
    setup_required: bool,
    warnings: Vec<&'static str>,
}

async fn status(
    State((settings, library)): State<(Arc<SettingsStore>, LibraryLifecycle)>,
) -> ApiResult<ApplicationStatus> {
    let unavailable = || {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Application status is unavailable.",
        )
    };
    let config = tokio::task::spawn_blocking(move || settings.current())
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
    let snapshot = library.snapshot();
    let mut warnings = vec![
        "This isolated rewrite is under development. Some retained features are still being integrated.",
    ];
    if snapshot.admission == AdmissionState::Unavailable {
        warnings.push("The selected external library is unavailable. Its files have been preserved; restore its location and restart the launcher.");
    }
    Ok(Json(ApplicationStatus {
        dev_mode: cfg!(debug_assertions),
        setup_required: !config.onboarding_done
            && snapshot.admission == AdmissionState::Open
            && snapshot
                .current
                .is_some_and(|generation| generation.mode == LibraryMode::Managed),
        warnings,
    }))
}

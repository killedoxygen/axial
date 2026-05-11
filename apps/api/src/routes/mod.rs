mod catalog;
mod config;
mod dev;
mod install;
mod instances;
mod java;
mod launch;
mod loaders;
mod music;
mod performance;
mod setup;
mod status;
mod system;
mod update;
mod version_info;
mod versions;

use crate::state::AppState;
use axum::{
    Router,
    http::{HeaderValue, Method, header},
};
use tower_http::cors::{AllowOrigin, CorsLayer};

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(status::router())
        .merge(system::router())
        .merge(config::router())
        .merge(dev::router())
        .merge(setup::router())
        .merge(catalog::router())
        .merge(instances::router())
        .merge(install::router())
        .merge(music::router())
        .merge(performance::router())
        .merge(update::router())
        .merge(launch::router())
        .merge(loaders::router())
        .merge(versions::router())
        .merge(version_info::router())
        .merge(java::router())
        .with_state(state)
        .layer(local_cors_layer())
}

fn local_cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| {
            is_allowed_local_origin(origin)
        }))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([header::CONTENT_TYPE])
}

fn is_allowed_local_origin(origin: &HeaderValue) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };

    origin == "tauri://localhost"
        || origin == "http://tauri.localhost"
        || origin == "https://tauri.localhost"
        || origin
            .strip_prefix("http://127.0.0.1:")
            .is_some_and(is_port_suffix)
        || origin
            .strip_prefix("http://localhost:")
            .is_some_and(is_port_suffix)
        || origin
            .strip_prefix("http://[::1]:")
            .is_some_and(is_port_suffix)
}

fn is_port_suffix(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

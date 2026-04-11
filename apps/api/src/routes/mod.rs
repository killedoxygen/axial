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
use axum::Router;

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
}

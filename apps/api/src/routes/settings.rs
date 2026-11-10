//! Settings, onboarding, interface preferences and feature flags.

use super::{
    Status,
    error::{ApiResult, json_rejection},
};
use axial_app::settings::{
    ConfigPatch, ConfigView, FlagOverridePatch, FlagsResponse, InterfacePreferencesReceipt,
    InterfacePreferencesSnapshot, InterfacePreferencesUpdate, MAX_INTERFACE_PREFERENCES_BYTES,
    SettingsService,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    routing::{get, post, put},
};
use serde::Deserialize;

pub fn router(settings: SettingsService) -> Router {
    Router::new()
        .route("/api/v1/config", get(config).put(update_config))
        .route("/api/v1/onboarding/complete", post(complete_onboarding))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .merge(
            Router::new()
                .route(
                    "/api/v1/config/interface-preferences",
                    get(interface_preferences).put(update_interface_preferences),
                )
                .layer(DefaultBodyLimit::max(
                    MAX_INTERFACE_PREFERENCES_BYTES + 1024,
                )),
        )
        .merge(
            Router::new()
                .route("/api/v1/flags", get(flags))
                .route("/api/v1/flags/{key}", put(update_flag))
                .layer(DefaultBodyLimit::max(4096)),
        )
        .with_state(settings)
}

async fn config(State(settings): State<SettingsService>) -> ApiResult<ConfigView> {
    Ok(Json(settings.config().await?))
}

async fn update_config(
    State(settings): State<SettingsService>,
    request: Result<Json<ConfigPatch>, JsonRejection>,
) -> ApiResult<ConfigView> {
    let Json(patch) = request.map_err(json_rejection)?;
    Ok(Json(settings.commit(patch).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OnboardingCompletion {
    expected_revision: u64,
}

async fn complete_onboarding(
    State(settings): State<SettingsService>,
    request: Result<Json<OnboardingCompletion>, JsonRejection>,
) -> ApiResult<Status> {
    let Json(request) = request.map_err(json_rejection)?;
    settings
        .complete_onboarding(request.expected_revision)
        .await?;
    Ok(Json(Status::OK))
}

async fn interface_preferences(
    State(settings): State<SettingsService>,
) -> ApiResult<InterfacePreferencesSnapshot> {
    Ok(Json(settings.interface_preferences().await?))
}

async fn update_interface_preferences(
    State(settings): State<SettingsService>,
    request: Result<Json<InterfacePreferencesUpdate>, JsonRejection>,
) -> ApiResult<InterfacePreferencesReceipt> {
    let Json(update) = request.map_err(json_rejection)?;
    Ok(Json(settings.update_interface_preferences(update).await?))
}

async fn flags(State(settings): State<SettingsService>) -> ApiResult<FlagsResponse> {
    Ok(Json(settings.flags().await?))
}

async fn update_flag(
    State(settings): State<SettingsService>,
    Path(key): Path<String>,
    request: Result<Json<FlagOverridePatch>, JsonRejection>,
) -> ApiResult<FlagsResponse> {
    let Json(patch) = request.map_err(json_rejection)?;
    Ok(Json(settings.update_flag(key, patch).await?))
}

#[cfg(test)]
mod tests {
    use crate::fixture::{Api, Profile};
    use axial_app::settings::{ConfigView, MAX_INTERFACE_PREFERENCES_BYTES, STATE_INSPECTOR_FLAG};
    use reqwest::{Method, StatusCode};
    use serde_json::{Value, json};

    const INTERFACE: &str = "/api/v1/config/interface-preferences";

    async fn raw(api: &Api, path: &str, body: String) -> (StatusCode, Value) {
        Api::json(
            api.builder(Method::PUT, path)
                .header("content-type", "application/json")
                .body(body),
        )
        .await
    }

    #[tokio::test]
    async fn refusals_are_fixed_bounded_json_and_change_nothing() {
        let profile = Profile::start().await;
        let api = &profile.api;
        let unknown = (
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"error":"Invalid JSON request."}),
        );
        for body in [
            json!({"theme":"nether"}),
            json!({"expected_revision":0,"username":"Steve"}),
            json!({"expected_revision":0,"launch_auth_mode":"online"}),
            json!({"expected_revision":0,"guardian_mode":"secret-value"}),
            json!({"expected_revision":0,"library_dir":"/private/secret-value"}),
        ] {
            assert_eq!(
                api.response(Method::PUT, "/api/v1/config", Some(body))
                    .await,
                unknown
            );
        }
        let invalid = [
            (
                "/api/v1/config",
                json!({"expected_revision":0,"max_memory_mb":32769}),
                StatusCode::BAD_REQUEST,
            ),
            (
                "/api/v1/onboarding/complete",
                json!({}),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                &format!("/api/v1/flags/{STATE_INSPECTOR_FLAG}"),
                json!({"expected_revision":0}),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                "/api/v1/flags/unknown",
                json!({"expected_revision":0,"enabled":true}),
                StatusCode::NOT_FOUND,
            ),
            (
                INTERFACE,
                json!({"expected_revision":0,"change":{"kind":"route"}}),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                INTERFACE,
                json!({"expected_revision":0,"change":{"kind":"local","preferences":{}}}),
                StatusCode::BAD_REQUEST,
            ),
        ];
        for (path, body, status) in invalid {
            let method = if path.ends_with("complete") {
                Method::POST
            } else {
                Method::PUT
            };
            assert_eq!(
                api.response(method, path, Some(body)).await.0,
                status,
                "{path}"
            );
        }
        let syntax = (
            StatusCode::BAD_REQUEST,
            json!({"error":"Invalid JSON syntax."}),
        );
        assert_eq!(
            raw(api, "/api/v1/config", "{\"theme\":\"secret-value".into()).await,
            syntax
        );
        assert_eq!(raw(api, INTERFACE,
            r#"{"expected_revision":0,"change":{"kind":"route","route":{"name":"instance","id":"\ud800"}}}"#.into()
        ).await, syntax);
        for (path, limit) in [
            (INTERFACE, MAX_INTERFACE_PREFERENCES_BYTES + 1024),
            ("/api/v1/config", 64 * 1024),
        ] {
            assert_eq!(
                raw(api, path, format!("{}null", " ".repeat(limit))).await,
                (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    json!({"error":"Request body is too large."})
                )
            );
        }
        assert_eq!(
            api.read(INTERFACE).await.1,
            json!({"revision":0,"value":null})
        );
        assert_eq!(
            profile.services.settings.current().unwrap(),
            ConfigView::default()
        );
        profile.close().await;
    }

    #[tokio::test]
    async fn commands_return_their_retained_acknowledgements() {
        let profile = Profile::start().await;
        let api = &profile.api;
        assert_eq!(
            api.post(
                "/api/v1/onboarding/complete",
                json!({"expected_revision":0})
            )
            .await,
            json!({"status":"ok"})
        );
        let config = api
            .put(
                "/api/v1/config",
                json!({"expected_revision":1,"theme":"nether"}),
            )
            .await;
        assert_eq!(
            (config["revision"].clone(), config["theme"].clone()),
            (json!(2), json!("nether"))
        );
        assert!(config.get("interface_preferences").is_none());
        let value = json!({
            "version":1,
            "preferences":{
                "theme":"nether","customHue":180.0,"customVibrancy":80.0,"lightness":20.0,
                "sounds":false,"hideSkinNametag":true,"selectedSkin":"skin-one",
                "selectedSkinsByAccount":{"account-one":"skin-two"},
                "shortcuts":{"settings":{"key":",","ctrl":true}},
                "overlayPositions":{"status":{"x":12.0,"y":34.0,"scaleX":0.5}},
                "lastUpdateCheckAt":"2026-09-27T12:00:00Z","dismissedUpdateVersion":"1.2.3"
            },
            "route":{"name":"instance","id":"12345678-1234-4234-8234-123456789abc"}
        });
        let replace = json!({"expected_revision":0,"change":{"kind":"replace","value":value}});
        assert_eq!(api.put(INTERFACE, replace).await, json!({"revision":1}));
        assert_eq!(
            api.get(INTERFACE).await,
            json!({"revision":1,"value":value})
        );
        profile.close().await;
    }
}

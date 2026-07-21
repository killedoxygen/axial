//! Application-owned feature flag workflow.

use crate::{
    observability::telemetry::{
        TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent,
    },
    state::AppState,
};
use axial_config::{ConfigStoreError, FEATURE_FLAGS, FlagStage, find_flag};
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};

const CONFIG_SAVE_ERROR_MESSAGE: &str =
    "Could not save settings. Check app data permissions and try again.";

type ApiError = (StatusCode, Json<serde_json::Value>);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FlagSource {
    Default,
    Override,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FlagViewModel {
    pub key: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub stage: FlagStage,
    pub dev_only: bool,
    pub default_enabled: bool,
    pub enabled: bool,
    pub source: FlagSource,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FlagsResponse {
    pub flags: Vec<FlagViewModel>,
}

#[derive(Debug, Default, Deserialize)]
pub struct FlagOverridePatch {
    pub enabled: Option<bool>,
}

pub fn list_flags(state: &AppState) -> FlagsResponse {
    list_flags_for_build(state, cfg!(debug_assertions))
}

fn list_flags_for_build(state: &AppState, development_build: bool) -> FlagsResponse {
    let config = state.config().current();
    let flags = FEATURE_FLAGS
        .iter()
        .filter(|flag| visible_in_build(flag, development_build))
        .map(|flag| {
            let (enabled, source) = match config.feature_overrides.get(flag.key).copied() {
                Some(enabled) => (enabled, FlagSource::Override),
                None => (flag.default_enabled, FlagSource::Default),
            };
            FlagViewModel {
                key: flag.key,
                title: flag.title,
                description: flag.description,
                stage: flag.stage,
                dev_only: flag.dev_only,
                default_enabled: flag.default_enabled,
                enabled,
                source,
            }
        })
        .collect();

    FlagsResponse { flags }
}

pub async fn update_flag(
    state: &AppState,
    key: &str,
    patch: FlagOverridePatch,
) -> Result<FlagsResponse, ApiError> {
    let Some(flag) = find_visible_flag(key) else {
        return Err(unknown_flag_response());
    };

    let key = flag.key.to_string();
    state
        .mutate_config(move |latest| -> Result<(), ConfigStoreError> {
            if let Some(enabled) = patch.enabled {
                latest.feature_overrides.insert(key, enabled);
            } else {
                latest.feature_overrides.remove(&key);
            }
            Ok(())
        })
        .await
        .map_err(|error| {
            emit_config_save_failed(state, &error);
            config_update_error_response(error)
        })?;

    Ok(list_flags(state))
}

fn find_visible_flag(key: &str) -> Option<&'static axial_config::FeatureFlagDef> {
    find_flag(key).filter(|flag| visible_in_build(flag, cfg!(debug_assertions)))
}

fn visible_in_build(flag: &axial_config::FeatureFlagDef, development_build: bool) -> bool {
    development_build || !flag.dev_only
}

fn unknown_flag_response() -> ApiError {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "unknown feature flag" })),
    )
}

fn emit_config_save_failed(state: &AppState, error: &ConfigStoreError) {
    if matches!(error, ConfigStoreError::Validation(_)) {
        return;
    }
    state.telemetry().emit(TelemetryEvent::error_captured(
        TelemetryErrorKind::ConfigSaveFailed,
        TelemetryErrorArea::Config,
        TelemetryErrorLevel::Error,
        CONFIG_SAVE_ERROR_MESSAGE,
    ));
}

fn config_update_error_response(error: ConfigStoreError) -> ApiError {
    match error {
        ConfigStoreError::Validation(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error.to_string() })),
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": CONFIG_SAVE_ERROR_MESSAGE })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{FlagOverridePatch, FlagSource};
    use crate::state::{AppState, AppStateInit, InstallStore, SessionStore};
    use axial_config::{
        AppConfig, AppPaths, ConfigStore, FEATURE_FLAGS, InstanceRegistrySnapshot, InstanceStore,
    };
    use axial_performance::PerformanceManager;
    use axum::Json;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn p00_b10_contract_local_registry_uses_default_without_override() {
        let fixture = TestFixture::new("list-default");
        let response = super::list_flags(&fixture.state);
        let flag = response
            .flags
            .iter()
            .find(|flag| flag.key == seed_key())
            .expect("seed flag should be visible in debug tests");

        assert!(!flag.enabled);
        assert_eq!(flag.source, FlagSource::Default);
    }

    #[test]
    fn p00_b10_contract_release_projection_hides_dev_only_registry() {
        let fixture = TestFixture::new("release-projection");
        let response = super::list_flags_for_build(&fixture.state, false);

        assert!(response.flags.iter().all(|flag| !flag.dev_only));
    }

    #[test]
    fn p00_b10_contract_wire_source_vocabulary_is_exactly_local() {
        let sources = [FlagSource::Default, FlagSource::Override];
        let expected = sources.clone().map(|source| match source {
            FlagSource::Default => "default",
            FlagSource::Override => "override",
        });
        assert_eq!(
            serde_json::to_value(sources).expect("serialize local flag sources"),
            serde_json::json!(expected)
        );
    }

    #[tokio::test]
    async fn p00_b10_contract_local_override_is_persisted() {
        let fixture = TestFixture::new("set-override");
        let response = super::update_flag(
            &fixture.state,
            seed_key(),
            FlagOverridePatch {
                enabled: Some(true),
            },
        )
        .await
        .expect("override update should succeed");
        let flag = response
            .flags
            .iter()
            .find(|flag| flag.key == seed_key())
            .expect("seed flag should remain listed");

        assert!(flag.enabled);
        assert_eq!(flag.source, FlagSource::Override);
        assert_eq!(
            fixture
                .state
                .config()
                .current()
                .feature_overrides
                .get(seed_key()),
            Some(&true)
        );

        let persisted = fs::read_to_string(&fixture.paths.config_file)
            .expect("config should be written to disk");
        let persisted_config =
            serde_json::from_str::<AppConfig>(&persisted).expect("config should deserialize");
        assert_eq!(
            persisted_config.feature_overrides.get(seed_key()),
            Some(&true)
        );
    }

    #[tokio::test]
    async fn p00_b10_contract_local_override_reset_restores_default() {
        let fixture = TestFixture::new("clear-override");
        super::update_flag(
            &fixture.state,
            seed_key(),
            FlagOverridePatch {
                enabled: Some(true),
            },
        )
        .await
        .expect("override update should succeed");

        let response = super::update_flag(
            &fixture.state,
            seed_key(),
            FlagOverridePatch { enabled: None },
        )
        .await
        .expect("clear override should succeed");
        let flag = response
            .flags
            .iter()
            .find(|flag| flag.key == seed_key())
            .expect("seed flag should remain listed");

        assert!(!flag.enabled);
        assert_eq!(flag.source, FlagSource::Default);
        assert!(
            !fixture
                .state
                .config()
                .current()
                .feature_overrides
                .contains_key(seed_key())
        );
    }

    #[tokio::test]
    async fn unknown_key_returns_404() {
        let fixture = TestFixture::new("unknown-key");
        let (status, Json(body)) = super::update_flag(
            &fixture.state,
            "missing.flag",
            FlagOverridePatch {
                enabled: Some(true),
            },
        )
        .await
        .expect_err("unknown key should fail");

        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(body, serde_json::json!({ "error": "unknown feature flag" }));
    }

    fn seed_key() -> &'static str {
        FEATURE_FLAGS[0].key
    }

    struct TestFixture {
        state: AppState,
        root: PathBuf,
        paths: AppPaths,
    }

    impl TestFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let config = Arc::new(
                ConfigStore::from_config(paths.clone(), AppConfig::default()).expect("set config"),
            );
            let instances = Arc::new(
                InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                    .expect("load instances"),
            );
            let state = AppState::new(AppStateInit {
                app_name: "Axial".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(
                    PerformanceManager::load_for_startup(&paths.config_dir)
                        .expect("performance manager"),
                ),
                startup_warnings: Vec::new(),
            });

            Self { state, root, paths }
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "axial-flags-application-{name}-{}-{nonce}",
            std::process::id()
        ))
    }
}

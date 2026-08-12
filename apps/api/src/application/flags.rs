//! Application-owned feature flag workflow.

use crate::{
    application::config::{ConfigFailureTelemetry, persist_config_mutation},
    state::AppState,
};
use axial_config::{FEATURE_FLAGS, FlagStage, find_flag};
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};

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
    persist_config_mutation(
        state,
        ConfigFailureTelemetry::RespectCommittedConsent,
        move |latest| {
            if let Some(enabled) = patch.enabled {
                latest.feature_overrides.insert(key, enabled);
            } else {
                latest.feature_overrides.remove(&key);
            }
            Ok(())
        },
    )
    .await?;

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

#[cfg(test)]
mod tests {
    use super::{FlagOverridePatch, FlagSource};
    use crate::{
        observability::telemetry::{DEFAULT_POSTHOG_HOST, TelemetryHub},
        state::{AppState, AppStateInit, InstallStore, SessionStore},
    };
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

        let persisted = fs::read_to_string(fixture.paths.config_file())
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

    #[tokio::test]
    async fn p02_b02_contract_flag_failure_respects_committed_telemetry_consent() {
        for (telemetry_enabled, expected_events) in [(false, 0), (true, 1)] {
            let fixture = TestFixture::with_config(
                if telemetry_enabled {
                    "flag-failure-enabled"
                } else {
                    "flag-failure-disabled"
                },
                AppConfig {
                    telemetry_enabled,
                    telemetry_install_id: if telemetry_enabled {
                        "123e4567-e89b-12d3-a456-426614174000".to_string()
                    } else {
                        String::new()
                    },
                    ..AppConfig::default()
                },
            );
            fixture.block_config_file();

            assert!(
                super::update_flag(
                    &fixture.state,
                    seed_key(),
                    FlagOverridePatch {
                        enabled: Some(true),
                    },
                )
                .await
                .is_err()
            );
            assert_eq!(
                fixture.state.telemetry().queue_len_for_test(),
                expected_events
            );

            fixture.unblock_config_file();
            super::update_flag(
                &fixture.state,
                seed_key(),
                FlagOverridePatch {
                    enabled: Some(true),
                },
            )
            .await
            .expect("retry exact retained flag update");
        }
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
            Self::with_config(name, AppConfig::default())
        }

        fn with_config(name: &str, initial: AppConfig) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let root_session = crate::state::test_root_session(&paths);
            let config = Arc::new(
                ConfigStore::from_config(paths.clone(), Arc::clone(&root_session), initial)
                    .expect("set config"),
            );
            let instances = Arc::new(
                InstanceStore::from_snapshot(
                    paths.clone(),
                    root_session,
                    InstanceRegistrySnapshot::default(),
                )
                .expect("load instances"),
            );
            let telemetry = Arc::new(TelemetryHub::new(
                config.clone(),
                Some("phc_test".to_string()),
                DEFAULT_POSTHOG_HOST.to_string(),
            ));
            let state = AppState::new_with_telemetry(
                AppStateInit {
                    app_name: "Axial".to_string(),
                    version: "test".to_string(),
                    config,
                    instances,
                    installs: Arc::new(InstallStore::new()),
                    sessions: Arc::new(SessionStore::new()),
                    performance: Arc::new(
                        PerformanceManager::load_for_startup(paths.performance_dir())
                            .expect("performance manager"),
                    ),
                    startup_warnings: Vec::new(),
                },
                telemetry,
            );

            Self { state, root, paths }
        }

        fn block_config_file(&self) {
            let _ = fs::remove_file(self.paths.config_file());
            fs::create_dir_all(self.paths.config_file()).expect("block config file with directory");
        }

        fn unblock_config_file(&self) {
            fs::remove_dir_all(self.paths.config_file()).expect("remove config file blocker");
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_paths(root: &Path) -> AppPaths {
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
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

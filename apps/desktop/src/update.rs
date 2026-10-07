//! Package effects are owned here; callers can request work but cannot provide
//! a feed, signing key, package path, or installation bytes.

use crate::lifecycle::DesktopLifecycle;
use axial_app::{
    tasks::TaskOwner,
    update::{
        NativeUpdateAdapter, UpdateAttempt, UpdateError, UpdateFlow, UpdateFuture, UpdateInfo,
        UpdateService,
    },
};
use semver::Version;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{AppHandle, Runtime};
use tauri_plugin_updater::{RemoteRelease, Update, Updater, UpdaterExt};

const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_PACKAGE_BYTES: u64 = 512 * 1024 * 1024;
const CONFIG_REQUIRED: &str =
    "In-app updates require a supported desktop package and a configured trusted release feed.";
const CHECK_FAILED: &str = "Could not check the trusted update feed. Try again later.";
const DOWNLOAD_FAILED: &str = "The update could not be downloaded and its signature verified. Check for updates and try again.";
const INSTALL_FAILED: &str = "The update could not be installed. Restart Axial before continuing.";
const INTERRUPTED: &str = "The update operation did not settle. Restart remains blocked until application work is settled.";

/// Called during native setup, before the main window accepts update commands.
/// Missing release inputs are an explicit unsupported state, never no-update success.
pub fn configure(
    app: &AppHandle,
    service: UpdateService,
    tasks: TaskOwner,
    lifecycle: DesktopLifecycle,
) {
    let result = (|| {
        if !supported_package() {
            return Err(CONFIG_REQUIRED);
        }
        let value = app
            .config()
            .plugins
            .0
            .get("updater")
            .ok_or(CONFIG_REQUIRED)?;
        let config: tauri_plugin_updater::Config =
            serde_json::from_value(value.clone()).map_err(|_| CONFIG_REQUIRED)?;
        validate_config(&config)?;
        app.plugin(tauri_plugin_updater::Builder::new().build())
            .map_err(|_| CONFIG_REQUIRED)?;
        let target = rewrite_target().ok_or(CONFIG_REQUIRED)?;
        let updater = build_updater(app, &target).map_err(|_| CONFIG_REQUIRED)?;
        let adapter = NativeUpdates {
            updater: Arc::new(updater),
            target,
            tasks,
            lifecycle,
            state: Arc::new(Mutex::new(Packages::default())),
        };
        service
            .attach_adapter(Arc::new(adapter))
            .map_err(|_| CONFIG_REQUIRED)
    })();
    if let Err(reason) = result {
        service.configure(Some(reason));
    }
}

fn validate_config(config: &tauri_plugin_updater::Config) -> Result<(), &'static str> {
    if config.pubkey.trim().is_empty()
        || config.endpoints.is_empty()
        || config.endpoints.iter().any(|url| {
            url.scheme() != "https" || !url.username().is_empty() || url.password().is_some()
        })
        || config.dangerous_insecure_transport_protocol
        || config.dangerous_accept_invalid_certs
        || config.dangerous_accept_invalid_hostnames
        || config.allow_downgrades
        || !config.require_signed_version
    {
        return Err(CONFIG_REQUIRED);
    }
    // Tauri decodes the public key and validates signatures during download.
    Ok(())
}

fn supported_package() -> bool {
    use tauri::utils::{config::BundleType, platform::bundle_type};
    matches!(
        bundle_type(),
        Some(BundleType::App | BundleType::AppImage | BundleType::Nsis | BundleType::Msi)
    )
}

fn rewrite_target() -> Option<String> {
    tauri_plugin_updater::target().map(|target| format!("axial-rewrite-{target}"))
}

fn build_updater<R: Runtime>(
    app: &AppHandle<R>,
    target: &str,
) -> tauri_plugin_updater::Result<Updater> {
    app.updater_builder()
        .target(target)
        .timeout(CHECK_TIMEOUT)
        .version_comparator(release_is_newer)
        .configure_client(|client| client.https_only(true))
        .build()
}

fn release_is_newer(current: Version, release: RemoteRelease) -> bool {
    let next = release.version;
    let channel_rank = |version: &Version| match version.pre.as_str().split('.').next() {
        Some("alpha") => 1,
        Some("beta") => 2,
        Some("rc") => 3,
        _ => 0,
    };
    (next.major, next.minor, next.patch)
        .cmp(&(current.major, current.minor, current.patch))
        .then_with(|| next.pre.is_empty().cmp(&current.pre.is_empty()))
        .then_with(|| channel_rank(&next).cmp(&channel_rank(&current)))
        .then_with(|| next.pre.cmp(&current.pre))
        .is_gt()
}

#[derive(Default)]
struct Packages {
    release: Option<Update>,
    staged: Option<VerifiedPackage>,
    applying: bool,
}

/// Only `download_verified` constructs this; its bytes never leave the native owner.
struct VerifiedPackage {
    update: Update,
    bytes: Vec<u8>,
}

#[derive(Clone)]
struct NativeUpdates {
    updater: Arc<Updater>,
    target: String,
    tasks: TaskOwner,
    lifecycle: DesktopLifecycle,
    state: Arc<Mutex<Packages>>,
}

impl NativeUpdateAdapter for NativeUpdates {
    fn check(&self, service: UpdateService) -> UpdateFuture<UpdateInfo> {
        let native = self.clone();
        Box::pin(async move {
            let attempt = {
                let mut state = native.state.lock().expect("native update state poisoned");
                let attempt = service.begin_check()?;
                state.release = None;
                attempt
            };
            let work_service = service.clone();
            let tasks = native.tasks.clone();
            let work = tasks.try_spawn((), move |cancel| async move {
                let guard = AttemptGuard::new(work_service.clone(), attempt);
                let result = tokio::select! {
                    result = native.updater.check() => result.map_err(|_| UpdateError::Failed(CHECK_FAILED.into())),
                    _ = cancel.cancelled() => Err(UpdateError::Failed("Update check cancelled during shutdown.".into())),
                };
                let result = result.and_then(|release| {
                    if let Some(update) = &release {
                        validate_release(update, &native.target)?;
                    }
                    let mut state = native.state.lock().expect("native update state poisoned");
                    let selected = release.as_ref().map(|update| (update.version.as_str(), notes_url(update)));
                    if !work_service.checked(attempt, selected) {
                        return Err(UpdateError::StaleRelease);
                    }
                    state.release = release;
                    work_service.snapshot().info.ok_or(UpdateError::StaleRelease)
                });
                guard.finish(&result);
                result
            });
            match work {
                Ok(work) => work
                    .join()
                    .await
                    .map_err(|_| UpdateError::Failed(INTERRUPTED.into()))?,
                Err(_) => {
                    service.failed(
                        attempt,
                        "Update check is blocked while application shutdown is in progress.",
                    );
                    Err(UpdateError::Busy)
                }
            }
        })
    }

    fn download(&self, service: UpdateService, version: String) -> UpdateFuture<UpdateFlow> {
        let native = self.clone();
        Box::pin(async move {
            let (attempt, update) = {
                let state = native.state.lock().expect("native update state poisoned");
                let update = state.release.clone().ok_or(UpdateError::StaleRelease)?;
                if update.version != version {
                    return Err(UpdateError::StaleRelease);
                }
                (service.begin_download(&version)?, update)
            };
            let work_service = service.clone();
            let tasks = native.tasks.clone();
            let work = tasks.try_spawn((), move |cancel| async move {
                let guard = AttemptGuard::new(work_service.clone(), attempt);
                let result = tokio::select! {
                    result = download_verified(update, &work_service, attempt) => result,
                    _ = cancel.cancelled() => Err(UpdateError::Failed("Update download cancelled during shutdown.".into())),
                };
                let result = result.and_then(|package| retain_stage(&native.state, &work_service, attempt, package));
                guard.finish(&result);
            });
            if work.is_err() {
                service.failed(
                    attempt,
                    "Update download is blocked while application shutdown is in progress.",
                );
                return Err(UpdateError::Busy);
            }
            // Dropping this waiter does not cancel TaskOwner's accepted download.
            Ok(service.snapshot().flow)
        })
    }

    fn apply(&self, service: UpdateService) -> UpdateFuture<UpdateFlow> {
        let native = self.clone();
        Box::pin(async move {
            {
                let mut state = native.state.lock().expect("native update state poisoned");
                if state.applying {
                    return Err(UpdateError::Busy);
                }
                if state.staged.is_none() {
                    return Err(UpdateError::NotReady);
                }
                state.applying = true;
            }
            // Applying is a terminal shell action: its worker must survive a
            // disconnected HTTP waiter and cannot live inside the owner it joins.
            #[cfg(windows)]
            let (accepted, response) = tokio::sync::oneshot::channel();
            let worker = tokio::spawn(async move {
                if let Err(message) = native.lifecycle.prepare_update().await {
                    native
                        .state
                        .lock()
                        .expect("native update state poisoned")
                        .applying = false;
                    return Err(UpdateError::Failed(message));
                }
                let mut installation = InstallationSettlement {
                    lifecycle: native.lifecycle.clone(),
                    started: false,
                };
                let attempt = service.begin_apply()?;
                let guard = AttemptGuard::new(service.clone(), attempt);
                #[cfg(windows)]
                {
                    // The Windows plugin exits the process after spawning its
                    // installer. Release the HTTP response first, then join that
                    // server before permitting the native installation effect.
                    let _ = accepted.send(service.snapshot().flow);
                    if let Err(message) = native.lifecycle.prepare_update_process_exit().await {
                        let error = UpdateError::Failed(message);
                        guard.finish(&Err::<(), _>(error.clone()));
                        return Err(error);
                    }
                }
                let package = native
                    .state
                    .lock()
                    .expect("native update state poisoned")
                    .staged
                    .take()
                    .ok_or(UpdateError::NotReady)?;
                installation.started = true;
                let result =
                    tokio::task::spawn_blocking(move || package.update.install(&package.bytes))
                        .await;
                let result = match result {
                    Ok(result) => {
                        installation.finish()?;
                        result.map_err(|_| UpdateError::Failed(INSTALL_FAILED.into()))
                    }
                    // Unwinding or runtime interruption cannot prove installation
                    // settled, so the lifecycle intentionally refuses restart.
                    Err(_) => Err(UpdateError::Failed(INTERRUPTED.into())),
                };
                if result.is_ok() {
                    service.installed(attempt);
                }
                guard.finish(&result);
                result?;
                Ok(service.snapshot().flow)
            });
            #[cfg(not(windows))]
            {
                worker
                    .await
                    .map_err(|_| UpdateError::Failed(INTERRUPTED.into()))?
            }
            #[cfg(windows)]
            {
                let mut worker = worker;
                tokio::select! {
                    biased;
                    result = &mut worker => result.map_err(|_| UpdateError::Failed(INTERRUPTED.into()))?,
                    result = response => result.map_err(|_| UpdateError::Failed(INTERRUPTED.into())),
                }
            }
        })
    }
}

/// Once shutdown is prepared, errors before the installer starts have no native
/// effect to join. After it starts, only its actual return can release the fence.
struct InstallationSettlement {
    lifecycle: DesktopLifecycle,
    started: bool,
}

impl InstallationSettlement {
    fn finish(&self) -> Result<(), UpdateError> {
        self.lifecycle
            .finish_update_settled()
            .map_err(UpdateError::Failed)
    }
}

impl Drop for InstallationSettlement {
    fn drop(&mut self) {
        if !self.started {
            let _ = self.lifecycle.finish_update_settled();
        }
    }
}

fn validate_release(update: &Update, target: &str) -> Result<(), UpdateError> {
    // Dynamic responses cannot establish the artifact's rewrite/platform slot.
    let selected = update
        .raw_json
        .get("platforms")
        .and_then(|value| value.get(target));
    if update.target != target
        || selected.is_none()
        || update.download_url.scheme() != "https"
        || !update.download_url.username().is_empty()
        || update.download_url.password().is_some()
        || update.signature.trim().is_empty()
        || update.raw_json.get("notes_url").is_some_and(|value| {
            value.as_str().is_none_or(|url| {
                !tauri::Url::parse(url).is_ok_and(|url| {
                    url.scheme() == "https" && url.username().is_empty() && url.password().is_none()
                })
            })
        })
    {
        return Err(UpdateError::Failed(
            "The release does not contain a signed update for this application and platform."
                .into(),
        ));
    }
    Ok(())
}

/// The trusted static feed supplies this optional extension to retain the
/// existing release-notes entrypoint. Standard Tauri `notes` is text, not a URL.
fn notes_url(update: &Update) -> &str {
    update
        .raw_json
        .get("notes_url")
        .and_then(|value| value.as_str())
        .unwrap_or("")
}

fn retain_stage(
    state: &Mutex<Packages>,
    service: &UpdateService,
    attempt: UpdateAttempt,
    package: VerifiedPackage,
) -> Result<(), UpdateError> {
    let mut state = state.lock().expect("native update state poisoned");
    if !service.staged(attempt) {
        return Err(UpdateError::StaleRelease);
    }
    state.staged = Some(package);
    Ok(())
}

async fn download_verified(
    mut update: Update,
    service: &UpdateService,
    attempt: UpdateAttempt,
) -> Result<VerifiedPackage, UpdateError> {
    update.timeout = Some(DOWNLOAD_TIMEOUT);
    let (limit, mut exceeded) = tokio::sync::mpsc::channel(1);
    let mut received = 0u64;
    let download = update.download(
        |chunk, total| {
            received = received.saturating_add(chunk as u64);
            if received > MAX_PACKAGE_BYTES || total.is_some_and(|total| total > MAX_PACKAGE_BYTES)
            {
                let _ = limit.try_send(());
            }
            service.progress(attempt, received, total);
        },
        || service.verifying(attempt),
    );
    let result = tokio::select! {
        biased;
        _ = exceeded.recv() => Err(UpdateError::Failed("The update package exceeds the supported size.".into())),
        result = download => result.map_err(|_| UpdateError::Failed(DOWNLOAD_FAILED.into())),
    };
    let bytes = result?;
    if bytes.len() as u64 > MAX_PACKAGE_BYTES {
        return Err(UpdateError::Failed(
            "The update package exceeds the supported size.".into(),
        ));
    }
    Ok(VerifiedPackage { update, bytes })
}

struct AttemptGuard {
    service: UpdateService,
    attempt: UpdateAttempt,
    finished: bool,
}
impl AttemptGuard {
    fn new(service: UpdateService, attempt: UpdateAttempt) -> Self {
        Self {
            service,
            attempt,
            finished: false,
        }
    }
    fn finish<T>(mut self, result: &Result<T, UpdateError>) {
        if let Err(error) = result {
            self.service.failed(self.attempt, &error.to_string());
        }
        self.finished = true;
    }
}
impl Drop for AttemptGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.service.failed(self.attempt, INTERRUPTED);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_app::update::UpdatePhase;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // RFC 8032's published first test key. It is never a configured release key.
    // The bytes below are inert text, not an executable or installable package.
    const PAYLOAD: &[u8] = b"Axial updater fixture only; not an installable package.\n";
    const KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IFJGQyA4MDMyIHRlc3Qga2V5LCBuZXZlciBhIHJlbGVhc2Uga2V5ClJXUUJBZ01FQlFZSENOZGFtQUdDc1FxMzFVdiswOGxrQnpvTzRYTHoycVlqSmE4Q0dtajNCMUVhCg==";
    const SIGNATURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHRlc3QgZml4dHVyZSBvbmx5ClJXUUJBZ01FQlFZSENFemNDaVFkeUFsc0FYMTBlOXVRR2lON3MyR3RIeE1KUkwxMnNTMlVHWHB0Vmlmb3BFL29TMUs4SUJBbS8vdzNseTBmQnNTZUVxVzEyQTBkMTlqY1ZBVT0KdHJ1c3RlZCBjb21tZW50OiB0aW1lc3RhbXA6MTcwMDAwMDAwMAlmaWxlOmZpeHR1cmUJdmVyc2lvbjo5LjkuOQovakppZ3RuT3NBT1pYQzBJeGtTN1hhSjBZOHpmd0wxRkI0ZStOOEJTdjQySHNJMHdlQzl6cmQrRUxNZnRMWHdoNHVLUFdyeGZOUjdxeDltR0pTcU1CZz09Cg==";
    const TARGET: &str = "axial-rewrite-fixture-platform";

    struct Fixture {
        _app: tauri::App<tauri::test::MockRuntime>,
        updater: Arc<Updater>,
        server: tokio::task::JoinHandle<()>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    async fn fixture(
        current_version: &str,
        target: &str,
        version: &str,
        payload: &[u8],
        signature: &str,
    ) -> Fixture {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let manifest = serde_json::to_vec(&json!({
            "version": version,
            "platforms": { target: { "url": format!("{origin}/artifact"), "signature": signature } }
        }))
        .unwrap();
        let payload = payload.to_vec();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut chunk = [0; 1024];
                    let count = socket.read(&mut chunk).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..count]);
                    assert!(request.len() < 16 * 1024);
                }
                let body = if request.starts_with(b"GET /artifact ") {
                    &payload
                } else {
                    &manifest
                };
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(body).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        let mut context = tauri::test::mock_context(tauri::test::noop_assets());
        context.package_info_mut().version = current_version.parse().unwrap();
        context.config_mut().plugins.0.insert(
            "updater".into(),
            json!({
                "pubkey": KEY,
                "endpoints": [format!("{origin}/feed")],
                "dangerousInsecureTransportProtocol": true,
                "requireSignedVersion": true
            }),
        );
        let app = tauri::test::mock_builder()
            .plugin(tauri_plugin_updater::Builder::new().build())
            .build(context)
            .unwrap();
        let updater = app
            .updater_builder()
            .target(TARGET)
            .timeout(Duration::from_secs(5))
            .version_comparator(release_is_newer)
            .no_proxy()
            .build()
            .unwrap();
        Fixture {
            _app: app,
            updater: Arc::new(updater),
            server,
        }
    }

    #[tokio::test]
    async fn update_check_preserves_release_channel_precedence() {
        for (current, version, available) in [
            ("0.4.0-dev.5", "0.4.0-alpha.1", true),
            ("0.4.0-dev.5", "0.4.0-beta.1", true),
            ("0.4.0-alpha.1", "0.4.0-dev.6", false),
            ("0.4.0-beta.1", "0.4.0-dev.6", false),
            ("0.4.0-beta.1", "0.4.0-alpha.9", false),
            ("0.4.0-alpha.1", "0.4.0-beta.1", true),
            ("0.4.0-beta.1", "0.4.0-rc.1", true),
            ("0.4.0-dev.9", "0.4.0-dev.10", true),
            ("0.4.0-rc.9", "0.4.0-rc.10", true),
            ("0.4.0-dev.10", "0.4.0-dev.9", false),
            ("0.4.0-rc.1", "0.4.0", true),
            ("0.4.0", "0.4.0-rc.10", false),
            ("0.4.0", "0.4.0", false),
            ("0.4.0-dev.5", "0.4.0-dev.5", false),
            ("0.4.1-dev.1", "0.4.0", false),
            ("0.4.0", "0.4.1-dev.1", true),
            ("0.4.0+build.1", "0.4.0+build.2", false),
            ("0.4.0-dev.5+build.1", "0.4.0-dev.5+build.2", false),
        ] {
            let fixture = fixture(current, TARGET, version, PAYLOAD, SIGNATURE).await;
            let update = fixture.updater.check().await.unwrap();
            assert_eq!(
                update.as_ref().map(|update| update.version.as_str()),
                available.then_some(version),
                "{current} -> {version}"
            );
        }
    }

    async fn lifecycle_fixture() -> (tempfile::TempDir, DesktopLifecycle, TaskOwner) {
        let directory = tempfile::tempdir().unwrap();
        let services = axial_api::start_in_profile(directory.path().join("rewrite"), None)
            .await
            .unwrap();
        let skin_files = crate::native_skin::NativeSkinFiles::new(
            services.library.clone(),
            services.tasks.clone(),
        );
        let lifecycle = DesktopLifecycle::new(
            services.tasks.clone(),
            services.server,
            crate::discord_presence::PresenceObserver::disabled_for_test(),
            skin_files,
            services.skins,
        )
        .without_interface_preferences_for_test();
        (directory, lifecycle, services.tasks)
    }

    fn download_service(version: &str) -> (UpdateService, UpdateAttempt) {
        let service = UpdateService::new("0.1.0", "fixture", "fixture");
        service.configure(None);
        let check = service.begin_check().unwrap();
        assert!(service.checked(check, Some((version, ""))));
        let attempt = service.begin_download(version).unwrap();
        (service, attempt)
    }

    #[tokio::test]
    async fn plugin_verifies_exact_bytes_before_native_stage_accepts_them() {
        let fixture = fixture(
            env!("CARGO_PKG_VERSION"),
            TARGET,
            "9.9.9",
            PAYLOAD,
            SIGNATURE,
        )
        .await;
        let update = fixture.updater.check().await.unwrap().unwrap();
        let (service, attempt) = download_service("9.9.9");
        let package = download_verified(update, &service, attempt).await.unwrap();
        assert_eq!(package.bytes, PAYLOAD);
        let state = Mutex::new(Packages::default());
        retain_stage(&state, &service, attempt, package).unwrap();
        assert_eq!(service.snapshot().flow.phase, UpdatePhase::Ready);
        assert!(state.lock().unwrap().staged.is_some());
        assert!(service.snapshot().flow.can_apply);
        // No test invokes install or requests restart.
    }

    #[tokio::test]
    async fn bad_signature_and_tampered_bytes_never_create_a_stage() {
        for (payload, signature) in [
            (PAYLOAD, "invalid-signature"),
            (b"tampered".as_slice(), SIGNATURE),
        ] {
            let fixture = fixture(
                env!("CARGO_PKG_VERSION"),
                TARGET,
                "9.9.9",
                payload,
                signature,
            )
            .await;
            let update = fixture.updater.check().await.unwrap().unwrap();
            let (service, attempt) = download_service("9.9.9");
            let guard = AttemptGuard::new(service.clone(), attempt);
            let result = download_verified(update, &service, attempt).await;
            assert!(result.is_err());
            guard.finish(&result);
            assert_eq!(service.snapshot().flow.phase, UpdatePhase::Failed);
            assert!(!service.snapshot().flow.can_apply);
            assert_eq!(service.begin_apply(), Err(UpdateError::NotReady));
        }
    }

    #[tokio::test]
    async fn valid_signature_cannot_be_relabelled_as_a_different_version() {
        let fixture = fixture(
            env!("CARGO_PKG_VERSION"),
            TARGET,
            "99.0.0",
            PAYLOAD,
            SIGNATURE,
        )
        .await;
        let update = fixture.updater.check().await.unwrap().unwrap();
        let (service, attempt) = download_service("99.0.0");
        assert!(download_verified(update, &service, attempt).await.is_err());
        assert!(!service.snapshot().flow.can_apply);
    }

    #[tokio::test]
    async fn a_release_for_another_platform_is_not_an_available_update() {
        let fixture = fixture(
            env!("CARGO_PKG_VERSION"),
            "axial-rewrite-other-platform",
            "9.9.9",
            PAYLOAD,
            SIGNATURE,
        )
        .await;
        assert!(matches!(
            fixture.updater.check().await,
            Err(tauri_plugin_updater::Error::TargetNotFound(_))
        ));
    }

    #[tokio::test]
    async fn release_notes_preserve_only_a_valid_https_entrypoint() {
        let fixture = fixture(
            env!("CARGO_PKG_VERSION"),
            TARGET,
            "9.9.9",
            PAYLOAD,
            SIGNATURE,
        )
        .await;
        let mut update = fixture.updater.check().await.unwrap().unwrap();
        update.download_url = "https://example.invalid/rewrite/package".parse().unwrap();
        assert!(validate_release(&update, TARGET).is_ok());
        assert_eq!(notes_url(&update), "");
        for value in [
            json!("file:///tmp/release"),
            json!("https://user:password@example.invalid/notes"),
            json!(42),
        ] {
            update.raw_json["notes_url"] = value;
            assert!(validate_release(&update, TARGET).is_err());
        }
        update.raw_json["notes_url"] = json!("https://example.invalid/rewrite/notes");
        assert!(validate_release(&update, TARGET).is_ok());
        assert_eq!(notes_url(&update), "https://example.invalid/rewrite/notes");
    }

    #[tokio::test]
    async fn stale_verified_completion_cannot_replace_a_newer_download() {
        let fixture = fixture(
            env!("CARGO_PKG_VERSION"),
            TARGET,
            "9.9.9",
            PAYLOAD,
            SIGNATURE,
        )
        .await;
        let update = fixture.updater.check().await.unwrap().unwrap();
        let (service, first) = download_service("9.9.9");
        let package = download_verified(update, &service, first).await.unwrap();
        service.failed(first, "First download cancelled.");
        let second = service.begin_download("9.9.9").unwrap();
        service.progress(second, 1, Some(10));
        let before = service.snapshot();
        let state = Mutex::new(Packages::default());
        assert_eq!(
            retain_stage(&state, &service, first, package),
            Err(UpdateError::StaleRelease)
        );
        assert!(state.lock().unwrap().staged.is_none());
        assert_eq!(service.snapshot(), before);
    }

    #[tokio::test]
    async fn apply_rejection_before_installation_releases_the_shutdown_fence() {
        let fixture = fixture(
            env!("CARGO_PKG_VERSION"),
            TARGET,
            "9.9.9",
            PAYLOAD,
            SIGNATURE,
        )
        .await;
        let update = fixture.updater.check().await.unwrap().unwrap();
        let (service, attempt) = download_service("9.9.9");
        let package = download_verified(update, &service, attempt).await.unwrap();
        // A cancelled domain attempt cannot authorize installation even when
        // the native owner still has successfully verified, inert fixture bytes.
        service.failed(attempt, "The download attempt was cancelled.");
        let (_directory, lifecycle, tasks) = lifecycle_fixture().await;
        let native = NativeUpdates {
            updater: fixture.updater.clone(),
            target: TARGET.into(),
            tasks,
            lifecycle: lifecycle.clone(),
            state: Arc::new(Mutex::new(Packages {
                staged: Some(package),
                ..Default::default()
            })),
        };
        assert_eq!(native.apply(service).await, Err(UpdateError::NotReady));
        // This only joins lifecycle work; it never requests a process restart.
        lifecycle
            .prepare_exit(crate::lifecycle::TerminalIntent::Restart)
            .await
            .unwrap();
        assert!(!lifecycle.exit_allowed());
    }

    #[tokio::test]
    async fn losing_an_installation_waiter_cannot_claim_its_effects_settled() {
        let (_directory, lifecycle, _tasks) = lifecycle_fixture().await;
        lifecycle.prepare_update().await.unwrap();
        let installation = InstallationSettlement {
            lifecycle: lifecycle.clone(),
            started: true,
        };
        drop(installation);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                lifecycle.shutdown_after_event_loop()
            )
            .await
            .is_err()
        );
        assert!(!lifecycle.exit_allowed());
        // The retained owner supplies settlement separately after its effects
        // return. No installer is invoked by this fixture.
        lifecycle.finish_update_settled().unwrap();
        lifecycle.shutdown_after_event_loop().await;
    }

    #[test]
    fn release_configuration_never_relaxes_transport_or_version_trust() {
        let trusted = json!({ "pubkey": KEY, "endpoints": ["https://example.invalid/rewrite/latest.json"], "requireSignedVersion": true });
        let config = serde_json::from_value(trusted.clone()).unwrap();
        assert!(validate_config(&config).is_ok());
        for (field, value) in [
            ("requireSignedVersion", json!(false)),
            ("allowDowngrades", json!(true)),
            ("dangerousInsecureTransportProtocol", json!(true)),
            ("dangerousAcceptInvalidCerts", json!(true)),
            ("dangerousAcceptInvalidHostnames", json!(true)),
            ("pubkey", json!("")),
            ("endpoints", json!([])),
        ] {
            let mut invalid = trusted.clone();
            invalid[field] = value;
            let config = serde_json::from_value(invalid).unwrap();
            assert!(validate_config(&config).is_err(), "{field}");
        }
    }
}

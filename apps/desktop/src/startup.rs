//! A failed filesystem admission may still own a native preservation obligation.
//! Keep it alive outside the error UI until preservation explicitly succeeds.
use axial_api::StartupError;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};
use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

const PRESERVATION_RETRY: Duration = Duration::from_secs(2);

#[derive(Default)]
struct ResetDecision(AtomicU8);

impl ResetDecision {
    fn choose(&self, confirmed: bool) {
        let _ = self.0.compare_exchange(
            0,
            if confirmed { 1 } else { 2 },
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn finish(&self) -> bool {
        // An unexpected event-loop return closes the decision gate. A late
        // callback cannot turn preservation into deletion afterward.
        self.choose(false);
        self.0.load(Ordering::Acquire) == 1
    }
}

struct Failure {
    message: String,
    error: Mutex<Option<StartupError>>,
    #[cfg(debug_assertions)]
    reset: Mutex<Option<crate::reset::PendingReset>>,
    resetting: bool,
    safe_message: Mutex<Option<String>>,
    preserved: AtomicBool,
    attempts: AtomicU64,
}

pub async fn report(context: tauri::Context<tauri::Wry>, error: StartupError) -> String {
    let message = error.to_string();
    tracing::error!(error = %message, "Desktop startup failed; preserving application ownership");
    let failure = Arc::new(Failure {
        message,
        error: Mutex::new(Some(error)),
        #[cfg(debug_assertions)]
        reset: Mutex::new(None),
        resetting: false,
        safe_message: Mutex::new(None),
        preserved: AtomicBool::new(false),
        attempts: AtomicU64::new(0),
    });
    report_failure(context, failure).await
}

/// Reuse the retained retry owner after the old desktop/services have dropped.
/// Tao permits only one event loop per process: post-exit reset stays headless,
/// retains exact authority and logs retries until restart is safe.
#[cfg(debug_assertions)]
pub async fn complete_reset(pending: crate::reset::PendingReset) {
    let failure = Arc::new(Failure {
        message: "Reset is incomplete because launcher-owned data could not be deleted. Axial will retry.".into(),
        error: Mutex::new(None),
        reset: Mutex::new(Some(pending)),
        resetting: true,
        safe_message: Mutex::new(None),
        preserved: AtomicBool::new(false),
        attempts: AtomicU64::new(0),
    });
    preserve(failure).await;
}

async fn report_failure(context: tauri::Context<tauri::Wry>, failure: Arc<Failure>) -> String {
    #[cfg(debug_assertions)]
    let reset_decision = failure
        .error
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .is_some_and(StartupError::interrupted_reset)
        .then(|| Arc::new(ResetDecision::default()));
    #[cfg(not(debug_assertions))]
    let reset_decision: Option<Arc<ResetDecision>> = None;
    let setup_decision = reset_decision.clone();
    let (callback_finished, callback_completion) = tokio::sync::oneshot::channel();
    let page_failure = failure.clone();
    let window_failure = failure.clone();
    let setup_failure = failure.clone();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .register_uri_scheme_protocol("axial-startup", move |_, _| {
            tauri::http::Response::builder()
                .header("Content-Type", "text/html; charset=utf-8")
                .header("Cache-Control", "no-store")
                .header("Content-Security-Policy", "default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'")
                .body(failure_page(&page_failure).into_bytes())
                .expect("startup response uses constant valid headers")
        })
        .on_window_event(move |window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window_failure.preserved.load(Ordering::Acquire) { window.app_handle().exit(1); }
                else { api.prevent_close(); }
            }
        })
        .setup(move |app| {
            if let Some(decision) = setup_decision {
                // This is the process's first and only native event loop. No
                // webview/storage owner is created in the interrupted profile.
                let handle = app.handle().clone();
                app.dialog().message("The previous development-profile reset was interrupted. The reset has not continued. Reset profile deletes the current launcher-owned files, including files added since the interruption. Preserve files closes Axial without continuing the reset.")
                    .title("Development profile reset interrupted")
                    .kind(MessageDialogKind::Warning)
                    .buttons(MessageDialogButtons::OkCancelCustom("Reset profile".into(), "Preserve files".into()))
                    .show(move |confirmed| {
                        decision.choose(confirmed);
                        handle.exit(1);
                        drop(handle);
                        // The post-loop reset waits until this callback has
                        // released its last explicitly retained AppHandle.
                        let _ = callback_finished.send(());
                    });
                return Ok(());
            }
            // This window has no native capability or domain connection.
            let window = WebviewWindowBuilder::new(app, "startup-error", WebviewUrl::CustomProtocol(
                tauri::Url::parse("axial-startup://localhost/").expect("fixed startup URL"),
            ))
                .title("Axial could not start")
                .inner_size(560.0, 300.0)
                .resizable(false)
                .center()
                .on_navigation(startup_navigation_allowed)
                .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
                .build();
            if window.is_err() {
                tracing::error!("Could not open the preservation window; using native error dialog");
                show_preservation_dialog(app.handle().clone(), setup_failure);
            }
            Ok(())
        })
        .build(context);
    let app = match app {
        Ok(app) => app,
        Err(_) => {
            // UI failure does not permit dropping an unresolved root owner.
            tracing::error!(
                "Could not open the startup error window; preservation will keep retrying"
            );
            return preserve(failure).await;
        }
    };
    #[cfg(target_os = "macos")]
    let termination = match crate::termination::install(app.handle()) {
        Ok(guard) => guard,
        Err(_) => {
            tracing::error!(
                "Could not guard native termination; preserving without the error window"
            );
            return preserve(failure).await;
        }
    };
    if let Some(decision) = reset_decision {
        #[cfg(debug_assertions)]
        let restart_environment = app.env();
        app.run_return(|_, _| {});
        #[cfg(target_os = "macos")]
        drop(termination);
        let confirmed = decision.finish();
        #[cfg(debug_assertions)]
        if confirmed && callback_completion.await.is_ok() {
            let error = failure
                .error
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            let retained = Arc::new(Failure {
                message: "Reset is incomplete. Axial will retain the profile and retry.".into(),
                error: Mutex::new(error),
                reset: Mutex::new(None),
                resetting: true,
                safe_message: Mutex::new(None),
                preserved: AtomicBool::new(false),
                attempts: AtomicU64::new(0),
            });
            preserve(retained).await;
            tauri::process::restart(&restart_environment);
        }
        #[cfg(not(debug_assertions))]
        let _ = (confirmed, callback_completion);
        // Cancel, window close, UI failure and unexpected loop return preserve
        // ownership. None can fall through to normal application startup.
        return preserve(failure).await;
    }
    let worker_failure = failure.clone();
    let worker = tokio::spawn(async move { preserve(worker_failure).await });
    let exit_failure = failure.clone();
    app.run_return(move |_, event| {
        if let RunEvent::ExitRequested { api, .. } = event {
            if !exit_failure.preserved.load(Ordering::Acquire) {
                api.prevent_exit();
            }
        }
    });
    #[cfg(target_os = "macos")]
    drop(termination);
    match worker.await {
        Ok(message) => message,
        Err(_) => {
            tracing::error!(
                "Startup preservation waiter stopped; retaining ownership and retrying"
            );
            preserve(failure).await
        }
    }
}

fn show_preservation_dialog(app: tauri::AppHandle, failure: Arc<Failure>) {
    let status = if failure.preserved.load(Ordering::Acquire) {
        "Application files have been preserved. You can now close Axial."
    } else {
        "Application files are still being preserved. Axial will retry and cannot close until preservation succeeds."
    };
    let handle = app.clone();
    app.dialog()
        .message(format!("{}\n\n{status}", failure.message))
        .title("Axial could not start")
        .kind(MessageDialogKind::Error)
        .show(move |_| {
            if failure.preserved.load(Ordering::Acquire) {
                handle.exit(1);
            } else {
                show_preservation_dialog(handle, failure);
            }
        });
}

async fn preserve(failure: Arc<Failure>) -> String {
    loop {
        failure.attempts.fetch_add(1, Ordering::Relaxed);
        let retained = failure.clone();
        match tokio::task::spawn_blocking(move || try_preserve(&retained)).await {
            Ok(Some(message)) => {
                failure.preserved.store(true, Ordering::Release);
                if failure.resetting {
                    tracing::info!("Development profile reset completed; restart is permitted");
                } else {
                    tracing::error!(error = %message, "Startup failed; application files have been preserved");
                }
                return message;
            }
            Ok(None) => tracing::warn!(
                resetting = failure.resetting,
                "Filesystem settlement is incomplete; application ownership remains retained"
            ),
            Err(_) => {
                tracing::error!("Startup preservation attempt was interrupted; retaining ownership")
            }
        }
        tokio::time::sleep(PRESERVATION_RETRY).await;
    }
}

fn startup_navigation_allowed(url: &tauri::Url) -> bool {
    url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && matches!(
            (url.scheme(), url.host_str()),
            ("axial-startup", Some("localhost")) | ("http", Some("axial-startup.localhost"))
        )
}

fn failure_page(failure: &Failure) -> String {
    let preserved = failure.preserved.load(Ordering::Acquire);
    let refresh = if preserved {
        ""
    } else {
        "<meta http-equiv=\"refresh\" content=\"2\">"
    };
    let status = if preserved {
        "Application files have been preserved. Close this window, then try opening Axial again."
            .to_string()
    } else {
        format!(
            "Application files need to be preserved before Axial can close. Retrying every two seconds. Attempts: {}.",
            failure.attempts.load(Ordering::Relaxed)
        )
    };
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">{refresh}<style>html{{color-scheme:light dark}}body{{font:14px/1.5 system-ui,sans-serif;margin:32px;background:Canvas;color:CanvasText}}h1{{font-size:22px;line-height:1.2;margin:0 0 16px}}p{{overflow-wrap:anywhere}}#status{{margin-top:24px}}</style><title>Axial could not start</title></head><body><h1>Axial could not start</h1><p>{}</p><p id="status" role="status">{}</p></body></html>"#,
        escape_html(&failure.message),
        escape_html(&status)
    )
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn try_preserve(failure: &Failure) -> Option<String> {
    #[cfg(debug_assertions)]
    if failure.resetting {
        let mut reset = failure
            .reset
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if reset.is_none() {
            let mut error = failure
                .error
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(pending) = error.as_mut() {
                match pending.take_interrupted_reset_session() {
                    Ok(session) => {
                        *reset = Some(crate::reset::PendingReset::from_confirmed_startup(session));
                        error.take();
                    }
                    Err(_) => return None,
                }
            }
        }
        if let Some(pending) = reset.as_mut() {
            if pending.try_clear().is_err() {
                return None;
            }
            reset.take();
            let message = "Development profile reset completed.".to_string();
            *failure
                .safe_message
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(message.clone());
            return Some(message);
        }
        return failure
            .safe_message
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
    }
    let mut error = failure
        .error
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some(pending) = error.take() else {
        return failure
            .safe_message
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
    };
    match pending.try_preserve() {
        Ok(message) => {
            *failure
                .safe_message
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(message.clone());
            Some(message)
        }
        Err(pending) => {
            *error = Some(pending);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_reset_requires_fresh_choice_and_late_callbacks_cannot_authorize_it() {
        let unexpected = ResetDecision::default();
        assert!(!unexpected.finish());
        unexpected.choose(true);
        assert!(!unexpected.finish());
        let cancel = ResetDecision::default();
        cancel.choose(false);
        assert!(!cancel.finish());
        let accepted = ResetDecision::default();
        accepted.choose(true);
        assert!(accepted.finish());
    }

    #[tokio::test]
    async fn simple_startup_failure_is_not_exitable_until_preservation_is_explicit() {
        let failure = Arc::new(Failure {
            message: "Existing <files> remain & protected.".into(),
            error: Mutex::new(Some(StartupError::from("Safe startup failure."))),
            #[cfg(debug_assertions)]
            reset: Mutex::new(None),
            resetting: false,
            safe_message: Mutex::new(None),
            preserved: AtomicBool::new(false),
            attempts: AtomicU64::new(0),
        });
        assert!(!failure.preserved.load(Ordering::Acquire));
        let blocked = failure_page(&failure);
        assert!(blocked.contains("&lt;files&gt; remain &amp; protected"));
        assert!(blocked.contains("http-equiv=\"refresh\""));
        assert_eq!(preserve(failure.clone()).await, "Safe startup failure.");
        assert!(failure.preserved.load(Ordering::Acquire));
        assert!(failure.error.lock().unwrap().is_none());
        assert!(!failure_page(&failure).contains("http-equiv=\"refresh\""));
        assert_eq!(
            try_preserve(&failure).as_deref(),
            Some("Safe startup failure.")
        );
    }

    #[test]
    fn startup_error_surface_cannot_navigate_to_the_main_app_or_remote_content() {
        for raw in [
            "axial-startup://localhost/",
            "http://axial-startup.localhost/",
        ] {
            assert!(startup_navigation_allowed(&tauri::Url::parse(raw).unwrap()));
        }
        for raw in [
            "tauri://localhost/",
            "https://example.com/",
            "http://127.0.0.1:1234/",
            "axial-startup://user@localhost/",
            "axial-startup://localhost.evil/",
        ] {
            assert!(
                !startup_navigation_allowed(&tauri::Url::parse(raw).unwrap()),
                "{raw}"
            );
        }
    }
}

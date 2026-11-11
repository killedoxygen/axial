//! A failed filesystem admission may still own a native preservation obligation.
//! Keep it alive outside the error UI until preservation explicitly succeeds.
use axial_api::StartupError;
use axial_app::tasks::retry_until_settled;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

struct Failure {
    message: String,
    error: Mutex<Option<StartupError>>,
    safe_message: Mutex<Option<String>>,
    preserved: AtomicBool,
    attempts: AtomicU64,
}

impl Failure {
    fn new(message: String, error: StartupError) -> Self {
        Self {
            message,
            error: Mutex::new(Some(error)),
            safe_message: Mutex::new(None),
            preserved: AtomicBool::new(false),
            attempts: AtomicU64::new(0),
        }
    }
}

pub async fn report(context: tauri::Context<tauri::Wry>, error: StartupError) -> String {
    let message = error.to_string();
    tracing::error!(error = %message, "Desktop startup failed; preserving application ownership");
    report_failure(context, Arc::new(Failure::new(message, error))).await
}

async fn report_failure(context: tauri::Context<tauri::Wry>, failure: Arc<Failure>) -> String {
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
    let message = retry_until_settled("Startup file preservation", || {
        failure.attempts.fetch_add(1, Ordering::Relaxed);
        let retained = failure.clone();
        async move {
            match tokio::task::spawn_blocking(move || try_preserve(&retained)).await {
                Ok(Some(message)) => Ok(message),
                Ok(None) => Err("filesystem settlement is incomplete"),
                Err(_) => Err("the preservation attempt was interrupted"),
            }
        }
    })
    .await;
    failure.preserved.store(true, Ordering::Release);
    tracing::error!(error = %message, "Startup failed; application files have been preserved");
    message
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
    let mut error = crate::lock(&failure.error);
    let Some(pending) = error.take() else {
        return crate::lock(&failure.safe_message).clone();
    };
    match pending.try_preserve() {
        Ok(message) => {
            *crate::lock(&failure.safe_message) = Some(message.clone());
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

    #[tokio::test]
    async fn simple_startup_failure_is_not_exitable_until_preservation_is_explicit() {
        let failure = Arc::new(Failure::new(
            "Existing <files> remain & protected.".into(),
            StartupError::from("Safe startup failure."),
        ));
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

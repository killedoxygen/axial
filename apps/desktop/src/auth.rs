//! The OAuth window owns only the native interaction. Account revision checks,
//! callback validation and credential commits remain in the account service.
use crate::bootstrap::DesktopBootstrap;
use axial_app::{
    accounts::{
        microsoft,
        session::{AuthError, AuthService},
    },
    tasks::CancellationToken,
};
use serde::Serialize;
use std::{
    path::PathBuf,
    sync::{Arc, Weak},
    time::Duration,
};
use tauri::{AppHandle, State, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};
use tokio::sync::{Mutex, mpsc};

const LOGIN_WINDOW: &str = "microsoft-sign-in";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone)]
pub struct NativeSignIn {
    auth: Weak<AuthService>,
    webview_directory: PathBuf,
    gate: Arc<Mutex<()>>,
}

impl NativeSignIn {
    pub fn new(auth: Arc<AuthService>, webview_directory: PathBuf) -> Self {
        Self {
            auth: Arc::downgrade(&auth),
            webview_directory,
            gate: Arc::new(Mutex::new(())),
        }
    }
}

#[derive(Serialize)]
pub struct NativeMicrosoftSignIn {
    status: &'static str,
    login_id: Option<String>,
    profile_name: Option<String>,
    owns_minecraft_java: Option<bool>,
}

impl NativeMicrosoftSignIn {
    fn cancelled() -> Self {
        Self {
            status: "cancelled",
            login_id: None,
            profile_name: None,
            owns_minecraft_java: None,
        }
    }
}

#[tauri::command]
pub async fn microsoft_sign_in(
    app: AppHandle,
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
    sign_in: State<'_, NativeSignIn>,
) -> Result<NativeMicrosoftSignIn, String> {
    crate::window::require_main_window(&window, &bootstrap)?;
    let sign_in = sign_in.inner().clone();
    let permit = sign_in
        .gate
        .clone()
        .try_lock_owned()
        .map_err(|_| "Microsoft sign-in is already open.")?;
    // The native facade can outlive main through Tauri's managed state. Retain
    // the actual service once at admission, then carry it through all effects.
    let auth = sign_in
        .auth
        .upgrade()
        .ok_or("Microsoft sign-in is unavailable while the application is closing.")?;
    let tasks = auth.task_owner().clone();
    tasks
        .try_spawn(permit, move |cancel| sign_in.run(app, cancel, auth))
        .map_err(|_| "Microsoft sign-in is unavailable while the application is closing.")?
        .join()
        .await
        .map_err(|_| "Microsoft sign-in stopped before it could finish.")?
}

enum LoginEvent {
    Callback(Url),
    Closed,
}

/// Destroy is native cleanup only; dropping a response does not drop this owner.
struct LoginWindow<R: tauri::Runtime>(WebviewWindow<R>);
impl<R: tauri::Runtime> Drop for LoginWindow<R> {
    fn drop(&mut self) {
        let _ = self.0.destroy();
    }
}

impl NativeSignIn {
    async fn run<R: tauri::Runtime>(
        self,
        app: AppHandle<R>,
        cancel: CancellationToken,
        auth: Arc<AuthService>,
    ) -> Result<NativeMicrosoftSignIn, String> {
        let pending = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(NativeMicrosoftSignIn::cancelled()),
            result = auth.begin_login() => result.map_err(auth_error)?,
        };
        let url = Url::parse(pending.auth_request_uri())
            .map_err(|_| "Microsoft sign-in returned an invalid URL.")?;
        if !oauth_navigation_allowed(&url) {
            return Err("Microsoft sign-in returned an invalid URL.".into());
        }
        tokio::fs::create_dir_all(&self.webview_directory)
            .await
            .map_err(|_| "Could not prepare the Microsoft sign-in window.")?;
        let (events, mut receiver) = mpsc::channel(1);
        let navigation_events = events.clone();
        let window = WebviewWindowBuilder::new(&app, LOGIN_WINDOW, WebviewUrl::External(url))
            .title("Sign in with Microsoft")
            .inner_size(520.0, 720.0)
            .resizable(true)
            .center()
            .data_directory(self.webview_directory)
            // WKWebView ignores the directory; OAuth cookies must not cross profiles.
            .incognito(cfg!(target_os = "macos"))
            .on_navigation(move |url| {
                if microsoft::is_redirect_url(url) {
                    let _ = navigation_events.try_send(LoginEvent::Callback(url.clone()));
                    return false;
                }
                oauth_navigation_allowed(url)
            })
            .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
            .build()
            .map_err(|_| "Could not open the Microsoft sign-in window.")?;
        window.on_window_event(move |event| {
            if matches!(event, WindowEvent::Destroyed) {
                let _ = events.try_send(LoginEvent::Closed);
            }
        });
        let window = LoginWindow(window);
        let callback = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(NativeMicrosoftSignIn::cancelled()),
            _ = tokio::time::sleep(LOGIN_TIMEOUT) => return Err("Microsoft sign-in timed out.".into()),
            event = receiver.recv() => match event {
                Some(LoginEvent::Callback(callback)) => callback,
                Some(LoginEvent::Closed) | None => return Ok(NativeMicrosoftSignIn::cancelled()),
            },
        };
        drop(window);
        match auth.finish_login(pending, callback).await {
            Ok(capture) => Ok(NativeMicrosoftSignIn {
                status: "authenticated",
                login_id: capture.login_id().map(str::to_owned),
                profile_name: Some(capture.display_name().into()),
                owns_minecraft_java: Some(true),
            }),
            Err(AuthError::Provider(error))
                if error.kind() == microsoft::MicrosoftAuthErrorKind::Cancelled =>
            {
                Ok(NativeMicrosoftSignIn::cancelled())
            }
            Err(error) => Err(auth_error(error)),
        }
    }
}

fn auth_error(error: AuthError) -> String {
    match error {
        AuthError::Provider(error) => error.user_message(),
        other => other.to_string(),
    }
}

fn oauth_navigation_allowed(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retained_login_future_owns_auth_until_it_returns_but_idle_facade_does_not() {
        let directory =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let services = axial_api::start_in_profile(directory.path().join("rewrite"), None)
            .await
            .unwrap();
        let native = NativeSignIn::new(
            services.auth.clone(),
            services.profile_root.join("oauth-webview"),
        );
        let auth = Arc::downgrade(&services.auth);
        let metadata = Arc::downgrade(services.settings.metadata());
        let app = tauri::test::mock_app();
        let cancel = CancellationToken::new();
        cancel.cancel();
        // Exercise the actual native login future's ownership without opening
        // an OAuth window or contacting a provider. Cancellation wins first.
        let login =
            native
                .clone()
                .run(app.handle().clone(), cancel, native.auth.upgrade().unwrap());
        services.server.shutdown().await.unwrap();
        drop(services);
        assert!(auth.upgrade().is_some());
        assert!(metadata.upgrade().is_some());
        assert_eq!(login.await.unwrap().status, "cancelled");
        assert!(auth.upgrade().is_none());
        assert!(metadata.upgrade().is_none());
        assert!(native.auth.upgrade().is_none());
    }

    #[test]
    fn oauth_window_navigation_cannot_enter_local_app_or_script_origins() {
        assert!(oauth_navigation_allowed(
            &Url::parse("https://login.live.com/authorize").unwrap()
        ));
        for raw in [
            "tauri://localhost/",
            "http://127.0.0.1:34123/",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://user:pass@login.live.com/",
        ] {
            assert!(
                !oauth_navigation_allowed(&Url::parse(raw).unwrap()),
                "{raw}"
            );
        }
    }
}

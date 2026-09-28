use axial_api::transport::ApiTransportBootstrap;
use tauri::{State, Url, WebviewWindow};

pub const APPLICATION_ID: &str = "com.mateoltd.axial.rewrite";

pub fn confine_content_policy(
    config: &mut tauri::Config,
    base_url: &str,
) -> Result<(), std::io::Error> {
    let url = Url::parse(base_url).map_err(std::io::Error::other)?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(std::io::Error::other(
            "The local API must bind an explicit IPv4 loopback port.",
        ));
    }
    let origin = url.origin().ascii_serialization();
    config.app.security.csp = Some(tauri::utils::config::Csp::Policy(format!(
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob: {origin}; connect-src 'self' ipc: http://ipc.localhost {origin}; font-src 'self'; media-src 'self' blob: {origin}; object-src 'none'; base-uri 'none'; frame-src 'none'; form-action 'none'"
    )));
    Ok(())
}

/// Process capabilities reach only the local, admitted main WebView.
#[derive(Clone)]
pub struct DesktopBootstrap {
    transport: ApiTransportBootstrap,
    dev_origin: Option<String>,
}

impl DesktopBootstrap {
    pub fn new(transport: ApiTransportBootstrap, dev_origin: Option<&str>) -> Result<Self, String> {
        let dev_origin = dev_origin.map(admit_development_origin).transpose()?;
        Ok(Self {
            transport,
            dev_origin,
        })
    }

    pub fn allows_main_url(&self, url: &Url) -> bool {
        main_navigation_allowed(url, self.dev_origin.as_deref())
    }
}

fn admit_development_origin(origin: &str) -> Result<String, String> {
    let url = Url::parse(origin).map_err(|_| "Invalid desktop development origin.")?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"));
    if url.scheme() != "http"
        || !loopback
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Desktop development origin must be an exact loopback HTTP origin.".into());
    }
    Ok(url.origin().ascii_serialization())
}

pub fn main_navigation_allowed(url: &Url, dev_origin: Option<&str>) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    matches!(
        (url.scheme(), url.host_str(), url.port()),
        ("tauri", Some("localhost"), None) | ("http", Some("tauri.localhost"), None)
    ) || dev_origin.is_some_and(|origin| url.origin().ascii_serialization() == origin)
}

#[tauri::command]
pub fn api_transport_bootstrap(
    window: WebviewWindow,
    state: State<'_, DesktopBootstrap>,
) -> Result<ApiTransportBootstrap, String> {
    crate::window::require_main_window(&window, &state)?;
    Ok(state.transport.clone())
}

#[tauri::command]
pub fn app_version(
    window: WebviewWindow,
    state: State<'_, DesktopBootstrap>,
) -> Result<&'static str, String> {
    crate::window::require_main_window(&window, &state)?;
    Ok(env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_policy_binds_only_the_actual_api_port() {
        let mut config: tauri::Config =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        confine_content_policy(&mut config, "http://127.0.0.1:38471").unwrap();
        let tauri::utils::config::Csp::Policy(policy) = config.app.security.csp.unwrap() else {
            panic!("expected one content policy");
        };
        assert!(policy.contains("http://127.0.0.1:38471"));
        assert!(!policy.contains("127.0.0.1:*"));
        for invalid in [
            "http://localhost:38471",
            "https://127.0.0.1:38471",
            "http://127.0.0.1",
            "http://user@127.0.0.1:38471",
            "http://127.0.0.1:38471?token=secret",
        ] {
            let mut config: tauri::Config =
                serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
            assert!(
                confine_content_policy(&mut config, invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn local_navigation_rejects_remote_credentials_and_origin_lookalikes() {
        for url in ["tauri://localhost/", "http://tauri.localhost/index.html"] {
            assert!(main_navigation_allowed(&Url::parse(url).unwrap(), None));
        }
        for url in [
            "https://tauri.localhost/",
            "http://tauri.localhost:8080/",
            "tauri://localhost.evil/",
            "tauri://user@localhost/",
            "https://example.com/",
            "file:///etc/passwd",
            "data:text/html,hello",
        ] {
            assert!(
                !main_navigation_allowed(&Url::parse(url).unwrap(), None),
                "{url}"
            );
        }
    }

    #[test]
    fn development_navigation_requires_one_exact_origin() {
        let origin = admit_development_origin("http://127.0.0.1:5173").unwrap();
        assert!(main_navigation_allowed(
            &Url::parse("http://127.0.0.1:5173/page").unwrap(),
            Some(&origin)
        ));
        for url in [
            "http://127.0.0.1:5174/",
            "http://localhost:5173/",
            "http://127.0.0.1.evil:5173/",
        ] {
            assert!(!main_navigation_allowed(
                &Url::parse(url).unwrap(),
                Some(&origin)
            ));
        }
        for origin in [
            "https://example.com",
            "http://0.0.0.0:5173",
            "http://localhost:5173/page",
            "http://user@localhost:5173",
        ] {
            assert!(admit_development_origin(origin).is_err(), "{origin}");
        }
    }
}

use axum::{
    Json,
    extract::{Extension, Request, State},
    http::{HeaderValue, Method, StatusCode, Uri, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::{Host, Url};

pub const CAPABILITY_HEADER: &str = "x-axial-capability";
pub const TICKET_QUERY: &str = "axial_ticket";
const MEDIA_TICKET_TTL: Duration = Duration::from_secs(30 * 60);
const STREAM_TICKET_TTL: Duration = Duration::from_secs(60);
const MAX_LIVE_TICKETS: usize = 256;

#[derive(Clone)]
pub struct LocalApiAuthority {
    inner: Arc<AuthorityInner>,
}

struct AuthorityInner {
    base_url: String,
    capability: String,
    allowed_origins: Vec<String>,
    tickets: Mutex<HashMap<String, Ticket>>,
    bypass: bool,
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ApiTransportBootstrap {
    pub base_url: String,
    pub capability: String,
}

struct Ticket {
    expires_at: Instant,
    kind: TicketKind,
}

enum TicketKind {
    Media,
    Stream { target: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TicketRequest {
    audience: TicketAudience,
    #[serde(default)]
    target: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TicketAudience {
    Media,
    Stream,
}

#[derive(Serialize)]
pub(crate) struct TicketResponse {
    ticket: String,
    expires_in_seconds: u64,
}

impl LocalApiAuthority {
    pub fn new(addr: SocketAddr, extra_origin: Option<&str>) -> Result<Self, String> {
        if !addr.ip().is_loopback() {
            return Err("local API address must be loopback".to_string());
        }
        let mut allowed_origins = vec![format!("http://{addr}")];
        allowed_origins.push(platform_webview_origin().to_string());
        if let Some(origin) = extra_origin {
            let origin = canonical_origin(origin)?;
            if !allowed_origins.contains(&origin) {
                allowed_origins.push(origin);
            }
        }
        let mut secret = [0_u8; 32];
        OsRng.fill_bytes(&mut secret);
        Ok(Self {
            inner: Arc::new(AuthorityInner {
                base_url: format!("http://{addr}"),
                capability: URL_SAFE_NO_PAD.encode(secret),
                allowed_origins,
                tickets: Mutex::new(HashMap::new()),
                bypass: false,
            }),
        })
    }

    pub fn bootstrap(&self) -> ApiTransportBootstrap {
        ApiTransportBootstrap {
            base_url: self.inner.base_url.clone(),
            capability: self.inner.capability.clone(),
        }
    }

    pub(crate) fn allows_origin(&self, origin: &HeaderValue) -> bool {
        origin.to_str().is_ok_and(|origin| {
            self.inner
                .allowed_origins
                .iter()
                .any(|allowed| fixed_time_eq(allowed.as_bytes(), origin.as_bytes()))
        })
    }

    #[cfg(test)]
    pub(crate) fn bypass_for_test() -> Self {
        Self {
            inner: Arc::new(AuthorityInner {
                base_url: String::new(),
                capability: String::new(),
                allowed_origins: Vec::new(),
                tickets: Mutex::new(HashMap::new()),
                bypass: true,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn capability_for_test(&self) -> &str {
        &self.inner.capability
    }

    fn authenticate(&self, request: &Request) -> bool {
        if self.inner.bypass {
            return true;
        }
        if request
            .headers()
            .get(CAPABILITY_HEADER)
            .is_some_and(|value| fixed_time_eq(self.inner.capability.as_bytes(), value.as_bytes()))
        {
            return true;
        }
        self.authenticate_ticket(request.method(), request.uri())
    }

    fn authenticate_ticket(&self, method: &Method, uri: &Uri) -> bool {
        let Some(ticket) = query_ticket(uri) else {
            return false;
        };
        let Ok(mut tickets) = self.inner.tickets.lock() else {
            return false;
        };
        let now = Instant::now();
        tickets.retain(|_, entry| entry.expires_at > now);
        let Some(entry) = tickets.get(&ticket) else {
            return false;
        };
        let valid = match &entry.kind {
            TicketKind::Media => {
                matches!(*method, Method::GET | Method::HEAD) && is_media_path(uri.path())
            }
            TicketKind::Stream { target } => method == Method::GET && target == uri.path(),
        };
        if valid && matches!(entry.kind, TicketKind::Stream { .. }) {
            tickets.remove(&ticket);
        }
        valid
    }

    fn mint(&self, request: TicketRequest) -> Result<TicketResponse, &'static str> {
        let (kind, ttl) = match request.audience {
            TicketAudience::Media if request.target.is_none() => {
                (TicketKind::Media, MEDIA_TICKET_TTL)
            }
            TicketAudience::Stream => {
                let target = request.target.ok_or("stream ticket target is required")?;
                let uri = target
                    .parse::<Uri>()
                    .map_err(|_| "stream ticket target is invalid")?;
                if uri.scheme().is_some()
                    || uri.authority().is_some()
                    || uri.query().is_some()
                    || !is_stream_path(uri.path())
                {
                    return Err("stream ticket target is invalid");
                }
                (TicketKind::Stream { target }, STREAM_TICKET_TTL)
            }
            TicketAudience::Media => return Err("media ticket does not accept a target"),
        };
        let mut tickets = self
            .inner
            .tickets
            .lock()
            .map_err(|_| "transport ticket registry is unavailable")?;
        let now = Instant::now();
        tickets.retain(|_, entry| entry.expires_at > now);
        if tickets.len() >= MAX_LIVE_TICKETS {
            return Err("transport ticket capacity is exhausted");
        }
        let ticket = loop {
            let mut bytes = [0_u8; 32];
            OsRng.fill_bytes(&mut bytes);
            let candidate = URL_SAFE_NO_PAD.encode(bytes);
            if !tickets.contains_key(&candidate) {
                break candidate;
            }
        };
        tickets.insert(
            ticket.clone(),
            Ticket {
                expires_at: now + ttl,
                kind,
            },
        );
        Ok(TicketResponse {
            ticket,
            expires_in_seconds: ttl.as_secs(),
        })
    }
}

impl fmt::Debug for LocalApiAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalApiAuthority")
            .field("allowed_origin_count", &self.inner.allowed_origins.len())
            .finish_non_exhaustive()
    }
}

pub(crate) async fn authenticate_request(
    State(authority): State<LocalApiAuthority>,
    request: Request,
    next: Next,
) -> Response {
    let origin_allowed = request
        .headers()
        .get(header::ORIGIN)
        .is_none_or(|origin| authority.allows_origin(origin));
    if !origin_allowed {
        return transport_error(StatusCode::FORBIDDEN, "request origin is not allowed");
    }
    if request.method() == Method::OPTIONS {
        if request.headers().contains_key(header::ORIGIN) {
            return next.run(request).await;
        }
        return transport_error(StatusCode::FORBIDDEN, "request origin is required");
    }
    if request.method() == Method::POST
        && request.uri().path() == "/api/v1/transport/bootstrap"
        && request.headers().contains_key(header::ORIGIN)
    {
        return next.run(request).await;
    }
    if !authority.authenticate(&request) {
        return transport_error(StatusCode::UNAUTHORIZED, "API capability is required");
    }
    next.run(request).await
}

pub(crate) async fn create_bootstrap(
    Extension(authority): Extension<LocalApiAuthority>,
) -> Json<ApiTransportBootstrap> {
    Json(authority.bootstrap())
}

pub(crate) async fn create_ticket(
    Extension(authority): Extension<LocalApiAuthority>,
    Json(request): Json<TicketRequest>,
) -> Response {
    match authority.mint(request) {
        Ok(ticket) => Json(ticket).into_response(),
        Err(error) => transport_error(StatusCode::BAD_REQUEST, error),
    }
}

fn transport_error(status: StatusCode, error: &'static str) -> Response {
    (status, Json(serde_json::json!({ "error": error }))).into_response()
}

fn query_ticket(uri: &Uri) -> Option<String> {
    let mut result = None;
    for (name, value) in url::form_urlencoded::parse(uri.query()?.as_bytes()) {
        if name == TICKET_QUERY {
            if result.is_some() || value.is_empty() {
                return None;
            }
            result = Some(value.into_owned());
        }
    }
    result
}

fn fixed_time_eq(expected: &[u8], actual: &[u8]) -> bool {
    let mut difference = expected.len() ^ actual.len();
    for (index, expected) in expected.iter().enumerate() {
        difference |= usize::from(*expected ^ actual.get(index).copied().unwrap_or_default());
    }
    difference == 0
}

fn canonical_origin(origin: &str) -> Result<String, String> {
    let parsed = Url::parse(origin).map_err(|_| "AXIAL_WEB_ORIGIN must be an absolute URL")?;
    let local_host = match parsed.host() {
        Some(Host::Domain("localhost")) => true,
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        _ => false,
    };
    if parsed.scheme() != "http"
        || !local_host
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("AXIAL_WEB_ORIGIN must contain only a loopback HTTP origin".to_string());
    }
    Ok(parsed.origin().ascii_serialization())
}

fn is_stream_path(path: &str) -> bool {
    let parts = path.split('/').collect::<Vec<_>>();
    matches!(parts.as_slice(), ["", "api", "v1", "install", _, "events"])
        || matches!(
            parts.as_slice(),
            ["", "api", "v1", "loaders", "install", _, "events"]
        )
        || matches!(parts.as_slice(), ["", "api", "v1", "launch", _, "events"])
}

fn is_media_path(path: &str) -> bool {
    path == "/api/v1/music/track"
        || matches!(
            path,
            "/api/v1/skin/profile/file"
                | "/api/v1/skin/cape/file"
                | "/api/v1/skin/head"
                | "/api/v1/skin/lookup/file"
                | "/api/v1/skin/lookup/head"
                | "/api/v1/skin/lookup/cape"
        )
        || (path.starts_with("/api/v1/skins/")
            && (path.ends_with("/file") || path.contains("/texture")))
        || (path.starts_with("/api/v1/instances/") && path.ends_with("/file"))
}

#[cfg(windows)]
fn platform_webview_origin() -> &'static str {
    "http://tauri.localhost"
}

#[cfg(not(windows))]
fn platform_webview_origin() -> &'static str {
    "tauri://localhost"
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_stream::stream;
    use axum::{Router, body::Body, middleware, routing::post};
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    fn authority() -> LocalApiAuthority {
        LocalApiAuthority::new("127.0.0.1:43430".parse().unwrap(), None).unwrap()
    }

    fn protected_router(authority: LocalApiAuthority, effects: Arc<AtomicUsize>) -> Router {
        async fn effect(State(effects): State<Arc<AtomicUsize>>, body: Body) -> StatusCode {
            effects.fetch_add(1, Ordering::SeqCst);
            drop(body);
            StatusCode::NO_CONTENT
        }
        Router::new()
            .route("/api/v1/status", post(effect))
            .route("/api/v1/launch/{id}/events", post(effect))
            .route("/api/v1/music/track", post(effect))
            .with_state(effects)
            .layer(middleware::from_fn_with_state(
                authority,
                authenticate_request,
            ))
    }

    #[tokio::test]
    async fn missing_wrong_and_cross_origin_authority_never_poll_body_or_reach_effect() {
        let authority = authority();
        let effects = Arc::new(AtomicUsize::new(0));
        for (capability, origin, expected) in [
            (None, None, StatusCode::UNAUTHORIZED),
            (Some("wrong"), None, StatusCode::UNAUTHORIZED),
            (
                Some(authority.capability_for_test()),
                Some("https://public.example"),
                StatusCode::FORBIDDEN,
            ),
        ] {
            let polls = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&polls);
            let body = Body::from_stream(stream! {
                observed.fetch_add(1, Ordering::SeqCst);
                yield Ok::<_, Infallible>("body");
            });
            let mut request = Request::builder()
                .method(Method::POST)
                .uri("/api/v1/status");
            if let Some(capability) = capability {
                request = request.header(CAPABILITY_HEADER, capability);
            }
            if let Some(origin) = origin {
                request = request.header(header::ORIGIN, origin);
            }
            let response = protected_router(authority.clone(), Arc::clone(&effects))
                .oneshot(request.body(body).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            assert_eq!(polls.load(Ordering::SeqCst), 0);
            assert_eq!(effects.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn exact_capability_reaches_handler_and_rotates_with_authority() {
        let first = authority();
        let effects = Arc::new(AtomicUsize::new(0));
        let response = protected_router(first.clone(), Arc::clone(&effects))
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/status")
                    .header(CAPABILITY_HEADER, first.capability_for_test())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(effects.load(Ordering::SeqCst), 1);

        let second = authority();
        let response = protected_router(second, effects)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/status")
                    .header(CAPABILITY_HEADER, first.capability_for_test())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn stream_ticket_is_exact_and_single_use_while_media_ticket_is_path_bounded() {
        let authority = authority();
        let stream = authority
            .mint(TicketRequest {
                audience: TicketAudience::Stream,
                target: Some("/api/v1/launch/session/events".to_string()),
            })
            .unwrap();
        let stream_uri = format!(
            "/api/v1/launch/session/events?{TICKET_QUERY}={}",
            stream.ticket
        )
        .parse::<Uri>()
        .unwrap();
        assert!(authority.authenticate_ticket(&Method::GET, &stream_uri));
        assert!(!authority.authenticate_ticket(&Method::GET, &stream_uri));

        let media = authority
            .mint(TicketRequest {
                audience: TicketAudience::Media,
                target: None,
            })
            .unwrap();
        let media_uri = format!("/api/v1/music/track?{TICKET_QUERY}={}", media.ticket)
            .parse::<Uri>()
            .unwrap();
        assert!(authority.authenticate_ticket(&Method::GET, &media_uri));
        assert!(authority.authenticate_ticket(&Method::GET, &media_uri));
        let control_uri = format!("/api/v1/status?{TICKET_QUERY}={}", media.ticket)
            .parse::<Uri>()
            .unwrap();
        assert!(!authority.authenticate_ticket(&Method::GET, &control_uri));

        let expired = "expired-ticket".to_string();
        authority.inner.tickets.lock().unwrap().insert(
            expired.clone(),
            Ticket {
                expires_at: Instant::now() - Duration::from_secs(1),
                kind: TicketKind::Media,
            },
        );
        let expired_uri = format!("/api/v1/music/track?{TICKET_QUERY}={expired}")
            .parse::<Uri>()
            .unwrap();
        assert!(!authority.authenticate_ticket(&Method::GET, &expired_uri));
    }

    #[test]
    fn non_loopback_and_non_origin_configuration_is_rejected() {
        assert!(LocalApiAuthority::new("0.0.0.0:43430".parse().unwrap(), None).is_err());
        assert!(
            LocalApiAuthority::new(
                "127.0.0.1:43430".parse().unwrap(),
                Some("https://example.test/path"),
            )
            .is_err()
        );
        assert!(
            LocalApiAuthority::new(
                "127.0.0.1:43430".parse().unwrap(),
                Some("https://localhost:3000"),
            )
            .is_err()
        );
        assert!(
            LocalApiAuthority::new(
                "127.0.0.1:43430".parse().unwrap(),
                Some("http://public.example"),
            )
            .is_err()
        );

        let authority = authority();
        assert!(authority.allows_origin(&HeaderValue::from_static("http://127.0.0.1:43430")));
        assert!(!authority.allows_origin(&HeaderValue::from_static("http://127.0.0.1:43431")));
        assert!(!authority.allows_origin(&HeaderValue::from_static("http://localhost:43430")));
        assert!(LocalApiAuthority::new("[::1]:0".parse().unwrap(), None).is_ok());
    }
}

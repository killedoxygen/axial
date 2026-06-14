use reqwest::{Client, StatusCode};
use serde::{Deserialize, de::DeserializeOwned};
use std::{fmt, time::Duration};

const AUTH_CHAIN_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_AUTH_CHAIN_RESPONSE_BYTES: usize = 1024 * 1024;
const MINECRAFT_PROFILE_ENDPOINT: &str = "https://api.minecraftservices.com/minecraft/profile";
const MINECRAFT_OWNERSHIP_ENDPOINT: &str = "https://api.minecraftservices.com/entitlements/mcstore";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthChainEndpoints {
    pub(crate) minecraft_profile: String,
    pub(crate) minecraft_ownership: String,
}

impl AuthChainEndpoints {
    pub(crate) fn production() -> Self {
        Self {
            minecraft_profile: MINECRAFT_PROFILE_ENDPOINT.to_string(),
            minecraft_ownership: MINECRAFT_OWNERSHIP_ENDPOINT.to_string(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct AuthChainClient {
    http: Client,
    endpoints: AuthChainEndpoints,
}

impl AuthChainClient {
    pub(crate) fn new() -> Result<Self, AuthChainError> {
        Self::with_endpoints(AuthChainEndpoints::production())
    }

    pub(crate) fn with_endpoints(endpoints: AuthChainEndpoints) -> Result<Self, AuthChainError> {
        let http = Client::builder()
            .timeout(AUTH_CHAIN_TIMEOUT)
            .build()
            .map_err(|_| AuthChainError::new(AuthChainErrorKind::ClientBuild))?;

        Ok(Self { http, endpoints })
    }

    pub(crate) async fn minecraft_profile(
        &self,
        minecraft_access_token: &str,
    ) -> Result<MinecraftProfile, AuthChainError> {
        self.get_bearer_json(&self.endpoints.minecraft_profile, minecraft_access_token)
            .await
    }

    pub(crate) async fn minecraft_ownership(
        &self,
        minecraft_access_token: &str,
    ) -> Result<MinecraftOwnership, AuthChainError> {
        let response: MinecraftOwnershipResponse = self
            .get_bearer_json(&self.endpoints.minecraft_ownership, minecraft_access_token)
            .await?;

        Ok(MinecraftOwnership {
            owns_minecraft_java: response.items.iter().any(MinecraftOwnershipItem::is_java),
        })
    }

    async fn get_bearer_json<R>(
        &self,
        endpoint: &str,
        bearer_token: &str,
    ) -> Result<R, AuthChainError>
    where
        R: DeserializeOwned,
    {
        let response = self
            .http
            .get(endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .bearer_auth(bearer_token)
            .send()
            .await
            .map_err(|_| AuthChainError::new(AuthChainErrorKind::Request))?;

        parse_response(response).await
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct MinecraftProfile {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) skins: Vec<MinecraftSkin>,
    #[serde(default)]
    pub(crate) capes: Vec<MinecraftCape>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct MinecraftSkin {
    pub(crate) id: String,
    pub(crate) state: String,
    pub(crate) url: String,
    pub(crate) variant: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct MinecraftCape {
    pub(crate) id: String,
    pub(crate) state: String,
    pub(crate) url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MinecraftOwnership {
    pub(crate) owns_minecraft_java: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthChainErrorKind {
    ClientBuild,
    Request,
    UpstreamRejected,
    UpstreamUnavailable,
    Parse,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthChainError {
    kind: AuthChainErrorKind,
    message: &'static str,
}

impl AuthChainError {
    fn new(kind: AuthChainErrorKind) -> Self {
        Self {
            kind,
            message: kind.message(),
        }
    }

    pub(crate) fn kind(&self) -> AuthChainErrorKind {
        self.kind
    }
}

impl fmt::Display for AuthChainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for AuthChainError {}

impl AuthChainErrorKind {
    fn message(self) -> &'static str {
        match self {
            Self::ClientBuild => "failed to initialize auth-chain client",
            Self::Request => "failed to reach auth-chain provider",
            Self::UpstreamRejected => "auth-chain provider rejected request",
            Self::UpstreamUnavailable => "auth-chain provider is unavailable",
            Self::Parse => "failed to parse auth-chain provider response",
        }
    }
}

#[derive(Debug, Deserialize)]
struct MinecraftOwnershipResponse {
    #[serde(default)]
    items: Vec<MinecraftOwnershipItem>,
}

#[derive(Debug, Deserialize)]
struct MinecraftOwnershipItem {
    name: String,
}

impl MinecraftOwnershipItem {
    fn is_java(&self) -> bool {
        matches!(self.name.as_str(), "game_minecraft" | "product_minecraft")
    }
}

async fn parse_response<R>(response: reqwest::Response) -> Result<R, AuthChainError>
where
    R: DeserializeOwned,
{
    let status = response.status();
    if !status.is_success() {
        return Err(AuthChainError::new(error_kind_for_status(status)));
    }

    let body = bounded_response_body(response).await?;
    serde_json::from_slice::<R>(&body).map_err(|_| AuthChainError::new(AuthChainErrorKind::Parse))
}

async fn bounded_response_body(mut response: reqwest::Response) -> Result<Vec<u8>, AuthChainError> {
    if response
        .content_length()
        .is_some_and(|content_length| content_length > MAX_AUTH_CHAIN_RESPONSE_BYTES as u64)
    {
        return Err(AuthChainError::new(AuthChainErrorKind::Parse));
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| AuthChainError::new(AuthChainErrorKind::Parse))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_AUTH_CHAIN_RESPONSE_BYTES {
            return Err(AuthChainError::new(AuthChainErrorKind::Parse));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

fn error_kind_for_status(status: StatusCode) -> AuthChainErrorKind {
    if status.is_server_error() {
        AuthChainErrorKind::UpstreamUnavailable
    } else {
        AuthChainErrorKind::UpstreamRejected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        body::Bytes,
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::get,
    };
    use serde_json::Value;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn auth_chain_reads_minecraft_profile_with_bearer_token() {
        let (endpoints, mut requests) = auth_chain_test_server(AuthChainServerMode::Success).await;
        let client = AuthChainClient::with_endpoints(endpoints).expect("auth chain client");

        let profile = client
            .minecraft_profile("minecraft-access-token")
            .await
            .expect("profile response");

        assert_eq!(profile.id, "4f9c7f7d0b1245d9a5c2f03a8c120001");
        assert_eq!(profile.name, "ProfileName");
        assert_eq!(profile.skins[0].variant, "SLIM");

        let profile = requests.recv().await.expect("profile request");
        assert_eq!(profile.path, "/minecraft/profile");
        assert_eq!(
            profile.authorization.as_deref(),
            Some("Bearer minecraft-access-token")
        );
        assert_eq!(profile.body, Value::Null);

        assert!(
            tokio::time::timeout(Duration::from_millis(100), requests.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn auth_chain_maps_upstream_rejections_to_bounded_errors() {
        let (endpoints, mut requests) =
            auth_chain_test_server(AuthChainServerMode::ProfileRejected).await;
        let client = AuthChainClient::with_endpoints(endpoints).expect("auth chain client");

        let error = client
            .minecraft_profile("minecraft-access-token")
            .await
            .expect_err("profile rejection");

        assert_eq!(error.kind(), AuthChainErrorKind::UpstreamRejected);
        assert_eq!(error.to_string(), "auth-chain provider rejected request");
        let debug = format!("{error:?}");
        assert!(!debug.contains("minecraft-access-token"));
        assert!(!debug.contains("provider-secret-payload"));
        assert_eq!(
            requests.recv().await.expect("profile request").path,
            "/minecraft/profile"
        );
    }

    #[tokio::test]
    async fn auth_chain_reports_empty_minecraft_ownership_without_error() {
        let (endpoints, _requests) = auth_chain_test_server(AuthChainServerMode::NoOwnership).await;
        let client = AuthChainClient::with_endpoints(endpoints).expect("auth chain client");

        let ownership = client
            .minecraft_ownership("minecraft-access-token")
            .await
            .expect("ownership response");

        assert!(!ownership.owns_minecraft_java);
    }

    #[tokio::test]
    async fn auth_chain_rejects_oversized_provider_content_length() {
        let endpoint = spawn_raw_auth_response(
            "200 OK",
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                (
                    "Content-Length".to_string(),
                    (MAX_AUTH_CHAIN_RESPONSE_BYTES + 1).to_string(),
                ),
            ],
            b"[]".to_vec(),
        )
        .await;
        let client =
            AuthChainClient::with_endpoints(single_endpoint_auth_chain_endpoints(endpoint))
                .expect("auth chain client");

        let error = client
            .minecraft_profile("minecraft-access-token")
            .await
            .expect_err("oversized content length should fail");

        assert_eq!(error.kind(), AuthChainErrorKind::Parse);
    }

    #[tokio::test]
    async fn auth_chain_rejects_stream_past_response_limit() {
        let endpoint = spawn_raw_auth_response(
            "200 OK",
            vec![("Content-Type".to_string(), "application/json".to_string())],
            vec![b' '; MAX_AUTH_CHAIN_RESPONSE_BYTES + 1],
        )
        .await;
        let client =
            AuthChainClient::with_endpoints(single_endpoint_auth_chain_endpoints(endpoint))
                .expect("auth chain client");

        let error = client
            .minecraft_profile("minecraft-access-token")
            .await
            .expect_err("oversized stream should fail");

        assert_eq!(error.kind(), AuthChainErrorKind::Parse);
    }

    #[derive(Clone, Copy)]
    enum AuthChainServerMode {
        Success,
        ProfileRejected,
        NoOwnership,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct RecordedRequest {
        path: String,
        authorization: Option<String>,
        content_type: Option<String>,
        accept: Option<String>,
        body: Value,
    }

    async fn auth_chain_test_server(
        mode: AuthChainServerMode,
    ) -> (AuthChainEndpoints, mpsc::UnboundedReceiver<RecordedRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = Router::new()
            .route("/minecraft/profile", get(record_minecraft_profile))
            .route("/minecraft/ownership", get(record_minecraft_ownership))
            .with_state(TestServerState { tx, mode });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind auth chain test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("auth chain test server");
        });

        (
            AuthChainEndpoints {
                minecraft_profile: format!("{base_url}/minecraft/profile"),
                minecraft_ownership: format!("{base_url}/minecraft/ownership"),
            },
            rx,
        )
    }

    #[derive(Clone)]
    struct TestServerState {
        tx: mpsc::UnboundedSender<RecordedRequest>,
        mode: AuthChainServerMode,
    }

    async fn record_minecraft_profile(
        State(state): State<TestServerState>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        record_request(&state.tx, "/minecraft/profile", &headers, &Bytes::new());

        if matches!(state.mode, AuthChainServerMode::ProfileRejected) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "provider-secret-payload" })),
            );
        }

        (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": "4f9c7f7d0b1245d9a5c2f03a8c120001",
                "name": "ProfileName",
                "skins": [{
                    "id": "skin-id",
                    "state": "ACTIVE",
                    "url": "https://textures.minecraft.net/texture/skin",
                    "variant": "SLIM"
                }],
                "capes": [{
                    "id": "cape-id",
                    "state": "INACTIVE",
                    "url": "https://textures.minecraft.net/texture/cape"
                }]
            })),
        )
    }

    async fn record_minecraft_ownership(
        State(state): State<TestServerState>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        record_request(&state.tx, "/minecraft/ownership", &headers, &Bytes::new());

        let items = if matches!(state.mode, AuthChainServerMode::NoOwnership) {
            serde_json::json!([])
        } else {
            serde_json::json!([{ "name": "game_minecraft" }])
        };

        (
            StatusCode::OK,
            Json(serde_json::json!({
                "items": items,
                "signature": "ignored",
                "keyId": "ignored"
            })),
        )
    }

    fn record_request(
        tx: &mpsc::UnboundedSender<RecordedRequest>,
        path: &str,
        headers: &HeaderMap,
        body: &Bytes,
    ) {
        let body = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(body).expect("request json")
        };
        let request = RecordedRequest {
            path: path.to_string(),
            authorization: header_value(headers, "authorization"),
            content_type: header_value(headers, "content-type")
                .and_then(|value| value.split(';').next().map(str::to_string)),
            accept: header_value(headers, "accept"),
            body,
        };

        tx.send(request).expect("record request");
    }

    fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned)
    }

    fn single_endpoint_auth_chain_endpoints(endpoint: String) -> AuthChainEndpoints {
        AuthChainEndpoints {
            minecraft_profile: endpoint.clone(),
            minecraft_ownership: endpoint,
        }
    }

    async fn spawn_raw_auth_response(
        status: &str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind raw auth response server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let status = status.to_string();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut response = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
                for (name, value) in headers {
                    response.push_str(&format!("{name}: {value}\r\n"));
                }
                response.push_str("\r\n");
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            }
        });

        endpoint
    }
}

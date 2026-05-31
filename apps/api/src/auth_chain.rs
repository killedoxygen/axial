#![allow(dead_code)]

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{fmt, time::Duration};

const AUTH_CHAIN_TIMEOUT: Duration = Duration::from_secs(20);
const XBOX_USER_AUTHENTICATE_ENDPOINT: &str = "https://user.auth.xboxlive.com/user/authenticate";
const XSTS_AUTHORIZE_ENDPOINT: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";
const MINECRAFT_LOGIN_WITH_XBOX_ENDPOINT: &str =
    "https://api.minecraftservices.com/authentication/login_with_xbox";
const MINECRAFT_PROFILE_ENDPOINT: &str = "https://api.minecraftservices.com/minecraft/profile";
const MINECRAFT_OWNERSHIP_ENDPOINT: &str = "https://api.minecraftservices.com/entitlements/mcstore";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthChainEndpoints {
    pub(crate) xbox_user_authenticate: String,
    pub(crate) xsts_authorize: String,
    pub(crate) minecraft_login_with_xbox: String,
    pub(crate) minecraft_profile: String,
    pub(crate) minecraft_ownership: String,
}

impl AuthChainEndpoints {
    pub(crate) fn production() -> Self {
        Self {
            xbox_user_authenticate: XBOX_USER_AUTHENTICATE_ENDPOINT.to_string(),
            xsts_authorize: XSTS_AUTHORIZE_ENDPOINT.to_string(),
            minecraft_login_with_xbox: MINECRAFT_LOGIN_WITH_XBOX_ENDPOINT.to_string(),
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

    pub(crate) async fn exchange_msa_access_token(
        &self,
        msa_access_token: &str,
    ) -> Result<AuthChainExchange, AuthChainError> {
        let xbox_live = self.authenticate_xbox_live(msa_access_token).await?;
        let xsts = self.authorize_xsts(xbox_live.token()).await?;
        let minecraft = self.login_minecraft(&xsts).await?;
        let profile = self.minecraft_profile(minecraft.access_token()).await?;
        let ownership = self.minecraft_ownership(minecraft.access_token()).await?;

        Ok(AuthChainExchange {
            xbox_live,
            xsts,
            minecraft,
            profile,
            ownership,
        })
    }

    pub(crate) async fn authenticate_xbox_live(
        &self,
        msa_access_token: &str,
    ) -> Result<XboxLiveToken, AuthChainError> {
        let body = XboxAuthenticateRequest {
            properties: XboxAuthenticateProperties {
                auth_method: "RPS",
                site_name: "user.auth.xboxlive.com",
                rps_ticket: format!("d={msa_access_token}"),
            },
            relying_party: "http://auth.xboxlive.com",
            token_type: "JWT",
        };
        let response: XboxTokenResponse = self
            .post_json(&self.endpoints.xbox_user_authenticate, &body)
            .await?;
        let user_hash = response.user_hash()?;

        Ok(XboxLiveToken {
            token: response.token,
            user_hash,
        })
    }

    pub(crate) async fn authorize_xsts(
        &self,
        xbox_live_token: &str,
    ) -> Result<XstsToken, AuthChainError> {
        let body = XstsAuthorizeRequest {
            properties: XstsAuthorizeProperties {
                sandbox_id: "RETAIL",
                user_tokens: vec![xbox_live_token.to_string()],
            },
            relying_party: "rp://api.minecraftservices.com/",
            token_type: "JWT",
        };
        let response: XboxTokenResponse = self
            .post_json(&self.endpoints.xsts_authorize, &body)
            .await?;
        let user_hash = response.user_hash()?;

        Ok(XstsToken {
            token: response.token,
            user_hash,
        })
    }

    pub(crate) async fn login_minecraft(
        &self,
        xsts: &XstsToken,
    ) -> Result<MinecraftAccessToken, AuthChainError> {
        let body = MinecraftLoginRequest {
            identity_token: format!("XBL3.0 x={};{}", xsts.user_hash().as_str(), xsts.token()),
        };
        let response: MinecraftLoginResponse = self
            .post_json(&self.endpoints.minecraft_login_with_xbox, &body)
            .await?;

        Ok(MinecraftAccessToken {
            access_token: response.access_token,
            expires_in: response.expires_in,
            token_type: response.token_type,
        })
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

    async fn post_json<T, R>(&self, endpoint: &str, body: &T) -> Result<R, AuthChainError>
    where
        T: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let response = self
            .http
            .post(endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(body)
            .send()
            .await
            .map_err(|_| AuthChainError::new(AuthChainErrorKind::Request))?;

        parse_response(response).await
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

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct AuthChainExchange {
    pub(crate) xbox_live: XboxLiveToken,
    pub(crate) xsts: XstsToken,
    pub(crate) minecraft: MinecraftAccessToken,
    pub(crate) profile: MinecraftProfile,
    pub(crate) ownership: MinecraftOwnership,
}

impl fmt::Debug for AuthChainExchange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthChainExchange")
            .field("xbox_live", &self.xbox_live)
            .field("xsts", &self.xsts)
            .field("minecraft", &self.minecraft)
            .field("profile", &self.profile)
            .field("ownership", &self.ownership)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct XboxUserHash(String);

impl XboxUserHash {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct XboxLiveToken {
    token: String,
    pub(crate) user_hash: XboxUserHash,
}

impl XboxLiveToken {
    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}

impl fmt::Debug for XboxLiveToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("XboxLiveToken")
            .field("token", &"[redacted]")
            .field("user_hash", &self.user_hash)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct XstsToken {
    token: String,
    pub(crate) user_hash: XboxUserHash,
}

impl XstsToken {
    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn user_hash(&self) -> &XboxUserHash {
        &self.user_hash
    }
}

impl fmt::Debug for XstsToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("XstsToken")
            .field("token", &"[redacted]")
            .field("user_hash", &self.user_hash)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct MinecraftAccessToken {
    access_token: String,
    pub(crate) expires_in: u64,
    pub(crate) token_type: Option<String>,
}

impl MinecraftAccessToken {
    pub(crate) fn access_token(&self) -> &str {
        &self.access_token
    }
}

impl fmt::Debug for MinecraftAccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MinecraftAccessToken")
            .field("access_token", &"[redacted]")
            .field("expires_in", &self.expires_in)
            .field("token_type", &self.token_type)
            .finish()
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
    MissingUserHash,
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

    pub(crate) fn message(&self) -> &'static str {
        self.message
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
            Self::MissingUserHash => "auth-chain provider response missed Xbox user hash",
        }
    }
}

#[derive(Debug, Serialize)]
struct XboxAuthenticateRequest {
    #[serde(rename = "Properties")]
    properties: XboxAuthenticateProperties,
    #[serde(rename = "RelyingParty")]
    relying_party: &'static str,
    #[serde(rename = "TokenType")]
    token_type: &'static str,
}

#[derive(Debug, Serialize)]
struct XboxAuthenticateProperties {
    #[serde(rename = "AuthMethod")]
    auth_method: &'static str,
    #[serde(rename = "SiteName")]
    site_name: &'static str,
    #[serde(rename = "RpsTicket")]
    rps_ticket: String,
}

#[derive(Debug, Serialize)]
struct XstsAuthorizeRequest {
    #[serde(rename = "Properties")]
    properties: XstsAuthorizeProperties,
    #[serde(rename = "RelyingParty")]
    relying_party: &'static str,
    #[serde(rename = "TokenType")]
    token_type: &'static str,
}

#[derive(Debug, Serialize)]
struct XstsAuthorizeProperties {
    #[serde(rename = "SandboxId")]
    sandbox_id: &'static str,
    #[serde(rename = "UserTokens")]
    user_tokens: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct XboxTokenResponse {
    #[serde(rename = "Token")]
    token: String,
    #[serde(rename = "DisplayClaims")]
    display_claims: XboxDisplayClaims,
}

impl XboxTokenResponse {
    fn user_hash(&self) -> Result<XboxUserHash, AuthChainError> {
        self.display_claims
            .xui
            .first()
            .map(|claim| XboxUserHash(claim.uhs.clone()))
            .filter(|user_hash| !user_hash.0.trim().is_empty())
            .ok_or_else(|| AuthChainError::new(AuthChainErrorKind::MissingUserHash))
    }
}

#[derive(Debug, Deserialize)]
struct XboxDisplayClaims {
    xui: Vec<XboxUserClaim>,
}

#[derive(Debug, Deserialize)]
struct XboxUserClaim {
    uhs: String,
}

#[derive(Debug, Serialize)]
struct MinecraftLoginRequest {
    #[serde(rename = "identityToken")]
    identity_token: String,
}

#[derive(Debug, Deserialize)]
struct MinecraftLoginResponse {
    access_token: String,
    expires_in: u64,
    token_type: Option<String>,
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

    response
        .json::<R>()
        .await
        .map_err(|_| AuthChainError::new(AuthChainErrorKind::Parse))
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
        routing::{get, post},
    };
    use serde_json::Value;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn auth_chain_exchanges_msa_token_through_xbox_xsts_and_minecraft_services() {
        let (endpoints, mut requests) = auth_chain_test_server(AuthChainServerMode::Success).await;
        let client = AuthChainClient::with_endpoints(endpoints).expect("auth chain client");

        let exchange = client
            .exchange_msa_access_token("msa-access-token")
            .await
            .expect("auth chain exchange");

        assert_eq!(exchange.xbox_live.user_hash.as_str(), "xbl-user-hash");
        assert_eq!(exchange.xsts.user_hash().as_str(), "xsts-user-hash");
        assert_eq!(exchange.minecraft.expires_in, 86_400);
        assert_eq!(exchange.minecraft.token_type, Some("Bearer".to_string()));
        assert_eq!(exchange.profile.id, "4f9c7f7d0b1245d9a5c2f03a8c120001");
        assert_eq!(exchange.profile.name, "ProfileName");
        assert_eq!(exchange.profile.skins[0].variant, "SLIM");
        assert!(exchange.ownership.owns_minecraft_java);

        let xbl = requests.recv().await.expect("xbl request");
        assert_eq!(xbl.path, "/xbl");
        assert_eq!(xbl.content_type.as_deref(), Some("application/json"));
        assert_eq!(xbl.accept.as_deref(), Some("application/json"));
        assert_eq!(xbl.body["Properties"]["AuthMethod"], "RPS");
        assert_eq!(xbl.body["Properties"]["SiteName"], "user.auth.xboxlive.com");
        assert_eq!(xbl.body["Properties"]["RpsTicket"], "d=msa-access-token");
        assert_eq!(xbl.body["RelyingParty"], "http://auth.xboxlive.com");
        assert_eq!(xbl.body["TokenType"], "JWT");

        let xsts = requests.recv().await.expect("xsts request");
        assert_eq!(xsts.path, "/xsts");
        assert_eq!(xsts.body["Properties"]["SandboxId"], "RETAIL");
        assert_eq!(
            xsts.body["Properties"]["UserTokens"],
            serde_json::json!(["xbl-token"])
        );
        assert_eq!(xsts.body["RelyingParty"], "rp://api.minecraftservices.com/");
        assert_eq!(xsts.body["TokenType"], "JWT");

        let minecraft_login = requests.recv().await.expect("minecraft login request");
        assert_eq!(minecraft_login.path, "/minecraft/login");
        assert_eq!(
            minecraft_login.body["identityToken"],
            "XBL3.0 x=xsts-user-hash;xsts-token"
        );

        let profile = requests.recv().await.expect("profile request");
        assert_eq!(profile.path, "/minecraft/profile");
        assert_eq!(
            profile.authorization.as_deref(),
            Some("Bearer minecraft-access-token")
        );
        assert_eq!(profile.body, Value::Null);

        let ownership = requests.recv().await.expect("ownership request");
        assert_eq!(ownership.path, "/minecraft/ownership");
        assert_eq!(
            ownership.authorization.as_deref(),
            Some("Bearer minecraft-access-token")
        );
        assert_eq!(ownership.body, Value::Null);

        assert!(
            tokio::time::timeout(Duration::from_millis(100), requests.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn auth_chain_maps_upstream_rejections_to_bounded_errors() {
        let (endpoints, mut requests) =
            auth_chain_test_server(AuthChainServerMode::XstsRejected).await;
        let client = AuthChainClient::with_endpoints(endpoints).expect("auth chain client");

        let error = client
            .exchange_msa_access_token("msa-access-token")
            .await
            .expect_err("xsts rejection");

        assert_eq!(error.kind(), AuthChainErrorKind::UpstreamRejected);
        assert_eq!(error.message(), "auth-chain provider rejected request");
        let debug = format!("{error:?}");
        assert!(!debug.contains("msa-access-token"));
        assert!(!debug.contains("xbl-token"));
        assert!(!debug.contains("provider-secret-payload"));
        assert_eq!(requests.recv().await.expect("xbl request").path, "/xbl");
        assert_eq!(requests.recv().await.expect("xsts request").path, "/xsts");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), requests.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn auth_chain_maps_missing_user_hash_to_bounded_parse_category() {
        let (endpoints, _requests) =
            auth_chain_test_server(AuthChainServerMode::MissingUserHash).await;
        let client = AuthChainClient::with_endpoints(endpoints).expect("auth chain client");

        let error = client
            .authenticate_xbox_live("msa-access-token")
            .await
            .expect_err("missing user hash");

        assert_eq!(error.kind(), AuthChainErrorKind::MissingUserHash);
        assert_eq!(
            error.message(),
            "auth-chain provider response missed Xbox user hash"
        );
        let debug = format!("{error:?}");
        assert!(!debug.contains("msa-access-token"));
        assert!(!debug.contains("xbl-token"));
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

    #[derive(Clone, Copy)]
    enum AuthChainServerMode {
        Success,
        XstsRejected,
        MissingUserHash,
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
            .route("/xbl", post(record_xbl))
            .route("/xsts", post(record_xsts))
            .route("/minecraft/login", post(record_minecraft_login))
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
                xbox_user_authenticate: format!("{base_url}/xbl"),
                xsts_authorize: format!("{base_url}/xsts"),
                minecraft_login_with_xbox: format!("{base_url}/minecraft/login"),
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

    async fn record_xbl(
        State(state): State<TestServerState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Json<Value>) {
        record_request(&state.tx, "/xbl", &headers, &body);

        let display_claims = match state.mode {
            AuthChainServerMode::MissingUserHash => serde_json::json!({ "xui": [] }),
            _ => serde_json::json!({ "xui": [{ "uhs": "xbl-user-hash" }] }),
        };

        (
            StatusCode::OK,
            Json(serde_json::json!({
                "Token": "xbl-token",
                "DisplayClaims": display_claims,
            })),
        )
    }

    async fn record_xsts(
        State(state): State<TestServerState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Json<Value>) {
        record_request(&state.tx, "/xsts", &headers, &body);

        if matches!(state.mode, AuthChainServerMode::XstsRejected) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": "provider-secret-payload",
                    "Token": "xsts-token"
                })),
            );
        }

        (
            StatusCode::OK,
            Json(serde_json::json!({
                "Token": "xsts-token",
                "DisplayClaims": {
                    "xui": [{ "uhs": "xsts-user-hash" }]
                },
            })),
        )
    }

    async fn record_minecraft_login(
        State(state): State<TestServerState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Json<Value>) {
        record_request(&state.tx, "/minecraft/login", &headers, &body);

        (
            StatusCode::OK,
            Json(serde_json::json!({
                "access_token": "minecraft-access-token",
                "expires_in": 86400,
                "token_type": "Bearer"
            })),
        )
    }

    async fn record_minecraft_profile(
        State(state): State<TestServerState>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        record_request(&state.tx, "/minecraft/profile", &headers, &Bytes::new());

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
}

use base64::{
    Engine,
    engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD},
};
use chrono::{DateTime, Utc};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey, signature::Signer};
use rand::{RngCore, rngs::OsRng};
use reqwest::{Client, StatusCode, header::HeaderMap};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{fmt, sync::OnceLock, time::Duration};
use uuid::Uuid;

const MICROSOFT_CLIENT_ID: &str = "00000000402b5328";
pub const MICROSOFT_AUTH_REDIRECT_URL: &str = "https://login.live.com/oauth20_desktop.srf";
const REQUESTED_SCOPE: &str = "service::user.auth.xboxlive.com::MBI_SSL";
const DEVICE_AUTHENTICATE_ENDPOINT: &str = "https://device.auth.xboxlive.com/device/authenticate";
const SISU_AUTHENTICATE_ENDPOINT: &str = "https://sisu.xboxlive.com/authenticate";
const SISU_AUTHORIZE_ENDPOINT: &str = "https://sisu.xboxlive.com/authorize";
const XSTS_AUTHORIZE_ENDPOINT: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";
const MICROSOFT_TOKEN_ENDPOINT: &str = "https://login.live.com/oauth20_token.srf";
const MINECRAFT_LAUNCHER_LOGIN_ENDPOINT: &str = "https://api.minecraftservices.com/launcher/login";
const MINECRAFT_PROFILE_ENDPOINT: &str = "https://api.minecraftservices.com/minecraft/profile";
const MINECRAFT_ENTITLEMENTS_ENDPOINT: &str =
    "https://api.minecraftservices.com/entitlements/mcstore";
const MINECRAFT_SERVICES_USER_AGENT: &str = "Axial (https://github.com/mateoltd/axial)";
const MICROSOFT_AUTH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_MICROSOFT_AUTH_RESPONSE_BYTES: usize = 1024 * 1024;
const MINECRAFT_ACCESS_TOKEN_EXPIRES_IN: u64 = 86_400;

static MICROSOFT_AUTH_CLIENT: OnceLock<Result<Client, MicrosoftAuthError>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MicrosoftAuthStep {
    DeviceToken,
    SisuAuthenticate,
    OAuthToken,
    OAuthRefresh,
    SisuAuthorize,
    XstsAuthorize,
    MinecraftToken,
    MinecraftEntitlements,
    MinecraftProfile,
    Callback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MicrosoftAuthErrorKind {
    ClientBuild,
    Request,
    UpstreamRejected,
    UpstreamUnavailable,
    Parse,
    MissingRefreshToken,
    MissingSessionId,
    MissingUserHash,
    InvalidCallback,
    Cancelled,
    OwnershipMissing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MicrosoftAuthError {
    kind: MicrosoftAuthErrorKind,
    step: MicrosoftAuthStep,
    provider_status: Option<u16>,
}

impl MicrosoftAuthError {
    fn new(kind: MicrosoftAuthErrorKind, step: MicrosoftAuthStep) -> Self {
        Self {
            kind,
            step,
            provider_status: None,
        }
    }

    fn upstream(status: StatusCode, step: MicrosoftAuthStep) -> Self {
        Self {
            kind: error_kind_for_status(status),
            step,
            provider_status: Some(status.as_u16()),
        }
    }

    pub fn kind(&self) -> MicrosoftAuthErrorKind {
        self.kind
    }

    pub fn step(&self) -> MicrosoftAuthStep {
        self.step
    }

    pub fn provider_status(&self) -> Option<u16> {
        self.provider_status
    }

    pub fn message(&self) -> &'static str {
        match self.kind {
            MicrosoftAuthErrorKind::ClientBuild => "failed to initialize Microsoft sign-in client",
            MicrosoftAuthErrorKind::Request => "failed to reach Microsoft sign-in services",
            MicrosoftAuthErrorKind::UpstreamRejected => "Microsoft sign-in request was rejected",
            MicrosoftAuthErrorKind::UpstreamUnavailable => {
                "Microsoft sign-in services are unavailable"
            }
            MicrosoftAuthErrorKind::Parse => "failed to parse Microsoft sign-in response",
            MicrosoftAuthErrorKind::MissingRefreshToken => {
                "Microsoft sign-in needs re-verification"
            }
            MicrosoftAuthErrorKind::MissingSessionId => {
                "Microsoft sign-in response missed a session id"
            }
            MicrosoftAuthErrorKind::MissingUserHash => {
                "Microsoft sign-in response missed the Xbox user hash"
            }
            MicrosoftAuthErrorKind::InvalidCallback => "Microsoft sign-in callback was rejected",
            MicrosoftAuthErrorKind::Cancelled => "Microsoft sign-in was cancelled",
            MicrosoftAuthErrorKind::OwnershipMissing => {
                "This Microsoft account does not own Minecraft Java"
            }
        }
    }

    pub fn user_message(&self) -> String {
        let mut message = self.message().to_string();
        if let Some(status) = self.provider_status {
            message.push_str(&format!(" (HTTP {status})"));
        }
        message
    }
}

impl fmt::Display for MicrosoftAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for MicrosoftAuthError {}

pub struct MicrosoftLoginFlow {
    verifier: String,
    state: String,
    session_id: String,
    auth_request_uri: String,
    device_pair: DeviceTokenPair,
}

impl fmt::Debug for MicrosoftLoginFlow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MicrosoftLoginFlow")
            .field("verifier", &"[redacted]")
            .field("session_id", &"[redacted]")
            .field("auth_request_uri", &"[redacted]")
            .finish()
    }
}

impl MicrosoftLoginFlow {
    pub fn auth_request_uri(&self) -> &str {
        &self.auth_request_uri
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MicrosoftLoginOutcome {
    pub login_id: String,
    pub profile_name: String,
    pub owns_minecraft_java: bool,
}

pub async fn begin_login() -> Result<MicrosoftLoginFlow, MicrosoftAuthError> {
    let current_date = Utc::now();
    let device_pair = DeviceTokenPair::new(current_date).await?;

    let verifier = generate_oauth_challenge();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = generate_oauth_challenge();
    let (session_id, auth_request_uri) =
        sisu_authenticate(&device_pair, &challenge, &state, current_date).await?;
    validate_authorization_url(&auth_request_uri)?;

    Ok(MicrosoftLoginFlow {
        verifier,
        state,
        session_id,
        auth_request_uri,
        device_pair,
    })
}

pub async fn finish_login(
    flow: MicrosoftLoginFlow,
    callback: &url::Url,
) -> Result<MicrosoftMinecraftSession, MicrosoftAuthError> {
    let code = validate_callback(callback, &flow.state)?;
    let oauth = oauth_token(&code, &flow.verifier).await?;
    minecraft_session_from_oauth(oauth, &flow.device_pair, Some(&flow.session_id)).await
}

pub async fn refresh_login(
    refresh_token: &str,
) -> Result<MicrosoftMinecraftSession, MicrosoftAuthError> {
    if refresh_token.trim().is_empty() {
        return Err(MicrosoftAuthError::new(
            MicrosoftAuthErrorKind::MissingRefreshToken,
            MicrosoftAuthStep::OAuthRefresh,
        ));
    }
    let mut oauth = oauth_refresh(refresh_token).await?;
    if oauth.refresh_token.is_none() {
        oauth.refresh_token = Some(refresh_token.to_owned());
    }
    let device_pair = DeviceTokenPair::new(oauth.current_date).await?;
    minecraft_session_from_oauth(oauth, &device_pair, None).await
}

#[derive(Clone)]
pub(super) struct ProfileClient {
    profile_endpoint: String,
    entitlements_endpoint: String,
}

pub(super) struct SyncedProfile {
    pub profile: MinecraftProfile,
    pub owns_minecraft_java: bool,
}

impl Default for ProfileClient {
    fn default() -> Self {
        Self {
            profile_endpoint: MINECRAFT_PROFILE_ENDPOINT.into(),
            entitlements_endpoint: MINECRAFT_ENTITLEMENTS_ENDPOINT.into(),
        }
    }
}

impl ProfileClient {
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn fixture(base: &str) -> Self {
        let url = url::Url::parse(base).unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert!(url.port().is_some() && url.username().is_empty() && url.password().is_none());
        assert!(url.path() == "/" && url.query().is_none() && url.fragment().is_none());
        Self {
            profile_endpoint: url.join("profile").unwrap().into(),
            entitlements_endpoint: url.join("entitlements").unwrap().into(),
        }
    }

    pub(super) async fn sync(&self, token: &str) -> Result<SyncedProfile, MicrosoftAuthError> {
        let profile = minecraft_profile(token, &self.profile_endpoint).await?;
        let owns_minecraft_java =
            minecraft_entitlements(token, &self.entitlements_endpoint).await?;
        Ok(SyncedProfile {
            profile,
            owns_minecraft_java,
        })
    }

    async fn owned_profile(&self, token: &str) -> Result<MinecraftProfile, MicrosoftAuthError> {
        if !minecraft_entitlements(token, &self.entitlements_endpoint).await? {
            return Err(MicrosoftAuthError::new(
                MicrosoftAuthErrorKind::OwnershipMissing,
                MicrosoftAuthStep::MinecraftEntitlements,
            ));
        }
        minecraft_profile(token, &self.profile_endpoint).await
    }
}

/// Compare parsed URL components: string prefix checks admit lookalike paths.
pub fn is_redirect_url(url: &url::Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("login.live.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/oauth20_desktop.srf"
        && url.fragment().is_none()
}

fn validate_callback(url: &url::Url, expected_state: &str) -> Result<String, MicrosoftAuthError> {
    let invalid = || {
        MicrosoftAuthError::new(
            MicrosoftAuthErrorKind::InvalidCallback,
            MicrosoftAuthStep::Callback,
        )
    };
    if !is_redirect_url(url) {
        return Err(invalid());
    }
    let fields = url.query_pairs().collect::<Vec<_>>();
    let states = fields
        .iter()
        .filter(|(key, _)| key == "state")
        .collect::<Vec<_>>();
    if states.len() != 1 || states[0].1.as_ref() != expected_state {
        return Err(invalid());
    }
    let errors = fields
        .iter()
        .filter(|(key, _)| key == "error")
        .collect::<Vec<_>>();
    let codes = fields
        .iter()
        .filter(|(key, _)| key == "code")
        .collect::<Vec<_>>();
    if !errors.is_empty() {
        if errors.len() == 1 && errors[0].1 == "access_denied" && codes.is_empty() {
            return Err(MicrosoftAuthError::new(
                MicrosoftAuthErrorKind::Cancelled,
                MicrosoftAuthStep::Callback,
            ));
        }
        return Err(invalid());
    }
    if codes.len() != 1 || codes[0].1.trim().is_empty() || codes[0].1.len() > 8192 {
        return Err(invalid());
    }
    Ok(codes[0].1.to_string())
}

fn validate_authorization_url(raw: &str) -> Result<(), MicrosoftAuthError> {
    let invalid = || {
        MicrosoftAuthError::new(
            MicrosoftAuthErrorKind::Parse,
            MicrosoftAuthStep::SisuAuthenticate,
        )
    };
    let url = url::Url::parse(raw).map_err(|_| invalid())?;
    if url.scheme() != "https"
        || url.host_str() != Some("login.live.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct MinecraftProfile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub skins: Vec<MinecraftSkin>,
    #[serde(default)]
    pub capes: Vec<MinecraftCape>,
}
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct MinecraftSkin {
    pub id: String,
    pub state: String,
    pub url: String,
    pub variant: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct MinecraftCape {
    pub id: String,
    pub state: String,
    pub url: String,
}

/// Private provider result. Credentials are neither serializable nor debug printable.
pub struct MicrosoftMinecraftSession {
    pub(crate) microsoft_access_token: String,
    pub(crate) microsoft_refresh_token: Option<String>,
    pub(crate) microsoft_expires_at: u64,
    pub(crate) minecraft_access_token: String,
    pub(crate) minecraft_expires_at: u64,
    pub profile: MinecraftProfile,
}

#[derive(Debug)]
struct DeviceTokenPair {
    token: DeviceToken,
    key: DeviceProofKey,
}

impl DeviceTokenPair {
    async fn new(current_date: DateTime<Utc>) -> Result<Self, MicrosoftAuthError> {
        let key = DeviceProofKey::new()?;
        let token = device_token(&key, current_date).await?;
        Ok(Self { token, key })
    }
}

struct DeviceProofKey {
    id: Uuid,
    signing_key: SigningKey,
    x: String,
    y: String,
}

impl fmt::Debug for DeviceProofKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceProofKey")
            .field("id", &self.id)
            .field("signing_key", &"[redacted]")
            .field("x", &self.x)
            .field("y", &self.y)
            .finish()
    }
}

impl DeviceProofKey {
    fn new() -> Result<Self, MicrosoftAuthError> {
        let signing_key = SigningKey::random(&mut OsRng);
        let public_key = VerifyingKey::from(&signing_key);
        let encoded_point = public_key.to_encoded_point(false);
        let x = encoded_point
            .x()
            .map(|value| URL_SAFE_NO_PAD.encode(value))
            .ok_or_else(|| {
                MicrosoftAuthError::new(
                    MicrosoftAuthErrorKind::Parse,
                    MicrosoftAuthStep::DeviceToken,
                )
            })?;
        let y = encoded_point
            .y()
            .map(|value| URL_SAFE_NO_PAD.encode(value))
            .ok_or_else(|| {
                MicrosoftAuthError::new(
                    MicrosoftAuthErrorKind::Parse,
                    MicrosoftAuthStep::DeviceToken,
                )
            })?;

        Ok(Self {
            id: Uuid::new_v4(),
            signing_key,
            x,
            y,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DeviceToken {
    token: String,
    #[serde(default)]
    display_claims: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RedirectUri {
    msa_oauth_redirect: String,
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: Option<String>,
    expires_in: u64,
    scope: Option<String>,
}

struct OAuthToken {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: Option<String>,
    expires_in: u64,
    scope: Option<String>,
    current_date: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SisuAuthorize {
    title_token: DeviceToken,
    user_token: DeviceToken,
    #[serde(skip)]
    current_date: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct MinecraftTokenResponse {
    access_token: String,
    token_type: Option<String>,
    expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct MinecraftEntitlementsResponse {
    items: Vec<MinecraftEntitlement>,
}
#[derive(Debug, Deserialize)]
struct MinecraftEntitlement {
    name: String,
}

async fn device_token(
    key: &DeviceProofKey,
    current_date: DateTime<Utc>,
) -> Result<DeviceToken, MicrosoftAuthError> {
    let response = send_signed_request(
        None,
        DEVICE_AUTHENTICATE_ENDPOINT,
        "/device/authenticate",
        json!({
            "Properties": {
                "AuthMethod": "ProofOfPossession",
                "Id": format!("{{{}}}", key.id.to_string().to_uppercase()),
                "DeviceType": "Win32",
                "Version": "10.16.0",
                "ProofKey": {
                    "kty": "EC",
                    "x": key.x,
                    "y": key.y,
                    "crv": "P-256",
                    "alg": "ES256",
                    "use": "sig"
                }
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT"
        }),
        key,
        MicrosoftAuthStep::DeviceToken,
        current_date,
    )
    .await?;

    Ok(response.body)
}

async fn sisu_authenticate(
    pair: &DeviceTokenPair,
    challenge: &str,
    state: &str,
    current_date: DateTime<Utc>,
) -> Result<(String, String), MicrosoftAuthError> {
    let response: SignedRequestResponse<RedirectUri> = send_signed_request(
        None,
        SISU_AUTHENTICATE_ENDPOINT,
        "/authenticate",
        json!({
            "AppId": MICROSOFT_CLIENT_ID,
            "DeviceToken": pair.token.token,
            "Offers": [REQUESTED_SCOPE],
            "Query": {
                "code_challenge": challenge,
                "code_challenge_method": "S256",
                "state": state,
                "prompt": "select_account"
            },
            "RedirectUri": MICROSOFT_AUTH_REDIRECT_URL,
            "Sandbox": "RETAIL",
            "TokenType": "code",
            "TitleId": "1794566092"
        }),
        &pair.key,
        MicrosoftAuthStep::SisuAuthenticate,
        current_date,
    )
    .await?;

    let session_id = response
        .headers
        .get("X-SessionId")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            MicrosoftAuthError::new(
                MicrosoftAuthErrorKind::MissingSessionId,
                MicrosoftAuthStep::SisuAuthenticate,
            )
        })?;

    Ok((session_id, response.body.msa_oauth_redirect))
}

async fn oauth_token(code: &str, verifier: &str) -> Result<OAuthToken, MicrosoftAuthError> {
    let client = auth_client()?;
    let response = client
        .post(MICROSOFT_TOKEN_ENDPOINT)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[
            ("client_id", MICROSOFT_CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", MICROSOFT_AUTH_REDIRECT_URL),
            ("scope", REQUESTED_SCOPE),
        ])
        .send()
        .await
        .map_err(|_| {
            MicrosoftAuthError::new(
                MicrosoftAuthErrorKind::Request,
                MicrosoftAuthStep::OAuthToken,
            )
        })?;

    let current_date = get_date_header(response.headers());
    let body: OAuthTokenResponse = parse_response(response, MicrosoftAuthStep::OAuthToken).await?;

    Ok(OAuthToken {
        access_token: body.access_token,
        refresh_token: body.refresh_token,
        id_token: body.id_token,
        token_type: body.token_type,
        expires_in: body.expires_in,
        scope: body.scope,
        current_date,
    })
}

async fn oauth_refresh(refresh_token: &str) -> Result<OAuthToken, MicrosoftAuthError> {
    let client = auth_client()?;
    let response = client
        .post(MICROSOFT_TOKEN_ENDPOINT)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[
            ("client_id", MICROSOFT_CLIENT_ID),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
            ("redirect_uri", MICROSOFT_AUTH_REDIRECT_URL),
            ("scope", REQUESTED_SCOPE),
        ])
        .send()
        .await
        .map_err(|_| {
            MicrosoftAuthError::new(
                MicrosoftAuthErrorKind::Request,
                MicrosoftAuthStep::OAuthRefresh,
            )
        })?;

    let current_date = get_date_header(response.headers());
    let body: OAuthTokenResponse =
        parse_response(response, MicrosoftAuthStep::OAuthRefresh).await?;

    Ok(OAuthToken {
        access_token: body.access_token,
        refresh_token: body.refresh_token,
        id_token: body.id_token,
        token_type: body.token_type,
        expires_in: body.expires_in,
        scope: body.scope,
        current_date,
    })
}

async fn minecraft_session_from_oauth(
    oauth: OAuthToken,
    device_pair: &DeviceTokenPair,
    session_id: Option<&str>,
) -> Result<MicrosoftMinecraftSession, MicrosoftAuthError> {
    let authorized = sisu_authorize(
        session_id,
        &oauth.access_token,
        device_pair,
        oauth.current_date,
    )
    .await?;
    let xsts = xsts_authorize(&authorized, device_pair, authorized.current_date).await?;
    let minecraft = minecraft_token(&xsts).await?;
    let profile = ProfileClient::default()
        .owned_profile(&minecraft.access_token)
        .await?;
    if oauth.access_token.trim().is_empty() || minecraft.access_token.trim().is_empty() {
        return Err(MicrosoftAuthError::new(
            MicrosoftAuthErrorKind::Parse,
            MicrosoftAuthStep::MinecraftToken,
        ));
    }
    let now = Utc::now().timestamp().max(0) as u64;
    Ok(MicrosoftMinecraftSession {
        microsoft_access_token: oauth.access_token,
        microsoft_refresh_token: oauth.refresh_token,
        microsoft_expires_at: now.saturating_add(oauth.expires_in),
        minecraft_access_token: minecraft.access_token,
        minecraft_expires_at: now.saturating_add(
            minecraft
                .expires_in
                .unwrap_or(MINECRAFT_ACCESS_TOKEN_EXPIRES_IN),
        ),
        profile,
    })
}

async fn sisu_authorize(
    session_id: Option<&str>,
    access_token: &str,
    pair: &DeviceTokenPair,
    current_date: DateTime<Utc>,
) -> Result<SisuAuthorize, MicrosoftAuthError> {
    let response: SignedRequestResponse<SisuAuthorize> = send_signed_request(
        None,
        SISU_AUTHORIZE_ENDPOINT,
        "/authorize",
        json!({
            "AccessToken": format!("t={access_token}"),
            "AppId": MICROSOFT_CLIENT_ID,
            "DeviceToken": pair.token.token,
            "ProofKey": {
                "kty": "EC",
                "x": pair.key.x,
                "y": pair.key.y,
                "crv": "P-256",
                "alg": "ES256",
                "use": "sig"
            },
            "Sandbox": "RETAIL",
            "SessionId": session_id,
            "SiteName": "user.auth.xboxlive.com",
            "RelyingParty": "http://xboxlive.com",
            "UseModernGamertag": true
        }),
        &pair.key,
        MicrosoftAuthStep::SisuAuthorize,
        current_date,
    )
    .await?;

    Ok(SisuAuthorize {
        current_date: response.current_date,
        ..response.body
    })
}

async fn xsts_authorize(
    authorize: &SisuAuthorize,
    pair: &DeviceTokenPair,
    current_date: DateTime<Utc>,
) -> Result<DeviceToken, MicrosoftAuthError> {
    let response = send_signed_request(
        None,
        XSTS_AUTHORIZE_ENDPOINT,
        "/xsts/authorize",
        json!({
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT",
            "Properties": {
                "SandboxId": "RETAIL",
                "UserTokens": [authorize.user_token.token],
                "DeviceToken": pair.token.token,
                "TitleToken": authorize.title_token.token
            }
        }),
        &pair.key,
        MicrosoftAuthStep::XstsAuthorize,
        current_date,
    )
    .await?;

    Ok(response.body)
}

async fn minecraft_token(
    token: &DeviceToken,
) -> Result<MinecraftTokenResponse, MicrosoftAuthError> {
    let user_hash = token
        .display_claims
        .get("xui")
        .and_then(|value| value.get(0))
        .and_then(|value| value.get("uhs"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            MicrosoftAuthError::new(
                MicrosoftAuthErrorKind::MissingUserHash,
                MicrosoftAuthStep::MinecraftToken,
            )
        })?;

    let client = auth_client()?;
    let response = client
        .post(MINECRAFT_LAUNCHER_LOGIN_ENDPOINT)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::USER_AGENT, MINECRAFT_SERVICES_USER_AGENT)
        .json(&json!({
            "platform": "PC_LAUNCHER",
            "xtoken": format!("XBL3.0 x={};{}", user_hash, token.token)
        }))
        .send()
        .await
        .map_err(|_| {
            MicrosoftAuthError::new(
                MicrosoftAuthErrorKind::Request,
                MicrosoftAuthStep::MinecraftToken,
            )
        })?;

    parse_response(response, MicrosoftAuthStep::MinecraftToken).await
}

async fn minecraft_entitlements(token: &str, endpoint: &str) -> Result<bool, MicrosoftAuthError> {
    let client = auth_client()?;
    let response = client
        .get(endpoint)
        .query(&[("requestId", Uuid::new_v4().to_string())])
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::USER_AGENT, MINECRAFT_SERVICES_USER_AGENT)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|_| {
            MicrosoftAuthError::new(
                MicrosoftAuthErrorKind::Request,
                MicrosoftAuthStep::MinecraftEntitlements,
            )
        })?;

    let entitlements: MinecraftEntitlementsResponse =
        parse_response(response, MicrosoftAuthStep::MinecraftEntitlements).await?;
    Ok(entitlements
        .items
        .iter()
        .any(|item| matches!(item.name.as_str(), "game_minecraft" | "product_minecraft")))
}

async fn minecraft_profile(
    token: &str,
    endpoint: &str,
) -> Result<MinecraftProfile, MicrosoftAuthError> {
    let client = auth_client()?;
    let response = client
        .get(endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::USER_AGENT, MINECRAFT_SERVICES_USER_AGENT)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|_| {
            MicrosoftAuthError::new(
                MicrosoftAuthErrorKind::Request,
                MicrosoftAuthStep::MinecraftProfile,
            )
        })?;

    let profile: MinecraftProfile =
        parse_response(response, MicrosoftAuthStep::MinecraftProfile).await?;
    validate_profile(&profile)?;
    Ok(profile)
}

struct SignedRequestResponse<T> {
    headers: HeaderMap,
    current_date: DateTime<Utc>,
    body: T,
}

async fn send_signed_request<T: DeserializeOwned>(
    authorization: Option<&str>,
    url: &str,
    url_path: &str,
    raw_body: serde_json::Value,
    key: &DeviceProofKey,
    step: MicrosoftAuthStep,
    current_date: DateTime<Utc>,
) -> Result<SignedRequestResponse<T>, MicrosoftAuthError> {
    let authorization_bytes =
        authorization.map_or_else(Vec::new, |value| value.as_bytes().to_vec());
    let body = serde_json::to_vec(&raw_body)
        .map_err(|_| MicrosoftAuthError::new(MicrosoftAuthErrorKind::Parse, step))?;
    let file_time = ((current_date.timestamp() as u128) + 11_644_473_600) * 10_000_000;

    let mut signature_payload = Vec::new();
    signature_payload.extend_from_slice(&1_u32.to_be_bytes());
    signature_payload.push(0);
    signature_payload.extend_from_slice(&(file_time as u64).to_be_bytes());
    signature_payload.push(0);
    signature_payload.extend_from_slice(b"POST");
    signature_payload.push(0);
    signature_payload.extend_from_slice(url_path.as_bytes());
    signature_payload.push(0);
    signature_payload.extend_from_slice(&authorization_bytes);
    signature_payload.push(0);
    signature_payload.extend_from_slice(&body);
    signature_payload.push(0);

    let signature: Signature = key.signing_key.sign(&signature_payload);
    let mut signature_buffer = Vec::new();
    signature_buffer.extend_from_slice(&1_i32.to_be_bytes());
    signature_buffer.extend_from_slice(&(file_time as u64).to_be_bytes());
    signature_buffer.extend_from_slice(&signature.r().to_bytes());
    signature_buffer.extend_from_slice(&signature.s().to_bytes());
    let signature = BASE64_STANDARD.encode(signature_buffer);

    let client = auth_client()?;
    let mut request = client
        .post(url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .header("Signature", signature);

    if url != SISU_AUTHORIZE_ENDPOINT {
        request = request.header("x-xbl-contract-version", "1");
    }

    if let Some(authorization) = authorization {
        request = request.header(reqwest::header::AUTHORIZATION, authorization);
    }

    let response = request
        .body(body)
        .send()
        .await
        .map_err(|_| MicrosoftAuthError::new(MicrosoftAuthErrorKind::Request, step))?;

    let headers = response.headers().clone();
    let current_date = get_date_header(&headers);
    let body = parse_response(response, step).await?;

    Ok(SignedRequestResponse {
        headers,
        current_date,
        body,
    })
}

async fn parse_response<T: DeserializeOwned>(
    response: reqwest::Response,
    step: MicrosoftAuthStep,
) -> Result<T, MicrosoftAuthError> {
    let status = response.status();
    if !status.is_success() {
        return Err(MicrosoftAuthError::upstream(status, step));
    }

    let body = bounded_response_body(response, step).await?;
    serde_json::from_slice(&body)
        .map_err(|_| MicrosoftAuthError::new(MicrosoftAuthErrorKind::Parse, step))
}

async fn bounded_response_body(
    mut response: reqwest::Response,
    step: MicrosoftAuthStep,
) -> Result<Vec<u8>, MicrosoftAuthError> {
    if response
        .content_length()
        .is_some_and(|content_length| content_length > MAX_MICROSOFT_AUTH_RESPONSE_BYTES as u64)
    {
        return Err(MicrosoftAuthError::new(MicrosoftAuthErrorKind::Parse, step));
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| MicrosoftAuthError::new(MicrosoftAuthErrorKind::Request, step))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_MICROSOFT_AUTH_RESPONSE_BYTES {
            return Err(MicrosoftAuthError::new(MicrosoftAuthErrorKind::Parse, step));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

fn auth_client() -> Result<&'static Client, MicrosoftAuthError> {
    MICROSOFT_AUTH_CLIENT
        .get_or_init(|| {
            Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(MICROSOFT_AUTH_TIMEOUT)
                .build()
                .map_err(|_| {
                    MicrosoftAuthError::new(
                        MicrosoftAuthErrorKind::ClientBuild,
                        MicrosoftAuthStep::DeviceToken,
                    )
                })
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn get_date_header(headers: &HeaderMap) -> DateTime<Utc> {
    headers
        .get(reqwest::header::DATE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| DateTime::parse_from_rfc2822(value).ok())
        .map_or_else(Utc::now, |value| value.with_timezone(&Utc))
}

fn generate_oauth_challenge() -> String {
    let mut bytes = [0_u8; 64];
    OsRng.fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn error_kind_for_status(status: StatusCode) -> MicrosoftAuthErrorKind {
    if status.is_server_error() {
        MicrosoftAuthErrorKind::UpstreamUnavailable
    } else {
        MicrosoftAuthErrorKind::UpstreamRejected
    }
}

pub fn validate_profile(profile: &MinecraftProfile) -> Result<(), MicrosoftAuthError> {
    if uuid::Uuid::parse_str(&profile.id)
        .ok()
        .is_none_or(|id| id.is_nil())
        || profile.name.is_empty()
        || profile.name.len() > 16
        || !profile
            .name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        || profile.skins.len() > 128
        || profile.capes.len() > 128
    {
        return Err(MicrosoftAuthError::new(
            MicrosoftAuthErrorKind::Parse,
            MicrosoftAuthStep::MinecraftProfile,
        ));
    }
    for raw in profile
        .skins
        .iter()
        .map(|skin| &skin.url)
        .chain(profile.capes.iter().map(|cape| &cape.url))
    {
        let valid = url::Url::parse(raw).ok().is_some_and(|url| {
            matches!(url.scheme(), "https" | "http")
                && url.host_str() == Some("textures.minecraft.net")
                && url.port().is_none()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path().strip_prefix("/texture/").is_some_and(|hash| {
                    !hash.is_empty()
                        && hash.len() <= 128
                        && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        });
        if !valid {
            return Err(MicrosoftAuthError::new(
                MicrosoftAuthErrorKind::Parse,
                MicrosoftAuthStep::MinecraftProfile,
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn login_and_refresh_profile_admission_requires_positive_entitlements() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = ProfileClient::fixture(&format!("http://{}", listener.local_addr().unwrap()));
        let response = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut bytes = [0; 1024];
                let read = stream.read(&mut bytes).await.unwrap();
                assert!(read > 0 && request.len() + read <= 8192);
                request.extend_from_slice(&bytes[..read]);
                if request.windows(4).any(|end| end == b"\r\n\r\n") {
                    break;
                }
            }
            assert!(request.starts_with(b"GET /entitlements?"));
            let body = br#"{"items":[]}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(body).await.unwrap();
        });
        let error = client
            .owned_profile("synthetic-minecraft")
            .await
            .unwrap_err();
        assert_eq!(error.kind(), MicrosoftAuthErrorKind::OwnershipMissing);
        assert_eq!(error.step(), MicrosoftAuthStep::MinecraftEntitlements);
        response.await.unwrap();
    }

    #[test]
    fn callback_requires_exact_redirect_and_unique_matching_state_and_code() {
        let accepted = url::Url::parse(
            "https://login.live.com/oauth20_desktop.srf?code=abc123&state=expected",
        )
        .unwrap();
        assert_eq!(validate_callback(&accepted, "expected").unwrap(), "abc123");
        for raw in [
            "https://login.live.com/oauth20_desktop.srf.evil?code=abc&state=expected",
            "https://login.live.com.evil/oauth20_desktop.srf?code=abc&state=expected",
            "http://login.live.com/oauth20_desktop.srf?code=abc&state=expected",
            "https://evil@login.live.com/oauth20_desktop.srf?code=abc&state=expected",
            "https://login.live.com/oauth20_desktop.srf?code=abc&state=wrong",
            "https://login.live.com/oauth20_desktop.srf?code=abc",
            "https://login.live.com/oauth20_desktop.srf?code=abc&state=expected&state=expected",
            "https://login.live.com/oauth20_desktop.srf?code=abc&code=def&state=expected",
            "https://login.live.com/oauth20_desktop.srf?code=abc&state=expected#fragment",
        ] {
            assert!(
                validate_callback(&url::Url::parse(raw).unwrap(), "expected").is_err(),
                "{raw}"
            );
        }
    }
    #[test]
    fn cancellation_still_requires_state() {
        let cancelled = url::Url::parse(
            "https://login.live.com/oauth20_desktop.srf?error=access_denied&state=expected",
        )
        .unwrap();
        assert_eq!(
            validate_callback(&cancelled, "expected")
                .unwrap_err()
                .kind(),
            MicrosoftAuthErrorKind::Cancelled
        );
        assert_eq!(
            validate_callback(&cancelled, "different")
                .unwrap_err()
                .kind(),
            MicrosoftAuthErrorKind::InvalidCallback
        );
    }
    #[test]
    fn profile_rejects_untrusted_texture_and_invalid_identity() {
        let mut profile = MinecraftProfile {
            id: "12345678123442348234123456789abc".into(),
            name: "Player_1".into(),
            skins: vec![],
            capes: vec![],
        };
        assert!(validate_profile(&profile).is_ok());
        profile.skins.push(MinecraftSkin {
            id: "skin".into(),
            state: "ACTIVE".into(),
            variant: "CLASSIC".into(),
            url: "http://127.0.0.1/secret".into(),
        });
        assert!(validate_profile(&profile).is_err());
        profile.skins.clear();
        profile.id = "provider-garbage".into();
        assert!(validate_profile(&profile).is_err());
    }
    #[test]
    fn microsoft_auth_error_messages_do_not_expose_tokens() {
        let error = MicrosoftAuthError::upstream(
            StatusCode::UNAUTHORIZED,
            MicrosoftAuthStep::XstsAuthorize,
        );
        assert_eq!(error.provider_status(), Some(401));
        assert!(!format!("{error:?}").contains("access_token"));
    }
}

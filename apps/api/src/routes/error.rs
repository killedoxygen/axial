//! The one owner of public HTTP failures: each feature error's status and its
//! sanitized body. Bodies carry owner copy, never extractor or provider detail.

use axial_app::{
    accounts::{credential_store::CredentialError, model::AccountError, session::AuthError},
    content::install::MutationError,
    content::packs::PackError,
    install::queue::InstallError,
    instances::{delete::DeletionError, model::InstanceError},
    launch::coordinator::{LaunchError, LaunchErrorResponse},
    media::MediaError,
    music::MusicError,
    composition::{
        CompositionError,
    },
    public::{ErrorResponse, Retry},
    resources::{ResourceError, folders::FolderError, mods::ModImportError},
    settings::SettingsError,
    skins::{SkinError, library::SkinLibraryError, lookup::ProfileMediaError},
    update::UpdateError,
};
use axum::{
    Json,
    extract::{
        Query,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use std::fmt::Display;

pub(crate) type ApiResult<T> = Result<Json<T>, ApiError>;

/// A rendered public failure.
pub(crate) struct ApiError(Response);

impl ApiError {
    pub(crate) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self::body(status, ErrorResponse::new(message))
    }

    pub(crate) fn body(status: StatusCode, body: impl Serialize) -> Self {
        Self((status, Json(body)).into_response())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        self.0
    }
}

/// A feature failure whose display text is already sanitized public copy.
pub(crate) trait PublicError: Sized + Display {
    fn status(&self) -> StatusCode;

    /// Only refusals the owner makes before any effect, because other work
    /// holds the state, may be repeated unchanged.
    fn retry(&self) -> Option<Retry> {
        None
    }

    /// Owners whose failures carry more than a message override the body.
    fn into_api(self) -> ApiError {
        ApiError::body(
            self.status(),
            ErrorResponse {
                error: self.to_string(),
                retry: self.retry(),
            },
        )
    }
}

impl<E: PublicError> From<E> for ApiError {
    fn from(error: E) -> Self {
        error.into_api()
    }
}

/// Extractor rejections can echo untrusted input; routes answer with fixed refusals.
pub(crate) trait Extracted<T> {
    fn or_refuse<E: Into<ApiError>>(self, refusal: impl FnOnce() -> E) -> Result<T, ApiError>;
}

impl<T> Extracted<T> for Result<Json<T>, JsonRejection> {
    fn or_refuse<E: Into<ApiError>>(self, refusal: impl FnOnce() -> E) -> Result<T, ApiError> {
        self.map(|Json(value)| value).map_err(|_| refusal().into())
    }
}

impl<T> Extracted<T> for Result<Query<T>, QueryRejection> {
    fn or_refuse<E: Into<ApiError>>(self, refusal: impl FnOnce() -> E) -> Result<T, ApiError> {
        self.map(|Query(value)| value).map_err(|_| refusal().into())
    }
}

/// Settings bodies keep the rejection's status with fixed copy.
pub(crate) fn json_rejection(error: JsonRejection) -> ApiError {
    let status = error.status();
    let message = match status {
        StatusCode::BAD_REQUEST => "Invalid JSON syntax.",
        StatusCode::PAYLOAD_TOO_LARGE => "Request body is too large.",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "JSON content type is required.",
        _ => "Invalid JSON request.",
    };
    ApiError::new(status, message)
}

impl PublicError for SettingsError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Validation(_) => StatusCode::BAD_REQUEST,
            Self::Conflict => StatusCode::CONFLICT,
            Self::UnknownFlag => StatusCode::NOT_FOUND,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Corrupt | Self::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// Account failures name a stable code beside their message.
#[derive(Serialize)]
struct AccountFailure {
    error: String,
    status: &'static str,
}

pub(crate) fn account_failure(status: StatusCode, error: String, code: &'static str) -> ApiError {
    ApiError::body(
        status,
        AccountFailure {
            error,
            status: code,
        },
    )
}

impl PublicError for AccountError {
    fn status(&self) -> StatusCode {
        account_code(self).0
    }

    fn into_api(self) -> ApiError {
        let (status, code) = account_code(&self);
        account_failure(status, self.to_string(), code)
    }
}

impl PublicError for AuthError {
    fn status(&self) -> StatusCode {
        auth_code(self).0
    }

    fn into_api(self) -> ApiError {
        let (status, code) = auth_code(&self);
        account_failure(status, self.to_string(), code)
    }
}

fn account_code(error: &AccountError) -> (StatusCode, &'static str) {
    match error {
        AccountError::InvalidInput(_) | AccountError::NotOffline | AccountError::NotMicrosoft => {
            (StatusCode::BAD_REQUEST, "account_input_invalid")
        }
        AccountError::NotFound => (StatusCode::NOT_FOUND, "account_not_found"),
        AccountError::StaleCapture | AccountError::AlreadyExists => {
            (StatusCode::CONFLICT, "account_changed")
        }
        AccountError::NoSelection => (StatusCode::PRECONDITION_FAILED, "account_required"),
        AccountError::InvalidStoredData | AccountError::Storage => {
            (StatusCode::SERVICE_UNAVAILABLE, "account_unavailable")
        }
    }
}

fn auth_code(error: &AuthError) -> (StatusCode, &'static str) {
    match error {
        AuthError::Account(error) => account_code(error),
        AuthError::SignInRequired | AuthError::LoginExpired => {
            (StatusCode::PRECONDITION_FAILED, "sign_in_required")
        }
        AuthError::OwnershipMissing => (StatusCode::CONFLICT, "minecraft_ownership_missing"),
        AuthError::Credentials(CredentialError::Stale) => (StatusCode::CONFLICT, "account_changed"),
        AuthError::Credentials(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "credential_store_unavailable",
        ),
        AuthError::Provider(_) => (StatusCode::BAD_GATEWAY, "minecraft_auth_chain_failed"),
        AuthError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "account_unavailable"),
    }
}

impl PublicError for MutationError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Changed
            | Self::Conflict
            | Self::Pending
            | Self::Required
            | Self::Managed
            | Self::Duplicate(_)
            | Self::Incompatible(_)
            | Self::Cancelled
            | Self::Pack(
                PackError::Cancelled | PackError::SelectionChanged | PackError::Conflict(_),
            ) => StatusCode::CONFLICT,
            Self::NotFound | Self::Pack(PackError::NoMatchingVersion) => StatusCode::NOT_FOUND,
            Self::Capacity | Self::Pack(PackError::TooLarge) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Unavailable | Self::Pack(PackError::Download | PackError::Provider) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::Integrity | Self::Pack(PackError::Integrity) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::StreamingUnsupported => StatusCode::NOT_IMPLEMENTED,
            Self::Pack(_) | Self::Content(_) => StatusCode::BAD_REQUEST,
            Self::Files | Self::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl PublicError for ResourceError {
    fn status(&self) -> StatusCode {
        match self {
            Self::InvalidName => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict
            | Self::Busy
            | Self::Managed
            | Self::Pending
            | Self::Unsupported
            | Self::Required(_) => StatusCode::CONFLICT,
            Self::Limit => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Files => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn into_api(self) -> ApiError {
        match self {
            Self::Required(refusal) => ApiError::body(StatusCode::CONFLICT, refusal),
            error => ApiError::new(error.status(), error.to_string()),
        }
    }
}

impl PublicError for ModImportError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Count | Self::NotJar | Self::InvalidArchive(_) => StatusCode::BAD_REQUEST,
            Self::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Repeated(_) | Self::Duplicate(_) | Self::Busy | Self::Pending | Self::Changed => {
                StatusCode::CONFLICT
            }
            Self::Files => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl PublicError for FolderError {
    fn status(&self) -> StatusCode {
        match self {
            Self::InvalidFolder => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Busy | Self::Pending => StatusCode::CONFLICT,
            Self::Capacity | Self::Closed => StatusCode::SERVICE_UNAVAILABLE,
            Self::Prepare | Self::Spawn => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl PublicError for InstanceError {
    fn status(&self) -> StatusCode {
        match self {
            Self::InvalidId | Self::InvalidName | Self::InvalidInput | Self::InvalidSettings => {
                StatusCode::BAD_REQUEST
            }
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::InstalledVersionsDegraded => StatusCode::PRECONDITION_FAILED,
            Self::Conflict
            | Self::NameConflict
            | Self::Busy
            | Self::Cancelled
            | Self::ManagedDuplicateUnavailable
            | Self::DirectoryUnavailable
            | Self::SettlementRequired
            | Self::Storage(_) => StatusCode::CONFLICT,
            Self::Closed
            | Self::SetupUnavailable
            | Self::LibraryUnavailable
            | Self::VersionUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    fn retry(&self) -> Option<Retry> {
        matches!(self, Self::Busy).then_some(Retry::Busy)
    }
}

impl PublicError for DeletionError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Instance(error) => error.status(),
            Self::NotFound => StatusCode::NOT_FOUND,
            _ => StatusCode::CONFLICT,
        }
    }

    fn retry(&self) -> Option<Retry> {
        match self {
            Self::Instance(error) => error.retry(),
            _ => None,
        }
    }
}

impl PublicError for InstallError {
    fn status(&self) -> StatusCode {
        match self {
            Self::InvalidRequest => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Busy
            | Self::SettlementRequired
            | Self::NotReady
            | Self::ClientJarMissing
            | Self::ClientJarCorrupt
            | Self::VersionJsonMissing
            | Self::LibrariesMissing
            | Self::LibrariesCorrupt
            | Self::AssetIndexMissing
            | Self::AssetIndexCorrupt => StatusCode::CONFLICT,
            Self::AtCapacity => StatusCode::TOO_MANY_REQUESTS,
            Self::ContentUnavailable | Self::LoaderUnavailable | Self::Cancelled | Self::Closed => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn retry(&self) -> Option<Retry> {
        matches!(self, Self::Busy | Self::AtCapacity).then_some(Retry::Busy)
    }
}

impl PublicError for LaunchError {
    fn status(&self) -> StatusCode {
        match self {
            Self::InvalidRequest
            | Self::InvalidIntent
            | Self::InvalidMemory
            | Self::InvalidUsername => StatusCode::BAD_REQUEST,
            Self::InstanceNotFound => StatusCode::NOT_FOUND,
            Self::Closed
            | Self::AtCapacity
            | Self::IntentUnavailable
            | Self::RecoveryIncomplete => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::CONFLICT,
        }
    }

    fn into_api(self) -> ApiError {
        ApiError::body(self.status(), LaunchErrorResponse::from(self))
    }
}

impl PublicError for CompositionError {
    fn status(&self) -> StatusCode {
        match self {
            Self::InstanceNotFound | Self::SnapshotNotFound => StatusCode::NOT_FOUND,
            Self::Storage(_) => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::CONFLICT,
        }
    }
}

impl PublicError for SkinError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Library(error) => error.status(),
            Self::Profile(error) => error.status(),
        }
    }
}

impl PublicError for SkinLibraryError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Conflict => StatusCode::CONFLICT,
            Self::Image(MediaError::TooLarge) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Capacity | Self::StorageFull => StatusCode::INSUFFICIENT_STORAGE,
            Self::Storage | Self::PermissionDenied | Self::InvalidData => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

impl PublicError for ProfileMediaError {
    fn status(&self) -> StatusCode {
        match self {
            Self::InvalidUsername | Self::InvalidTexture | Self::InvalidImage | Self::Rejected => {
                StatusCode::BAD_REQUEST
            }
            Self::AccountRequired => StatusCode::UNAUTHORIZED,
            Self::NotFound | Self::MissingSkin | Self::MissingCape | Self::SavedSkinMissing => {
                StatusCode::NOT_FOUND
            }
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::OwnershipMissing
            | Self::StaleIdentity
            | Self::StaleIntent
            | Self::Cancelled
            | Self::Busy => StatusCode::CONFLICT,
            Self::ShuttingDown => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::BAD_GATEWAY,
        }
    }
}

impl PublicError for MusicError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::DownloadFailed => StatusCode::BAD_GATEWAY,
        }
    }
}

/// Update failures name a stable code beside their message.
#[derive(Serialize)]
struct UpdateFailure {
    code: &'static str,
    error: String,
}

pub(crate) fn update_failure(status: StatusCode, code: &'static str, error: String) -> ApiError {
    ApiError::body(status, UpdateFailure { code, error })
}

impl PublicError for UpdateError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Unsupported => StatusCode::NOT_IMPLEMENTED,
            Self::Busy | Self::StaleRelease | Self::NotReady => StatusCode::CONFLICT,
            Self::Failed(_) => StatusCode::BAD_GATEWAY,
        }
    }

    fn into_api(self) -> ApiError {
        let code = match self {
            Self::Unsupported => "update_unsupported",
            Self::Busy => "update_busy",
            Self::StaleRelease => "update_stale_release",
            Self::NotReady => "update_not_ready",
            Self::Failed(_) => "update_failed",
        };
        let message = if matches!(self, Self::Unsupported) {
            "In-app updates are unavailable for this build because a trusted release feed has not been configured.".into()
        } else {
            self.to_string().chars().take(500).collect()
        };
        update_failure(self.status(), code, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body(error: impl PublicError) -> (StatusCode, serde_json::Value) {
        let response = error.into_api().into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 64 << 10)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn only_owner_busy_refusals_are_marked_repeatable() {
        for (error, status) in [
            (body(InstanceError::Busy).await, StatusCode::CONFLICT),
            (body(InstallError::Busy).await, StatusCode::CONFLICT),
            (body(InstallError::AtCapacity).await, StatusCode::TOO_MANY_REQUESTS),
        ] {
            assert_eq!(error.0, status);
            assert_eq!(error.1["retry"], "busy", "{}", error.1);
        }
        // Genuine conflicts and unavailability with the same status classes
        // keep their own copy and carry no retry.
        for (error, status) in [
            (body(InstanceError::SettlementRequired).await, StatusCode::CONFLICT),
            (body(InstallError::NotReady).await, StatusCode::CONFLICT),
            (body(InstanceError::SetupUnavailable).await, StatusCode::SERVICE_UNAVAILABLE),
            (body(SettingsError::Unavailable).await, StatusCode::SERVICE_UNAVAILABLE),
        ] {
            assert_eq!(error.0, status);
            let object = error.1.as_object().unwrap();
            assert_eq!(object.keys().collect::<Vec<_>>(), ["error"], "{}", error.1);
        }
        // The exact bodies the frontend startup test replays through its transport.
        for (error, serialized) in [
            (
                body(InstanceError::Busy).await.1,
                r#"{"error":"The instance is busy. Wait for the current operation to finish.","retry":"busy"}"#,
            ),
            (
                body(InstanceError::SettlementRequired).await.1,
                r#"{"error":"The operation requires settlement before this instance can be used."}"#,
            ),
            (
                body(SettingsError::Unavailable).await.1,
                r#"{"error":"Settings are temporarily unavailable."}"#,
            ),
        ] {
            assert_eq!(error, serde_json::from_str::<serde_json::Value>(serialized).unwrap());
        }
    }
}

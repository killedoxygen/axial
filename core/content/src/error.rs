use axial_minecraft::download::ExecutionDownloadError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ContentError {
    #[error("content request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("content response was not valid: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("content provider returned {status} for {context}")]
    Status {
        status: reqwest::StatusCode,
        context: String,
    },
    #[error("content file operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Download(#[from] ExecutionDownloadError),
    #[error("content download preparation failed: {0}")]
    DownloadPreparation(String),
    #[error("content is not available for the requested loader or game version")]
    Unavailable,
    #[error("content request was invalid: {0}")]
    Invalid(String),
}

pub type ContentResult<T> = Result<T, ContentError>;

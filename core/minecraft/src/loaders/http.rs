use super::types::{LoaderError, LoaderProviderFailureKind};
use crate::http::{self, FetchError};
use serde::de::DeserializeOwned;

const MAX_LOADER_JSON_BYTES: u64 = 8 * 1024 * 1024;

pub async fn fetch_json<T>(url: &str) -> Result<T, LoaderError>
where
    T: DeserializeOwned,
{
    let bytes = fetch(url, MAX_LOADER_JSON_BYTES)
        .await
        .map_err(provider_error)?;
    serde_json::from_slice(&bytes).map_err(|_| LoaderError::ProviderDataInvalid {
        kind: LoaderProviderFailureKind::SchemaInvalid,
        status: None,
    })
}

pub async fn fetch_bytes(url: &str, max_size: u64) -> Result<Vec<u8>, LoaderError> {
    fetch(url, max_size)
        .await
        .map_err(|error| match error.status() {
            Some(404) => LoaderError::ArtifactMissing("artifact returned HTTP 404".to_string()),
            _ => provider_error(error),
        })
}

async fn fetch(url: &str, limit: u64) -> Result<Vec<u8>, FetchError> {
    let client = http::client_for(url)?;
    Ok(http::fetch(client, url, limit, http::Expected::default())
        .await?
        .bytes)
}

#[cfg(feature = "test-support")]
pub(crate) fn validate_loopback_url_for_test(url: &reqwest::Url) -> Result<(), LoaderError> {
    if !http::is_loopback_http(url) || !url.username().is_empty() || url.password().is_some() {
        return Err(insecure_loader_source_error(
            "loader test source must use literal loopback HTTP",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(insecure_loader_source_error(
            "loader test source cannot contain query or fragment",
        ));
    }
    Ok(())
}

#[cfg(feature = "test-support")]
pub(crate) async fn fetch_json_from_loopback_for_test<T>(
    url: &reqwest::Url,
) -> Result<T, LoaderError>
where
    T: DeserializeOwned,
{
    validate_loopback_url_for_test(url)?;
    fetch_json(url.as_str()).await
}

#[cfg(feature = "test-support")]
pub(crate) async fn fetch_bytes_from_loopback_for_test(
    url: &reqwest::Url,
    max_size: u64,
) -> Result<Vec<u8>, LoaderError> {
    validate_loopback_url_for_test(url)?;
    fetch_bytes(url.as_str(), max_size).await
}

fn insecure_loader_source_error(message: &str) -> LoaderError {
    LoaderError::InstallExecutionFailed(message.to_string())
}

fn provider_error(error: FetchError) -> LoaderError {
    let unavailable = |kind| LoaderError::ProviderUnavailable { kind, status: None };
    match error {
        FetchError::Refused => insecure_loader_source_error("loader source must use HTTPS"),
        FetchError::Request(error) if error.is_redirect() => {
            insecure_loader_source_error("loader source redirect was refused")
        }
        FetchError::Request(error) => match error.status() {
            Some(status) => provider_status_error(status),
            None if error.is_timeout() => unavailable(LoaderProviderFailureKind::Timeout),
            None => unavailable(LoaderProviderFailureKind::Network),
        },
        FetchError::Incomplete => unavailable(LoaderProviderFailureKind::Network),
        FetchError::TooLarge { .. } | FetchError::Allocation => LoaderError::ProviderDataInvalid {
            kind: LoaderProviderFailureKind::ResponseTooLarge,
            status: None,
        },
        FetchError::SizeMismatch { .. } | FetchError::DigestMismatch { .. } => {
            LoaderError::Verify("loader source does not match its metadata".to_string())
        }
    }
}

fn provider_status_error(status: reqwest::StatusCode) -> LoaderError {
    let kind = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        LoaderProviderFailureKind::HttpRateLimited
    } else if status.is_server_error() {
        LoaderProviderFailureKind::HttpServer
    } else if status == reqwest::StatusCode::NOT_FOUND {
        LoaderProviderFailureKind::HttpNotFound
    } else {
        LoaderProviderFailureKind::HttpStatus
    };
    LoaderError::ProviderUnavailable {
        kind,
        status: Some(status.as_u16()),
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_LOADER_JSON_BYTES, fetch_bytes, fetch_json};
    use crate::http::test_server::{response, serve};
    use crate::loaders::types::{LoaderError, LoaderProviderFailureKind};
    use serde_json::Value;

    #[tokio::test]
    async fn production_loader_source_rejects_http_before_request() {
        let error = fetch_bytes("http://192.0.2.1/installer.jar", 16)
            .await
            .expect_err("production loader source must reject HTTP");
        assert!(matches!(
            error,
            LoaderError::InstallExecutionFailed(message)
                if message == "loader source must use HTTPS"
        ));
    }

    #[tokio::test]
    async fn fetch_bytes_maps_http_404_to_artifact_missing_without_retry() {
        let server = serve(vec![response("404 Not Found", &[], b"missing")]).await;
        let url = server.url("/missing-installer.jar");
        match fetch_bytes(&url, 1024).await.expect_err("404 error") {
            LoaderError::ArtifactMissing(message) => {
                assert!(message.contains("HTTP 404") && !message.contains(&url));
            }
            error => panic!("expected ArtifactMissing, got {error:?}"),
        }
        assert_eq!(server.finish().len(), 1);
    }

    #[tokio::test]
    async fn fetch_json_parses_bounded_bodies_and_classifies_failures_without_payload() {
        let oversized = MAX_LOADER_JSON_BYTES + 1;
        let server = serve(vec![
            response("200 OK", &[], br#"{"value":"ok"}"#),
            response("200 OK", &[("Content-Length", &oversized.to_string())], b""),
            response("400 Bad Request", &[], br#"{"token":"secret"}"#),
            response("200 OK", &[], b"{not-json"),
        ])
        .await;
        let url = server.url("/metadata.json");
        let value: Value = fetch_json(&url).await.unwrap();
        assert_eq!(value["value"], "ok");
        for expected in [
            LoaderError::ProviderDataInvalid {
                kind: LoaderProviderFailureKind::ResponseTooLarge,
                status: None,
            },
            LoaderError::ProviderUnavailable {
                kind: LoaderProviderFailureKind::HttpStatus,
                status: Some(400),
            },
            LoaderError::ProviderDataInvalid {
                kind: LoaderProviderFailureKind::SchemaInvalid,
                status: None,
            },
        ] {
            let error = fetch_json::<Value>(&url).await.unwrap_err();
            let encoded = error.to_string();
            assert_eq!(format!("{error:?}"), format!("{expected:?}"));
            for private in ["secret", "not-json", "metadata.json"] {
                assert!(!encoded.contains(private), "{encoded}");
            }
        }
        assert_eq!(server.finish().len(), 4);
    }
}

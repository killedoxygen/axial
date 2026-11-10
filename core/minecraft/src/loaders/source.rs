use super::http::fetch_bytes;
use super::types::LoaderError;
use sha1::Digest as _;
use std::sync::Arc;

const MAX_SOURCE_SHA1_PROOF_BYTES: u64 = 128;

/// Bytes that matched the live SHA-1 sidecar published beside `provider_url`.
#[derive(Debug)]
pub(crate) struct VerifiedLoaderSource {
    bytes: Arc<[u8]>,
    provider_url: String,
    logical_identity: String,
}

impl VerifiedLoaderSource {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn shared_bytes(&self) -> Arc<[u8]> {
        Arc::clone(&self.bytes)
    }

    pub(crate) fn matches_contract(&self, provider_url: &str, logical_identity: &str) -> bool {
        self.provider_url == provider_url && self.matches_logical_identity(logical_identity)
    }

    pub(crate) fn matches_logical_identity(&self, logical_identity: &str) -> bool {
        self.logical_identity == logical_identity
    }

    pub(crate) fn into_shared_bytes_for(
        self,
        provider_url: &str,
        logical_identity: &str,
    ) -> Result<Arc<[u8]>, LoaderError> {
        if !self.matches_contract(provider_url, logical_identity) {
            return Err(LoaderError::Verify(
                "authenticated loader source does not match its selected contract".to_string(),
            ));
        }
        Ok(self.bytes)
    }

    #[cfg(test)]
    pub(crate) fn from_test_bytes(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Arc::from(bytes),
            provider_url: "https://fixtures.invalid/loader-source.jar".to_string(),
            logical_identity: "test-loader-source".to_string(),
        }
    }
}

pub(crate) async fn fetch_sha1_verified_source(
    source_url: &str,
    max_source_bytes: u64,
    source_label: &'static str,
    logical_identity: &str,
) -> Result<VerifiedLoaderSource, LoaderError> {
    let proof_url = sha1_sidecar_url(source_url)?;
    let proof_bytes = fetch_bytes(&proof_url, MAX_SOURCE_SHA1_PROOF_BYTES).await?;
    let sha1 = strict_sha1_proof(&proof_bytes, source_label)?;
    let bytes = fetch_bytes(source_url, max_source_bytes).await?;
    if <[u8; 20]>::from(sha1::Sha1::digest(&bytes)) != sha1 {
        return Err(LoaderError::Verify(format!(
            "{source_label} does not match its live sha1 proof"
        )));
    }
    Ok(VerifiedLoaderSource {
        bytes: Arc::from(bytes),
        provider_url: source_url.to_string(),
        logical_identity: logical_identity.to_string(),
    })
}

fn sha1_sidecar_url(source_url: &str) -> Result<String, LoaderError> {
    let mut url = reqwest::Url::parse(source_url)
        .map_err(|_| LoaderError::InvalidProfile("loader source URL is invalid".to_string()))?;
    if url.query().is_some() {
        return Err(LoaderError::InvalidProfile(
            "loader source URL must not contain a query".to_string(),
        ));
    }
    if url.fragment().is_some() {
        return Err(LoaderError::InvalidProfile(
            "loader source URL must not contain a fragment".to_string(),
        ));
    }
    let proof_path = format!("{}.sha1", url.path());
    url.set_path(&proof_path);
    Ok(url.into())
}

fn strict_sha1_proof(bytes: &[u8], source_label: &str) -> Result<[u8; 20], LoaderError> {
    let digest = match bytes {
        [digest @ .., b'\r', b'\n'] | [digest @ .., b'\n'] | digest if digest.len() == 40 => digest,
        _ => return Err(source_proof_error(source_label)),
    };
    hex::FromHex::from_hex(digest).map_err(|_| source_proof_error(source_label))
}

fn source_proof_error(source_label: &str) -> LoaderError {
    LoaderError::InvalidProfile(format!(
        "{source_label} sha1 proof must contain exactly one 40-hex digest"
    ))
}

#[cfg(test)]
mod tests {
    use super::{sha1_sidecar_url, strict_sha1_proof};

    #[test]
    fn sha1_sidecar_url_appends_to_the_parsed_path() {
        assert_eq!(
            sha1_sidecar_url("https://maven.example.test/path/artifact.jar").expect("sidecar URL"),
            "https://maven.example.test/path/artifact.jar.sha1"
        );
        assert_eq!(
            sha1_sidecar_url("https://maven.example.test/path/artifact%20name.jar")
                .expect("encoded sidecar URL"),
            "https://maven.example.test/path/artifact%20name.jar.sha1"
        );
    }

    #[test]
    fn sha1_sidecar_url_rejects_query_and_fragment_components() {
        for source_url in [
            "https://maven.example.test/artifact.jar?token=value",
            "https://maven.example.test/artifact.jar#digest",
        ] {
            assert!(sha1_sidecar_url(source_url).is_err());
        }
    }

    #[test]
    fn strict_sha1_proof_accepts_only_digest_with_optional_line_ending() {
        let digest = b"0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            strict_sha1_proof(digest, "test source").expect("bare digest"),
            <[u8; 20] as hex::FromHex>::from_hex(digest).unwrap()
        );
        let mut lf = digest.to_vec();
        lf.push(b'\n');
        assert!(strict_sha1_proof(&lf, "test source").is_ok());
        let mut crlf = digest.to_vec();
        crlf.extend_from_slice(b"\r\n");
        assert!(strict_sha1_proof(&crlf, "test source").is_ok());
    }

    #[test]
    fn strict_sha1_proof_rejects_tokens_whitespace_and_non_hex() {
        for proof in [
            b"0123456789abcdef0123456789abcdef01234567 artifact.jar".as_slice(),
            b" 0123456789abcdef0123456789abcdef01234567".as_slice(),
            b"g123456789abcdef0123456789abcdef01234567".as_slice(),
        ] {
            assert!(strict_sha1_proof(proof, "test source").is_err());
        }
    }
}

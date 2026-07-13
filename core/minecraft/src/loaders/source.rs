#[cfg(not(test))]
use super::http::fetch_bytes;
#[cfg(test)]
use super::http::fetch_bytes_for_test as fetch_bytes;
use super::types::LoaderError;
use sha1::Digest as _;

const MAX_SOURCE_SHA1_PROOF_BYTES: u64 = 128;

#[derive(Debug)]
pub(crate) struct VerifiedLoaderSource {
    bytes: Vec<u8>,
    provider_url: String,
    logical_identity: String,
    expected_sha1: String,
}

impl VerifiedLoaderSource {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn matches_contract(&self, provider_url: &str, logical_identity: &str) -> bool {
        self.provider_url == provider_url
            && self.logical_identity == logical_identity
            && self.expected_sha1.len() == 40
            && self
                .expected_sha1
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
    }

    pub(crate) fn matches_logical_identity(&self, logical_identity: &str) -> bool {
        self.logical_identity == logical_identity
            && self.expected_sha1.len() == 40
            && self
                .expected_sha1
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
    }

    pub(crate) fn into_bytes_for(
        self,
        provider_url: &str,
        logical_identity: &str,
    ) -> Result<Vec<u8>, LoaderError> {
        if self.provider_url != provider_url
            || self.logical_identity != logical_identity
            || self.expected_sha1.len() != 40
        {
            return Err(LoaderError::Verify(
                "authenticated loader source does not match its selected contract".to_string(),
            ));
        }
        Ok(self.bytes)
    }

    #[cfg(test)]
    pub(crate) fn from_test_bytes(bytes: Vec<u8>) -> Self {
        Self {
            expected_sha1: format!("{:x}", sha1::Sha1::digest(&bytes)),
            bytes,
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
    let bytes = fetch_bytes(source_url, max_source_bytes).await?;
    let proof_bytes = fetch_bytes(&proof_url, MAX_SOURCE_SHA1_PROOF_BYTES).await?;
    let proof = strict_sha1_proof(&proof_bytes, source_label)?;
    let actual = format!("{:x}", sha1::Sha1::digest(&bytes));
    if !proof.eq_ignore_ascii_case(&actual) {
        return Err(LoaderError::Verify(format!(
            "{source_label} does not match its live sha1 proof"
        )));
    }
    Ok(VerifiedLoaderSource {
        bytes,
        provider_url: source_url.to_string(),
        logical_identity: logical_identity.to_string(),
        expected_sha1: proof.to_ascii_lowercase(),
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

fn strict_sha1_proof<'a>(bytes: &'a [u8], source_label: &str) -> Result<&'a str, LoaderError> {
    let digest = match bytes {
        [digest @ .., b'\n'] if digest.len() == 40 => digest,
        [digest @ .., b'\r', b'\n'] if digest.len() == 40 => digest,
        digest if digest.len() == 40 => digest,
        _ => return Err(source_proof_error(source_label)),
    };
    if !digest.iter().all(u8::is_ascii_hexdigit) {
        return Err(source_proof_error(source_label));
    }
    std::str::from_utf8(digest).map_err(|_| source_proof_error(source_label))
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
            std::str::from_utf8(digest).expect("ASCII digest")
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

#[cfg(not(test))]
use super::http::fetch_bytes;
#[cfg(test)]
use super::http::fetch_bytes_for_test as fetch_bytes;
use super::types::LoaderError;
use sha1::Digest as _;

const MAX_SOURCE_SHA1_PROOF_BYTES: u64 = 128;

pub(crate) struct VerifiedLoaderSource {
    bytes: Vec<u8>,
}

impl VerifiedLoaderSource {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

pub(crate) async fn fetch_sha1_verified_source(
    source_url: &str,
    max_source_bytes: u64,
    source_label: &'static str,
) -> Result<VerifiedLoaderSource, LoaderError> {
    let bytes = fetch_bytes(source_url, max_source_bytes).await?;
    let proof_url = format!("{source_url}.sha1");
    let proof_bytes = fetch_bytes(&proof_url, MAX_SOURCE_SHA1_PROOF_BYTES).await?;
    let proof = strict_sha1_proof(&proof_bytes, source_label)?;
    let actual = format!("{:x}", sha1::Sha1::digest(&bytes));
    if !proof.eq_ignore_ascii_case(&actual) {
        return Err(LoaderError::Verify(format!(
            "{source_label} does not match its live sha1 proof"
        )));
    }
    Ok(VerifiedLoaderSource { bytes })
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
    use super::strict_sha1_proof;

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

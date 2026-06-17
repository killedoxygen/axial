use super::integrity::is_sha1_hex;
use super::model::{
    DownloadError, DownloadIntegrityError, ExecutionDownloadError, ExecutionDownloadFact,
    ExecutionDownloadFactKind, ExecutionDownloadOwnership, ExpectedIntegrity,
    SelectedDownloadArtifactDescriptor, SelectedDownloadArtifactKind,
};
use super::path_safety::{safe_download_fact_value, safe_download_target_label};
use std::io;
use std::path::Path;
use tokio::sync::mpsc;

const DEFAULT_SELECTED_ARTIFACT_MAX_BYTES: u64 = 512 << 20;

pub(super) struct ExecutionDownloadRequest<'a> {
    pub(super) url: &'a str,
    pub(super) destination: &'a Path,
    pub(super) expected: &'a ExpectedIntegrity,
    pub(super) ownership: ExecutionDownloadOwnership,
}

impl<'a> ExecutionDownloadRequest<'a> {
    pub(super) fn launcher_managed(
        url: &'a str,
        destination: &'a Path,
        expected: &'a ExpectedIntegrity,
    ) -> Self {
        Self {
            url,
            destination,
            expected,
            ownership: ExecutionDownloadOwnership::LauncherManaged,
        }
    }
}

pub(super) fn emit_selected_download_descriptor(
    descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
    kind: SelectedDownloadArtifactKind,
    destination: &Path,
    provider_url: &str,
    expected: &ExpectedIntegrity,
) {
    let Some(descriptor_tx) = descriptor_tx else {
        return;
    };
    let Some(sha1) = expected.sha1.as_deref() else {
        return;
    };
    if !is_sha1_hex(sha1) {
        return;
    }
    let descriptor = SelectedDownloadArtifactDescriptor::new(
        kind,
        safe_download_target_label(destination),
        destination.to_path_buf(),
        provider_url.to_string(),
        sha1.to_ascii_lowercase(),
        expected.size,
        expected
            .size
            .unwrap_or(DEFAULT_SELECTED_ARTIFACT_MAX_BYTES)
            .max(1),
    );
    let _ = descriptor_tx.send(descriptor);
}

pub(super) fn emit_execution_download_facts(
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    facts: &[ExecutionDownloadFact],
) {
    if let Some(fact_tx) = fact_tx {
        for fact in facts {
            let _ = fact_tx.send(fact.clone());
        }
    }
}

pub(super) fn execution_download_error(
    kind: ExecutionDownloadFactKind,
    facts: Vec<ExecutionDownloadFact>,
    error: DownloadError,
) -> ExecutionDownloadError {
    ExecutionDownloadError { kind, facts, error }
}

pub(super) fn no_download_fact_fields() -> Vec<(&'static str, &'static str)> {
    Vec::new()
}

pub(super) fn execution_download_fact<K, V, I>(
    kind: ExecutionDownloadFactKind,
    target: &str,
    fields: I,
) -> ExecutionDownloadFact
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    ExecutionDownloadFact {
        kind,
        target: safe_download_fact_value(target, "artifact"),
        fields: fields
            .into_iter()
            .map(|(key, value)| {
                (
                    safe_download_fact_value(key.as_ref(), "field"),
                    safe_download_fact_value(value.as_ref(), "value"),
                )
            })
            .collect(),
    }
}

pub(super) fn metadata_facts(
    expected: &ExpectedIntegrity,
    target: &str,
) -> Vec<ExecutionDownloadFact> {
    let mut facts = Vec::new();
    if expected.size.is_none() {
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::MetadataMissing,
            target,
            vec![("field", "size")],
        ));
    }
    if expected.sha1.is_none() {
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::MetadataMissing,
            target,
            vec![("field", "sha1")],
        ));
    }
    facts
}

pub(super) fn selected_artifact_missing_fact(
    destination: &Path,
    expected: &ExpectedIntegrity,
) -> Option<ExecutionDownloadFact> {
    let sha1 = expected.sha1.as_deref()?;
    if !is_sha1_hex(sha1) {
        return None;
    }
    Some(execution_download_fact(
        ExecutionDownloadFactKind::ArtifactMissing,
        &safe_download_target_label(destination),
        no_download_fact_fields(),
    ))
}

pub(super) fn size_mismatch_fact(
    target: &str,
    expected: u64,
    actual: u64,
) -> ExecutionDownloadFact {
    execution_download_fact(
        ExecutionDownloadFactKind::SizeMismatch,
        target,
        vec![
            ("expected_bytes", expected.to_string()),
            ("actual_bytes", actual.to_string()),
        ],
    )
}

pub(super) fn integrity_mismatch_fact(
    target: &str,
    error: &DownloadIntegrityError,
) -> ExecutionDownloadFact {
    match error {
        DownloadIntegrityError::SizeMismatch {
            expected, actual, ..
        } => size_mismatch_fact(target, *expected, *actual),
        DownloadIntegrityError::Sha1Mismatch { .. }
        | DownloadIntegrityError::MissingSha1 { .. } => execution_download_fact(
            ExecutionDownloadFactKind::ChecksumMismatch,
            target,
            vec![("algorithm", "sha1")],
        ),
    }
}

pub(super) fn io_execution_fact_kind(kind: io::ErrorKind) -> ExecutionDownloadFactKind {
    match kind {
        io::ErrorKind::PermissionDenied => ExecutionDownloadFactKind::PermissionFailure,
        _ => ExecutionDownloadFactKind::TempWriteFailed,
    }
}

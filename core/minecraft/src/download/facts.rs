use super::model::{
    DownloadError, DownloadIntegrityError, ExecutionDownloadError, ExecutionDownloadFact,
    ExecutionDownloadFactKind, ExpectedIntegrity, SelectedDownloadArtifactKind,
};
use super::path_safety::safe_download_fact_value;
use std::io;
use tokio::sync::mpsc;

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

pub(super) fn selected_download_source_label(
    kind: SelectedDownloadArtifactKind,
    identity: &str,
) -> String {
    let prefix = selected_download_target_prefix(kind);
    let suffix = safe_download_fact_value(identity, prefix);
    if suffix == prefix {
        format!("{prefix}_source")
    } else {
        format!("{prefix}_source_{suffix}")
    }
}

fn selected_download_target_prefix(kind: SelectedDownloadArtifactKind) -> &'static str {
    match kind {
        SelectedDownloadArtifactKind::VersionJson => "minecraft_version_json",
        SelectedDownloadArtifactKind::ClientJar => "minecraft_client",
        SelectedDownloadArtifactKind::Library => "minecraft_library",
        SelectedDownloadArtifactKind::AssetIndex => "minecraft_asset_index",
        SelectedDownloadArtifactKind::AssetObject => "minecraft_asset_object",
        SelectedDownloadArtifactKind::LogConfig => "minecraft_log_config",
    }
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
        DownloadIntegrityError::Sha1Mismatch { .. } => execution_download_fact(
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

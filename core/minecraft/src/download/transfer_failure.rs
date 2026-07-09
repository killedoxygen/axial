use super::facts::{
    execution_download_error, execution_download_fact, io_execution_fact_kind,
    no_download_fact_fields,
};
use super::model::{
    DownloadError, ExecutionDownloadError, ExecutionDownloadFact, ExecutionDownloadFactKind,
};
use super::transfer::discard_download_temp;
use std::io;
use std::path::Path;

pub(super) fn record_io_failure_fact_pair(
    facts: &mut Vec<ExecutionDownloadFact>,
    target: &str,
    error_kind: io::ErrorKind,
    failure_kind: ExecutionDownloadFactKind,
) -> ExecutionDownloadFactKind {
    let io_kind = io_execution_fact_kind(error_kind);
    facts.push(execution_download_fact(
        io_kind,
        target,
        no_download_fact_fields(),
    ));
    facts.push(execution_download_fact(
        failure_kind,
        target,
        no_download_fact_fields(),
    ));
    io_kind
}

pub(super) fn finish_execution_error(
    facts: &mut Vec<ExecutionDownloadFact>,
    kind: ExecutionDownloadFactKind,
    error: DownloadError,
) -> ExecutionDownloadError {
    execution_download_error(kind, std::mem::take(facts), error)
}

pub(super) async fn finish_execution_error_after_temp_discard(
    temp_path: &Path,
    target: &str,
    facts: &mut Vec<ExecutionDownloadFact>,
    kind: ExecutionDownloadFactKind,
    error: DownloadError,
) -> ExecutionDownloadError {
    discard_download_temp(temp_path, target, facts).await;
    finish_execution_error(facts, kind, error)
}

pub(super) async fn finish_io_failure_after_temp_discard(
    temp_path: &Path,
    target: &str,
    facts: &mut Vec<ExecutionDownloadFact>,
    failure_kind: ExecutionDownloadFactKind,
    report_kind: ExecutionDownloadFactKind,
    error: io::Error,
) -> ExecutionDownloadError {
    record_io_failure_fact_pair(facts, target, error.kind(), failure_kind);
    finish_execution_error_after_temp_discard(
        temp_path,
        target,
        facts,
        report_kind,
        DownloadError::FileOperation(error),
    )
    .await
}

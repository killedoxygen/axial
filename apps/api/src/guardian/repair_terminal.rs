use crate::state::failure_memory::GuardianFailureMemoryEntry;
use chrono::DateTime;
use std::future::Future;

pub(super) async fn complete_repair_terminal<Journal, Memory, Outcome, Value, Error>(
    journal: Journal,
    record_memory_best_effort: Memory,
    build_public_outcome: Outcome,
) -> Result<Value, Error>
where
    Journal: Future<Output = Result<Option<Error>, Error>>,
    Memory: FnOnce(),
    Outcome: FnOnce() -> Value,
{
    if let Some(error) = journal.await? {
        return Err(error);
    }
    record_memory_best_effort();
    Ok(build_public_outcome())
}

pub(super) fn active_repair_suppression_until(
    entry: &GuardianFailureMemoryEntry,
    observed_at: &str,
) -> Option<String> {
    let suppression_until = entry.suppression_until.as_ref()?;
    let suppression_timestamp = DateTime::parse_from_rfc3339(suppression_until).ok()?;
    let observed_timestamp = DateTime::parse_from_rfc3339(observed_at).ok()?;
    (suppression_timestamp > observed_timestamp).then(|| suppression_until.clone())
}

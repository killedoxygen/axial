use super::model::DownloadProgress;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Shared byte ledger for one install: pipelines register planned bytes as
/// their manifests resolve and record completed bytes as artifacts finish
/// (downloaded or verified in place). The installer stamps every outgoing
/// progress event with the current totals so progress reflects actual planned
/// work instead of a fixed phase table.
///
/// Phases whose planned bytes are not known upfront (asset objects until the
/// index is parsed, the managed runtime until its manifest is fetched) reserve
/// a contribution first; stamping stays disabled until every reservation
/// resolves, so a partially registered plan is never reported as near-complete.
#[derive(Debug, Default)]
pub(super) struct TransferPlan {
    done: AtomicU64,
    total: AtomicU64,
    pending_contributions: AtomicU64,
}

impl TransferPlan {
    pub(super) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(super) fn expect_contribution(&self) {
        self.pending_contributions.fetch_add(1, Ordering::Release);
    }

    pub(super) fn resolve_contribution(&self, bytes: u64) {
        self.contribute_total(bytes);
        self.pending_contributions.fetch_sub(1, Ordering::Release);
    }

    pub(super) fn contribute_total(&self, bytes: u64) {
        if bytes > 0 {
            self.total.fetch_add(bytes, Ordering::Release);
        }
    }

    pub(super) fn add_done(&self, bytes: u64) {
        if bytes > 0 {
            self.done.fetch_add(bytes, Ordering::Release);
        }
    }

    pub(super) fn stamp(&self, progress: &mut DownloadProgress) {
        if self.pending_contributions.load(Ordering::Acquire) != 0 {
            return;
        }
        let total = self.total.load(Ordering::Acquire);
        if total == 0 {
            return;
        }
        let done = self.done.load(Ordering::Acquire).min(total);
        progress.bytes_done = Some(done);
        progress.bytes_total = Some(total);
    }
}

#[cfg(test)]
mod tests {
    use super::TransferPlan;
    use crate::download::model::progress;

    fn stamped(plan: &TransferPlan) -> (Option<u64>, Option<u64>) {
        let mut event = progress("assets", 0, 0, None);
        plan.stamp(&mut event);
        (event.bytes_done, event.bytes_total)
    }

    #[test]
    fn stamp_reports_nothing_for_an_empty_plan() {
        let plan = TransferPlan::default();
        assert_eq!(stamped(&plan), (None, None));
    }

    #[test]
    fn stamp_stays_disabled_until_reserved_contributions_resolve() {
        let plan = TransferPlan::default();
        plan.contribute_total(100);
        plan.add_done(100);
        plan.expect_contribution();

        assert_eq!(stamped(&plan), (None, None));

        plan.resolve_contribution(300);
        assert_eq!(stamped(&plan), (Some(100), Some(400)));
    }

    #[test]
    fn stamp_supports_zero_byte_resolutions() {
        let plan = TransferPlan::default();
        plan.contribute_total(50);
        plan.expect_contribution();
        plan.resolve_contribution(0);
        plan.add_done(10);

        assert_eq!(stamped(&plan), (Some(10), Some(50)));
    }

    #[test]
    fn stamp_clamps_done_to_the_planned_total() {
        let plan = TransferPlan::default();
        plan.contribute_total(80);
        plan.add_done(200);

        assert_eq!(stamped(&plan), (Some(80), Some(80)));
    }
}

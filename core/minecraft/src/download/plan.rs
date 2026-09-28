use super::model::DownloadProgress;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Shared byte ledger for one install: the installer registers every known
/// byte count and reserves every unknown lane before its first progress event.
/// Pipelines resolve reservations as their manifests become available and
/// record completed bytes as artifacts finish (downloaded or verified in
/// place). The installer stamps outgoing progress only after the complete
/// denominator is known.
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
    invalid: AtomicBool,
}

#[derive(Debug)]
pub(super) struct TransferPlanContribution {
    plan: Arc<TransferPlan>,
    pending: bool,
}

impl TransferPlan {
    pub(super) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(super) fn reserve_contribution(self: &Arc<Self>) -> TransferPlanContribution {
        self.pending_contributions.fetch_add(1, Ordering::Release);
        TransferPlanContribution {
            plan: Arc::clone(self),
            pending: true,
        }
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
        progress.bytes_done = None;
        progress.bytes_total = None;
        if self.pending_contributions.load(Ordering::Acquire) != 0
            || self.invalid.load(Ordering::Acquire)
        {
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

impl TransferPlanContribution {
    pub(super) fn resolve(mut self, bytes: u64) {
        self.finish(Some(bytes));
    }

    fn finish(&mut self, bytes: Option<u64>) {
        if !self.pending {
            return;
        }
        if let Some(bytes) = bytes {
            self.plan.contribute_total(bytes);
        } else {
            self.plan.invalid.store(true, Ordering::Release);
        }
        let previous = self
            .plan
            .pending_contributions
            .fetch_sub(1, Ordering::AcqRel);
        assert!(previous > 0, "transfer-plan contribution underflow");
        self.pending = false;
    }
}

impl Drop for TransferPlanContribution {
    fn drop(&mut self) {
        self.finish(None);
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
        let plan = TransferPlan::shared();
        plan.contribute_total(100);
        plan.add_done(100);
        let contribution = plan.reserve_contribution();

        assert_eq!(stamped(plan.as_ref()), (None, None));

        contribution.resolve(300);
        assert_eq!(stamped(plan.as_ref()), (Some(100), Some(400)));
    }

    #[test]
    fn stamp_supports_zero_byte_resolutions() {
        let plan = TransferPlan::shared();
        plan.contribute_total(50);
        plan.reserve_contribution().resolve(0);
        plan.add_done(10);

        assert_eq!(stamped(plan.as_ref()), (Some(10), Some(50)));
    }

    #[test]
    fn stamp_waits_for_every_unknown_lane_before_exposing_the_complete_total() {
        let plan = TransferPlan::shared();
        plan.contribute_total(200);
        plan.add_done(20);
        let assets = plan.reserve_contribution();
        let runtime = plan.reserve_contribution();

        assets.resolve(300);
        assert_eq!(stamped(plan.as_ref()), (None, None));

        runtime.resolve(500);
        assert_eq!(stamped(plan.as_ref()), (Some(20), Some(1_000)));
    }

    #[test]
    fn abandoned_contribution_invalidates_progress_stamping() {
        let plan = TransferPlan::shared();
        plan.contribute_total(50);
        drop(plan.reserve_contribution());

        assert_eq!(stamped(plan.as_ref()), (None, None));
    }

    #[test]
    fn concurrent_abandonment_never_exposes_a_partial_denominator() {
        let plan = TransferPlan::shared();
        plan.contribute_total(100);
        plan.add_done(100);
        let resolved = plan.reserve_contribution();
        let abandoned = plan.reserve_contribution();
        let worker = std::thread::spawn(move || resolved.resolve(300));

        drop(abandoned);
        worker.join().expect("resolved transfer contribution");

        assert_eq!(stamped(plan.as_ref()), (None, None));
    }

    #[test]
    fn concurrent_contributions_publish_one_complete_denominator() {
        let plan = TransferPlan::shared();
        let workers = (1_u64..=8)
            .map(|bytes| {
                let contribution = plan.reserve_contribution();
                std::thread::spawn(move || contribution.resolve(bytes * 10))
            })
            .collect::<Vec<_>>();

        for worker in workers {
            worker.join().expect("transfer contribution worker");
        }

        assert_eq!(stamped(plan.as_ref()), (Some(0), Some(360)));
    }

    #[test]
    fn stamp_clamps_done_to_the_planned_total() {
        let plan = TransferPlan::default();
        plan.contribute_total(80);
        plan.add_done(200);

        assert_eq!(stamped(&plan), (Some(80), Some(80)));
    }
}

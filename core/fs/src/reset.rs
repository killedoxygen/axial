//! Clears an admitted root after every owner of its contents has drained.

use super::*;

/// Retains the root's reset authority across retries. The lease and `keep`
/// survive; everything else in the root is deleted.
#[must_use = "retain the reset until try_clear succeeds"]
pub struct PendingRootReset {
    keep: LeafName,
    session: Option<RootSession>,
    kept: Option<FileCapability>,
    outcome: Option<ResetStartOutcome>,
}

impl PendingRootReset {
    pub fn new(session: RootSession, keep: LeafName) -> Self {
        Self {
            keep,
            session: Some(session),
            kept: None,
            outcome: None,
        }
    }

    pub fn try_clear(&mut self) -> io::Result<()> {
        if let Some(session) = self.session.take() {
            match session.root().and_then(|root| root.open_file(&self.keep)) {
                Ok(kept) => {
                    self.kept = Some(kept);
                    self.outcome = Some(session.begin_reset());
                }
                Err(error) => {
                    self.session = Some(session);
                    return Err(error);
                }
            }
        }
        for _ in 0..8 {
            let Some(outcome) = self.outcome.take() else {
                return Ok(());
            };
            self.outcome = match outcome {
                ResetStartOutcome::Ready(authority) => {
                    let kept = self.kept.as_ref().expect("reset retains its kept file");
                    if let Err(error) =
                        authority.try_clear_root(&[(kept.name.as_os_str(), kept.identity)])
                    {
                        self.outcome = Some(ResetStartOutcome::Ready(authority));
                        return Err(error);
                    }
                    authority.release().err().map(ResetStartOutcome::Ready)
                }
                ResetStartOutcome::Pending(drain) => Some(drain.try_settle()),
                ResetStartOutcome::Refused(failure) => Some(failure.retry()),
                ResetStartOutcome::Failed(failure) => Some(failure.retry()),
                ResetStartOutcome::Recovery { recovery } => Some(recovery.settle()),
            };
        }
        match self.outcome {
            None => Ok(()),
            Some(_) => Err(io::Error::other("root reset remains unsettled")),
        }
    }
}

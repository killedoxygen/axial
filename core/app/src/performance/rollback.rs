//! Exact snapshot selection and restart classification for Performance.
//!
//! The retained Performance leaf owns the actual per-file manifests, artifact
//! copies and publication receipts. A status row is never a substitute for those
//! receipts and never grants cleanup authority.

use axial_performance::{
    CompositionState, ManagedCompositionInstallPlan, RollbackSnapshotSummary,
    RollbackSnapshotTarget,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ExpectedComposition {
    /// Only settle the leaf's own exact publication receipts, then report health.
    Inspection,
    Absent,
    Graph {
        composition_id: String,
        game_version: String,
        loader: String,
        graph_sha512: String,
        /// Exact provider pins are retained in addition to the aggregate graph
        /// digest, so an unresolved record remains independently inspectable.
        artifacts: Vec<ExpectedArtifact>,
    },
    Snapshot {
        snapshot_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prepared: Option<PreparedSnapshot>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedSnapshot {
    target: RollbackSnapshotTarget,
    composition_id: Option<String>,
    artifact_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpectedArtifact {
    project_id: String,
    version_id: String,
    filename: String,
    size: u64,
    sha512: String,
}

impl ExpectedComposition {
    pub(crate) fn from_snapshot(snapshot: &RollbackSnapshotSummary) -> Self {
        Self::Snapshot {
            snapshot_id: snapshot.id.clone(),
            prepared: Some(PreparedSnapshot {
                target: snapshot.target,
                composition_id: snapshot.composition_id.clone(),
                artifact_count: snapshot.artifact_count,
            }),
        }
    }

    pub(crate) fn from_plan(plan: &ManagedCompositionInstallPlan) -> Self {
        Self::Graph {
            composition_id: plan.composition_id().to_owned(),
            game_version: plan.game_version().to_owned(),
            loader: plan.loader().to_owned(),
            graph_sha512: plan.graph_digest().to_owned(),
            artifacts: plan
                .pins()
                .iter()
                .map(|pin| ExpectedArtifact {
                    project_id: pin.project_id().to_owned(),
                    version_id: pin.version_id().to_owned(),
                    filename: pin.filename().to_owned(),
                    size: pin.size(),
                    sha512: pin.sha512().to_owned(),
                })
                .collect(),
        }
    }

    /// Call only with a state returned by successful leaf recovery: matching
    /// metadata alone does not prove any artifact is present or owned.
    pub(crate) fn matches_verified(&self, state: Option<&CompositionState>) -> bool {
        match (self, state) {
            (Self::Absent, None) => true,
            (
                Self::Graph {
                    composition_id,
                    game_version,
                    loader,
                    graph_sha512,
                    artifacts,
                },
                Some(state),
            ) => {
                state.composition_id == *composition_id
                    && state.game_version == *game_version
                    && state.loader == *loader
                    && state.graph_sha512 == *graph_sha512
                    && state.installed_mods.len() == artifacts.len()
                    && artifacts.iter().all(|expected| {
                        state.installed_mods.iter().any(|installed| {
                            installed.project_id == expected.project_id
                                && installed.version_id == expected.version_id
                                && installed.filename == expected.filename
                                && installed.size == expected.size
                                && installed
                                    .integrity
                                    .sha512
                                    .eq_ignore_ascii_case(&expected.sha512)
                                && installed.ownership_class
                                    == axial_performance::OwnershipClass::CompositionManaged
                        })
                    })
            }
            // A snapshot ID alone cannot prove its restored state. The leaf
            // restore outcome or a previously persisted exact result is needed.
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerformanceRollbackListResponse {
    pub snapshots: Vec<RollbackSnapshotSummary>,
}

pub(crate) fn select_snapshot<'a>(
    snapshots: &'a [RollbackSnapshotSummary],
    requested: Option<&str>,
) -> Result<&'a RollbackSnapshotSummary, super::mutation::PerformanceMutationError> {
    let snapshot = match requested {
        Some(id) if !id.is_empty() => snapshots.iter().find(|snapshot| snapshot.id == id),
        Some(_) => None,
        None => {
            let mut latest = snapshots.iter().filter(|snapshot| snapshot.latest);
            let first = latest.next();
            if latest.next().is_some() {
                return Err(super::mutation::PerformanceMutationError::Unsettled);
            }
            first
        }
    }
    .ok_or(super::mutation::PerformanceMutationError::SnapshotNotFound)?;
    if !snapshot.rollback_available {
        return Err(super::mutation::PerformanceMutationError::SnapshotUnavailable);
    }
    Ok(snapshot)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestartDisposition {
    Applied,
    RestoredBefore,
    Preserve,
}

/// Recovery is evidence driven. In particular, "the target is absent" cannot
/// imply an interrupted install succeeded, and a different user composition
/// cannot be discarded to make an old request look successful.
pub(crate) fn classify_recovered(
    before: Option<&CompositionState>,
    expected: &ExpectedComposition,
    result: Option<&Option<CompositionState>>,
    current: Option<&CompositionState>,
) -> RestartDisposition {
    if result.is_some_and(|result| result.as_ref() == current) || expected.matches_verified(current)
    {
        RestartDisposition::Applied
    } else if before == current {
        RestartDisposition::RestoredBefore
    } else {
        RestartDisposition::Preserve
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_identifier_is_never_treated_as_restoration_evidence() {
        let expected = ExpectedComposition::Snapshot {
            snapshot_id: "snapshot-1".into(),
            prepared: None,
        };
        assert!(!expected.matches_verified(None));
        assert_eq!(
            classify_recovered(None, &expected, None, None),
            RestartDisposition::RestoredBefore
        );
        assert_eq!(
            classify_recovered(None, &expected, Some(&None), None),
            RestartDisposition::Applied
        );
    }

    #[test]
    fn duplicate_latest_snapshots_do_not_choose_arbitrarily() {
        let summary = |id: &str| RollbackSnapshotSummary {
            id: id.into(),
            created_at: "2026-09-08T00:00:00Z".into(),
            target: axial_performance::RollbackSnapshotTarget::ManagedStateAbsent,
            composition_id: None,
            tier: None,
            installed_count: 0,
            artifact_count: 0,
            ownership_class: axial_performance::OwnershipClass::CompositionManaged,
            rollback_available: true,
            latest: true,
        };
        let snapshots = vec![summary("one"), summary("two")];
        assert!(select_snapshot(&snapshots, None).is_err());
        assert_eq!(select_snapshot(&snapshots, Some("two")).unwrap().id, "two");
    }
}

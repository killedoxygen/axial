use crate::guardian::{
    GuardianWholeInstanceRematerializationOffer, GuardianWholeInstanceRematerializationOutcome,
    execute_whole_instance_rematerialization,
};
use crate::state::contracts::OperationId;
use crate::state::{
    AppState, RegisteredWholeInstanceRematerializationAdmission, RequestProducerHandoff,
};
use std::future::Future;

const WHOLE_INSTANCE_REMATERIALIZATION_SUPPRESSION_MINUTES: i64 = 15;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("explicit whole-instance rematerialization failed: {class}")]
pub(crate) struct ExplicitWholeInstanceRematerializationError {
    class: &'static str,
}

#[expect(
    dead_code,
    reason = "Phase 4 backend contract; Phase 6 transport deferred"
)]
pub(crate) async fn execute_explicit_whole_instance_rematerialization(
    state: AppState,
    handoff: RequestProducerHandoff,
    offer: GuardianWholeInstanceRematerializationOffer,
) -> Result<
    GuardianWholeInstanceRematerializationOutcome,
    ExplicitWholeInstanceRematerializationError,
> {
    let result_rx = spawn_explicit_whole_instance_rematerialization(
        state,
        handoff,
        offer,
        OperationId::new(format!(
            "guardian-whole-instance-rematerialization:{}",
            uuid::Uuid::new_v4()
        )),
        chrono::Duration::minutes(WHOLE_INSTANCE_REMATERIALIZATION_SUPPRESSION_MINUTES),
    )?;
    result_rx
        .await
        .map_err(|_| application_error("producer_stopped"))?
}

fn spawn_explicit_whole_instance_rematerialization(
    state: AppState,
    handoff: RequestProducerHandoff,
    offer: GuardianWholeInstanceRematerializationOffer,
    operation_id: OperationId,
    suppression_for: chrono::Duration,
) -> Result<
    tokio::sync::oneshot::Receiver<
        Result<
            GuardianWholeInstanceRematerializationOutcome,
            ExplicitWholeInstanceRematerializationError,
        >,
    >,
    ExplicitWholeInstanceRematerializationError,
> {
    spawn_explicit_whole_instance_rematerialization_with(
        state,
        handoff,
        offer,
        operation_id,
        suppression_for,
        execute_whole_instance_rematerialization,
    )
}

pub(crate) fn spawn_explicit_whole_instance_rematerialization_with<Executor, ExecutorFuture>(
    state: AppState,
    handoff: RequestProducerHandoff,
    offer: GuardianWholeInstanceRematerializationOffer,
    operation_id: OperationId,
    suppression_for: chrono::Duration,
    executor: Executor,
) -> Result<
    tokio::sync::oneshot::Receiver<
        Result<
            GuardianWholeInstanceRematerializationOutcome,
            ExplicitWholeInstanceRematerializationError,
        >,
    >,
    ExplicitWholeInstanceRematerializationError,
>
where
    Executor: FnOnce(RegisteredWholeInstanceRematerializationAdmission) -> ExecutorFuture
        + Send
        + 'static,
    ExecutorFuture: Future<
            Output = Result<
                GuardianWholeInstanceRematerializationOutcome,
                crate::guardian::GuardianWholeInstanceRematerializationError,
            >,
        > + Send
        + 'static,
{
    let producer = state
        .try_claim_request_producer(&handoff)
        .map_err(|_| application_error("shutdown"))?;
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    producer.spawn(async move {
        let result = async {
            let authorization = offer.into_authorization();
            let admission = state
                .admit_whole_instance_rematerialization(
                    authorization,
                    operation_id,
                    suppression_for,
                )
                .await
                .map_err(|error| application_error(error.class()))?;
            executor(admission)
                .await
                .map_err(|error| application_error(error.class()))
        }
        .await;
        let _ = result_tx.send(result);
    });
    Ok(result_rx)
}

fn application_error(class: &'static str) -> ExplicitWholeInstanceRematerializationError {
    ExplicitWholeInstanceRematerializationError { class }
}

#[cfg(test)]
mod tests {
    #[test]
    fn explicit_owner_has_no_discovery_route_launch_retry_or_scheduler_caller() {
        let owner = include_str!("whole_instance_rematerialization.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source precedes tests");
        let routes = [
            include_str!("../routes/mod.rs"),
            include_str!("../routes/instances.rs"),
            include_str!("../routes/launch/mod.rs"),
        ]
        .join("\n");
        let launch = include_str!("launch.rs");
        let scheduler = include_str!("integrity_scheduler.rs");

        assert!(!owner.contains("whole_instance_rematerialization_eligibility"));
        assert!(!owner.contains("assess_whole_instance_rematerialization"));
        assert!(!owner.contains("prepare_launch_preflight"));
        assert!(!owner.contains("retry_launch"));
        assert!(!routes.contains("execute_explicit_whole_instance_rematerialization"));
        assert!(!launch.contains("execute_explicit_whole_instance_rematerialization"));
        assert!(!scheduler.contains("execute_explicit_whole_instance_rematerialization"));
    }
}

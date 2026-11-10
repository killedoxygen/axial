use super::error::{ApiResult, Extracted};
use axial_app::instances::{
    create::{
        CreateInstanceRequest, InstanceService, PendingInstancesResponse, RecoveredCreationResponse,
    },
    delete::{DeleteIntent, DeletionResponse, DeletionSnapshot},
    duplicate::DuplicateRequest,
    from_pack::CreateFromModpackRequest,
    model::{EnrichedInstance, InstanceError, InstanceId, InstancePatch},
    setup::{
        CreateInstanceResponse, CreateLoaderBuildsView, CreateView, InstanceSetupExecuteRequest,
        InstanceSetupPlanRequest, InstanceSetupPlanResponse, ListResponse, SetupService,
    },
};
use axial_app::launch::session::SessionManager;
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    routing::{get, post},
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone)]
struct Services {
    instances: Arc<InstanceService>,
    setup: Arc<SetupService>,
    sessions: SessionManager,
}

/// Authentication and origin admission are attached once by API composition.
pub fn router(
    instances: Arc<InstanceService>,
    setup: Arc<SetupService>,
    sessions: SessionManager,
) -> Router<()> {
    Router::new()
        .route("/api/v1/instances", get(list).post(create))
        .route("/api/v1/instances/modpack", post(create_modpack))
        .route("/api/v1/instances/create-view", get(create_view))
        .route(
            "/api/v1/instances/create-view/loader-builds",
            get(loader_builds),
        )
        .route("/api/v1/instances/pending", get(pending))
        .route("/api/v1/instances/setup/plan", post(plan_setup))
        .route("/api/v1/instances/setup", post(execute_setup))
        .route("/api/v1/instances/{id}/setup/resume", post(resume_setup))
        .route(
            "/api/v1/instances/pending/{id}/resume",
            post(resume_creation),
        )
        .route(
            "/api/v1/instances/deletions/{operation_id}",
            get(deletion_status),
        )
        .route(
            "/api/v1/instances/{id}",
            get(detail).put(update).delete(delete),
        )
        .route("/api/v1/instances/{id}/duplicate", post(duplicate))
        .layer(DefaultBodyLimit::max(32 * 1024))
        .with_state(Services {
            instances,
            setup: setup.clone(),
            sessions,
        })
        .merge(
            Router::new()
                .route("/api/v1/setup/init", post(initialize))
                .with_state(setup),
        )
}

fn identity(value: &str) -> Result<InstanceId, InstanceError> {
    value.parse()
}

async fn initialize(
    State(setup): State<Arc<SetupService>>,
) -> ApiResult<axial_app::instances::setup::SetupStatusResponse> {
    Ok(Json(setup.initialize().await?))
}

async fn list(State(services): State<Services>) -> ApiResult<ListResponse> {
    Ok(Json(services.setup.list().await?))
}

async fn detail(
    State(services): State<Services>,
    Path(id): Path<String>,
) -> ApiResult<EnrichedInstance> {
    let record = services.instances.registry().get_live(&identity(&id)?)?;
    let instances = services.setup.enrich_all(vec![record.instance]).await?;
    let [instance] =
        <[_; 1]>::try_from(instances).map_err(|_| InstanceError::VersionUnavailable)?;
    Ok(Json(instance))
}

async fn create(
    State(services): State<Services>,
    body: Result<Json<CreateInstanceRequest>, JsonRejection>,
) -> ApiResult<CreateInstanceResponse> {
    let request = body.or_refuse(|| InstanceError::InvalidInput)?;
    Ok(Json(services.setup.create(request).await?))
}

async fn create_modpack(
    State(services): State<Services>,
    body: Result<Json<CreateFromModpackRequest>, JsonRejection>,
) -> ApiResult<CreateInstanceResponse> {
    let request = body.or_refuse(|| InstanceError::InvalidInput)?;
    Ok(Json(services.setup.create_from_modpack(request).await?))
}

async fn update(
    State(services): State<Services>,
    Path(id): Path<String>,
    body: Result<Json<InstancePatch>, JsonRejection>,
) -> ApiResult<EnrichedInstance> {
    let patch = body.or_refuse(|| InstanceError::InvalidInput)?;
    let instance = services
        .instances
        .update_record_with_sessions(&identity(&id)?, patch, &services.sessions)?
        .instance;
    Ok(Json(services.setup.enrich_committed(instance).await))
}

async fn duplicate(
    State(services): State<Services>,
    Path(id): Path<String>,
    body: Result<Option<Json<DuplicateRequest>>, JsonRejection>,
) -> ApiResult<EnrichedInstance> {
    let request = body
        .map_err(|_| InstanceError::InvalidInput)?
        .map(|Json(request)| request)
        .unwrap_or_default();
    let work = services.instances.duplicate(&identity(&id)?, request)?;
    let instance = work
        .join()
        .await
        .map_err(|_| InstanceError::SettlementRequired)??;
    Ok(Json(services.setup.enrich_committed(instance).await))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteQuery {
    #[serde(default)]
    keep_files: bool,
    operation_id: Option<uuid::Uuid>,
}

async fn delete(
    State(services): State<Services>,
    Path(id): Path<String>,
    query: Result<Query<DeleteQuery>, QueryRejection>,
) -> ApiResult<DeletionResponse> {
    let query = query.or_refuse(|| InstanceError::InvalidInput)?;
    let intent = if query.keep_files {
        DeleteIntent::KeepFiles
    } else {
        DeleteIntent::DeleteFiles
    };
    let work = services.instances.delete(
        &identity(&id)?,
        intent,
        query.operation_id.unwrap_or_else(uuid::Uuid::new_v4),
    )?;
    let snapshot = work
        .join()
        .await
        .map_err(|_| InstanceError::SettlementRequired)??;
    Ok(Json(snapshot.into()))
}

async fn deletion_status(
    State(services): State<Services>,
    Path(operation_id): Path<String>,
) -> ApiResult<DeletionSnapshot> {
    let operation_id =
        uuid::Uuid::parse_str(&operation_id).map_err(|_| InstanceError::InvalidInput)?;
    Ok(Json(services.instances.deletion_status(operation_id)?))
}

async fn pending(State(services): State<Services>) -> ApiResult<PendingInstancesResponse> {
    Ok(Json(PendingInstancesResponse {
        creations: services.instances.pending()?,
        deletions: services.instances.pending_deletions()?,
    }))
}

async fn plan_setup(
    State(services): State<Services>,
    body: Result<Json<InstanceSetupPlanRequest>, JsonRejection>,
) -> ApiResult<InstanceSetupPlanResponse> {
    let request = body.or_refuse(|| InstanceError::InvalidInput)?;
    Ok(Json(services.setup.plan_setup(request).await?))
}

async fn execute_setup(
    State(services): State<Services>,
    body: Result<Json<InstanceSetupExecuteRequest>, JsonRejection>,
) -> ApiResult<CreateInstanceResponse> {
    let request = body.or_refuse(|| InstanceError::InvalidInput)?;
    Ok(Json(services.setup.execute_setup(request).await?))
}

async fn resume_setup(
    State(services): State<Services>,
    Path(id): Path<String>,
) -> ApiResult<CreateInstanceResponse> {
    Ok(Json(services.setup.resume_setup(&identity(&id)?).await?))
}

async fn resume_creation(
    State(services): State<Services>,
    Path(id): Path<String>,
) -> ApiResult<RecoveredCreationResponse> {
    let work = services.instances.recover_creation(&identity(&id)?)?;
    let instance = work
        .join()
        .await
        .map_err(|_| InstanceError::SettlementRequired)??;
    Ok(Json(RecoveredCreationResponse { instance }))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewQuery {
    source: Option<String>,
}
async fn create_view(
    State(services): State<Services>,
    query: Result<Query<ViewQuery>, QueryRejection>,
) -> ApiResult<CreateView> {
    let query = query.or_refuse(|| InstanceError::InvalidInput)?;
    Ok(Json(
        services.setup.create_view(query.source.as_deref()).await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildsQuery {
    source: String,
    minecraft_version: String,
}
async fn loader_builds(
    State(services): State<Services>,
    query: Result<Query<BuildsQuery>, QueryRejection>,
) -> ApiResult<CreateLoaderBuildsView> {
    let query = query.or_refuse(|| InstanceError::InvalidInput)?;
    Ok(Json(
        services
            .setup
            .loader_builds(&query.source, &query.minecraft_version)
            .await?,
    ))
}

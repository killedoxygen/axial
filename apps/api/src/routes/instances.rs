use axial_app::instances::{
    create::{CreateInstanceRequest, InstanceService},
    delete::{DeleteIntent, DeletionError},
    duplicate::DuplicateRequest,
    from_pack::CreateFromModpackRequest,
    model::{InstanceError, InstanceId, InstancePatch},
    setup::{InstanceSetupExecuteRequest, InstanceSetupPlanRequest, SetupService},
};
use axial_app::launch::session::SessionManager;
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
struct Services {
    instances: Arc<InstanceService>,
    setup: Arc<SetupService>,
    sessions: SessionManager,
}
type ApiError = (StatusCode, Json<Value>);

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
            setup,
            sessions,
        })
}

async fn list(State(services): State<Services>) -> Result<Json<Value>, ApiError> {
    let records = services.instances.registry().list().map_err(error)?;
    let versions = services.setup.installed().await.map_err(error)?;
    let instances = services
        .setup
        .enrich_all(
            records.into_iter().map(|record| record.instance).collect(),
            &versions,
        )
        .await;
    let last_instance_id = services
        .instances
        .registry()
        .last_instance_id()
        .map_err(error)?;
    Ok(Json(
        json!({"instances": instances, "last_instance_id": last_instance_id}),
    ))
}

async fn detail(
    State(services): State<Services>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let record = services
        .instances
        .registry()
        .get_live(&identity(&id)?)
        .map_err(error)?;
    let versions = services.setup.installed().await.map_err(error)?;
    Ok(Json(json!(
        services.setup.enrich(record.instance, &versions).await
    )))
}

async fn create(
    State(services): State<Services>,
    body: Result<Json<CreateInstanceRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(request) = body.map_err(|_| invalid())?;
    services
        .setup
        .create(request)
        .await
        .map(|response| Json(json!(response)))
        .map_err(error)
}

async fn create_modpack(
    State(services): State<Services>,
    body: Result<Json<CreateFromModpackRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(request) = body.map_err(|_| invalid())?;
    services
        .setup
        .create_from_modpack(request)
        .await
        .map(|response| Json(json!(response)))
        .map_err(error)
}

async fn update(
    State(services): State<Services>,
    Path(id): Path<String>,
    body: Result<Json<InstancePatch>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(patch) = body.map_err(|_| invalid())?;
    let instance = services
        .instances
        .update_with_sessions(&identity(&id)?, patch, &services.sessions)
        .map_err(error)?;
    let versions = services.setup.installed().await.map_err(error)?;
    Ok(Json(json!(
        services.setup.enrich(instance, &versions).await
    )))
}

async fn duplicate(
    State(services): State<Services>,
    Path(id): Path<String>,
    body: Result<Option<Json<DuplicateRequest>>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let request = body
        .map_err(|_| invalid())?
        .map(|Json(request)| request)
        .unwrap_or_default();
    let work = services
        .instances
        .duplicate(&identity(&id)?, request)
        .map_err(error)?;
    let instance = work
        .join()
        .await
        .map_err(|_| error(InstanceError::SettlementRequired))?
        .map_err(error)?;
    let versions = services.setup.installed().await.map_err(error)?;
    Ok(Json(json!(
        services.setup.enrich(instance, &versions).await
    )))
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
) -> Result<Json<Value>, ApiError> {
    let Query(query) = query.map_err(|_| invalid())?;
    let intent = if query.keep_files {
        DeleteIntent::KeepFiles
    } else {
        DeleteIntent::DeleteFiles
    };
    let work = services
        .instances
        .delete(
            &identity(&id)?,
            intent,
            query.operation_id.unwrap_or_else(uuid::Uuid::new_v4),
        )
        .map_err(deletion_error)?;
    let snapshot = work
        .join()
        .await
        .map_err(|_| error(InstanceError::SettlementRequired))?
        .map_err(deletion_error)?;
    Ok(Json(
        json!({ "status": snapshot.status, "deletion": snapshot }),
    ))
}

async fn deletion_status(
    State(services): State<Services>,
    Path(operation_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let operation_id = uuid::Uuid::parse_str(&operation_id).map_err(|_| invalid())?;
    services
        .instances
        .deletion_status(operation_id)
        .map(|snapshot| Json(json!(snapshot)))
        .map_err(deletion_error)
}

async fn pending(State(services): State<Services>) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        json!({ "creations": services.instances.pending().map_err(error)?,
        "deletions": services.instances.pending_deletions().map_err(deletion_error)? }),
    ))
}

async fn plan_setup(
    State(services): State<Services>,
    body: Result<Json<InstanceSetupPlanRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(request) = body.map_err(|_| invalid())?;
    services
        .setup
        .plan_setup(request)
        .await
        .map(|plan| Json(json!(plan)))
        .map_err(error)
}

async fn execute_setup(
    State(services): State<Services>,
    body: Result<Json<InstanceSetupExecuteRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(request) = body.map_err(|_| invalid())?;
    services
        .setup
        .execute_setup(request)
        .await
        .map(|result| Json(json!(result)))
        .map_err(error)
}

async fn resume_setup(
    State(services): State<Services>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    services
        .setup
        .resume_setup(&identity(&id)?)
        .await
        .map(|result| Json(json!(result)))
        .map_err(error)
}

async fn resume_creation(
    State(services): State<Services>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let instance = services
        .instances
        .recover_creation(&identity(&id)?)
        .map_err(error)?
        .join()
        .await
        .map_err(|_| error(InstanceError::SettlementRequired))?
        .map_err(error)?;
    Ok(Json(json!({"instance": instance})))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewQuery {
    source: Option<String>,
}
async fn create_view(
    State(services): State<Services>,
    query: Result<Query<ViewQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(query) = query.map_err(|_| invalid())?;
    services
        .setup
        .create_view(query.source.as_deref())
        .await
        .map(|view| Json(json!(view)))
        .map_err(error)
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
) -> Result<Json<Value>, ApiError> {
    let Query(query) = query.map_err(|_| invalid())?;
    services
        .setup
        .loader_builds(&query.source, &query.minecraft_version)
        .await
        .map(|view| Json(json!(view)))
        .map_err(error)
}

fn identity(value: &str) -> Result<InstanceId, ApiError> {
    value.parse().map_err(error)
}
fn invalid() -> ApiError {
    error(InstanceError::InvalidInput)
}

pub(super) fn error(failure: InstanceError) -> ApiError {
    let status = match failure {
        InstanceError::InvalidId
        | InstanceError::InvalidName
        | InstanceError::InvalidInput
        | InstanceError::InvalidSettings => StatusCode::BAD_REQUEST,
        InstanceError::NotFound => StatusCode::NOT_FOUND,
        InstanceError::Conflict
        | InstanceError::NameConflict
        | InstanceError::Busy
        | InstanceError::Cancelled
        | InstanceError::ManagedDuplicateUnavailable => StatusCode::CONFLICT,
        InstanceError::Closed
        | InstanceError::SetupUnavailable
        | InstanceError::LibraryUnavailable
        | InstanceError::VersionUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        InstanceError::DirectoryUnavailable
        | InstanceError::SettlementRequired
        | InstanceError::Storage(_) => StatusCode::CONFLICT,
    };
    (status, Json(json!({"error": failure.to_string()})))
}

fn deletion_error(failure: DeletionError) -> ApiError {
    match failure {
        DeletionError::Instance(failure) => error(failure),
        other => (
            if matches!(other, DeletionError::NotFound) {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::CONFLICT
            },
            Json(json!({"error": other.to_string()})),
        ),
    }
}

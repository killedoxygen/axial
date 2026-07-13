use super::create::handle_create_instance;
use super::create::{CreateSelection, resolve_create_selection};
use super::{CreateInstanceRequest, CreateInstanceResponse};
use crate::application::content::{
    ContentInstallRequest, PlanReason, TargetRef, queue_content_install_with_cleanup_after,
    queue_modpack_install_after,
};
use crate::application::{ContentPlanRequest, ResolutionPlan, content_plan};
use crate::application::{ModpackInstallRequest, modpack_target};
use crate::state::AppState;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type ApiError = (StatusCode, Json<serde_json::Value>);

const SETUP_PLAN_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_SETUP_PLANS: usize = 64;
const MAX_SETUP_SELECTIONS: usize = 40;
static SETUP_PLAN_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static SETUP_PLANS: OnceLock<Mutex<HashMap<String, StoredSetupPlan>>> = OnceLock::new();

#[derive(Debug, Clone, Deserialize)]
pub struct InstanceSetupPlanRequest {
    pub selection_id: String,
    pub target: TargetRef,
    pub selections: Vec<crate::application::content::ContentSelection>,
}

#[derive(Debug, Serialize)]
pub struct InstanceSetupPlanResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    pub expires_at_ms: u64,
    pub selection_id: String,
    pub plan: ResolutionPlan,
}

#[derive(Debug, Deserialize)]
pub struct InstanceSetupExecuteRequest {
    pub plan_id: String,
    #[serde(flatten)]
    pub create: CreateInstanceRequest,
}

#[derive(Debug, Deserialize)]
pub struct ModpackInstanceSetupRequest {
    pub canonical_id: String,
    pub version_id: String,
    #[serde(flatten)]
    pub create: CreateInstanceRequest,
}

#[derive(Clone)]
struct StoredSetupPlan {
    selection_id: String,
    request: ContentPlanRequest,
    fingerprint: u64,
    expires_at: Instant,
}

fn setup_plans() -> &'static Mutex<HashMap<String, StoredSetupPlan>> {
    SETUP_PLANS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub async fn plan_instance_setup(
    state: &AppState,
    request: InstanceSetupPlanRequest,
) -> Result<InstanceSetupPlanResponse, ApiError> {
    let requested_selection_id = request.selection_id.trim().to_string();
    if requested_selection_id.is_empty() {
        return Err(bad_request("selection_id is required"));
    }
    if request.selections.is_empty() || request.selections.len() > MAX_SETUP_SELECTIONS {
        return Err(bad_request(
            "setup selections must contain between 1 and 40 items",
        ));
    }
    let selection = resolve_create_selection(
        state,
        &CreateInstanceRequest {
            selection_id: requested_selection_id,
            ..CreateInstanceRequest::default()
        },
    )
    .await?;
    if !target_matches_selection(&request.target, &selection) {
        return Err(conflict(
            "Setup target does not match the selected Minecraft version and loader.",
        ));
    }
    let selection_id = selection.exact_selection_id();
    let mut content_request = ContentPlanRequest {
        target: request.target,
        selections: request.selections,
    };
    let plan = content_plan(state, content_request.clone()).await?;
    pin_selected_versions(&mut content_request, &plan);
    let expires_at_ms = now_ms().saturating_add(SETUP_PLAN_TTL.as_millis() as u64);
    let plan_id = if plan.conflicts.is_empty() {
        let fingerprint = plan_fingerprint(&selection_id, &content_request, &plan);
        let plan_id = new_plan_id(fingerprint);
        let stored = StoredSetupPlan {
            selection_id: selection_id.clone(),
            request: content_request,
            fingerprint,
            expires_at: Instant::now() + SETUP_PLAN_TTL,
        };
        let mut plans = setup_plans()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        plans.retain(|_, plan| plan.expires_at > Instant::now());
        if plans.len() >= MAX_SETUP_PLANS {
            return Err(conflict(
                "Too many setup plans are active. Wait a moment and try again.",
            ));
        }
        plans.insert(plan_id.clone(), stored);
        Some(plan_id)
    } else {
        None
    };

    Ok(InstanceSetupPlanResponse {
        plan_id,
        expires_at_ms,
        selection_id,
        plan,
    })
}

pub async fn execute_instance_setup(
    state: &AppState,
    mut request: InstanceSetupExecuteRequest,
) -> Result<CreateInstanceResponse, ApiError> {
    let plan_id = request.plan_id.trim();
    if plan_id.is_empty() || plan_id.len() > 128 {
        return Err(bad_request("plan_id is invalid"));
    }
    let stored = {
        let mut plans = setup_plans()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        plans.remove(plan_id)
    }
    .ok_or_else(|| conflict("Setup plan is missing or expired. Review the setup again."))?;
    if stored.expires_at <= Instant::now() {
        return Err(conflict("Setup plan expired. Review the setup again."));
    }
    if request.create.selection_id.trim() != stored.selection_id {
        return Err(conflict("Setup selection changed. Review the setup again."));
    }

    let current_plan = content_plan(state, stored.request.clone()).await?;
    if !current_plan.conflicts.is_empty()
        || plan_fingerprint(&stored.selection_id, &stored.request, &current_plan)
            != stored.fingerprint
    {
        return Err(conflict(
            "Setup availability changed. Review the compatible versions again.",
        ));
    }

    request.create.selection_id = stored.selection_id;
    let mut created = handle_create_instance(state, request.create).await?;
    let instance_id = created.instance.id.clone();
    let prerequisite_queue_id = created
        .queued_install
        .as_ref()
        .and_then(|queued| queued.queue_id.clone());
    match queue_content_install_with_cleanup_after(
        state,
        ContentInstallRequest {
            instance_id: instance_id.clone(),
            selections: stored.request.selections,
            allow_incompatible: false,
        },
        true,
        prerequisite_queue_id,
    )
    .await
    {
        Ok(queue) => {
            created.install_queue = Some(queue);
            Ok(created)
        }
        Err(error) => {
            let _ = state.instances().remove(&instance_id, true);
            Err(error)
        }
    }
}

pub async fn execute_modpack_instance_setup(
    state: &AppState,
    mut request: ModpackInstanceSetupRequest,
) -> Result<CreateInstanceResponse, ApiError> {
    let target = modpack_target(state, &request.canonical_id, Some(&request.version_id)).await?;
    let selection = resolve_create_selection(state, &request.create).await?;
    let target_ref = TargetRef::Draft {
        loader: target.loader.clone(),
        game_version: target.minecraft.clone(),
    };
    if !target_matches_selection(&target_ref, &selection)
        || selection.exact_selection_id() != target.selection_id
    {
        return Err(conflict(
            "Modpack target does not match the selected Minecraft version and loader build.",
        ));
    }
    request.create.selection_id = target.selection_id;
    let mut created = handle_create_instance(state, request.create).await?;
    let instance_id = created.instance.id.clone();
    let prerequisite_queue_id = created
        .queued_install
        .as_ref()
        .and_then(|queued| queued.queue_id.clone());
    let queued = queue_modpack_install_after(
        state,
        ModpackInstallRequest {
            instance_id: instance_id.clone(),
            canonical_id: target.canonical_id.as_str().to_string(),
            version_id: Some(target.version_id),
            selected_paths: Vec::new(),
            include_overrides: true,
        },
        prerequisite_queue_id,
        true,
    )
    .await;
    match queued {
        Ok(queue) => {
            created.install_queue = Some(queue);
            Ok(created)
        }
        Err(error) => {
            let _ = state.instances().remove(&instance_id, true);
            Err(error)
        }
    }
}

fn plan_fingerprint(
    selection_id: &str,
    request: &ContentPlanRequest,
    plan: &ResolutionPlan,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    selection_id.hash(&mut hasher);
    match &request.target {
        TargetRef::Draft {
            loader,
            game_version,
        } => {
            "draft".hash(&mut hasher);
            loader.as_deref().unwrap_or("vanilla").hash(&mut hasher);
            game_version.hash(&mut hasher);
        }
        TargetRef::Instance { instance_id } => {
            "instance".hash(&mut hasher);
            instance_id.hash(&mut hasher);
        }
    }
    let mut selections: Vec<_> = request.selections.iter().collect();
    selections.sort_by(|left, right| left.canonical_id.cmp(&right.canonical_id));
    for selection in selections {
        selection.canonical_id.hash(&mut hasher);
        selection.kind.hash(&mut hasher);
        selection.version_id.hash(&mut hasher);
    }
    plan.loader.hash(&mut hasher);
    plan.game_version.hash(&mut hasher);
    let mut items: Vec<_> = plan.items.iter().collect();
    items.sort_by(|left, right| left.canonical_id.as_str().cmp(right.canonical_id.as_str()));
    for item in items {
        item.canonical_id.hash(&mut hasher);
        item.kind.hash(&mut hasher);
        item.project_id.hash(&mut hasher);
        item.version_id.hash(&mut hasher);
        item.filename.hash(&mut hasher);
        item.sha1.hash(&mut hasher);
        item.sha512.hash(&mut hasher);
        item.size.hash(&mut hasher);
        item.already_installed.hash(&mut hasher);
        item.update.hash(&mut hasher);
        let mut dependencies: Vec<_> = item.dependencies.iter().collect();
        dependencies.sort_by(|left, right| {
            dependency_kind_key(left.kind)
                .cmp(dependency_kind_key(right.kind))
                .then_with(|| left.project_id.cmp(&right.project_id))
                .then_with(|| left.version_id.cmp(&right.version_id))
        });
        for dependency in dependencies {
            dependency_kind_key(dependency.kind).hash(&mut hasher);
            dependency.project_id.hash(&mut hasher);
            dependency.version_id.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn dependency_kind_key(kind: axial_content::DependencyKind) -> &'static str {
    match kind {
        axial_content::DependencyKind::Required => "required",
        axial_content::DependencyKind::Optional => "optional",
        axial_content::DependencyKind::Incompatible => "incompatible",
        axial_content::DependencyKind::Embedded => "embedded",
    }
}

fn pin_selected_versions(request: &mut ContentPlanRequest, plan: &ResolutionPlan) {
    for selection in &mut request.selections {
        if let Some(item) = plan.items.iter().find(|item| {
            item.reason == PlanReason::Selected
                && item.canonical_id.as_str() == selection.canonical_id
        }) {
            selection.version_id = Some(item.version_id.clone());
        }
    }
}

fn target_matches_selection(target: &TargetRef, selection: &CreateSelection) -> bool {
    let TargetRef::Draft {
        loader,
        game_version,
    } = target
    else {
        return false;
    };
    let target_loader = loader.as_deref().unwrap_or("vanilla").trim();
    let selection_loader = match selection {
        CreateSelection::Vanilla { .. } => "vanilla",
        CreateSelection::Loader { component_id, .. } => component_id.short_key(),
    };
    target_loader == selection_loader && game_version.trim() == selection.minecraft_version()
}

fn new_plan_id(fingerprint: u64) -> String {
    let sequence = SETUP_PLAN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("setup-{fingerprint:016x}-{sequence:016x}")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn bad_request(message: &'static str) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": message })),
    )
}

fn conflict(message: &'static str) -> ApiError {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({ "error": message })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::content::{ContentSelection, PlanItem};
    use axial_content::{CanonicalId, ContentKind};

    fn plan_item(id: &str, version_id: &str, reason: PlanReason) -> PlanItem {
        PlanItem {
            canonical_id: CanonicalId(id.to_string()),
            title: id.to_string(),
            kind: ContentKind::Mod,
            project_id: id.trim_start_matches("modrinth:").to_string(),
            version_id: version_id.to_string(),
            version_number: version_id.to_string(),
            filename: format!("{version_id}.jar"),
            sha1: Some(format!("sha1-{version_id}")),
            sha512: Some(format!("sha512-{version_id}")),
            size: Some(1),
            dependencies: Vec::new(),
            reason,
            already_installed: false,
            update: false,
        }
    }

    #[test]
    fn setup_plan_pins_selected_versions_without_turning_dependencies_into_selections() {
        let mut request = ContentPlanRequest {
            target: TargetRef::Draft {
                loader: Some("fabric".to_string()),
                game_version: "1.21.1".to_string(),
            },
            selections: vec![ContentSelection {
                canonical_id: "modrinth:selected".to_string(),
                kind: ContentKind::Mod,
                version_id: None,
            }],
        };
        let plan = ResolutionPlan {
            instance_id: None,
            loader: "fabric".to_string(),
            game_version: "1.21.1".to_string(),
            items: vec![
                plan_item("modrinth:selected", "selected-v2", PlanReason::Selected),
                plan_item(
                    "modrinth:dependency",
                    "dependency-v1",
                    PlanReason::Dependency,
                ),
            ],
            conflicts: Vec::new(),
            total_download_bytes: 2,
        };

        pin_selected_versions(&mut request, &plan);

        assert_eq!(
            request.selections[0].version_id.as_deref(),
            Some("selected-v2")
        );
        assert_eq!(request.selections.len(), 1);
    }

    #[test]
    fn setup_plan_ids_are_unique_even_for_the_same_fingerprint() {
        assert_ne!(new_plan_id(7), new_plan_id(7));
    }

    #[test]
    fn setup_fingerprint_ignores_presentation_metadata() {
        let request = ContentPlanRequest {
            target: TargetRef::Draft {
                loader: Some("fabric".to_string()),
                game_version: "1.21.1".to_string(),
            },
            selections: vec![ContentSelection {
                canonical_id: "modrinth:selected".to_string(),
                kind: ContentKind::Mod,
                version_id: Some("selected-v2".to_string()),
            }],
        };
        let mut first_item = plan_item("modrinth:selected", "selected-v2", PlanReason::Selected);
        first_item.title = "Fallback version title".to_string();
        first_item.version_number = "display-v1".to_string();
        let first = ResolutionPlan {
            instance_id: None,
            loader: "fabric".to_string(),
            game_version: "1.21.1".to_string(),
            items: vec![first_item],
            conflicts: Vec::new(),
            total_download_bytes: 1,
        };
        let mut second_item = plan_item("modrinth:selected", "selected-v2", PlanReason::Selected);
        second_item.title = "Resolved project title".to_string();
        second_item.version_number = "different display label".to_string();
        let second = ResolutionPlan {
            instance_id: Some("presentation-only".to_string()),
            loader: "fabric".to_string(),
            game_version: "1.21.1".to_string(),
            items: vec![second_item],
            conflicts: Vec::new(),
            total_download_bytes: 999,
        };

        assert_eq!(
            plan_fingerprint("selection", &request, &first),
            plan_fingerprint("selection", &request, &second)
        );
    }

    #[test]
    fn setup_fingerprint_changes_with_file_or_dependency_identity() {
        let request = ContentPlanRequest {
            target: TargetRef::Draft {
                loader: Some("fabric".to_string()),
                game_version: "1.21.1".to_string(),
            },
            selections: vec![ContentSelection {
                canonical_id: "modrinth:selected".to_string(),
                kind: ContentKind::Mod,
                version_id: Some("selected-v2".to_string()),
            }],
        };
        let original = ResolutionPlan {
            instance_id: None,
            loader: "fabric".to_string(),
            game_version: "1.21.1".to_string(),
            items: vec![plan_item(
                "modrinth:selected",
                "selected-v2",
                PlanReason::Selected,
            )],
            conflicts: Vec::new(),
            total_download_bytes: 1,
        };
        let mut changed_item = plan_item("modrinth:selected", "selected-v2", PlanReason::Selected);
        changed_item.sha512 = Some("changed-sha512".to_string());
        changed_item
            .dependencies
            .push(axial_content::ContentDependency {
                project_id: Some("required-project".to_string()),
                version_id: Some("required-version".to_string()),
                kind: axial_content::DependencyKind::Required,
            });
        let changed = ResolutionPlan {
            instance_id: None,
            loader: "fabric".to_string(),
            game_version: "1.21.1".to_string(),
            items: vec![changed_item],
            conflicts: Vec::new(),
            total_download_bytes: 1,
        };

        assert_ne!(
            plan_fingerprint("selection", &request, &original),
            plan_fingerprint("selection", &request, &changed)
        );
    }
}

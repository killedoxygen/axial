//! Export the public wire contract the frontend consumes, never runtime commands or credentials.

use axial_app::{
    accounts::view::{
        AccountCommandResponse, AccountListResponse, AccountRemovedResponse, ProfileRefreshResponse,
    },
    catalog::VersionsResponse,
    content::{
        model::{ContentDetail, Page},
        packs::ModpackFilesPlan,
        provenance::RequiredRefusal,
        view::{
            CompatibilityResponse, InstalledContent, ResolutionPlan, SearchItem, TargetRef,
            UpdatesResponse,
        },
    },
    install::model::{InstallQueueRequest, InstallQueueStateResponse},
    instances::{
        create::{CreateInstanceRequest, PendingInstancesResponse},
        delete::{DeletionResponse, DeletionSnapshot},
        from_pack::ModpackTarget,
        model::{EnrichedInstance, InstancePatch, JavaSelection},
        setup::{
            CreateInstanceResponse, CreateLoaderBuildsView, CreateView, InstanceSetupPlanResponse,
            ListResponse, SetupStatusResponse,
        },
    },
    launch::{
        coordinator::{LaunchErrorResponse, LaunchIntentView, LaunchRequest, PreflightDiagnostics},
        reports::{LaunchProofView, LaunchReportsResponse},
        session::{SessionLogsResponse, SessionSnapshot, SessionsResponse},
    },
    music::MusicStatusResponse,
    public::{ApplicationStatus, ErrorResponse, OperationId, StatusResponse, Tone},
    resources::{
        InstanceLogTailResponse, InstanceResourcesResponse, ResourceCommand,
        mods::{InstanceModMetadata, ModImportResponse},
        packs::InstancePackInfo,
    },
    runtime::model::JavaRuntimesResponse,
    settings::{
        ConfigPatch, ConfigView, FlagsResponse, InterfacePreferencesReceipt, InterfacePreferencesSnapshot,
        InterfacePreferencesUpdate,
    },
    skins::{
        library::SkinNormalizeResponse,
        lookup::UsernameProfile,
        profile_change::SavedSkinsResponse,
        view::{ProfileResetResponse, SkinApplyResponse, SkinCancelResponse, SkinFlushResponse},
    },
    system::SystemResourceResponse,
    update::{UpdateFlow, UpdateInfo, UpdateSnapshot},
};
use std::{collections::BTreeMap, fs, path::Path};
use ts_rs::{Config, TS};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let destination = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../frontend/src/generated");
    match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => {
            let expected = tempfile::tempdir()?;
            export(expected.path())?;
            let generated = files(expected.path())?;
            fs::create_dir_all(&destination)?;
            for entry in fs::read_dir(&destination)? {
                let entry = entry?;
                if !generated.contains_key(&entry.file_name()) {
                    fs::remove_file(entry.path())?;
                }
            }
            for (name, contents) in generated {
                fs::write(destination.join(name), contents)?;
            }
            Ok(())
        }
        [flag] if flag == "--check" => {
            let expected = tempfile::tempdir()?;
            export(expected.path())?;
            if files(expected.path())? != files(&destination)? {
                return Err("Public wire types are stale; run task contracts:generate.".into());
            }
            println!("Public wire types match the Rust response contracts.");
            Ok(())
        }
        _ => Err("Usage: export_contracts [--check]".into()),
    }
}

macro_rules! export {
    ($config:expr; $($ty:ty),+ $(,)?) => {
        $(<$ty>::export_all($config)?;)+
    };
}

fn export(destination: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::default()
        .with_large_int("number")
        .with_out_dir(destination);
    export!(&config;
        // Shared
        ErrorResponse, OperationId, StatusResponse, ApplicationStatus, Tone,
        // Settings and system
        ConfigView, ConfigPatch, InterfacePreferencesSnapshot, InterfacePreferencesUpdate,
        InterfacePreferencesReceipt, FlagsResponse, SystemResourceResponse, MusicStatusResponse,
        JavaRuntimesResponse, UpdateInfo, UpdateFlow, UpdateSnapshot,
        // Instances
        ListResponse, EnrichedInstance, InstancePatch, CreateInstanceRequest, JavaSelection, CreateView, CreateLoaderBuildsView,
        CreateInstanceResponse, InstanceSetupPlanResponse, SetupStatusResponse,
        PendingInstancesResponse, DeletionSnapshot, DeletionResponse, VersionsResponse,
        // Resources
        InstanceResourcesResponse, InstanceLogTailResponse, InstanceModMetadata, InstancePackInfo,
        ModImportResponse, ResourceCommand,
        // Content
        Page<SearchItem>, ContentDetail, ResolutionPlan, TargetRef, CompatibilityResponse,
        InstalledContent, UpdatesResponse, ModpackTarget, ModpackFilesPlan, RequiredRefusal,
        // Installation
        InstallQueueStateResponse, InstallQueueRequest,
        // Launch
        LaunchRequest, LaunchErrorResponse, SessionSnapshot, SessionsResponse, SessionLogsResponse,
        LaunchIntentView,
        PreflightDiagnostics, LaunchReportsResponse, LaunchProofView,
        // Accounts and skins
        AccountListResponse, AccountCommandResponse, AccountRemovedResponse,
        ProfileRefreshResponse, SavedSkinsResponse, SkinNormalizeResponse, SkinApplyResponse,
        SkinFlushResponse, SkinCancelResponse, ProfileResetResponse, UsernameProfile,
    );
    Ok(())
}

fn files(directory: &Path) -> std::io::Result<BTreeMap<std::ffi::OsString, Vec<u8>>> {
    fs::read_dir(directory)?
        .map(|entry| {
            let entry = entry?;
            Ok((entry.file_name(), fs::read(entry.path())?))
        })
        .collect()
}

//! Export only reviewed public DTOs, never runtime commands or credentials.

use axial_app::{
    import::model::{
        ImportPreview, InstanceImportMappings, InstanceImportRequest, InstanceImportResponse,
        MetadataImportReceipt, MetadataImportRequest, MetadataImportResponse, MetadataImportStatus,
        RulesImportRequest, RulesImportResponse, RulesImportStatus, SkinImportRequest,
        SkinImportResponse, SkinImportStatus,
    },
    instances::{delete::DeletionSnapshot, setup::CreateLoaderBuildsView},
    public::{ErrorResponse, OperationId},
    resources::{InstanceLogTailResponse, InstanceResourcesResponse},
    settings::{
        ConfigView, FlagsResponse, InterfacePreferencesReceipt, InterfacePreferencesSnapshot,
        InterfacePreferencesUpdate,
    },
    skins::pending::PendingSkinStatus,
    system::SystemResourceResponse,
    update::UpdateSnapshot,
};
use std::{collections::BTreeMap, fs, path::Path};
use ts_rs::{Config, TS};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let destination = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../frontend/src/generated");
    match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => export(&destination),
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

fn export(destination: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::default()
        .with_large_int("number")
        .with_out_dir(destination);
    ErrorResponse::export_all(&config)?;
    OperationId::export_all(&config)?;
    UpdateSnapshot::export_all(&config)?;
    ConfigView::export_all(&config)?;
    InterfacePreferencesSnapshot::export_all(&config)?;
    InterfacePreferencesUpdate::export_all(&config)?;
    InterfacePreferencesReceipt::export_all(&config)?;
    FlagsResponse::export_all(&config)?;
    SystemResourceResponse::export_all(&config)?;
    InstanceResourcesResponse::export_all(&config)?;
    InstanceLogTailResponse::export_all(&config)?;
    PendingSkinStatus::export_all(&config)?;
    ImportPreview::export_all(&config)?;
    InstanceImportRequest::export_all(&config)?;
    InstanceImportResponse::export_all(&config)?;
    InstanceImportMappings::export_all(&config)?;
    DeletionSnapshot::export_all(&config)?;
    CreateLoaderBuildsView::export_all(&config)?;
    MetadataImportRequest::export_all(&config)?;
    MetadataImportReceipt::export_all(&config)?;
    MetadataImportResponse::export_all(&config)?;
    MetadataImportStatus::export_all(&config)?;
    SkinImportRequest::export_all(&config)?;
    SkinImportResponse::export_all(&config)?;
    SkinImportStatus::export_all(&config)?;
    RulesImportRequest::export_all(&config)?;
    RulesImportResponse::export_all(&config)?;
    RulesImportStatus::export_all(&config)?;
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

use super::*;
use axial_app::{
    instances::create::{CreateInstanceRequest, CreateTarget},
    library::{AdmissionState, LibraryMode},
};
use serde_json::{Value, json};

fn temporary() -> tempfile::TempDir {
    tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap()
}

fn select(root: &Path, external: &Path, library_id: LibraryId) {
    fs::write(
        root.join("library.json"),
        serde_json::to_vec(&json!({
            "mode":"existing", "library_id":library_id.to_string(), "path":external,
        }))
        .unwrap(),
    )
    .unwrap();
}

async fn get(services: &DesktopServices, route: &str) -> reqwest::Response {
    let bootstrap = services.server.bootstrap();
    reqwest::Client::new()
        .get(format!("{}/api/v1/{route}", bootstrap.base_url))
        .header(transport::CAPABILITY_HEADER, bootstrap.capability)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn external_selection_precedes_instance_composition_and_survives_reopen() {
    let temporary = temporary();
    let root = temporary.path().join("replacement");
    let external = temporary.path().join("external");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("keep.txt"), b"unrelated payload").unwrap();
    let initial = start_in_profile(root.clone(), None).await.unwrap();
    initial.server.shutdown().await.unwrap();
    drop(initial);
    let library_id = LibraryId::new();
    select(&root, &external, library_id);
    let services = start_in_profile(root.clone(), None).await.unwrap();
    let pin = services.library.admit().unwrap();
    assert_eq!(pin.library_id(), library_id);
    assert_eq!(pin.read_projection().unwrap(), external);
    assert_eq!(
        services.library.snapshot().current.unwrap().mode,
        LibraryMode::Existing
    );
    drop(pin);
    let status: Value = get(&services, "status")
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["setup_required"], false);
    let target = CreateTarget::loader_for_tests(
        axial_minecraft::LoaderComponentId::Fabric,
        "1.21.1",
        "0.16.9",
    )
    .unwrap();
    let instance = services
        .instances
        .create(
            CreateInstanceRequest {
                name: "External instance".into(),
                selection_id: target.selection_id().to_owned(),
                ..Default::default()
            },
            target,
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        services
            .instances
            .registry()
            .get_live(&instance.id)
            .unwrap()
            .library_id,
        library_id.to_string()
    );
    assert!(!root.join("instances").exists());
    assert!(external.join("instances").is_dir());
    services.server.shutdown().await.unwrap();
    drop(services);
    let reopened = start_in_profile(root, None).await.unwrap();
    assert_eq!(reopened.library.admit().unwrap().library_id(), library_id);
    let instances: Value = get(&reopened, "instances")
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(instances["instances"][0]["id"], instance.id.as_str());
    assert_eq!(
        fs::read(external.join("keep.txt")).unwrap(),
        b"unrelated payload"
    );
    reopened.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_selected_library_never_creates_or_falls_back_to_managed() {
    let temporary = temporary();
    let root = temporary.path().join("replacement");
    let external = temporary.path().join("missing");
    let initial = start_in_profile(root.clone(), None).await.unwrap();
    initial.server.shutdown().await.unwrap();
    drop(initial);
    select(&root, &external, LibraryId::new());
    let services = start_in_profile(root.clone(), None).await.unwrap();
    assert_eq!(
        services.library.snapshot().admission,
        AdmissionState::Unavailable
    );
    assert!(services.library.snapshot().current.is_none());
    assert!(!external.exists());
    assert!(!root.join("instances").exists());
    let status: Value = get(&services, "status")
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["setup_required"], false);
    assert!(
        status["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap()
                .contains("external library is unavailable"))
    );
    for route in ["versions", "instances"] {
        assert_eq!(
            get(&services, route).await.status(),
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        );
    }
    let bootstrap = services.server.bootstrap();
    assert_eq!(
        reqwest::Client::new()
            .post(format!("{}/api/v1/setup/init", bootstrap.base_url))
            .header(transport::CAPABILITY_HEADER, bootstrap.capability)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(services.library.admit_application_root().is_ok());
    services.server.shutdown().await.unwrap();
    assert!(!external.exists());
}

#[tokio::test]
async fn invalid_library_selection_refuses_startup_and_preserves_configuration() {
    let temporary = temporary();
    let root = temporary.path().join("replacement");
    let initial = start_in_profile(root.clone(), None).await.unwrap();
    initial.server.shutdown().await.unwrap();
    drop(initial);
    let bytes = br#"{"mode":"existing","library_id":"00000000-0000-0000-0000-000000000000","path":"relative"}"#;
    fs::write(root.join("library.json"), bytes).unwrap();
    let failure = match start_in_profile(root.clone(), None).await {
        Ok(services) => {
            services.server.shutdown().await.unwrap();
            panic!("invalid selection started");
        }
        Err(failure) => failure,
    };
    assert!(failure.try_preserve().is_ok());
    assert_eq!(fs::read(root.join("library.json")).unwrap(), bytes);
}

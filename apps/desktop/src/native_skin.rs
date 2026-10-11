//! OS-selected files are captured once, read through exact filesystem authority,
//! and reduced to bounded validated bytes before the frontend can consume them.
use crate::bootstrap::DesktopBootstrap;
use axial_app::{
    library::{
        ApplicationRootPin, LibraryLifecycle, NativeFileAdmission, NativeFileDestination,
        NativeFilePublishOutcome,
    },
    media::{
        AdmittedSkinFile, MediaError, NativeSelection, NativeSkinAdmission, NativeSkinScope,
        SKIN_PNG_MAX_BYTES,
    },
    skins::library::{SavedSkinLibrary, SavedSkinRecord},
    tasks::{CancellationToken, TaskOwner},
};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tauri::{AppHandle, DragDropEvent, Emitter, PhysicalPosition, State, WebviewWindow, Window};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::{Mutex, oneshot};

const DRAG_EVENT: &str = "axial:desktop:skin-drag";
const CLOSED: &str = "Skin file selection is unavailable while Axial is closing.";

#[derive(Clone)]
pub struct NativeSkinFiles {
    library: LibraryLifecycle,
    export: Arc<StdMutex<ExportState>>,
    tasks: TaskOwner,
    admission: NativeSkinAdmission,
    scope: NativeSkinScope,
    picker: Arc<Mutex<()>>,
    drag_eligible: Arc<AtomicBool>,
}

#[derive(Default)]
struct ExportState {
    closed: bool,
    saved: Option<Arc<SavedSkinLibrary>>,
    destination: Option<NativeFileDestination>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkinExportResult {
    Published,
    Cancelled,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", content = "message", rename_all = "snake_case")]
pub enum SkinExportError {
    Refused(String),
    Unknown,
}

impl SkinExportError {
    fn from_message(message: String) -> Self {
        if message == EXPORT_UNKNOWN {
            Self::Unknown
        } else {
            Self::Refused(message)
        }
    }
}

const EXPORT_UNKNOWN: &str =
    "Skin PNG export outcome is unknown. Check the chosen destination before trying again.";

struct SelectedSkin {
    selection: NativeSelection,
    name: String,
    file: NativeFileAdmission,
}

impl NativeSkinFiles {
    pub fn new(library: LibraryLifecycle, tasks: TaskOwner, saved: Arc<SavedSkinLibrary>) -> Self {
        Self {
            library,
            export: Arc::new(StdMutex::new(ExportState {
                saved: Some(saved),
                ..ExportState::default()
            })),
            tasks,
            admission: NativeSkinAdmission::new(),
            scope: NativeSkinScope::for_window(crate::window::MAIN_WINDOW)
                .expect("fixed main window scope"),
            picker: Arc::new(Mutex::new(())),
            drag_eligible: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn close(&self) {
        self.drag_eligible.store(false, Ordering::Release);
        self.admission.close();
        self.export
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .closed = true;
    }

    pub async fn drain(&self) -> Result<(), String> {
        self.admission.drain().await;
        // The original accepted task retains this lease through the dialog and write.
        let _picker = self.picker.lock().await;
        let export = self.export.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = export.lock().unwrap_or_else(|error| error.into_inner());
            if !state.closed {
                return Err(CLOSED.to_string());
            }
            if let Some(destination) = state.destination.as_mut() {
                let outcome = match destination.reconcile() {
                    NativeFilePublishOutcome::Pending(_) => destination.preserve(),
                    outcome => outcome,
                };
                if matches!(outcome, NativeFilePublishOutcome::Pending(_)) {
                    return Err(EXPORT_UNKNOWN.to_string());
                }
                state.destination = None;
            }
            let saved = state.saved.take();
            drop(state);
            drop(saved);
            Ok(())
        })
        .await
        .map_err(|_| EXPORT_UNKNOWN.to_string())?
    }

    fn admit_selection(&self, path: &Path) -> Result<SelectedSkin, String> {
        let selection = self.admission.begin(&self.scope).map_err(media_message)?;
        self.capture(selection, path)
    }

    fn capture(&self, selection: NativeSelection, path: &Path) -> Result<SelectedSkin, String> {
        if !path.is_absolute() || !has_png_extension(path) {
            return Err("Choose a PNG skin file.".into());
        }
        let root = self
            .library
            .admit_application_root()
            .map_err(|_| CLOSED.to_string())?;
        let file = root
            .admit_native_file(path, SKIN_PNG_MAX_BYTES as u64)
            .map_err(read_message)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("skin.png")
            .to_owned();
        Ok(SelectedSkin {
            selection,
            name,
            file,
        })
    }

    async fn pick(
        &self,
        app: AppHandle,
        cancel: CancellationToken,
    ) -> Result<Option<AdmittedSkinFile>, String> {
        let (sender, receiver) = oneshot::channel();
        let owner = self.clone();
        let callback_cancel = cancel.clone();
        app.dialog()
            .file()
            .add_filter("PNG skin", &["png"])
            .pick_file(move |selected| {
                if sender.is_closed() || callback_cancel.is_cancelled() {
                    return;
                }
                let selected = selected
                    .map(|selected| {
                        let path = selected.into_path().map_err(|_| {
                            "Native skin picker returned an invalid file.".to_string()
                        })?;
                        // Capture the handle/revision before passing into an async read.
                        owner.admit_selection(&path)
                    })
                    .transpose();
                let _ = sender.send(selected);
            });
        let Some(selected) = wait_for_picker(receiver, &cancel).await? else {
            return Ok(None);
        };
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let token = read_selection(selected).await?;
        self.consume(&token).map(Some)
    }

    async fn export_saved(
        &self,
        app: AppHandle,
        root: ApplicationRootPin,
        saved: Arc<SavedSkinLibrary>,
        expected: SavedSkinRecord,
        cancel: CancellationToken,
    ) -> Result<SkinExportResult, String> {
        let captured = tokio::task::spawn_blocking(move || saved.capture_export(&expected))
            .await
            .map_err(|_| "Could not capture the saved skin PNG.".to_string())?
            .map_err(|error| error.to_string())?;
        if cancel.is_cancelled() {
            return Ok(SkinExportResult::Cancelled);
        }
        let (sender, receiver) = oneshot::channel::<Result<bool, String>>();
        let export = self.export.clone();
        let callback_cancel = cancel.clone();
        app.dialog()
            .file()
            .add_filter("PNG skin", &["png"])
            .set_file_name(download_filename(&captured.skin.record.name))
            .save_file(move |selected| {
                let selection = (|| {
                    if callback_cancel.is_cancelled() {
                        return Ok(false);
                    }
                    let Some(selected) = selected else {
                        return Ok(false);
                    };
                    let path = selected.into_path().map_err(|_| {
                        "Native save dialog returned an invalid destination.".to_string()
                    })?;
                    if !has_png_extension(&path) {
                        return Err("Choose a destination ending in .png.".into());
                    }
                    let mut state = export.lock().unwrap_or_else(|error| error.into_inner());
                    if state.closed {
                        return Ok(false);
                    }
                    if state.destination.is_some() {
                        return Err(EXPORT_UNKNOWN.into());
                    }
                    let destination = root
                        .admit_native_destination(&path)
                        .map_err(export_message)?;
                    state.destination = Some(destination);
                    Ok(true)
                })();
                let _ = sender.send(selection);
            });
        // Cancellation cannot drop a dialog callback retaining the root. Wait for
        // its actual response before releasing the accepted task's picker lease.
        let selected = receiver.await.map_err(|_| {
            "Native save dialog stopped before selecting a destination.".to_string()
        })??;
        if !selected {
            return Ok(SkinExportResult::Cancelled);
        }
        let export = self.export.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = export.lock().unwrap_or_else(|error| error.into_inner());
            let destination = state
                .destination
                .as_mut()
                .ok_or_else(|| EXPORT_UNKNOWN.to_string())?;
            let outcome = if cancel.is_cancelled() {
                destination.cancel()
            } else {
                destination.publish(&captured.png)
            };
            finish_export(&mut state, outcome)
        })
        .await
        .map_err(|_| EXPORT_UNKNOWN.to_string())?
    }

    fn consume(&self, token: &str) -> Result<AdmittedSkinFile, String> {
        if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("Dropped skin file token is invalid.".into());
        }
        self.admission
            .consume(&self.scope, token)
            .map_err(media_message)
    }

    pub fn handle_drag(&self, window: &Window, event: &DragDropEvent) {
        match event {
            DragDropEvent::Enter { paths, position } => {
                let eligible = matches!(drop_selection(paths), DropSelection::One(_))
                    && !self.tasks.status().closing;
                self.drag_eligible.store(eligible, Ordering::Release);
                emit(window, "enter", eligible, None, Some(*position), None);
            }
            DragDropEvent::Over { position } => emit(
                window,
                "over",
                self.drag_eligible.load(Ordering::Acquire) && !self.tasks.status().closing,
                None,
                Some(*position),
                None,
            ),
            DragDropEvent::Leave => {
                self.drag_eligible.store(false, Ordering::Release);
                // Moving out of the drop zone does not revoke an issued token.
                emit(window, "leave", false, None, None, None);
            }
            DragDropEvent::Drop { paths, position } => {
                self.drag_eligible.store(false, Ordering::Release);
                let selection = match self.admission.begin(&self.scope) {
                    Ok(selection) => selection,
                    Err(error) => {
                        emit(
                            window,
                            "drop",
                            false,
                            None,
                            Some(*position),
                            Some(media_message(error)),
                        );
                        return;
                    }
                };
                let path = match drop_selection(paths) {
                    DropSelection::One(path) => path,
                    DropSelection::None => {
                        emit(window, "drop", false, None, Some(*position), None);
                        return;
                    }
                    DropSelection::Multiple => {
                        emit(
                            window,
                            "drop",
                            false,
                            None,
                            Some(*position),
                            Some("Drop one PNG skin file.".into()),
                        );
                        return;
                    }
                };
                let selected = match self.capture(selection, path) {
                    Ok(selected) => selected,
                    Err(error) => {
                        emit(window, "drop", false, None, Some(*position), Some(error));
                        return;
                    }
                };
                let window = window.clone();
                let failed_window = window.clone();
                let position = *position;
                // The accepted read and its captured file outlive any event listener.
                if self
                    .tasks
                    .try_spawn((), move |cancel| async move {
                        let result = if cancel.is_cancelled() {
                            Err(CLOSED.to_string())
                        } else {
                            read_selection(selected).await
                        };
                        match result {
                            Ok(token) => {
                                emit(&window, "drop", true, Some(token), Some(position), None)
                            }
                            Err(error) => {
                                emit(&window, "drop", false, None, Some(position), Some(error))
                            }
                        }
                    })
                    .is_err()
                {
                    emit(
                        &failed_window,
                        "drop",
                        false,
                        None,
                        Some(position),
                        Some(CLOSED.into()),
                    );
                }
            }
            _ => {}
        }
    }
}

async fn wait_for_picker(
    receiver: oneshot::Receiver<Result<Option<SelectedSkin>, String>>,
    cancel: &CancellationToken,
) -> Result<Option<SelectedSkin>, String> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Ok(None),
        selected = receiver => selected.map_err(|_| "Native skin picker stopped before returning a selection.".to_string())?,
    }
}

async fn read_selection(selected: SelectedSkin) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        let SelectedSkin {
            selection,
            name,
            file,
        } = selected;
        let bytes = file.read().map_err(read_message)?;
        selection
            .publish(&name, bytes)
            .map(|handle| handle.token)
            .map_err(media_message)
    })
    .await
    .map_err(|_| "Could not read skin file.".to_string())?
}

#[tauri::command]
pub async fn pick_skin_file(
    app: AppHandle,
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
    skin: State<'_, NativeSkinFiles>,
) -> Result<Option<AdmittedSkinFile>, String> {
    crate::window::require_main_window(&window, &bootstrap)?;
    let skin = skin.inner().clone();
    let permit = skin
        .picker
        .clone()
        .try_lock_owned()
        .map_err(|_| "The skin file picker is already open.")?;
    let tasks = skin.tasks.clone();
    tasks
        .try_spawn(
            permit,
            move |cancel| async move { skin.pick(app, cancel).await },
        )
        .map_err(|_| CLOSED.to_string())?
        .join()
        .await
        .map_err(|_| "Skin file selection stopped before it could finish.".to_string())?
}

#[tauri::command]
pub async fn export_saved_skin_png(
    app: AppHandle,
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
    skin: State<'_, NativeSkinFiles>,
    expected: SavedSkinRecord,
) -> Result<SkinExportResult, SkinExportError> {
    crate::window::require_main_window(&window, &bootstrap).map_err(SkinExportError::Refused)?;
    let skin = skin.inner().clone();
    let permit =
        skin.picker.clone().try_lock_owned().map_err(|_| {
            SkinExportError::Refused("The skin file dialog is already open.".into())
        })?;
    let root = skin
        .library
        .admit_application_root()
        .map_err(|_| SkinExportError::Refused(CLOSED.into()))?;
    let tasks = skin.tasks.clone();
    let export = skin.export.clone();
    let handle = {
        let state = export.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return Err(SkinExportError::Refused(CLOSED.into()));
        }
        if state.destination.is_some() {
            return Err(SkinExportError::Unknown);
        }
        let saved = state
            .saved
            .as_ref()
            .cloned()
            .ok_or_else(|| SkinExportError::Refused(CLOSED.into()))?;
        tasks
            .try_spawn((permit, root.clone()), move |cancel| async move {
                skin.export_saved(app, root, saved, expected, cancel).await
            })
            .map_err(|_| SkinExportError::Refused(CLOSED.into()))?
    };
    handle
        .join()
        .await
        .map_err(|_| SkinExportError::Unknown)?
        .map_err(SkinExportError::from_message)
}

fn finish_export(
    state: &mut ExportState,
    outcome: NativeFilePublishOutcome,
) -> Result<SkinExportResult, String> {
    match outcome {
        NativeFilePublishOutcome::Pending(_) => Err(EXPORT_UNKNOWN.into()),
        NativeFilePublishOutcome::Preserved => {
            state.destination = None;
            Err(EXPORT_UNKNOWN.into())
        }
        NativeFilePublishOutcome::Published => {
            state.destination = None;
            Ok(SkinExportResult::Published)
        }
        NativeFilePublishOutcome::Cancelled => {
            state.destination = None;
            Ok(SkinExportResult::Cancelled)
        }
        NativeFilePublishOutcome::NoEffect(error) => {
            state.destination = None;
            Err(export_message(error))
        }
    }
}

fn export_message(error: std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::AlreadyExists => {
            "A file already exists at that destination. Choose a new filename."
        }
        std::io::ErrorKind::PermissionDenied => {
            "Skin PNG destination access was denied. Choose another destination."
        }
        std::io::ErrorKind::StorageFull => {
            "The skin PNG destination is full. Free space and try again."
        }
        std::io::ErrorKind::InvalidInput
        | std::io::ErrorKind::InvalidData
        | std::io::ErrorKind::NotFound => {
            "Skin PNG destination changed or is unavailable. Choose it again."
        }
        _ => "Could not save skin PNG. No file was published.",
    }
    .into()
}

fn download_filename(name: &str) -> String {
    let normalized: String = name
        .trim()
        .chars()
        .map(|value| {
            if matches!(value, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
                || (value as u32) < 32
            {
                ' '
            } else {
                value
            }
        })
        .collect();
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    let units: Vec<u16> = normalized.encode_utf16().take(80).collect();
    let bounded = String::from_utf16_lossy(&units);
    let mut base = bounded.trim();
    if base.len() >= 4
        && base
            .get(base.len() - 4..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".png"))
    {
        base = &base[..base.len() - 4];
    }
    base = base.trim_end_matches(['.', ' ']).trim();
    format!("{}.png", if base.is_empty() { "saved-skin" } else { base })
}

#[tauri::command]
pub fn consume_skin_drop(
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
    skin: State<'_, NativeSkinFiles>,
    token: String,
) -> Result<AdmittedSkinFile, String> {
    crate::window::require_main_window(&window, &bootstrap)?;
    skin.consume(&token)
}

enum DropSelection<'a> {
    None,
    Multiple,
    One(&'a Path),
}

fn drop_selection(paths: &[PathBuf]) -> DropSelection<'_> {
    if !paths.iter().any(|path| has_png_extension(path)) {
        return DropSelection::None;
    }
    if paths.len() != 1 {
        return DropSelection::Multiple;
    }
    DropSelection::One(paths[0].as_path())
}

fn has_png_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("png"))
}

#[derive(Clone, Serialize)]
struct DragPayload {
    r#type: &'static str,
    eligible: bool,
    token: Option<String>,
    position: Option<DragPosition>,
    error: Option<String>,
}

#[derive(Clone, Serialize)]
struct DragPosition {
    x: f64,
    y: f64,
}

fn emit(
    window: &Window,
    kind: &'static str,
    eligible: bool,
    token: Option<String>,
    position: Option<PhysicalPosition<f64>>,
    error: Option<String>,
) {
    let _ = window.emit(
        DRAG_EVENT,
        DragPayload {
            r#type: kind,
            eligible,
            token,
            position: position.map(|position| DragPosition {
                x: position.x,
                y: position.y,
            }),
            error,
        },
    );
}

fn read_message(error: std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof => {
            "Skin file changed while it was being read. Choose it again."
        }
        std::io::ErrorKind::FileTooLarge => "Skin file is too large; choose a PNG under 256 KiB.",
        _ => "Could not read skin file.",
    }
    .into()
}

fn media_message(error: MediaError) -> String {
    match error {
        MediaError::TooLarge => "Skin file is too large; choose a PNG under 256 KiB.",
        MediaError::InvalidPng => "Choose a valid PNG skin file.",
        MediaError::InvalidDimensions => "Skin image must be 64x64 or 64x32.",
        MediaError::Closed => CLOSED,
        MediaError::Busy => "Another skin file is still being checked.",
        MediaError::InvalidSelection => "Dropped skin file is no longer available. Drop it again.",
        MediaError::FileChanged => "Skin file changed while it was being read. Choose it again.",
        _ => "Could not read skin file.",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, NativeSkinFiles) {
        let directory =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let library = match LibraryLifecycle::open(directory.path()) {
            axial_app::library::LibraryOpenOutcome::Ready(library) => library,
            axial_app::library::LibraryOpenOutcome::NoEffect(error) => {
                panic!("fixture acquisition failed: {error}")
            }
            axial_app::library::LibraryOpenOutcome::Unresolved(obligation) => {
                let result = obligation.acknowledge_preserved();
                assert!(result.is_ok(), "fixture acquisition could not be preserved");
                panic!("fixture acquisition was unresolved")
            }
        };
        let metadata = Arc::new(axial_app::storage::MetadataStore::in_memory().unwrap());
        axial_app::accounts::directory::AccountDirectory::new(metadata.clone()).unwrap();
        metadata
            .migrate(&[axial_app::skins::store::MIGRATION])
            .unwrap();
        let saved = Arc::new(SavedSkinLibrary::new(
            axial_app::skins::store::SavedSkinStore::new(metadata),
            library.admit_application_root().unwrap(),
        ));
        (
            directory,
            NativeSkinFiles::new(library, TaskOwner::new(4).unwrap(), saved),
        )
    }

    fn png() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 64, 64);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&vec![128; 64 * 64 * 4])
                .unwrap();
        }
        bytes
    }

    async fn finish(skin: &NativeSkinFiles) {
        skin.close();
        skin.drain().await.unwrap();
        skin.library.try_preserve().unwrap();
    }

    #[tokio::test]
    async fn accepted_export_outlives_its_waiter_and_never_changes_the_library_or_existing_target()
    {
        let (_directory, skin) = fixture();
        let external =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let source = skin.export.lock().unwrap().saved.as_ref().unwrap().clone();
        let saved = source
            .save_upload(
                &png(),
                axial_app::skins::library::SaveSkinOptions {
                    name: "Export skin".into(),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let captured = source.capture_export(&saved).unwrap();
        let before = source.get(&saved.texture_key).unwrap();
        let bytes = captured.png;
        let path = external.path().join(download_filename(&saved.name));
        let root = skin.library.admit_application_root().unwrap();
        skin.export.lock().unwrap().destination =
            Some(root.admit_native_destination(&path).unwrap());
        let export = skin.export.clone();
        let expected = bytes.clone();
        let permit = skin.picker.clone().try_lock_owned().unwrap();
        let (start, started) = oneshot::channel();
        let (settled, result) = oneshot::channel();
        let handle = skin
            .tasks
            .try_spawn((permit, root), move |_cancel| async move {
                started.await.unwrap();
                let result = tokio::task::spawn_blocking(move || {
                    let mut state = export.lock().unwrap();
                    let outcome = state.destination.as_mut().unwrap().publish(&expected);
                    finish_export(&mut state, outcome)
                })
                .await
                .unwrap();
                let _ = settled.send(result);
            })
            .unwrap();
        drop(handle);
        start.send(()).unwrap();
        assert_eq!(result.await.unwrap().unwrap(), SkinExportResult::Published);
        skin.tasks
            .shutdown(std::time::Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(source.get(&saved.texture_key).unwrap(), before);
        let root = skin.library.admit_application_root().unwrap();
        match root.admit_native_destination(&path) {
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists),
            Ok(mut destination) => {
                assert!(matches!(
                    destination.publish(b"replacement"),
                    NativeFilePublishOutcome::NoEffect(_)
                ));
            }
        }
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        drop(source);
        finish(&skin).await;
    }

    #[tokio::test]
    async fn selected_png_returns_exact_bytes_and_only_one_valid_token_consumption() {
        let (directory, skin) = fixture();
        let path = directory.path().join("selected.PNG");
        let bytes = png();
        std::fs::write(&path, &bytes).unwrap();
        let token = read_selection(skin.admit_selection(&path).unwrap())
            .await
            .unwrap();
        assert!(skin.consume("../../selected.PNG").is_err());
        assert!(skin.consume(&"0".repeat(64)).is_err());
        assert_eq!(
            skin.consume(&token).unwrap(),
            AdmittedSkinFile {
                name: "selected.PNG".into(),
                bytes
            }
        );
        assert!(skin.consume(&token).is_err());
        finish(&skin).await;
    }

    #[tokio::test]
    async fn invalid_png_revokes_previous_drop_and_cannot_return_bytes() {
        let (directory, skin) = fixture();
        let previous = skin
            .admission
            .begin(&skin.scope)
            .unwrap()
            .publish("previous.png", png())
            .unwrap();
        let path = directory.path().join("invalid.png");
        std::fs::write(&path, b"not a PNG").unwrap();
        let error = read_selection(skin.admit_selection(&path).unwrap())
            .await
            .unwrap_err();
        assert_eq!(error, "Choose a valid PNG skin file.");
        assert!(skin.consume(&previous.token).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"not a PNG");
        finish(&skin).await;
    }

    #[tokio::test]
    async fn close_waits_for_selected_file_and_refuses_late_publication() {
        let (directory, skin) = fixture();
        let path = directory.path().join("selected.png");
        std::fs::write(&path, png()).unwrap();
        let selected = skin.admit_selection(&path).unwrap();
        skin.close();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(5), skin.drain())
                .await
                .is_err()
        );
        assert_eq!(read_selection(selected).await.unwrap_err(), CLOSED);
        assert!(skin.admit_selection(&path).is_err());
        finish(&skin).await;
    }

    #[tokio::test]
    async fn cancelled_picker_returns_no_file_and_no_native_admission() {
        let (sender, receiver) = oneshot::channel();
        sender.send(Ok(None)).ok().unwrap();
        assert!(
            wait_for_picker(receiver, &CancellationToken::new())
                .await
                .unwrap()
                .is_none()
        );
        let (_sender, receiver) = oneshot::channel();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(wait_for_picker(receiver, &cancel).await.unwrap().is_none());
    }

    #[test]
    fn export_response_encodes_only_settled_success_and_owner_classified_refusal() {
        assert_eq!(
            serde_json::to_value(SkinExportResult::Published).unwrap(),
            serde_json::json!("published")
        );
        assert_eq!(
            serde_json::to_value(SkinExportResult::Cancelled).unwrap(),
            serde_json::json!("cancelled")
        );
        assert_eq!(
            serde_json::to_value(SkinExportError::Unknown).unwrap(),
            serde_json::json!({"status": "unknown"})
        );
        assert_eq!(
            serde_json::to_value(SkinExportError::Refused("Destination unavailable.".into()))
                .unwrap(),
            serde_json::json!({"status": "refused", "message": "Destination unavailable."})
        );
    }

    #[test]
    fn drop_selection_preserves_single_png_and_rejects_mixed_or_multiple_files() {
        assert!(matches!(
            drop_selection(&[PathBuf::from("skin.PNG")]),
            DropSelection::One(_)
        ));
        assert!(matches!(
            drop_selection(&[PathBuf::from("skin.png"), PathBuf::from("other.txt")]),
            DropSelection::Multiple
        ));
        assert!(matches!(
            drop_selection(&[PathBuf::from("notes.txt")]),
            DropSelection::None
        ));
    }

    #[test]
    fn drag_payload_matches_the_retained_frontend_contract() {
        let payload = DragPayload {
            r#type: "drop",
            eligible: true,
            token: Some("token".into()),
            position: Some(DragPosition { x: 20.0, y: 40.0 }),
            error: None,
        };
        assert_eq!(
            serde_json::to_value(payload).unwrap(),
            serde_json::json!({
                "type": "drop", "eligible": true, "token": "token", "position": { "x": 20.0, "y": 40.0 }, "error": null,
            })
        );
    }
}

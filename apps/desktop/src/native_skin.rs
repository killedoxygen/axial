//! OS-selected files are captured once, read through exact filesystem authority,
//! and reduced to bounded validated bytes before the frontend can consume them.
use crate::bootstrap::DesktopBootstrap;
use axial_app::{
    library::{LibraryLifecycle, NativeFileAdmission},
    media::{
        AdmittedSkinFile, MediaError, NativeSelection, NativeSkinAdmission, NativeSkinScope,
        SKIN_PNG_MAX_BYTES,
    },
    tasks::{CancellationToken, TaskOwner},
};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
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
    tasks: TaskOwner,
    admission: NativeSkinAdmission,
    scope: NativeSkinScope,
    picker: Arc<Mutex<()>>,
    drag_eligible: Arc<AtomicBool>,
}

struct SelectedSkin {
    selection: NativeSelection,
    name: String,
    file: NativeFileAdmission,
}

impl NativeSkinFiles {
    pub fn new(library: LibraryLifecycle, tasks: TaskOwner) -> Self {
        Self {
            library,
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
    }

    pub async fn drain(&self) {
        self.admission.drain().await;
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
        (
            directory,
            NativeSkinFiles::new(library, TaskOwner::new(4).unwrap()),
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
        skin.drain().await;
        skin.library.try_preserve().unwrap();
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

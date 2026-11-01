//! OS-selected mod jars and modpack archives are captured once through exact
//! filesystem authority and held for one bounded frontend read. The frontend
//! hands the bytes to the local API, which owns validation and import.
use crate::bootstrap::DesktopBootstrap;
use axial_app::{library::LibraryLifecycle, tasks::TaskOwner};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};
use tauri::{AppHandle, DragDropEvent, Emitter, PhysicalPosition, State, WebviewWindow, Window};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::{Mutex, oneshot};

const DRAG_EVENT: &str = "axial:desktop:content-drag";
const CLOSED: &str = "File selection is unavailable while Axial is closing.";
/// The native admission bound for one OS-selected file.
const MAX_FILE_BYTES: u64 = 32 << 20;
const MAX_FILES: usize = 64;
const MAX_TOTAL_BYTES: u64 = 256 << 20;
const HANDLE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentFileKind {
    /// One or more `.jar` files for an instance's mods folder.
    Mod,
    /// One `.mrpack` archive for a new instance.
    Modpack,
}

impl ContentFileKind {
    fn extension(self) -> &'static str {
        match self {
            Self::Mod => "jar",
            Self::Modpack => "mrpack",
        }
    }

    /// Mods may be added together; a modpack creates one instance.
    fn of(paths: &[PathBuf]) -> Option<Self> {
        let all =
            |kind: Self| !paths.is_empty() && paths.iter().all(|path| has_extension(path, kind));
        if all(Self::Mod) && paths.len() <= MAX_FILES {
            Some(Self::Mod)
        } else if paths.len() == 1 && all(Self::Modpack) {
            Some(Self::Modpack)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ContentSelection {
    token: String,
    kind: ContentFileKind,
    names: Vec<String>,
}

struct Pending {
    token: String,
    expires_at: Instant,
    files: Vec<Option<Vec<u8>>>,
}

#[derive(Clone)]
pub struct NativeContentFiles {
    library: LibraryLifecycle,
    tasks: TaskOwner,
    pending: Arc<StdMutex<Option<Pending>>>,
    picker: Arc<Mutex<()>>,
    drag: Arc<StdMutex<Option<ContentFileKind>>>,
}

struct Captured {
    kind: ContentFileKind,
    files: Vec<(String, axial_app::library::NativeFileAdmission)>,
}

impl NativeContentFiles {
    pub fn new(library: LibraryLifecycle, tasks: TaskOwner) -> Self {
        Self {
            library,
            tasks,
            pending: Arc::new(StdMutex::new(None)),
            picker: Arc::new(Mutex::new(())),
            drag: Arc::new(StdMutex::new(None)),
        }
    }

    /// Capture each handle synchronously, before any async read can race a rename.
    fn capture(&self, kind: ContentFileKind, paths: &[PathBuf]) -> Result<Captured, String> {
        if ContentFileKind::of(paths) != Some(kind) {
            return Err(match kind {
                ContentFileKind::Mod => format!("Choose up to {MAX_FILES} .jar mod files."),
                ContentFileKind::Modpack => "Choose one .mrpack modpack file.".into(),
            });
        }
        let root = self
            .library
            .admit_application_root()
            .map_err(|_| CLOSED.to_string())?;
        let files = paths
            .iter()
            .map(|path| {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| "A selected file name is not valid.".to_string())?
                    .to_owned();
                let file = root
                    .admit_native_file(path, MAX_FILE_BYTES)
                    .map_err(|error| read_message(&name, error))?;
                Ok((name, file))
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Captured { kind, files })
    }

    fn publish(
        &self,
        kind: ContentFileKind,
        files: Vec<(String, Vec<u8>)>,
    ) -> Result<ContentSelection, String> {
        if self.tasks.status().closing {
            return Err(CLOSED.into());
        }
        let token = uuid::Uuid::new_v4().simple().to_string();
        let names = files.iter().map(|(name, _)| name.clone()).collect();
        // A newer selection revokes any unread one.
        *self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Pending {
            token: token.clone(),
            expires_at: Instant::now() + HANDLE_TTL,
            files: files.into_iter().map(|(_, bytes)| Some(bytes)).collect(),
        });
        Ok(ContentSelection { token, kind, names })
    }

    fn take(&self, token: &str, index: usize) -> Result<Vec<u8>, String> {
        let invalid = || "The selected file is no longer available. Choose it again.".to_string();
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let current = pending
            .as_mut()
            .filter(|pending| pending.token == token && pending.expires_at > Instant::now())
            .ok_or_else(invalid)?;
        let bytes = current
            .files
            .get_mut(index)
            .and_then(Option::take)
            .ok_or_else(invalid)?;
        if current.files.iter().all(Option::is_none) {
            *pending = None;
        }
        Ok(bytes)
    }

    async fn pick(
        &self,
        app: AppHandle,
        kind: ContentFileKind,
    ) -> Result<Option<ContentSelection>, String> {
        let (sender, receiver) = oneshot::channel();
        let owner = self.clone();
        let dialog = app.dialog().file().add_filter(
            match kind {
                ContentFileKind::Mod => "Mod jar",
                ContentFileKind::Modpack => "Modrinth modpack",
            },
            &[kind.extension()],
        );
        let respond = move |paths: Option<Vec<PathBuf>>| {
            let _ = sender.send(paths.map(|paths| owner.capture(kind, &paths)).transpose());
        };
        let into_path = |selected: tauri_plugin_dialog::FilePath| selected.into_path().ok();
        match kind {
            ContentFileKind::Mod => dialog.pick_files(move |selected| {
                respond(
                    selected.map(|selected| selected.into_iter().filter_map(into_path).collect()),
                )
            }),
            ContentFileKind::Modpack => dialog.pick_file(move |selected| {
                respond(selected.and_then(into_path).map(|path| vec![path]))
            }),
        }
        let captured = receiver
            .await
            .map_err(|_| "The file picker stopped before returning a selection.".to_string())??;
        match captured {
            Some(captured) => self.read(captured).await.map(Some),
            None => Ok(None),
        }
    }

    async fn read(&self, captured: Captured) -> Result<ContentSelection, String> {
        let kind = captured.kind;
        let files = tokio::task::spawn_blocking(move || {
            let mut total = 0_u64;
            captured
                .files
                .into_iter()
                .map(|(name, file)| {
                    let bytes = file.read().map_err(|error| read_message(&name, error))?;
                    total = total.saturating_add(bytes.len() as u64);
                    if total > MAX_TOTAL_BYTES {
                        return Err("The selected files are too large to add at once.".to_string());
                    }
                    Ok((name, bytes))
                })
                .collect::<Result<Vec<_>, String>>()
        })
        .await
        .map_err(|_| "Could not read the selected files.".to_string())??;
        self.publish(kind, files)
    }

    pub fn handle_drag(&self, window: &Window, event: &DragDropEvent) {
        match event {
            DragDropEvent::Enter { paths, position } => {
                let kind = ContentFileKind::of(paths).filter(|_| !self.tasks.status().closing);
                *self.drag.lock().unwrap_or_else(|error| error.into_inner()) = kind;
                emit(window, "enter", kind, None, Some(*position), None);
            }
            DragDropEvent::Over { position } => {
                let kind = *self.drag.lock().unwrap_or_else(|error| error.into_inner());
                emit(window, "over", kind, None, Some(*position), None);
            }
            DragDropEvent::Leave => {
                self.drag
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                emit(window, "leave", None, None, None, None);
            }
            DragDropEvent::Drop { paths, position } => {
                self.drag
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                let position = *position;
                let Some(kind) = ContentFileKind::of(paths) else {
                    emit(window, "drop", None, None, Some(position), None);
                    return;
                };
                let captured = match self.capture(kind, paths) {
                    Ok(captured) => captured,
                    Err(error) => {
                        emit(
                            window,
                            "drop",
                            Some(kind),
                            None,
                            Some(position),
                            Some(error),
                        );
                        return;
                    }
                };
                let owner = self.clone();
                let window = window.clone();
                let failed = window.clone();
                if self
                    .tasks
                    .try_spawn((), move |_cancel| async move {
                        match owner.read(captured).await {
                            Ok(selection) => emit(
                                &window,
                                "drop",
                                Some(kind),
                                Some(selection),
                                Some(position),
                                None,
                            ),
                            Err(error) => emit(
                                &window,
                                "drop",
                                Some(kind),
                                None,
                                Some(position),
                                Some(error),
                            ),
                        }
                    })
                    .is_err()
                {
                    emit(
                        &failed,
                        "drop",
                        Some(kind),
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

fn has_extension(path: &Path, kind: ContentFileKind) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(kind.extension()))
}

fn read_message(name: &str, error: std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::FileTooLarge => format!("{name} is larger than the 32 MB file limit."),
        std::io::ErrorKind::PermissionDenied => format!("Axial was not allowed to read {name}."),
        _ => format!("Could not read {name}. Choose it again."),
    }
}

#[derive(Clone, Serialize)]
struct DragPayload {
    r#type: &'static str,
    kind: Option<ContentFileKind>,
    selection: Option<ContentSelection>,
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
    kind_name: &'static str,
    kind: Option<ContentFileKind>,
    selection: Option<ContentSelection>,
    position: Option<PhysicalPosition<f64>>,
    error: Option<String>,
) {
    let _ = window.emit(
        DRAG_EVENT,
        DragPayload {
            r#type: kind_name,
            kind,
            selection,
            position: position.map(|position| DragPosition {
                x: position.x,
                y: position.y,
            }),
            error,
        },
    );
}

#[tauri::command]
pub async fn pick_content_files(
    app: AppHandle,
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
    content: State<'_, NativeContentFiles>,
    kind: ContentFileKind,
) -> Result<Option<ContentSelection>, String> {
    crate::window::require_main_window(&window, &bootstrap)?;
    let content = content.inner().clone();
    let permit = content
        .picker
        .clone()
        .try_lock_owned()
        .map_err(|_| "A file picker is already open.")?;
    let tasks = content.tasks.clone();
    tasks
        .try_spawn(permit, move |_cancel| async move {
            content.pick(app, kind).await
        })
        .map_err(|_| CLOSED.to_string())?
        .join()
        .await
        .map_err(|_| "File selection stopped before it could finish.".to_string())?
}

/// Raw bytes, so a large jar is not inflated into a JSON number array.
#[tauri::command]
pub fn take_content_file(
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
    content: State<'_, NativeContentFiles>,
    token: String,
    index: usize,
) -> Result<tauri::ipc::Response, String> {
    crate::window::require_main_window(&window, &bootstrap)?;
    if token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("The selected file is no longer available. Choose it again.".into());
    }
    content.take(&token, index).map(tauri::ipc::Response::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_classify_jars_together_and_one_modpack_alone() {
        let paths = |names: &[&str]| {
            names
                .iter()
                .map(|name| PathBuf::from(format!("/x/{name}")))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ContentFileKind::of(&paths(&["a.jar", "B.JAR"])),
            Some(ContentFileKind::Mod)
        );
        assert_eq!(
            ContentFileKind::of(&paths(&["pack.mrpack"])),
            Some(ContentFileKind::Modpack)
        );
        for refused in [
            paths(&[]),
            paths(&["a.jar", "pack.mrpack"]),
            paths(&["one.mrpack", "two.mrpack"]),
            paths(&["skin.png"]),
            paths(&["a.jar.disabled"]),
        ] {
            assert_eq!(ContentFileKind::of(&refused), None, "{refused:?}");
        }
        let many = (0..=MAX_FILES)
            .map(|index| format!("m{index}.jar"))
            .collect::<Vec<_>>();
        let many = many.iter().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(ContentFileKind::of(&paths(&many)), None);
    }
}

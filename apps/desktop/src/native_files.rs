//! OS file selection shared by native drop zones and pickers. Each selected
//! file is captured synchronously through exact filesystem authority, before
//! an accepted read can race a rename, and the read outlives its waiter.
use axial_app::tasks::{CancellationToken, TaskOwner};
use serde::Serialize;
use std::{
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex, MutexGuard},
};
use tauri::{DragDropEvent, Emitter, PhysicalPosition, Window};
use tokio::sync::Mutex;

/// What one frontend surface accepts from native selections.
pub trait FileTarget: Clone + Send + Sync + 'static {
    const EVENT: &'static str;
    const CLOSED: &'static str;
    const PICKER_BUSY: &'static str;
    const PICKER_STOPPED: &'static str;
    /// What a set of paths would be accepted as.
    type Kind: Copy + Send + 'static;
    type Captured: Send + 'static;
    type Selection: Send + 'static;
    type Fields: Serialize + Clone;

    fn files(&self) -> &Files<Self::Kind>;
    /// `Err(None)` ignores the paths; `Err(Some(_))` refuses a drop with a message.
    fn classify(&self, paths: &[PathBuf]) -> Result<Self::Kind, Option<String>>;
    fn capture(&self, kind: Self::Kind, paths: &[PathBuf]) -> Result<Self::Captured, String>;
    fn read(
        &self,
        captured: Self::Captured,
    ) -> impl Future<Output = Result<Self::Selection, String>> + Send;
    /// The payload fields besides `type`, `position` and `error`.
    fn fields(
        phase: Phase,
        kind: Option<Self::Kind>,
        selection: Option<Self::Selection>,
    ) -> Self::Fields;
}

/// The shared state of one target: its task owner, the single native dialog
/// lease and the kind accepted by the drag in progress.
pub struct Files<K> {
    pub tasks: TaskOwner,
    pub picker: Arc<Mutex<()>>,
    hovered: Arc<StdMutex<Option<K>>>,
}

impl<K> Clone for Files<K> {
    fn clone(&self) -> Self {
        Self {
            tasks: self.tasks.clone(),
            picker: self.picker.clone(),
            hovered: self.hovered.clone(),
        }
    }
}

impl<K> Files<K> {
    pub fn new(tasks: TaskOwner) -> Self {
        Self {
            tasks,
            picker: Arc::new(Mutex::new(())),
            hovered: Arc::new(StdMutex::new(None)),
        }
    }

    fn hovered(&self) -> MutexGuard<'_, Option<K>> {
        self.hovered
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn closing(&self) -> bool {
        self.tasks.status().closing
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Enter,
    Over,
    Leave,
    Drop,
}

#[derive(Clone, Serialize)]
struct Payload<F> {
    r#type: Phase,
    #[serde(flatten)]
    fields: F,
    position: Option<Position>,
    error: Option<String>,
}

#[derive(Clone, Serialize)]
struct Position {
    x: f64,
    y: f64,
}

fn payload<T: FileTarget>(
    phase: Phase,
    kind: Option<T::Kind>,
    selection: Option<T::Selection>,
    position: Option<PhysicalPosition<f64>>,
    error: Option<String>,
) -> Payload<T::Fields> {
    Payload {
        r#type: phase,
        fields: T::fields(phase, kind, selection),
        position: position.map(|position| Position {
            x: position.x,
            y: position.y,
        }),
        error,
    }
}

fn emit<T: FileTarget>(window: &Window, payload: Payload<T::Fields>) {
    let _ = window.emit(T::EVENT, payload);
}

pub fn handle_drag<T: FileTarget>(target: &T, window: &Window, event: &DragDropEvent) {
    let files = target.files();
    match event {
        DragDropEvent::Enter { paths, position } => {
            let kind = target.classify(paths).ok().filter(|_| !files.closing());
            *files.hovered() = kind;
            emit::<T>(
                window,
                payload::<T>(Phase::Enter, kind, None, Some(*position), None),
            );
        }
        DragDropEvent::Over { position } => {
            let kind = files.hovered().filter(|_| !files.closing());
            emit::<T>(
                window,
                payload::<T>(Phase::Over, kind, None, Some(*position), None),
            );
        }
        DragDropEvent::Leave => {
            // Leaving the drop zone does not revoke an already issued selection.
            files.hovered().take();
            emit::<T>(window, payload::<T>(Phase::Leave, None, None, None, None));
        }
        DragDropEvent::Drop { paths, position } => {
            files.hovered().take();
            let position = Some(*position);
            let dropped = |kind, selection, error| {
                payload::<T>(Phase::Drop, kind, selection, position, error)
            };
            let kind = match target.classify(paths) {
                Ok(kind) => kind,
                Err(error) => return emit::<T>(window, dropped(None, None, error)),
            };
            let captured = match target.capture(kind, paths) {
                Ok(captured) => captured,
                Err(error) => return emit::<T>(window, dropped(Some(kind), None, Some(error))),
            };
            let owner = target.clone();
            let reader = window.clone();
            let accepted = files.tasks.try_spawn((), move |cancel| async move {
                let result = if cancel.is_cancelled() {
                    Err(T::CLOSED.to_string())
                } else {
                    owner.read(captured).await
                };
                let payload = match result {
                    Ok(selection) => {
                        payload::<T>(Phase::Drop, Some(kind), Some(selection), position, None)
                    }
                    Err(error) => {
                        payload::<T>(Phase::Drop, Some(kind), None, position, Some(error))
                    }
                };
                emit::<T>(&reader, payload);
            });
            if accepted.is_err() {
                emit::<T>(window, dropped(Some(kind), None, Some(T::CLOSED.into())));
            }
        }
        _ => {}
    }
}

/// One native dialog at a time. The accepted task holds the lease through the
/// dialog and its read, so a dropped command waiter cancels neither.
pub async fn pick<T, R, F>(
    target: &T,
    work: impl FnOnce(CancellationToken) -> F + Send + 'static,
) -> Result<R, String>
where
    T: FileTarget,
    R: Send + 'static,
    F: Future<Output = Result<R, String>> + Send + 'static,
{
    let files = target.files();
    let lease = files
        .picker
        .clone()
        .try_lock_owned()
        .map_err(|_| T::PICKER_BUSY)?;
    files
        .tasks
        .try_spawn(lease, work)
        .map_err(|_| T::CLOSED.to_string())?
        .join()
        .await
        .map_err(|_| T::PICKER_STOPPED.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        native_content::{ContentFileKind, NativeContentFiles},
        native_skin::NativeSkinFiles,
    };

    #[test]
    fn drag_payloads_match_the_frontend_contracts() {
        let position = Some(PhysicalPosition::new(20.0, 40.0));
        let skin =
            payload::<NativeSkinFiles>(Phase::Drop, Some(()), Some("token".into()), position, None);
        assert_eq!(
            serde_json::to_string(&skin).unwrap(),
            r#"{"type":"drop","eligible":true,"token":"token","position":{"x":20.0,"y":40.0},"error":null}"#
        );
        let refused =
            payload::<NativeSkinFiles>(Phase::Drop, Some(()), None, position, Some("No".into()));
        assert_eq!(serde_json::to_value(&refused).unwrap()["eligible"], false);
        let hovered = payload::<NativeSkinFiles>(Phase::Over, Some(()), None, position, None);
        assert_eq!(serde_json::to_value(&hovered).unwrap()["eligible"], true);
        let content = payload::<NativeContentFiles>(
            Phase::Leave,
            Some(ContentFileKind::Modpack),
            None,
            None,
            None,
        );
        assert_eq!(
            serde_json::to_string(&content).unwrap(),
            r#"{"type":"leave","kind":"modpack","selection":null,"position":null,"error":null}"#
        );
    }
}

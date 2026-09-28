//! Native selection is the only path-to-authority boundary for predecessor data.
use crate::bootstrap::DesktopBootstrap;
use axial_app::{
    catalog::Catalog,
    import::{
        CaptureLimits, ImportBlocker, ImportError, ImportPreview, ImportPreviews, Inventory,
        ReadOnlySource,
    },
    library::LibraryLifecycle,
    performance::rules::PerformanceRules,
    tasks::{CancellationToken, TaskOwner},
};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tauri::{AppHandle, State, WebviewWindow};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::{Mutex as AsyncMutex, oneshot};

const CLOSED: &str = "Profile selection is unavailable while Axial is closing.";
const CHANGED: &str = "The predecessor profile changed. Choose it again for a new preview.";

#[derive(Clone)]
pub struct NativeImports {
    library: LibraryLifecycle,
    tasks: TaskOwner,
    previews: Arc<ImportPreviews>,
    catalog: Arc<Catalog>,
    picker: Arc<AsyncMutex<()>>,
    state: Arc<Mutex<SelectionState>>,
    #[cfg(test)]
    before_validation: Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>,
}

#[derive(Default)]
struct SelectionState {
    rules: Option<PerformanceRules>,
    revision: u64,
    closed: bool,
    pending: Option<CancellationToken>,
    selected: Option<SelectedSources>,
}

#[derive(Clone)]
struct SelectedSources {
    profile: ReadOnlySource,
    external: BTreeMap<String, ReadOnlySource>,
    preview: ImportPreview,
}

struct Selection {
    revision: u64,
    cancel: CancellationToken,
    previous: Option<(SelectedSources, String)>,
}

impl NativeImports {
    pub fn new(
        library: LibraryLifecycle,
        tasks: TaskOwner,
        previews: Arc<ImportPreviews>,
        catalog: Arc<Catalog>,
        rules: PerformanceRules,
    ) -> Self {
        Self {
            library,
            tasks,
            previews,
            catalog,
            picker: Arc::new(AsyncMutex::new(())),
            state: Arc::new(Mutex::new(SelectionState {
                rules: Some(rules),
                ..SelectionState::default()
            })),
            #[cfg(test)]
            before_validation: Arc::new(Mutex::new(None)),
        }
    }

    /// Forgetting a preview never revokes an already accepted instance copy.
    pub fn forget(&self) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|_| CLOSED.to_owned())?;
        if let Some(cancel) = state.pending.take() {
            cancel.cancel();
        }
        state.selected = None;
        self.previews.forget().map_err(import_message)
    }

    /// Called before root-pin drain on shutdown/reset. Accepted copies retain
    /// their own inventory; selection and preview pins are released here.
    pub fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closed = true;
        if let Some(cancel) = state.pending.take() {
            cancel.cancel();
        }
        state.selected = None;
        let _ = self.previews.forget();
    }

    /// A managed native facade may outlive the event loop. Its rules retain
    /// SQLite, so release them only after every accepted preview has joined.
    pub fn release_after_shutdown(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.closed || self.tasks.shutdown_receipt().is_none() {
            return Err(CLOSED.into());
        }
        state.rules.take();
        Ok(())
    }

    fn begin(&self, external: Option<(&str, &str)>) -> Result<Selection, String> {
        let mut state = self.state.lock().map_err(|_| CLOSED.to_owned())?;
        if state.closed {
            return Err(CLOSED.into());
        }
        let previous = external
            .map(|(fingerprint, id)| {
                let selected = state.selected.as_ref().ok_or(CHANGED)?;
                if selected.preview.fingerprint != fingerprint
                    || !selected.preview.instances.iter().any(|row| {
                        row.legacy_id == id
                            && row.blockers.contains(&ImportBlocker::MissingInstanceSource)
                    })
                {
                    return Err(CHANGED);
                }
                Ok((selected.clone(), id.to_owned()))
            })
            .transpose()
            .map_err(str::to_owned)?;
        state.revision = state.revision.checked_add(1).ok_or(CLOSED)?;
        let cancel = CancellationToken::new();
        state.pending = Some(cancel.clone());
        Ok(Selection {
            revision: state.revision,
            cancel,
            previous,
        })
    }

    fn capture_path(&self, path: &Path) -> Result<ReadOnlySource, String> {
        let root = self
            .library
            .admit_application_root()
            .map_err(|_| CLOSED.to_owned())?;
        ReadOnlySource::from_native_selection(root, path).map_err(import_message)
    }

    fn validate_previous(&self, selection: &Selection) -> Result<(), String> {
        let Some((previous, _)) = &selection.previous else {
            return Ok(());
        };
        #[cfg(test)]
        {
            let hook = self.before_validation.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let current = self.previews.current().map_err(import_message)?;
        if current.fingerprint != previous.preview.fingerprint {
            return Err(CHANGED.into());
        }
        Ok(())
    }

    fn publish(
        &self,
        selection: &Selection,
        source: ReadOnlySource,
        inventory: Inventory,
        shutdown: &CancellationToken,
    ) -> Result<Option<ImportPreview>, String> {
        let mut state = self.state.lock().map_err(|_| CLOSED.to_owned())?;
        if state.closed
            || state.revision != selection.revision
            || selection.cancel.is_cancelled()
            || shutdown.is_cancelled()
        {
            return Ok(None);
        }
        let (profile, external) = match &selection.previous {
            Some((previous, id)) => {
                let mut external = previous.external.clone();
                external.insert(id.clone(), source.clone());
                (previous.profile.clone(), external)
            }
            None => (source, BTreeMap::new()),
        };
        let preview = self.previews.admit(inventory).map_err(import_message)?;
        state.selected = Some(SelectedSources {
            profile,
            external,
            preview: preview.clone(),
        });
        state.pending = None;
        Ok(Some(preview))
    }

    async fn capture(
        &self,
        selection: Selection,
        source: ReadOnlySource,
        shutdown: CancellationToken,
    ) -> Result<Option<ImportPreview>, String> {
        let rules = self
            .state
            .lock()
            .map_err(|_| CLOSED.to_owned())?
            .rules
            .clone()
            .ok_or_else(|| CLOSED.to_owned())?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let owner = self.clone();
        let local_cancel = selection.cancel.clone();
        let worker_shutdown = shutdown.clone();
        let work = async move {
            let captured = tokio::task::spawn_blocking(move || {
                if selection.cancel.is_cancelled() || worker_shutdown.is_cancelled() {
                    return Ok(None);
                }
                owner.validate_previous(&selection)?;
                let (profile, external) = match &selection.previous {
                    Some((previous, id)) => {
                        let mut external = previous.external.clone();
                        external.insert(id.clone(), source.clone());
                        (previous.profile.clone(), external)
                    }
                    None => (source.clone(), BTreeMap::new()),
                };
                let inventory = Inventory::capture_controlled(
                    &profile,
                    &external,
                    CaptureLimits::default(),
                    worker_cancelled,
                )
                .map_err(import_message)?;
                Ok::<_, String>(Some((owner, selection, source, inventory, worker_shutdown)))
            })
            .await
            .map_err(|_| "Could not finish the profile preview.".to_owned())??;
            let Some((owner, selection, source, inventory, worker_shutdown)) = captured else {
                return Ok(None);
            };
            let inventory = inventory
                .resolve_versions(&owner.catalog, &selection.cancel)
                .await
                .map_err(import_message)?;
            tokio::task::spawn_blocking(move || {
                if selection.cancel.is_cancelled() || worker_shutdown.is_cancelled() {
                    return Ok(None);
                }
                // Resolution rechecks source evidence; recheck the previous selection too.
                owner.validate_previous(&selection)?;
                let inventory = inventory
                    .resolve_rules_preview(&rules)
                    .map_err(import_message)?;
                owner.publish(&selection, source, inventory, &worker_shutdown)
            })
            .await
            .map_err(|_| "Could not finish the profile preview.".to_owned())?
        };
        tokio::pin!(work);
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => { cancelled.store(true, Ordering::Release); local_cancel.cancel(); let _ = work.await; Ok(None) }
            _ = local_cancel.cancelled() => { cancelled.store(true, Ordering::Release); let _ = work.await; Ok(None) }
            result = &mut work => result,
        }
    }

    async fn pick(
        &self,
        app: AppHandle,
        external: Option<(String, String)>,
        shutdown: CancellationToken,
    ) -> Result<Option<ImportPreview>, String> {
        let selection = self.begin(
            external
                .as_ref()
                .map(|(fingerprint, id)| (fingerprint.as_str(), id.as_str())),
        )?;
        let (sender, receiver) = oneshot::channel();
        let owner = self.clone();
        let callback_cancel = selection.cancel.clone();
        let callback_shutdown = shutdown.clone();
        app.dialog()
            .file()
            .set_title(if external.is_some() {
                "Choose this predecessor instance folder"
            } else {
                "Choose a predecessor Axial profile"
            })
            .pick_folder(move |selected| {
                if sender.is_closed()
                    || callback_cancel.is_cancelled()
                    || callback_shutdown.is_cancelled()
                {
                    return;
                }
                let selected = selected
                    .map(|file| {
                        let path = file.into_path().map_err(|_| {
                            "The folder picker returned an invalid selection.".to_owned()
                        })?;
                        // Capture the OS-selected capability before dispatching work.
                        owner.capture_path(&path)
                    })
                    .transpose();
                let _ = sender.send(selected);
            });
        let selected = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(None),
            _ = selection.cancel.cancelled() => return Ok(None),
            selected = receiver => selected.map_err(|_| "The folder picker stopped before returning a selection.".to_owned())??,
        };
        let Some(source) = selected else {
            return Ok(None);
        };
        self.capture(selection, source, shutdown).await
    }
}

async fn pick(
    imports: NativeImports,
    app: AppHandle,
    external: Option<(String, String)>,
) -> Result<Option<ImportPreview>, String> {
    let permit = imports
        .picker
        .clone()
        .try_lock_owned()
        .map_err(|_| "A profile folder is already being selected or checked.".to_owned())?;
    let tasks = imports.tasks.clone();
    tasks
        .try_spawn(permit, move |cancel| async move {
            imports.pick(app, external, cancel).await
        })
        .map_err(|_| CLOSED.to_owned())?
        .join()
        .await
        .map_err(|_| "Profile selection stopped before it could finish.".to_owned())?
}

#[tauri::command]
pub async fn pick_import_profile(
    app: AppHandle,
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
    imports: State<'_, NativeImports>,
) -> Result<Option<ImportPreview>, String> {
    crate::window::require_main_window(&window, &bootstrap)?;
    pick(imports.inner().clone(), app, None).await
}

#[tauri::command]
pub async fn pick_import_instance_source(
    app: AppHandle,
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
    imports: State<'_, NativeImports>,
    fingerprint: String,
    legacy_id: String,
) -> Result<Option<ImportPreview>, String> {
    crate::window::require_main_window(&window, &bootstrap)?;
    pick(imports.inner().clone(), app, Some((fingerprint, legacy_id))).await
}

#[tauri::command]
pub fn forget_import_profile(
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
    imports: State<'_, NativeImports>,
) -> Result<(), String> {
    crate::window::require_main_window(&window, &bootstrap)?;
    imports.forget()
}

fn import_message(error: ImportError) -> String {
    match error {
        ImportError::SourceChanged => CHANGED,
        ImportError::LimitExceeded => {
            "This predecessor profile exceeds the supported import limits."
        }
        ImportError::Cancelled => "Profile preview was cancelled.",
        ImportError::InvalidData => {
            "This folder is not a supported predecessor profile or contains unsupported data."
        }
        ImportError::Unavailable => CLOSED,
        ImportError::NoSource => "Choose a predecessor profile first.",
        ImportError::Io(_) => "The selected predecessor folder could not be read safely.",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    const INSTANCE: &str = "0000000000000001";

    struct Fixture {
        _root: tempfile::TempDir,
        baseline: PathBuf,
        imports: NativeImports,
        metadata: Arc<axial_app::storage::MetadataStore>,
    }

    impl Fixture {
        fn new() -> Self {
            let root =
                tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
            let baseline = root.path().join("old-profile");
            let replacement = root.path().join("replacement");
            fs::create_dir(&baseline).unwrap();
            fs::create_dir(&replacement).unwrap();
            copy_tree(
                &Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../acceptance/fixtures/profiles/offline-vanilla"),
                &baseline,
            );
            let library = match LibraryLifecycle::open(&replacement) {
                axial_app::library::LibraryOpenOutcome::Ready(library) => library,
                _ => panic!("independent replacement root must open"),
            };
            let metadata = Arc::new(
                axial_app::storage::MetadataStore::open(replacement.join("metadata.sqlite"))
                    .unwrap(),
            );
            metadata
                .migrate(&[
                    axial_app::performance::rules::MIGRATION,
                    axial_app::performance::rules::IMPORT_MIGRATION,
                ])
                .unwrap();
            let rules = PerformanceRules::with_remote(metadata.clone(), None, None).unwrap();
            Self {
                _root: root,
                baseline,
                metadata,
                imports: NativeImports::new(
                    library,
                    TaskOwner::new(4).unwrap(),
                    Arc::new(ImportPreviews::new()),
                    Arc::new(Catalog::new(
                        axial_app::network::ProviderClient::new(
                            axial_app::network::ClientConfig::default(),
                        )
                        .unwrap(),
                    )),
                    rules,
                ),
            }
        }

        async fn select(&self) -> ImportPreview {
            let selection = self.imports.begin(None).unwrap();
            let source = self.imports.capture_path(&self.baseline).unwrap();
            self.imports
                .capture(selection, source, CancellationToken::new())
                .await
                .unwrap()
                .unwrap()
        }

        fn finish(&self) {
            self.imports.close();
            self.imports.library.try_preserve().unwrap();
        }
    }

    fn copy_tree(source: &Path, destination: &Path) {
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let target = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                fs::create_dir(&target).unwrap();
                copy_tree(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    fn canary(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u64, u64)> {
        fn visit(root: &Path, path: &Path, output: &mut BTreeMap<PathBuf, (Vec<u8>, u64, u64)>) {
            let metadata = fs::symlink_metadata(path).unwrap();
            #[cfg(unix)]
            let identity = {
                use std::os::unix::fs::MetadataExt;
                (metadata.ino(), metadata.nlink())
            };
            #[cfg(not(unix))]
            let identity = (0, 0);
            let bytes = if metadata.is_file() {
                fs::read(path).unwrap()
            } else {
                vec![]
            };
            output.insert(
                path.strip_prefix(root).unwrap().to_owned(),
                (bytes, identity.0, identity.1),
            );
            if metadata.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    visit(root, &entry.unwrap().path(), output);
                }
            }
        }
        let mut result = BTreeMap::new();
        visit(root, root, &mut result);
        result
    }

    #[tokio::test]
    async fn managed_import_facades_release_sqlite_only_after_closed_work_joins() {
        let fixture = Fixture::new();
        let metadata = Arc::downgrade(&fixture.metadata);
        let retained_facade = fixture.imports.clone();
        drop(fixture.metadata);
        assert!(fixture.imports.release_after_shutdown().is_err());
        let (release, wait) = oneshot::channel();
        let work = fixture
            .imports
            .tasks
            .try_spawn((), |_| async move {
                let _ = wait.await;
            })
            .unwrap();
        fixture.imports.close();
        assert!(
            fixture
                .imports
                .tasks
                .shutdown(std::time::Duration::from_millis(10))
                .await
                .is_err()
        );
        assert!(fixture.imports.release_after_shutdown().is_err());
        assert!(metadata.upgrade().is_some());
        release.send(()).unwrap();
        work.join().await.unwrap();
        fixture
            .imports
            .tasks
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .unwrap();
        fixture.imports.release_after_shutdown().unwrap();
        assert!(metadata.upgrade().is_none());
        assert!(retained_facade.begin(None).is_err());
        retained_facade.release_after_shutdown().unwrap();
        fixture.imports.library.try_preserve().unwrap();
    }

    #[tokio::test]
    async fn native_selection_admits_only_read_only_preview_and_preserves_source_canaries() {
        let fixture = Fixture::new();
        let before = canary(&fixture.baseline);
        let preview = fixture.select().await;
        assert!(preview.instances[0].ordinary_import_available);
        assert!(!preview.cutover_available);
        assert_eq!(fixture.imports.previews.current().unwrap(), preview);
        assert!(
            !serde_json::to_string(&preview)
                .unwrap()
                .contains(fixture.baseline.to_str().unwrap())
        );
        assert_eq!(canary(&fixture.baseline), before);
        fixture.finish();
        assert_eq!(canary(&fixture.baseline), before);
    }

    #[tokio::test]
    async fn rules_preview_failure_preserves_the_previous_native_selection() {
        let fixture = Fixture::new();
        let preview = fixture.select().await;
        let before = canary(&fixture.baseline);
        fixture
            .metadata
            .transaction(|db| {
                db.execute_batch("DROP TABLE performance_rules_imports")?;
                Ok::<_, axial_app::storage::StorageError>(())
            })
            .unwrap();
        let selection = fixture.imports.begin(None).unwrap();
        let source = fixture.imports.capture_path(&fixture.baseline).unwrap();
        assert!(
            fixture
                .imports
                .capture(selection, source, CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(fixture.imports.previews.current().unwrap(), preview);
        assert_eq!(
            fixture
                .imports
                .state
                .lock()
                .unwrap()
                .selected
                .as_ref()
                .unwrap()
                .preview,
            preview
        );
        assert_eq!(canary(&fixture.baseline), before);
        fixture.finish();
    }

    #[tokio::test]
    async fn preview_actionability_uses_conversion_not_just_known_loader_or_missing_files() {
        let fixture = Fixture::new();
        let registry_path = fixture.baseline.join("instances.json");
        let mut registry: serde_json::Value =
            serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
        registry["instances"][0]["loader_key"] = serde_json::json!("fabric");
        fs::write(registry_path, serde_json::to_vec(&registry).unwrap()).unwrap();
        let before = canary(&fixture.baseline);
        let preview = fixture.select().await;
        assert!(!preview.instances[0].ordinary_import_available);
        assert!(
            !preview.instances[0]
                .blockers
                .contains(&ImportBlocker::UnsupportedLoader)
        );
        assert!(
            fixture
                .imports
                .previews
                .prepare_instance(&preview.fingerprint, INSTANCE)
                .is_err()
        );
        assert_eq!(canary(&fixture.baseline), before);
        fixture.finish();
    }

    #[tokio::test]
    async fn forget_releases_preview_but_prepared_copy_retains_its_exact_root_lifetime() {
        let fixture = Fixture::new();
        let preview = fixture.select().await;
        let prepared = fixture
            .imports
            .previews
            .prepare_instance(&preview.fingerprint, INSTANCE)
            .unwrap();
        fixture.imports.forget().unwrap();
        assert!(matches!(
            fixture.imports.previews.current(),
            Err(ImportError::NoSource)
        ));
        assert!(
            fixture
                .imports
                .library
                .wait_for_pins(std::time::Duration::from_millis(10))
                .await
                .is_err()
        );
        drop(prepared);
        fixture
            .imports
            .library
            .wait_for_pins(std::time::Duration::from_secs(1))
            .await
            .unwrap();
        fixture.finish();
    }

    #[test]
    fn forget_and_close_refuse_late_capture_publication() {
        for closing in [false, true] {
            let fixture = Fixture::new();
            let selection = fixture.imports.begin(None).unwrap();
            let source = fixture.imports.capture_path(&fixture.baseline).unwrap();
            let inventory = Inventory::capture(&source, &BTreeMap::new()).unwrap();
            if closing {
                fixture.imports.close();
            } else {
                fixture.imports.forget().unwrap();
            }
            assert!(
                fixture
                    .imports
                    .publish(&selection, source, inventory, &CancellationToken::new())
                    .unwrap()
                    .is_none()
            );
            assert!(matches!(
                fixture.imports.previews.current(),
                Err(ImportError::NoSource)
            ));
            if closing {
                assert!(fixture.imports.begin(None).is_err());
            }
            fixture.finish();
        }
    }

    #[tokio::test]
    async fn forget_and_close_do_not_wait_for_background_source_validation() {
        for closing in [false, true] {
            let fixture = Fixture::new();
            let external = fixture._root.path().join("external-instance");
            fs::rename(fixture.baseline.join("instances").join(INSTANCE), &external).unwrap();
            let preview = fixture.select().await;
            let before = (canary(&fixture.baseline), canary(&external));
            let selection = fixture
                .imports
                .begin(Some((&preview.fingerprint, INSTANCE)))
                .unwrap();
            let source = fixture.imports.capture_path(&external).unwrap();
            let (entered, parked) = oneshot::channel();
            let (release, resume) = std::sync::mpsc::channel();
            *fixture.imports.before_validation.lock().unwrap() = Some(Box::new(move || {
                let _ = entered.send(());
                let _ = resume.recv();
            }));
            let owner = fixture.imports.clone();
            let capture = tokio::spawn(async move {
                owner
                    .capture(selection, source, CancellationToken::new())
                    .await
            });
            tokio::time::timeout(std::time::Duration::from_secs(2), parked)
                .await
                .unwrap()
                .unwrap();
            let owner = fixture.imports.clone();
            let mut forgetting = tokio::task::spawn_blocking(move || {
                if closing {
                    owner.close();
                } else {
                    owner.forget().unwrap();
                }
            });
            let responsive =
                tokio::time::timeout(std::time::Duration::from_secs(2), &mut forgetting).await;
            // Always release the worker before asserting, including a regression
            // where forgetting incorrectly waits on its filesystem work.
            release.send(()).unwrap();
            let finished_without_validation = responsive.is_ok();
            match responsive {
                Ok(result) => result.unwrap(),
                Err(_) => forgetting.await.unwrap(),
            }
            assert!(capture.await.unwrap().unwrap().is_none());
            assert!(
                finished_without_validation,
                "forget/close must not wait for source validation"
            );
            assert!(matches!(
                fixture.imports.previews.current(),
                Err(ImportError::NoSource)
            ));
            assert_eq!((canary(&fixture.baseline), canary(&external)), before);
            fixture.finish();
        }
    }

    #[tokio::test]
    async fn cancelled_capture_keeps_previous_preview_and_source_unchanged() {
        let fixture = Fixture::new();
        let preview = fixture.select().await;
        let before = canary(&fixture.baseline);
        let selection = fixture.imports.begin(None).unwrap();
        let source = fixture.imports.capture_path(&fixture.baseline).unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            fixture
                .imports
                .capture(selection, source, cancel)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(fixture.imports.previews.current().unwrap(), preview);
        assert_eq!(canary(&fixture.baseline), before);
        fixture.finish();
    }

    #[tokio::test]
    async fn missing_external_payload_requires_separate_selection_bound_to_current_preview() {
        let fixture = Fixture::new();
        let external = fixture._root.path().join("external-instance");
        fs::rename(fixture.baseline.join("instances").join(INSTANCE), &external).unwrap();
        let before = (canary(&fixture.baseline), canary(&external));
        let preview = fixture.select().await;
        assert!(!preview.instances[0].ordinary_import_available);
        assert!(fixture.imports.begin(Some(("stale", INSTANCE))).is_err());
        assert!(
            fixture
                .imports
                .begin(Some((&preview.fingerprint, "../../outside")))
                .is_err()
        );
        let selection = fixture
            .imports
            .begin(Some((&preview.fingerprint, INSTANCE)))
            .unwrap();
        let source = fixture.imports.capture_path(&external).unwrap();
        let next = fixture
            .imports
            .capture(selection, source, CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert!(next.instances[0].ordinary_import_available);
        assert!(!next.cutover_available);
        assert!(
            fixture
                .imports
                .begin(Some((&next.fingerprint, INSTANCE)))
                .is_err()
        );
        assert_eq!((canary(&fixture.baseline), canary(&external)), before);
        fixture.finish();
    }

    #[tokio::test]
    async fn external_selection_cannot_silently_accept_changed_profile() {
        let fixture = Fixture::new();
        let external = fixture._root.path().join("external-instance");
        fs::rename(fixture.baseline.join("instances").join(INSTANCE), &external).unwrap();
        let preview = fixture.select().await;
        let selection = fixture
            .imports
            .begin(Some((&preview.fingerprint, INSTANCE)))
            .unwrap();
        let source = fixture.imports.capture_path(&external).unwrap();
        fs::write(fixture.baseline.join("new-retained-record.json"), b"{}").unwrap();
        let before = canary(&fixture.baseline);
        assert_eq!(
            fixture
                .imports
                .capture(selection, source, CancellationToken::new())
                .await
                .unwrap_err(),
            CHANGED
        );
        assert_eq!(canary(&fixture.baseline), before);
        fixture.finish();
    }

    #[test]
    fn native_errors_never_expose_selected_paths() {
        assert_eq!(
            import_message(ImportError::Io(std::io::Error::other(
                "/private/profile/secret"
            ))),
            "The selected predecessor folder could not be read safely."
        );
    }
}

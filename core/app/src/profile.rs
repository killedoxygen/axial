//! The isolated replacement profile: path admission, its identity marker and
//! the recovery obligations that refuse destructive reset or update.
use crate::{
    content::install::ContentMutations,
    install::queue::InstallQueue,
    instances::create::InstanceService,
    library::{ApplicationRootPin, LibraryLifecycle},
    performance::PerformanceService,
    public::DEVELOPMENT_APPLICATION_ID,
    resources::ResourceService,
    tasks::TaskOwner,
};
use axial_fs::{
    Directory, FileCreateOutcome, FileCreateResolution, LeafName, Outcome, Resolution, RootSession,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;

pub const MARKER: &str = ".axial-rewrite-profile";
const MARKER_LIMIT: u64 = 4096;
const INVALID: &str = "Use an absolute, isolated replacement profile directory.";
const UNKNOWN: &str = "The selected directory is not an admitted replacement profile. Existing files have been preserved.";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    application_id: String,
    profile_id: Uuid,
}

fn identity(marker: &[u8]) -> Option<Uuid> {
    let identity: Identity = serde_json::from_slice(marker).ok()?;
    (identity.application_id == DEVELOPMENT_APPLICATION_ID && !identity.profile_id.is_nil())
        .then_some(identity.profile_id)
}

pub fn marker_name() -> LeafName {
    LeafName::new(MARKER).expect("fixed profile marker")
}

/// Refuses filesystem roots and any path inside an installed application's data.
pub fn independent(root: &Path) -> bool {
    root.is_absolute()
        && root.parent().is_some()
        && !root.components().any(|part| {
            matches!(part, Component::ParentDir | Component::CurDir)
                || matches!(part, Component::Normal(name)
                    if name == "dev.mateoltd.axial" || name == "com.mateoltd.axial")
        })
}

/// Read and validate the marker through the retained application root.
pub fn marker(pin: &ApplicationRootPin) -> io::Result<Vec<u8>> {
    pin.read_projection()?;
    read_marker(&pin.directory()?)
}

/// Read and validate the marker of an already admitted root session.
pub fn session_marker(session: &RootSession) -> io::Result<Vec<u8>> {
    read_marker(&session.root()?)
}

fn read_marker(root: &Directory) -> io::Result<Vec<u8>> {
    let bytes = root.open_file(&marker_name())?.read_bounded(MARKER_LIMIT)?;
    identity(&bytes)
        .map(|_| bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "profile marker is invalid"))
}

/// An admitted profile path whose identity is bound once its root is open.
pub struct Profile {
    root: PathBuf,
    id: Uuid,
    /// `None` for an absent or empty directory that becomes a new profile.
    marker: Option<Vec<u8>>,
}

impl Profile {
    /// Path admission only reads. A refusal leaves no filesystem effect, so an
    /// unrelated directory never receives a root lease or marker.
    pub fn admit(requested: &Path) -> Result<Self, &'static str> {
        if !requested.is_absolute()
            || requested
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            return Err(INVALID);
        }
        // Resolve the existing ancestor before creating anything. An alias into
        // an installed profile must be refused just like its direct spelling.
        let mut ancestor = requested;
        let mut missing = Vec::new();
        while !ancestor.try_exists().map_err(|_| UNKNOWN)? {
            missing.push(ancestor.file_name().ok_or(INVALID)?);
            ancestor = ancestor.parent().ok_or(INVALID)?;
        }
        let mut root = fs::canonicalize(ancestor).map_err(|_| UNKNOWN)?;
        root.extend(missing.iter().rev());
        if !independent(&root) || std::env::home_dir().is_some_and(|home| root == home) {
            return Err(INVALID);
        }
        let marker = if missing.is_empty() {
            existing_marker(&root)?
        } else {
            None
        };
        let id = match &marker {
            Some(bytes) => identity(bytes).ok_or(UNKNOWN)?,
            None => Uuid::new_v4(),
        };
        Ok(Self { root, id, marker })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Bind the admitted identity through the opened root: an existing marker
    /// must be unchanged and a new one is published without replacement.
    pub fn bind(&self, library: &LibraryLifecycle) -> Result<(), &'static str> {
        let pin = library.admit_application_root().map_err(|_| UNKNOWN)?;
        match &self.marker {
            Some(expected) => (marker(&pin).ok().as_ref() == Some(expected))
                .then_some(())
                .ok_or(UNKNOWN),
            None => {
                pin.read_projection().map_err(|_| UNKNOWN)?;
                let identity = Identity {
                    application_id: DEVELOPMENT_APPLICATION_ID.into(),
                    profile_id: self.id,
                };
                let bytes = serde_json::to_vec(&identity).map_err(|_| UNKNOWN)?;
                publish_marker(&pin.directory().map_err(|_| UNKNOWN)?, &bytes)
            }
        }
    }
}

fn existing_marker(root: &Path) -> Result<Option<Vec<u8>>, &'static str> {
    if !fs::symlink_metadata(root).map_err(|_| UNKNOWN)?.is_dir() {
        return Err(UNKNOWN);
    }
    let marker = root.join(MARKER);
    match fs::symlink_metadata(&marker) {
        Ok(metadata) if metadata.is_file() && metadata.len() <= MARKER_LIMIT => {
            let mut bytes = Vec::new();
            fs::File::open(&marker)
                .and_then(|file| file.take(MARKER_LIMIT + 1).read_to_end(&mut bytes))
                .map_err(|_| UNKNOWN)?;
            (bytes.len() as u64 <= MARKER_LIMIT)
                .then_some(Some(bytes))
                .ok_or(UNKNOWN)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match fs::read_dir(root).map_err(|_| UNKNOWN)?.next() {
                None => Ok(None),
                Some(_) => Err(UNKNOWN),
            }
        }
        _ => Err(UNKNOWN),
    }
}

/// Failed stages stay with the root's capability authority, which settles
/// them when the retained library is preserved.
fn publish_marker(root: &Directory, bytes: &[u8]) -> Result<(), &'static str> {
    let mut stage = match root.create_stage() {
        FileCreateOutcome::Created(stage) => stage,
        FileCreateOutcome::NoEffect(_) => return Err(UNKNOWN),
        FileCreateOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            FileCreateResolution::Created(stage) => stage,
            FileCreateResolution::NoEffect(_) | FileCreateResolution::Indeterminate(_) => {
                return Err(UNKNOWN);
            }
        },
    };
    stage.write_all(bytes).map_err(|_| UNKNOWN)?;
    let sealed = stage.seal().map_err(|_| UNKNOWN)?;
    match sealed.promote_no_replace(root, root, &marker_name()) {
        Outcome::Applied(_) => Ok(()),
        Outcome::NoEffect { .. } => Err(UNKNOWN),
        Outcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            Resolution::Applied(_) => Ok(()),
            Resolution::NoEffect(_) | Resolution::Indeterminate(_) => Err(UNKNOWN),
        },
    }
}

/// Unsettled effects and recovery intents that a reset would discard or an
/// update would strand. Each check reads its owner's current state.
#[derive(Clone)]
pub struct Recovery {
    pub library: LibraryLifecycle,
    pub tasks: TaskOwner,
    pub instances: Arc<InstanceService>,
    pub installs: Arc<InstallQueue>,
    pub performance: PerformanceService,
    pub content: Arc<ContentMutations>,
    pub resources: Arc<ResourceService>,
}

impl Recovery {
    /// Destructive shell actions check this before changing terminal admission.
    pub fn ensure_no_interrupted_launch(&self) -> Result<(), &'static str> {
        self.library.ensure_no_interrupted_launch().map_err(|_| {
            "A previous game process has not been proven settled. The local API remains available."
        })
    }

    pub fn ensure_reset_allowed(&self) -> Result<(), &'static str> {
        self.ensure_no_interrupted_launch()?;
        if self.instances.has_pending_intents() {
            return Err("Reset is blocked while instance changes still require recovery.");
        }
        if self.tasks.status().is_idle() && self.content.has_unsettled_effects() {
            return Err("Reset is blocked while content changes still require recovery.");
        }
        Ok(())
    }

    pub fn ensure_reset_settled(&self) -> Result<(), &'static str> {
        self.ensure_reset_allowed()?;
        if self.tasks.shutdown_receipt().is_none()
            || self.content.has_unsettled_effects()
            || self.installs.has_unsettled_effects()
        {
            return Err("Reset is blocked while file changes still require recovery.");
        }
        Ok(())
    }

    pub fn ensure_update_allowed(&self) -> Result<(), &'static str> {
        self.ensure_no_interrupted_launch()?;
        if self.instances.has_pending_intents()
            || self.instances.has_unsettled_effects()
            || self.performance.has_unsettled_effects()
            || !self
                .performance
                .pending_count()
                .is_ok_and(|count| count == 0)
            || self.content.has_unsettled_effects()
            || self.resources.has_unsettled_effects()
            || self.installs.has_unsettled_effects()
        {
            return Err("Update is blocked while file changes still require recovery.");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary() -> tempfile::TempDir {
        tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap()
    }

    fn open(profile: &Profile) -> LibraryLifecycle {
        let id = crate::library::LibraryId::parse(&profile.id().to_string()).unwrap();
        match LibraryLifecycle::open_with_id(profile.root(), id) {
            crate::library::LibraryOpenOutcome::Ready(library) => library,
            _ => panic!("fixture root acquisition failed"),
        }
    }

    #[test]
    fn identity_persists_and_unknown_or_installed_paths_are_refused_without_effects() {
        let temporary = temporary();
        let isolated = temporary.path().join("replacement");
        let first = Profile::admit(&isolated).unwrap();
        let library = open(&first);
        first.bind(&library).unwrap();
        library.try_preserve().unwrap();
        drop(library);
        let written = fs::read(isolated.join(MARKER)).unwrap();
        let second = Profile::admit(&isolated).unwrap();
        assert_eq!(second.id(), first.id());
        let library = open(&second);
        second.bind(&library).unwrap();
        assert_eq!(
            marker(&library.admit_application_root().unwrap()).unwrap(),
            written
        );
        library.try_preserve().unwrap();
        drop(library);

        let unknown = temporary.path().join("unknown");
        fs::create_dir(&unknown).unwrap();
        fs::write(unknown.join("keep.txt"), "user data").unwrap();
        assert!(Profile::admit(&unknown).is_err());
        assert_eq!(fs::read_dir(&unknown).unwrap().count(), 1);
        assert!(Profile::admit(Path::new("relative-profile")).is_err());
        let installed = temporary.path().join("dev.mateoltd.axial").join("rewrite");
        assert!(Profile::admit(&installed).is_err());
        assert!(!temporary.path().join("dev.mateoltd.axial").exists());
    }

    #[test]
    fn a_marker_changed_after_admission_is_not_bound() {
        let temporary = temporary();
        let root = temporary.path().join("replacement");
        let first = Profile::admit(&root).unwrap();
        let library = open(&first);
        first.bind(&library).unwrap();
        library.try_preserve().unwrap();
        drop(library);
        let admitted = Profile::admit(&root).unwrap();
        let replaced = format!(
            "{{\"application_id\":\"{DEVELOPMENT_APPLICATION_ID}\",\"profile_id\":\"{}\"}}",
            Uuid::new_v4()
        );
        fs::write(root.join(MARKER), replaced).unwrap();
        let library = open(&admitted);
        assert!(admitted.bind(&library).is_err());
        library.try_preserve().unwrap();
    }
}

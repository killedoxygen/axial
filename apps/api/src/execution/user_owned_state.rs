use super::anchored_record::AnchoredRecordDirectory;
use axial_performance::ManagedArtifactWitnessProof;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const MAX_ACTIVE_MOD_ENTRIES: usize = 1024;
const MAX_ACTIVE_MOD_FILE_BYTES: u64 = 512 << 20;
const MAX_ACTIVE_MOD_TOTAL_BYTES: u64 = 4 << 30;
pub(crate) struct UserModSetObservation {
    entries: Vec<UserModSetEntry>,
}

pub(crate) struct UserModSetEntry {
    digest: String,
    size: u64,
    modified_at_ns: u64,
}

impl UserModSetObservation {
    pub(crate) fn into_entries(self) -> Vec<UserModSetEntry> {
        self.entries
    }
}

impl UserModSetEntry {
    pub(crate) fn into_parts(self) -> (String, u64, u64) {
        (self.digest, self.size, self.modified_at_ns)
    }
}

pub(crate) async fn observe_active_user_mod_set(
    mods_dir: PathBuf,
    managed: Vec<ManagedArtifactWitnessProof>,
) -> Option<UserModSetObservation> {
    tokio::task::spawn_blocking(move || observe_blocking(&mods_dir, &managed))
        .await
        .ok()
        .flatten()
}

fn observe_blocking(
    mods_dir: &Path,
    managed: &[ManagedArtifactWitnessProof],
) -> Option<UserModSetObservation> {
    let directory = AnchoredRecordDirectory::open(mods_dir).ok()?;
    let before_epoch = directory.epoch().ok()?;
    let mut names = directory.names_bounded(MAX_ACTIVE_MOD_ENTRIES).ok()??;
    names.sort();

    let mut total_bytes = 0_u64;
    let mut witnessed = Vec::new();
    let mut held_observations = Vec::new();
    for name in names {
        let name_text = name.to_str()?;
        if !is_active_mod_name(name_text)? {
            continue;
        }
        let observation = directory.digest(&name, MAX_ACTIVE_MOD_FILE_BYTES).ok()?;
        let (content_sha256, content_sha512, size, modified_at_ns) = observation.parts();
        total_bytes = total_bytes.checked_add(size)?;
        if total_bytes > MAX_ACTIVE_MOD_TOTAL_BYTES {
            return None;
        }
        let content_sha512 = hex_lower(&content_sha512);
        let composition_managed = managed
            .iter()
            .any(|proof| proof.matches_observation(name_text, &content_sha512));
        if !composition_managed {
            witnessed.push(UserModSetEntry {
                digest: opaque_user_mod_digest(name_text, size, modified_at_ns, &content_sha256),
                size,
                modified_at_ns,
            });
        }
        held_observations.push(observation);
    }
    if directory.epoch().ok()? != before_epoch {
        return None;
    }
    if held_observations
        .iter()
        .any(|observation| observation.revalidate().is_err())
        || directory.epoch().ok()? != before_epoch
    {
        return None;
    }
    witnessed.sort_by(|left, right| {
        (&left.digest, left.size, left.modified_at_ns).cmp(&(
            &right.digest,
            right.size,
            right.modified_at_ns,
        ))
    });
    Some(UserModSetObservation { entries: witnessed })
}

fn is_active_mod_name(name: &str) -> Option<bool> {
    if name.starts_with('.') || !name.to_ascii_lowercase().ends_with(".jar") {
        return Some(false);
    }
    (name.len() <= 255 && !name.chars().any(char::is_control)).then_some(true)
}

fn opaque_user_mod_digest(
    name: &str,
    size: u64,
    modified_at_ns: u64,
    content_digest: &[u8; 32],
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"axial.user-mod-witness.v1\0");
    digest.update((name.len() as u64).to_be_bytes());
    digest.update(name.as_bytes());
    digest.update(size.to_be_bytes());
    digest.update(modified_at_ns.to_be_bytes());
    digest.update(content_digest);
    hex_lower(&digest.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
    static_assertions::assert_not_impl_any!(
        ManagedArtifactWitnessProof: Clone, std::fmt::Debug, serde::Serialize
    );

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "axial-user-mod-witness-{label}-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).expect("create witness root");
            Self(root)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entries(root: &Path) -> Option<Vec<(String, u64, u64)>> {
        observe_blocking(root, &[]).map(|observation| {
            observation
                .into_entries()
                .into_iter()
                .map(UserModSetEntry::into_parts)
                .collect()
        })
    }

    #[test]
    fn active_mod_observation_detects_add_replace_remove_and_rename() {
        let root = TestRoot::new("drift");
        fs::write(root.0.join("first.jar"), b"first").expect("write first jar");
        let baseline = entries(&root.0).expect("baseline observation");
        assert_eq!(entries(&root.0), Some(baseline.clone()));

        fs::write(root.0.join("second.jar"), b"second").expect("add second jar");
        let added = entries(&root.0).expect("added observation");
        assert_ne!(added, baseline);

        fs::write(root.0.join("first.jar"), b"replacement").expect("replace first jar");
        let replaced = entries(&root.0).expect("replacement observation");
        assert_ne!(replaced, added);

        fs::remove_file(root.0.join("second.jar")).expect("remove second jar");
        let removed = entries(&root.0).expect("removed observation");
        assert_ne!(removed, replaced);

        fs::rename(root.0.join("first.jar"), root.0.join("renamed.jar")).expect("rename first jar");
        assert_ne!(entries(&root.0), Some(removed));
    }

    #[test]
    fn inactive_and_hidden_files_are_excluded() {
        let root = TestRoot::new("exclusions");
        fs::write(root.0.join("active.jar"), b"active").expect("write active jar");
        fs::write(root.0.join("disabled.jar.disabled"), b"disabled").expect("write disabled jar");
        fs::write(root.0.join("notes.txt"), b"notes").expect("write non-jar");
        fs::write(root.0.join(".axial-hidden.jar"), b"hidden").expect("write hidden jar");

        let observation = entries(&root.0).expect("bounded observation");
        assert_eq!(observation.len(), 1);
    }

    #[test]
    fn noncanonical_active_name_is_unavailable() {
        let root = TestRoot::new("noncanonical-name");
        fs::write(root.0.join("unsafe\n.jar"), b"unsafe").expect("write control-name jar");
        assert!(entries(&root.0).is_none());
    }

    #[test]
    fn directory_entry_overflow_is_unavailable() {
        let root = TestRoot::new("overflow");
        for index in 0..=MAX_ACTIVE_MOD_ENTRIES {
            fs::write(root.0.join(format!("entry-{index}.txt")), b"")
                .expect("write overflow entry");
        }
        assert!(entries(&root.0).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn links_and_non_utf8_active_names_are_unavailable() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;

        let symlink_root = TestRoot::new("symlink");
        fs::write(symlink_root.0.join("target.bin"), b"target").expect("write link target");
        symlink("target.bin", symlink_root.0.join("linked.jar")).expect("create symlink");
        assert!(entries(&symlink_root.0).is_none());

        let hardlink_root = TestRoot::new("hardlink");
        fs::write(hardlink_root.0.join("first.jar"), b"same").expect("write hardlink source");
        fs::hard_link(
            hardlink_root.0.join("first.jar"),
            hardlink_root.0.join("second.jar"),
        )
        .expect("create hardlink");
        assert!(entries(&hardlink_root.0).is_none());

        let non_utf8_root = TestRoot::new("non-utf8");
        let name = std::ffi::OsString::from_vec(vec![0xff, b'.', b'j', b'a', b'r']);
        fs::write(non_utf8_root.0.join(name), b"private").expect("write non-utf8 jar");
        assert!(entries(&non_utf8_root.0).is_none());
    }
}

use super::anchored_record::AnchoredRecordDirectory;
use axial_performance::ManagedArtifactWitnessProof;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

const MAX_ACTIVE_MOD_ENTRIES: usize = 1024;
const MAX_ACTIVE_MOD_FILE_BYTES: u64 = 512 << 20;
const MAX_ACTIVE_MOD_TOTAL_BYTES: u64 = 4 << 30;
const USER_CONFIG_ENUMERATION_LIMIT: usize = 1024;
const USER_CONFIG_FILE_LIMIT: usize = 64;
const USER_CONFIG_FILE_BYTE_LIMIT: u64 = 64 * 1024;
const USER_CONFIG_TOTAL_BYTE_LIMIT: u64 = 512 * 1024;

pub(crate) struct UserConfigCapture {
    entries: Vec<UserConfigCaptureEntry>,
}

pub(crate) enum UserConfigCaptureEntry {
    Absent {
        slot: String,
    },
    File {
        slot: String,
        bytes: Vec<u8>,
        sha256: [u8; 32],
    },
}

struct PendingUserConfigFile {
    slot: String,
    observation: super::anchored_record::AnchoredRecordObservation,
}

impl UserConfigCapture {
    pub(crate) fn into_entries(self) -> Vec<UserConfigCaptureEntry> {
        self.entries
    }
}

type UserConfigFilePayload = (Vec<u8>, [u8; 32]);

impl UserConfigCaptureEntry {
    pub(crate) fn into_parts(self) -> (String, Option<UserConfigFilePayload>) {
        match self {
            Self::Absent { slot } => (slot, None),
            Self::File {
                slot,
                bytes,
                sha256,
            } => (slot, Some((bytes, sha256))),
        }
    }
}

pub(crate) async fn capture_user_config(game_dir: PathBuf) -> io::Result<UserConfigCapture> {
    tokio::task::spawn_blocking(move || capture_user_config_blocking(&game_dir))
        .await
        .map_err(|_| io::Error::other("user config capture worker stopped"))?
}

fn capture_user_config_blocking(game_dir: &Path) -> io::Result<UserConfigCapture> {
    #[cfg(test)]
    {
        capture_user_config_blocking_with_hook(game_dir, || {})
    }
    #[cfg(not(test))]
    {
        capture_user_config_blocking_impl(game_dir)
    }
}

#[cfg(test)]
fn capture_user_config_blocking_with_hook(
    game_dir: &Path,
    after_reads: impl FnOnce(),
) -> io::Result<UserConfigCapture> {
    capture_user_config_blocking_impl(game_dir, after_reads)
}

fn capture_user_config_blocking_impl(
    game_dir: &Path,
    #[cfg(test)] after_reads: impl FnOnce(),
) -> io::Result<UserConfigCapture> {
    let game = AnchoredRecordDirectory::open(game_dir)?;
    let game_epoch = game.epoch()?;
    let mut absent_options = false;
    let mut pending = Vec::new();
    match game.read(OsStr::new("options.txt"), USER_CONFIG_FILE_BYTE_LIMIT) {
        Ok(observation) => pending.push(PendingUserConfigFile {
            slot: "options.txt".to_string(),
            observation,
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => absent_options = true,
        Err(error) => return Err(error),
    }

    let config_path = game_dir.join("config");
    let config = match AnchoredRecordDirectory::open(&config_path) {
        Ok(directory) => Some((directory.epoch()?, directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some((_, directory)) = &config {
        let names = directory
            .names_bounded(USER_CONFIG_ENUMERATION_LIMIT)?
            .ok_or_else(|| invalid_user_config_capture("config entry bound exceeded"))?;
        let mut selected = Vec::new();
        for name in names {
            let name = name
                .into_string()
                .map_err(|_| invalid_user_config_capture("config name is not UTF-8"))?;
            if !classify_user_config_leaf(&name)? {
                continue;
            }
            selected.push(name);
        }
        selected.sort_unstable();
        for name in selected {
            if pending.len() >= USER_CONFIG_FILE_LIMIT {
                return Err(invalid_user_config_capture(
                    "selected user config file bound exceeded",
                ));
            }
            let observation = directory.read(OsStr::new(&name), USER_CONFIG_FILE_BYTE_LIMIT)?;
            pending.push(PendingUserConfigFile {
                slot: format!("config/{name}"),
                observation,
            });
        }
    }

    #[cfg(test)]
    after_reads();
    let mut raw_bytes = 0_u64;
    for file in &pending {
        if file.observation.is_oversized() {
            return Err(invalid_user_config_capture(
                "selected user config file exceeds its byte bound",
            ));
        }
        let bytes = file
            .observation
            .bytes()
            .expect("non-oversized anchored capture has bytes");
        std::str::from_utf8(bytes)
            .map_err(|_| invalid_user_config_capture("selected user config is not UTF-8"))?;
        raw_bytes = raw_bytes
            .checked_add(bytes.len() as u64)
            .filter(|total| *total <= USER_CONFIG_TOTAL_BYTE_LIMIT)
            .ok_or_else(|| invalid_user_config_capture("user config byte bound exceeded"))?;
        file.observation.revalidate()?;
    }
    if let Some((epoch, directory)) = &config
        && directory.epoch()? != *epoch
    {
        return Err(invalid_user_config_capture(
            "config directory changed during capture",
        ));
    }
    if game.epoch()? != game_epoch {
        return Err(invalid_user_config_capture(
            "instance directory changed during capture",
        ));
    }

    let mut entries = Vec::with_capacity(pending.len() + usize::from(absent_options));
    if absent_options {
        entries.push(UserConfigCaptureEntry::Absent {
            slot: "options.txt".to_string(),
        });
    }
    for file in pending {
        let bytes = file
            .observation
            .into_bytes()
            .expect("non-oversized anchored capture has bytes");
        entries.push(UserConfigCaptureEntry::File {
            slot: file.slot,
            sha256: Sha256::digest(&bytes).into(),
            bytes,
        });
    }
    entries.sort_by(|left, right| user_config_entry_slot(left).cmp(user_config_entry_slot(right)));
    Ok(UserConfigCapture { entries })
}

pub(crate) fn classify_user_config_leaf(name: &str) -> io::Result<bool> {
    if name.is_empty()
        || name.len() > 255
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        return Err(invalid_user_config_capture("config name is not canonical"));
    }
    let Some((_, extension)) = name.rsplit_once('.') else {
        return Ok(false);
    };
    Ok(!name.starts_with('.')
        && matches!(
            extension.to_ascii_lowercase().as_str(),
            "cfg" | "conf" | "json" | "json5" | "properties" | "toml" | "txt" | "yaml" | "yml"
        ))
}

fn user_config_entry_slot(entry: &UserConfigCaptureEntry) -> &str {
    match entry {
        UserConfigCaptureEntry::Absent { slot } | UserConfigCaptureEntry::File { slot, .. } => slot,
    }
}

fn invalid_user_config_capture(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

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
        UserConfigCapture:
            Clone, std::fmt::Debug, serde::Serialize, serde::de::DeserializeOwned
    );
    static_assertions::assert_not_impl_any!(
        UserConfigCaptureEntry:
            Clone, std::fmt::Debug, serde::Serialize, serde::de::DeserializeOwned
    );
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

    fn captured_entries(root: &Path) -> io::Result<Vec<(String, Option<Vec<u8>>)>> {
        capture_user_config_blocking(root).map(|capture| {
            capture
                .into_entries()
                .into_iter()
                .map(|entry| {
                    let (slot, content) = entry.into_parts();
                    let bytes = content.map(|(bytes, sha256)| {
                        let expected: [u8; 32] = Sha256::digest(&bytes).into();
                        assert_eq!(sha256, expected);
                        bytes
                    });
                    (slot, bytes)
                })
                .collect()
        })
    }

    fn capture_error_kind(root: &Path) -> io::ErrorKind {
        match capture_user_config_blocking(root) {
            Ok(_) => panic!("expected user config capture to fail"),
            Err(error) => error.kind(),
        }
    }

    #[test]
    fn user_config_capture_is_closed_sorted_and_byte_exact() {
        let root = TestRoot::new("config-closed");
        let options = b"guiScale:3\nresourcePacks:[\"vanilla\"]\n";
        fs::write(root.0.join("options.txt"), options).expect("write options");
        let config = root.0.join("config");
        fs::create_dir_all(config.join("nested")).expect("create nested config");
        let allowed = [
            ("a.CFG", b"cfg\n".as_slice()),
            ("b.CoNf", b"conf\n".as_slice()),
            ("c.JSON", b"json\n".as_slice()),
            ("d.JsOn5", b"json5\n".as_slice()),
            ("e.PROPERTIES", b"properties\n".as_slice()),
            ("f.TomL", b"toml\n".as_slice()),
            ("g.TXT", b"txt\n".as_slice()),
            ("h.YaMl", b"yaml\n".as_slice()),
            ("i.YML", b"yml\n".as_slice()),
        ];
        for (name, bytes) in allowed.iter().rev() {
            fs::write(config.join(name), bytes).expect("write selected config");
        }
        fs::write(config.join("ignored.bin"), b"private").expect("write unsupported file");
        fs::write(config.join("ignored.json.bak"), b"private")
            .expect("write double-extension file");
        fs::write(config.join("no-extension"), b"private").expect("write extensionless file");
        fs::write(config.join(".hidden.yaml"), b"private: true\n").expect("write hidden file");
        fs::write(config.join("nested/deep.toml"), b"private = true\n").expect("write nested file");
        fs::write(root.0.join("servers.dat"), b"private").expect("write excluded state");
        for directory in [
            "mods",
            "saves",
            "resourcepacks",
            "shaderpacks",
            "logs",
            "screenshots",
        ] {
            fs::create_dir(root.0.join(directory)).expect("create excluded directory");
            fs::write(root.0.join(directory).join("excluded.toml"), b"private\n")
                .expect("write excluded root config");
        }

        let mut expected = allowed
            .into_iter()
            .map(|(name, bytes)| (format!("config/{name}"), Some(bytes.to_vec())))
            .collect::<Vec<_>>();
        expected.push(("options.txt".to_string(), Some(options.to_vec())));
        assert_eq!(
            captured_entries(&root.0).expect("capture closed config"),
            expected
        );
    }

    #[test]
    fn absent_options_and_exact_byte_bounds_are_enforced() {
        let absent = TestRoot::new("config-absent");
        assert_eq!(
            captured_entries(&absent.0).expect("capture absent options"),
            vec![("options.txt".to_string(), None)]
        );

        let per_file = TestRoot::new("config-file-bound");
        fs::write(
            per_file.0.join("options.txt"),
            vec![b'x'; USER_CONFIG_FILE_BYTE_LIMIT as usize],
        )
        .expect("write exact-bound options");
        assert_eq!(
            captured_entries(&per_file.0)
                .expect("accept exact per-file bound")
                .first()
                .and_then(|(_, bytes)| bytes.as_ref())
                .map(Vec::len),
            Some(USER_CONFIG_FILE_BYTE_LIMIT as usize)
        );
        fs::write(
            per_file.0.join("options.txt"),
            vec![b'x'; USER_CONFIG_FILE_BYTE_LIMIT as usize + 1],
        )
        .expect("write oversized options");
        assert_eq!(capture_error_kind(&per_file.0), io::ErrorKind::InvalidData);

        let aggregate = TestRoot::new("config-total-bound");
        let config = aggregate.0.join("config");
        fs::create_dir(&config).expect("create config");
        for index in 0..8 {
            fs::write(
                config.join(format!("entry-{index}.toml")),
                vec![b'x'; USER_CONFIG_FILE_BYTE_LIMIT as usize],
            )
            .expect("write aggregate config");
        }
        assert_eq!(
            captured_entries(&aggregate.0)
                .expect("accept exact aggregate byte bound")
                .len(),
            9
        );
        fs::write(
            config.join("entry-8.toml"),
            vec![b'x'; USER_CONFIG_FILE_BYTE_LIMIT as usize],
        )
        .expect("write aggregate overflow config");
        assert_eq!(capture_error_kind(&aggregate.0), io::ErrorKind::InvalidData);
    }

    #[test]
    fn exact_enumeration_and_selected_file_bounds_are_enforced() {
        let enumeration = TestRoot::new("config-enumeration-bound");
        let config = enumeration.0.join("config");
        fs::create_dir(&config).expect("create config");
        for index in 0..USER_CONFIG_ENUMERATION_LIMIT {
            fs::write(config.join(format!("ignored-{index}.bin")), b"")
                .expect("write enumerated config");
        }
        assert_eq!(
            captured_entries(&enumeration.0).expect("accept exact enumeration bound"),
            vec![("options.txt".to_string(), None)]
        );
        fs::write(
            config.join(format!("ignored-{}.bin", USER_CONFIG_ENUMERATION_LIMIT)),
            b"",
        )
        .expect("write enumeration overflow");
        assert_eq!(
            capture_error_kind(&enumeration.0),
            io::ErrorKind::InvalidData
        );

        let selected = TestRoot::new("config-selected-bound");
        let config = selected.0.join("config");
        fs::create_dir(&config).expect("create config");
        for index in 0..USER_CONFIG_FILE_LIMIT {
            fs::write(config.join(format!("selected-{index:02}.toml")), b"")
                .expect("write selected config");
        }
        assert_eq!(
            captured_entries(&selected.0)
                .expect("accept exact selected-file bound")
                .len(),
            USER_CONFIG_FILE_LIMIT + 1
        );
        fs::write(
            config.join(format!("selected-{}.toml", USER_CONFIG_FILE_LIMIT)),
            b"",
        )
        .expect("write selected-file overflow");
        assert_eq!(capture_error_kind(&selected.0), io::ErrorKind::InvalidData);
    }

    #[test]
    fn user_config_capture_rejects_invalid_utf8_content_and_selected_directories() {
        let invalid_content = TestRoot::new("config-invalid-content");
        fs::write(invalid_content.0.join("options.txt"), [0xff])
            .expect("write invalid UTF-8 content");
        assert_eq!(
            capture_error_kind(&invalid_content.0),
            io::ErrorKind::InvalidData
        );

        let directory = TestRoot::new("config-selected-directory");
        fs::create_dir(directory.0.join("config")).expect("create config");
        fs::create_dir(directory.0.join("config/directory.toml"))
            .expect("create selected directory");
        assert!(capture_user_config_blocking(&directory.0).is_err());

        assert_eq!(
            classify_user_config_leaf("control\n.toml")
                .expect_err("reject control name")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn user_config_capture_rejects_selected_leaf_and_directory_replacement_races() {
        let root = TestRoot::new("config-leaf-race");
        let options = root.0.join("options.txt");
        fs::write(&options, b"before\n").expect("write options");
        let displaced = root.0.join("options.displaced");

        assert!(
            capture_user_config_blocking_with_hook(&root.0, || {
                fs::rename(&options, &displaced).expect("displace selected file");
                fs::write(&options, b"after\n").expect("replace selected file");
            })
            .is_err()
        );

        let root = TestRoot::new("config-directory-race");
        let config = root.0.join("config");
        fs::create_dir(&config).expect("create config");
        fs::write(config.join("selected.toml"), b"before\n").expect("write selected config");
        let displaced = root.0.join("config.displaced");
        assert!(
            capture_user_config_blocking_with_hook(&root.0, || {
                fs::rename(&config, &displaced).expect("displace config directory");
                fs::create_dir(&config).expect("replace config directory");
                fs::write(config.join("selected.toml"), b"after\n")
                    .expect("replace selected config");
            })
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn user_config_capture_rejects_unsafe_selected_entries_and_non_utf8_names() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;

        let options_symlink = TestRoot::new("config-options-symlink");
        fs::write(options_symlink.0.join("target.txt"), b"target\n").expect("write options target");
        symlink("target.txt", options_symlink.0.join("options.txt"))
            .expect("create options symlink");
        assert!(capture_user_config_blocking(&options_symlink.0).is_err());

        let leaf_symlink = TestRoot::new("config-leaf-symlink");
        let config = leaf_symlink.0.join("config");
        fs::create_dir(&config).expect("create config");
        fs::write(config.join("target.bin"), b"target").expect("write target");
        symlink("target.bin", config.join("linked.toml")).expect("create selected symlink");
        assert!(capture_user_config_blocking(&leaf_symlink.0).is_err());

        let config_symlink = TestRoot::new("config-directory-symlink");
        fs::create_dir(config_symlink.0.join("actual-config")).expect("create config target");
        fs::write(
            config_symlink.0.join("actual-config/selected.toml"),
            b"private\n",
        )
        .expect("write config target");
        symlink("actual-config", config_symlink.0.join("config"))
            .expect("create config directory symlink");
        assert!(capture_user_config_blocking(&config_symlink.0).is_err());

        let hardlink_root = TestRoot::new("config-hardlink");
        let config = hardlink_root.0.join("config");
        fs::create_dir(&config).expect("create config");
        fs::write(config.join("first.toml"), b"same\n").expect("write hardlink source");
        fs::hard_link(config.join("first.toml"), config.join("second.toml"))
            .expect("create selected hardlink");
        assert!(capture_user_config_blocking(&hardlink_root.0).is_err());

        let fifo_root = TestRoot::new("config-fifo");
        let config = fifo_root.0.join("config");
        fs::create_dir(&config).expect("create config");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            config.join("special.toml"),
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .expect("create selected FIFO");
        assert!(capture_user_config_blocking(&fifo_root.0).is_err());

        let non_utf8_root = TestRoot::new("config-non-utf8");
        let config = non_utf8_root.0.join("config");
        fs::create_dir(&config).expect("create config");
        fs::write(
            config.join(std::ffi::OsString::from_vec(vec![
                0xff, b'.', b't', b'o', b'm', b'l',
            ])),
            b"private\n",
        )
        .expect("write non-UTF-8 config");
        assert!(capture_user_config_blocking(&non_utf8_root.0).is_err());

        let control_root = TestRoot::new("config-control-name");
        let config = control_root.0.join("config");
        fs::create_dir(&config).expect("create config");
        fs::write(config.join("unsafe\n.toml"), b"private\n").expect("write control-name config");
        assert_eq!(
            capture_error_kind(&control_root.0),
            io::ErrorKind::InvalidData
        );

        let backslash_root = TestRoot::new("config-backslash-name");
        let config = backslash_root.0.join("config");
        fs::create_dir(&config).expect("create config");
        fs::write(config.join("unsafe\\name.toml"), b"private\n")
            .expect("write backslash-name config");
        assert_eq!(
            capture_error_kind(&backslash_root.0),
            io::ErrorKind::InvalidData
        );
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

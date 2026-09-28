use std::fs;
use std::io;
use std::path::Path;

pub fn is_valid_frontend_relative_path(value: &str) -> bool {
    if value.is_empty()
        || value.starts_with('/')
        || value.contains('\\')
        || value.contains('\0')
        || value
            .split('/')
            .next()
            .is_some_and(|segment| segment.eq_ignore_ascii_case("generation.json"))
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._/-".contains(&byte))
    {
        return false;
    }
    value.split('/').all(|segment| {
        let stem = segment.split('.').next().unwrap_or_default();
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && !segment.ends_with('.')
            && !segment.ends_with(' ')
            && !matches!(
                stem.to_ascii_lowercase().as_str(),
                "con"
                    | "prn"
                    | "aux"
                    | "nul"
                    | "com1"
                    | "com2"
                    | "com3"
                    | "com4"
                    | "com5"
                    | "com6"
                    | "com7"
                    | "com8"
                    | "com9"
                    | "lpt1"
                    | "lpt2"
                    | "lpt3"
                    | "lpt4"
                    | "lpt5"
                    | "lpt6"
                    | "lpt7"
                    | "lpt8"
                    | "lpt9"
            )
    })
}

pub fn reset_frontend_destination(destination: &Path) -> io::Result<()> {
    match fs::remove_dir_all(destination) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::create_dir_all(destination)
}

#[cfg(test)]
mod tests {
    use super::{is_valid_frontend_relative_path, reset_frontend_destination};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn frontend_relative_paths_reject_nonportable_or_ambiguous_shapes() {
        for invalid in [
            "",
            "/a",
            "a/",
            "a//b",
            "a/./b",
            "a/../b",
            "a\\b",
            "name.",
            "CON.txt",
            "generation.json",
            "Generation.json",
            "generation.json/x",
            "GENERATION.JSON/x",
        ] {
            assert!(
                !is_valid_frontend_relative_path(invalid),
                "invalid path was accepted: {}",
                invalid
            );
        }
        for valid in ["app.js", "chunks/app-123.js", "fonts/name.woff2"] {
            assert!(
                is_valid_frontend_relative_path(valid),
                "valid path was rejected: {}",
                valid
            );
        }
    }

    #[test]
    fn frontend_destination_reset_removes_stale_entries_before_staging() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "axial-frontend-build-support-{}-{nonce}",
            std::process::id()
        ));
        let destination = root.join("embedded-frontend");
        fs::create_dir_all(destination.join("stale")).expect("create stale fixture");
        fs::write(destination.join("stale/file.js"), b"stale").expect("write stale fixture");

        reset_frontend_destination(&destination).expect("reset destination");

        assert!(destination.is_dir());
        assert_eq!(
            fs::read_dir(&destination)
                .expect("read destination")
                .count(),
            0
        );
        fs::remove_dir_all(root).expect("remove fixture");
    }
}

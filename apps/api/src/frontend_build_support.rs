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

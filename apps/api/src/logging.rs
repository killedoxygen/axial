use chrono::{SecondsFormat, Utc};
use croopor_config::AppPaths;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

pub fn timestamp_utc() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn append_trace(category: &str, id: &str, message: &str) {
    let path = trace_file_path(category, id);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "[{}] {}", timestamp_utc(), message);
    }
}

pub fn trace_file_path(category: &str, id: &str) -> PathBuf {
    let paths = AppPaths::detect();
    paths
        .config_dir
        .join("traces")
        .join(safe_segment(category))
        .join(format!("{}.log", safe_segment(id)))
}

fn safe_segment(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '_',
        })
        .collect()
}

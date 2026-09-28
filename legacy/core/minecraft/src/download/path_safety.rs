#[cfg(test)]
use std::path::Path;

#[cfg(test)]
pub(super) fn safe_download_target_label(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .and_then(|value| {
            let value = safe_download_fact_value(value, "artifact");
            (value != "artifact").then_some(value)
        })
        .unwrap_or_else(|| "artifact".to_string())
}

pub(super) fn safe_download_fact_value(value: &str, fallback: &str) -> String {
    let value = value.trim();
    if value.is_empty() || download_value_looks_sensitive(value) {
        return fallback.to_string();
    }

    let mut sanitized = String::with_capacity(value.len().min(96));
    for ch in value.chars().take(96) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '+' | ':') {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }
    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        fallback.to_string()
    } else {
        sanitized.to_string()
    }
}

pub(super) fn download_value_looks_sensitive(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    value.contains('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || lower.contains("-xmx")
        || lower.contains("-xms")
        || lower.contains("-xx:")
        || lower.contains("--access")
        || lower.contains("--username")
        || lower.contains("--uuid")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("provider_payload")
}

pub(super) fn bounded_provider_path_label(path: &str) -> String {
    const MAX_LABEL_CHARS: usize = 120;
    let sanitized = path.replace(['\r', '\n'], "?");
    let mut chars = sanitized.chars();
    let label = chars.by_ref().take(MAX_LABEL_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{label}...")
    } else {
        label
    }
}

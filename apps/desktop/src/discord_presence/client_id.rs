pub(super) fn configured_client_id() -> Option<String> {
    let raw = option_env!("AXIAL_DISCORD_APPLICATION_ID")?;
    let value = sanitize_client_id(raw);
    if value.is_none() {
        tracing::warn!("Discord application ID is invalid; presence is inactive");
    }
    value
}

fn sanitize_client_id(raw: &str) -> Option<String> {
    let value = raw.trim();
    ((6..=32).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_bounded_numeric_application_ids_enable_integration() {
        assert_eq!(
            sanitize_client_id(" 123456789012345678 ").as_deref(),
            Some("123456789012345678")
        );
        for invalid in ["", "12345", "abc123", "12345678901234567890123456789012345"] {
            assert_eq!(sanitize_client_id(invalid), None);
        }
    }
}

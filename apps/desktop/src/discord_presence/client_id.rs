use tracing::warn;

pub(super) fn configured_client_id() -> Option<String> {
    match option_env!("CROOPOR_DISCORD_APPLICATION_ID") {
        Some(raw) => match sanitize_client_id(raw) {
            Ok(value) => Some(value),
            Err(error) => {
                warn!(error = %error, "ignoring invalid Discord application id");
                None
            }
        },
        None => None,
    }
}

fn sanitize_client_id(raw: &str) -> Result<String, &'static str> {
    let value = raw.trim();
    if value.len() < 6 || value.len() > 32 {
        return Err("expected 6 to 32 digits");
    }
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("expected digits only");
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_must_be_numeric_application_id() {
        assert_eq!(
            sanitize_client_id(" 123456789012345678 "),
            Ok("123456789012345678".to_string())
        );
        assert!(sanitize_client_id("abc123").is_err());
        assert!(sanitize_client_id("12345").is_err());
        assert!(sanitize_client_id("123456789012345678901234567890123").is_err());
    }
}

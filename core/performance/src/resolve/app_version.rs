use super::model::ResolveError;

const RUNNING_APP_VERSION: &str = env!("CARGO_PKG_VERSION");

pub(super) fn validate_app_version_compatibility(
    minimum_app_version: &str,
) -> Result<(), ResolveError> {
    validate_app_version_compatibility_with_running(minimum_app_version, RUNNING_APP_VERSION)
}

pub(super) fn validate_app_version_compatibility_with_running(
    minimum_app_version: &str,
    running_app_version: &str,
) -> Result<(), ResolveError> {
    let minimum = parse_app_version(minimum_app_version)?;
    let running = parse_app_version(running_app_version).map_err(|_| {
        ResolveError::InvalidRunningAppVersion(running_app_version.trim().to_string())
    })?;
    if compare_app_versions(&minimum, &running).is_gt() {
        return Err(ResolveError::UnsupportedAppVersion {
            required: minimum_app_version.trim().to_string(),
            running: running_app_version.to_string(),
        });
    }
    Ok(())
}
struct AppVersion {
    major: u64,
    minor: u64,
    patch: u64,
    pre_release: Option<String>,
}

fn parse_app_version(value: &str) -> Result<AppVersion, ResolveError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ResolveError::MissingMinimumAppVersion);
    }

    let (release, pre_release) = match trimmed.split_once('-') {
        Some((release, pre_release)) => {
            if release.is_empty() || !valid_pre_release(pre_release) {
                return Err(ResolveError::InvalidMinimumAppVersion(trimmed.to_string()));
            }
            (release, Some(pre_release.to_string()))
        }
        None => (trimmed, None),
    };
    let parts = release.split('.').collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(ResolveError::InvalidMinimumAppVersion(trimmed.to_string()));
    }

    let parse_part = |part: &str| -> Result<u64, ResolveError> {
        if part.is_empty() || !part.chars().all(|character| character.is_ascii_digit()) {
            return Err(ResolveError::InvalidMinimumAppVersion(trimmed.to_string()));
        }
        part.parse::<u64>()
            .map_err(|_| ResolveError::InvalidMinimumAppVersion(trimmed.to_string()))
    };

    Ok(AppVersion {
        major: parse_part(parts[0])?,
        minor: parse_part(parts[1])?,
        patch: parse_part(parts[2])?,
        pre_release,
    })
}

fn valid_pre_release(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
}

fn compare_app_versions(left: &AppVersion, right: &AppVersion) -> std::cmp::Ordering {
    for ordering in [
        left.major.cmp(&right.major),
        left.minor.cmp(&right.minor),
        left.patch.cmp(&right.patch),
    ] {
        if !ordering.is_eq() {
            return ordering;
        }
    }
    match (&left.pre_release, &right.pre_release) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(left), Some(right)) => prerelease_channel_rank(left)
            .cmp(&prerelease_channel_rank(right))
            .then_with(|| left.cmp(right)),
    }
}

fn prerelease_channel_rank(pre_release: &str) -> u8 {
    match pre_release.split('.').next().unwrap_or_default() {
        "dev" => 0,
        "alpha" => 1,
        "beta" => 2,
        "rc" => 3,
        _ => 0,
    }
}

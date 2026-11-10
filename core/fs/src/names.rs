use super::*;

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LeafName(pub(crate) OsString);

impl LeafName {
    pub fn new(value: impl Into<OsString>) -> Result<Self, InvalidLeafName> {
        let value = value.into();
        validate_leaf_name(&value)?;
        Ok(Self(value))
    }

    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }
}

pub fn leaf_names_equivalent(first: &OsStr, second: &OsStr) -> bool {
    platform::leaf_names_equal(first, second)
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub(crate) struct LeafNameEquivalenceKey(pub(crate) Vec<u8>);

impl Borrow<[u8]> for LeafNameEquivalenceKey {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}

impl_redacted_debug!(LeafNameEquivalenceKey);

/// Returns the small set of lookup keys needed to find every name that the
/// platform or the portable spelling rules can consider equivalent.
pub(crate) fn leaf_name_equivalence_keys(name: &OsStr) -> Vec<LeafNameEquivalenceKey> {
    platform::leaf_name_equivalence_keys(name)
        .into_iter()
        .map(LeafNameEquivalenceKey)
        .collect()
}

impl fmt::Debug for LeafName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("LeafName").finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("filesystem capability leaf name is invalid")]
pub struct InvalidLeafName;

pub(crate) fn validate_leaf_name(value: &OsStr) -> Result<(), InvalidLeafName> {
    if value.is_empty() || value == OsStr::new(".") || value == OsStr::new("..") {
        return Err(InvalidLeafName);
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        let bytes = value.as_bytes();
        if bytes.len() > MAX_LEAF_UNITS || bytes.iter().any(|byte| matches!(byte, 0 | b'/')) {
            return Err(InvalidLeafName);
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        let units = value.encode_wide().collect::<Vec<_>>();
        if units.len() > MAX_LEAF_UNITS
            || units
                .iter()
                .any(|unit| matches!(*unit, 0 | 0x2f | 0x3a | 0x5c))
        {
            return Err(InvalidLeafName);
        }
    }

    #[cfg(not(any(unix, windows)))]
    if value.to_string_lossy().contains(['\0', '/', '\\']) {
        return Err(InvalidLeafName);
    }

    Ok(())
}

#[cfg(unix)]
pub fn resolve_owned_symlink_target_beneath(
    parent: &[LeafName],
    target: &OsStr,
) -> io::Result<Vec<LeafName>> {
    let path = Path::new(target);
    if target.is_empty() || path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "owned symlink target must be a nonempty relative path",
        ));
    }
    let mut resolved = parent.to_vec();
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                resolved.push(LeafName::new(name.to_os_string()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "owned symlink target contains an invalid name",
                    )
                })?);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if resolved.pop().is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "owned symlink target escapes its retained root",
                    ));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "owned symlink target is not relative",
                ));
            }
        }
    }
    if resolved.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "owned symlink target must name an entry beneath its root",
        ));
    }
    Ok(resolved)
}

use super::*;

pub type FileMoveOutcome = Outcome<FileCapability, FileCapability, FileMoveObligation>;

pub type FileMoveResolution = Resolution<FileCapability, FileCapability, FileMoveObligation>;

#[must_use = "file move obligations must be reconciled"]
pub struct FileMoveObligation {
    pub(crate) error: io::Error,
    pub(crate) file: Option<FileCapability>,
    pub(crate) destination: Directory,
    pub(crate) destination_name: LeafName,
    pub(crate) reported_success: bool,
    pub(crate) token: MoveEffectToken,
}

impl_redacted_debug!(FileMoveObligation);

impl FileMoveObligation {
    pub fn reconcile(mut self) -> FileMoveResolution {
        let file = self
            .file
            .take()
            .expect("file move obligation retains its file");
        match settle_file_move(
            file,
            &self.destination,
            &self.destination_name,
            self.reported_success,
            &mut self.token,
            true,
        ) {
            Ok((true, file)) => FileMoveResolution::Applied(file),
            Ok((false, file)) => FileMoveResolution::NoEffect(file),
            Err(file) => {
                self.file = Some(file);
                FileMoveResolution::Indeterminate(self)
            }
        }
    }
}

impl_error_accessor!(FileMoveObligation);

pub type DirectoryMoveOutcome = Outcome<Directory, Directory, DirectoryMoveObligation>;

pub type DirectoryMoveResolution = Resolution<Directory, Directory, DirectoryMoveObligation>;

#[must_use = "directory move obligations must be reconciled"]
pub struct DirectoryMoveObligation {
    pub(crate) error: io::Error,
    pub(crate) directory: Option<Directory>,
    pub(crate) destination: Directory,
    pub(crate) destination_name: LeafName,
    pub(crate) reported_success: bool,
    pub(crate) token: MoveEffectToken,
}

impl_redacted_debug!(DirectoryMoveObligation);

impl DirectoryMoveObligation {
    pub fn reconcile(mut self) -> DirectoryMoveResolution {
        let directory = self
            .directory
            .take()
            .expect("directory move obligation retains its directory");
        match settle_directory_move(
            directory,
            &self.destination,
            &self.destination_name,
            self.reported_success,
            &mut self.token,
        ) {
            Ok((true, directory)) => DirectoryMoveResolution::Applied(directory),
            Ok((false, directory)) => DirectoryMoveResolution::NoEffect(directory),
            Err(directory) => {
                self.directory = Some(directory);
                DirectoryMoveResolution::Indeterminate(self)
            }
        }
    }
}

impl_error_accessor!(DirectoryMoveObligation);

pub(crate) fn settle_file_move(
    file: FileCapability,
    destination: &Directory,
    destination_name: &LeafName,
    reported_success: bool,
    token: &mut MoveEffectToken,
    sync_parents: bool,
) -> Result<(bool, FileCapability), FileCapability> {
    let authority = match file.parent.authority() {
        Ok(authority) => authority,
        Err(_) => return Err(file),
    };
    let operation = match authority.enter() {
        Ok(operation) => operation,
        Err(_) => return Err(file),
    };
    if destination.validate(&operation).is_err()
        || !Weak::ptr_eq(&file.parent.inner.authority, &destination.inner.authority)
        || platform::file_identity(&file.handle).ok() != Some(file.identity)
    {
        return Err(file);
    }
    let source = platform::file_binding_state(
        file.parent.inner.handle(),
        file.name.as_os_str(),
        file.identity,
    );
    let target = platform::file_binding_state(
        destination.inner.handle(),
        destination_name.as_os_str(),
        file.identity,
    );
    match classify_move_topology(reported_success, source.ok(), target.ok()) {
        MoveTopology::Applied => {
            if (sync_parents && sync_rename_parents(&file.parent, destination).is_err())
                || destination.validate(&operation).is_err()
            {
                return Err(file);
            }
            let handle =
                match platform::open_file(destination.inner.handle(), destination_name.as_os_str())
                {
                    Ok(handle) if platform::file_identity(&handle).ok() == Some(file.identity) => {
                        handle
                    }
                    _ => return Err(file),
                };
            let moved = FileCapability::new(
                handle,
                file.identity,
                destination.clone(),
                destination_name.clone(),
                file.authority.clone(),
            );
            if moved.validate(&operation).is_err() || token.settle(&operation).is_err() {
                return Err(file);
            }
            Ok((true, moved))
        }
        MoveTopology::NoEffect => {
            if file.validate(&operation).is_err() || token.settle(&operation).is_err() {
                return Err(file);
            }
            Ok((false, file))
        }
        MoveTopology::Indeterminate => Err(file),
    }
}

pub(crate) fn settle_directory_move(
    directory: Directory,
    destination: &Directory,
    destination_name: &LeafName,
    reported_success: bool,
    token: &mut MoveEffectToken,
) -> Result<(bool, Directory), Directory> {
    let Some(binding) = directory.inner.parent.as_ref() else {
        return Err(directory);
    };
    let source_parent = binding.directory.clone();
    let source_name = binding.name.clone();
    let authority = match source_parent.authority() {
        Ok(authority) => authority,
        Err(_) => return Err(directory),
    };
    let operation = match authority.enter_directory_rebinding() {
        Ok(operation) => operation,
        Err(_) => return Err(directory),
    };
    if destination.validate(&operation).is_err()
        || !Weak::ptr_eq(&source_parent.inner.authority, &destination.inner.authority)
        || platform::directory_identity(directory.inner.handle()).ok()
            != Some(directory.inner.identity.physical)
    {
        return Err(directory);
    }
    let source = platform::directory_binding_state(
        source_parent.inner.handle(),
        &source_name,
        directory.inner.identity.physical,
    );
    let target = platform::directory_binding_state(
        destination.inner.handle(),
        destination_name.as_os_str(),
        directory.inner.identity.physical,
    );
    match classify_move_topology(reported_success, source.ok(), target.ok()) {
        MoveTopology::Applied => {
            if sync_rename_parents(&source_parent, destination).is_err()
                || destination.validate(&operation).is_err()
            {
                return Err(directory);
            }
            let (handle, identity) = match platform::open_directory(
                destination.inner.handle(),
                destination_name.as_os_str(),
            ) {
                Ok(opened) if opened.1 == directory.inner.identity.physical => opened,
                _ => return Err(directory),
            };
            let moved = Directory::from_handle(
                handle,
                authority.identity(identity),
                directory.inner.authority.clone(),
                Some(DirectoryParent {
                    directory: destination.clone(),
                    name: destination_name.as_os_str().to_os_string(),
                }),
            );
            if moved.validate(&operation).is_err() || token.settle(&operation).is_err() {
                return Err(directory);
            }
            Ok((true, moved))
        }
        MoveTopology::NoEffect => {
            if directory.validate(&operation).is_err() || token.settle(&operation).is_err() {
                return Err(directory);
            }
            Ok((false, directory))
        }
        MoveTopology::Indeterminate => Err(directory),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MoveTopology {
    Applied,
    NoEffect,
    Indeterminate,
}

pub(crate) fn classify_move_topology(
    reported_success: bool,
    source: Option<platform::BindingState>,
    destination: Option<platform::BindingState>,
) -> MoveTopology {
    if source == Some(platform::BindingState::Absent)
        && destination == Some(platform::BindingState::Exact)
    {
        MoveTopology::Applied
    } else if !reported_success && source == Some(platform::BindingState::Exact) {
        MoveTopology::NoEffect
    } else {
        MoveTopology::Indeterminate
    }
}

pub(crate) fn sync_rename_parents(source: &Directory, destination: &Directory) -> io::Result<()> {
    platform::sync_directory(destination.inner.handle())?;
    if !Arc::ptr_eq(&source.inner, &destination.inner) {
        platform::sync_directory(source.inner.handle())?;
    }
    Ok(())
}

pub(crate) fn random_leaf(prefix: &str) -> LeafName {
    use rand::RngCore;

    let mut nonce = [0_u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    LeafName::new(format!("{prefix}{}", hex::encode(nonce)))
        .expect("generated filesystem leaf is valid")
}

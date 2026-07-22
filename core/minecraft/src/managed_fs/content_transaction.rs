use super::ManagedTreeDirectory;
use crate::download::ManagedTransferAuthority;

#[must_use = "managed content transaction authority must be retained through settlement"]
pub struct ManagedContentTransactionRoot {
    directory: ManagedTreeDirectory,
    authority: ManagedTransferAuthority,
}

impl std::fmt::Debug for ManagedContentTransactionRoot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedContentTransactionRoot")
            .finish_non_exhaustive()
    }
}

impl ManagedContentTransactionRoot {
    pub fn bind(
        directory: ManagedTreeDirectory,
        authority: ManagedTransferAuthority,
    ) -> Self {
        Self {
            directory,
            authority,
        }
    }
}

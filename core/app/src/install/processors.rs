//! Shared loader continuation after the verified base checkpoint.
//!
//! Processor inputs remain opaque retained capabilities. This boundary cannot
//! construct a command from an arbitrary JAR/path, invent processor output proof,
//! or discard a process owner on cancellation. The retained implementation owns
//! archive traversal checks, exact outputs, process-tree containment and drainage.

use axial_minecraft::managed_path::ManagedLibraryOperation;
use axial_minecraft::loaders::{LoaderInstallBaseContinuation, LoaderInstallError};
use axial_minecraft::{DownloadProgress, KnownGoodInstallReceipt};

/// Continue any retained loader strategy with the exact acknowledged base inputs.
/// The returned child receipt still requires application activation and acknowledgement.
pub async fn continue_loader_install<F>(
    library: &ManagedLibraryOperation,
    continuation: LoaderInstallBaseContinuation,
    mut send: F,
) -> Result<KnownGoodInstallReceipt, LoaderInstallError>
where
    F: FnMut(DownloadProgress) + Send + 'static,
{
    axial_minecraft::continue_install_build_after_base(library, continuation, move |progress| {
        if !progress.done {
            send(progress);
        }
    }).await
}

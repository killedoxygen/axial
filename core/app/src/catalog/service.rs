//! Catalog reads as owned tasks holding a managed-library pin.

use super::{Catalog, VersionsResponse, installed_versions};
use crate::{
    library::LibraryLifecycle,
    tasks::{CancellationToken, TaskOwner},
};
use axial_minecraft::managed_path::ManagedLibraryOperation;
use std::{future::Future, sync::Arc};

/// Catalog reads are unavailable; callers state which catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unavailable;

#[derive(Clone)]
pub struct CatalogService {
    library: LibraryLifecycle,
    catalog: Arc<Catalog>,
    tasks: TaskOwner,
}

impl CatalogService {
    pub fn new(library: LibraryLifecycle, catalog: Arc<Catalog>, tasks: TaskOwner) -> Self {
        Self {
            library,
            catalog,
            tasks,
        }
    }

    pub async fn installed(&self) -> Result<VersionsResponse, Unavailable> {
        self.read(|catalog, operation, _| async move {
            let snapshot = catalog.cached_snapshot(&operation).await;
            installed_versions(&operation, Some(&snapshot))
                .await
                .map_err(|_| Unavailable)
        })
        .await
    }

    async fn read<T, F, Fut>(&self, work: F) -> Result<T, Unavailable>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Catalog>, ManagedLibraryOperation, CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, Unavailable>> + Send + 'static,
    {
        let pin = self.library.admit().map_err(|_| Unavailable)?;
        let operation = pin.managed_library().map_err(|_| Unavailable)?;
        let catalog = self.catalog.clone();
        self.tasks
            .try_spawn((pin, operation.clone()), move |cancel| {
                work(catalog, operation, cancel)
            })
            .map_err(|_| Unavailable)?
            .join()
            .await
            .map_err(|_| Unavailable)?
    }
}

//! Catalog reads as owned tasks holding a managed-library pin.

use super::{Catalog, CatalogFailure, CatalogSnapshot, VersionsResponse, installed_versions};
use crate::{
    library::LibraryLifecycle,
    tasks::{CancellationToken, TaskOwner},
};
use axial_minecraft::{
    LoaderBuildRecord, LoaderCatalogState, LoaderComponentId, LoaderError, LoaderGameVersion,
    loaders, managed_path::ManagedLibraryOperation,
};
use serde::Serialize;
use std::{future::Future, sync::Arc};

/// Catalog reads are unavailable; callers state which catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unavailable;

#[derive(Debug, Serialize)]
pub struct LoaderVersionsResponse {
    pub catalog_state: LoaderCatalogState,
    pub versions: Vec<LoaderGameVersion>,
}

#[derive(Debug, Serialize)]
pub struct LoaderBuildsResponse {
    pub builds: Vec<LoaderBuildRecord>,
    pub catalog_state: LoaderCatalogState,
}

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

    pub async fn snapshot(&self) -> Result<CatalogSnapshot, Unavailable> {
        self.read(|catalog, operation, cancel| async move {
            Ok(catalog.snapshot(&operation, &cancel).await)
        })
        .await
    }

    pub async fn loader_versions(
        &self,
        component: LoaderComponentId,
    ) -> Result<LoaderVersionsResponse, Unavailable> {
        self.read(move |catalog, operation, cancel| async move {
            let (mut versions, catalog_state) = loaders::fetch_supported_versions_cancellable(
                &operation,
                component,
                cancel.cancelled(),
            )
            .await
            .map_err(|_| Unavailable)?;
            let metadata = catalog
                .enrich_loader_versions(&operation, &mut versions, &cancel)
                .await;
            if metadata.failure == Some(CatalogFailure::Cancelled) {
                return Err(Unavailable);
            }
            Ok(LoaderVersionsResponse {
                catalog_state,
                versions,
            })
        })
        .await
    }

    pub async fn loader_builds(
        &self,
        component: LoaderComponentId,
        minecraft_version: String,
    ) -> Result<LoaderBuildsResponse, Unavailable> {
        self.read(move |_, operation, cancel| async move {
            let (builds, catalog_state) = loaders::fetch_builds_cancellable(
                &operation,
                component,
                &minecraft_version,
                cancel.cancelled(),
            )
            .await
            .map_err(|_: LoaderError| Unavailable)?;
            Ok(LoaderBuildsResponse {
                builds,
                catalog_state,
            })
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

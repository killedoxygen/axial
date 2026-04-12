use croopor_minecraft::{LoaderBuildRecord, LoaderCatalogState, LoaderComponentRecord};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct LoaderComponentsResponse {
    pub components: Vec<LoaderComponentRecord>,
}

#[derive(Debug, Serialize)]
pub struct LoaderBuildsResponse {
    pub builds: Vec<LoaderBuildRecord>,
    pub catalog: LoaderCatalogState,
}

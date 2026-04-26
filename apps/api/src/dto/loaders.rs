use croopor_minecraft::{
    LoaderBuildRecord, LoaderCatalogState, LoaderComponentRecord, LoaderGameVersion,
};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct LoaderComponentsResponse {
    pub components: Vec<LoaderComponentRecord>,
}

#[derive(Debug, Serialize)]
pub struct LoaderGameVersionsResponse {
    pub versions: Vec<LoaderGameVersion>,
    pub catalog: LoaderCatalogState,
}

#[derive(Debug, Serialize)]
pub struct LoaderBuildsResponse {
    pub builds: Vec<LoaderBuildRecord>,
    pub catalog: LoaderCatalogState,
}

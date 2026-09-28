pub(crate) const MAX_PROVIDER_METADATA_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_PROVIDER_DETAIL_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_DETAIL_BODY_BYTES: usize = 4 * 1024 * 1024;

pub(crate) const MAX_RESOLUTION_SELECTIONS: usize = 256;
pub(crate) const MAX_RESOLUTION_NODES: usize = 256;
pub(crate) const MAX_RESOLUTION_DEPTH: usize = 32;
pub(crate) const MAX_DEPENDENCIES_PER_NODE: usize = 256;
pub(crate) const MAX_RESOLUTION_EDGES: usize = 4096;
pub(crate) const MAX_RESOLUTION_QUEUE: usize = MAX_RESOLUTION_SELECTIONS + MAX_RESOLUTION_EDGES;
pub(crate) const MAX_RESOLUTION_CONFLICTS: usize = 256;
pub(crate) const MAX_RESOLUTION_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

pub(crate) const MAX_CONTENT_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const MAX_CONTENT_GRAPH_BYTES: u64 = 512 * 1024 * 1024;

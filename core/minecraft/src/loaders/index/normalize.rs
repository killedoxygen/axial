use crate::loaders::types::LoaderVersionIndex;

pub fn normalize_build_index(mut index: LoaderVersionIndex) -> LoaderVersionIndex {
    if !index.builds.iter().any(|build| build.latest)
        && let Some(first) = index.builds.first_mut()
    {
        first.latest = true;
    }

    if !index.builds.iter().any(|build| build.recommended)
        && let Some(first_stable) = index.builds.iter_mut().find(|build| build.stable)
    {
        first_stable.recommended = true;
    }

    index
}

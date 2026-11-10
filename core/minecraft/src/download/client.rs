const MIN_LIBRARY_DOWNLOAD_CONCURRENCY: usize = 4;
const MAX_LIBRARY_DOWNLOAD_CONCURRENCY: usize = 16;
const LIBRARY_DOWNLOADS_PER_CORE: usize = 2;
const MIN_ASSET_DOWNLOAD_CONCURRENCY: usize = 8;
const MAX_ASSET_DOWNLOAD_CONCURRENCY: usize = 32;
const ASSET_DOWNLOADS_PER_CORE: usize = 4;

pub(super) fn library_download_concurrency() -> usize {
    adaptive_download_concurrency(
        available_parallelism(),
        MIN_LIBRARY_DOWNLOAD_CONCURRENCY,
        MAX_LIBRARY_DOWNLOAD_CONCURRENCY,
        LIBRARY_DOWNLOADS_PER_CORE,
    )
}

pub(super) fn asset_download_concurrency() -> usize {
    adaptive_download_concurrency(
        available_parallelism(),
        MIN_ASSET_DOWNLOAD_CONCURRENCY,
        MAX_ASSET_DOWNLOAD_CONCURRENCY,
        ASSET_DOWNLOADS_PER_CORE,
    )
}

fn available_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(MIN_LIBRARY_DOWNLOAD_CONCURRENCY)
}

pub(super) fn adaptive_download_concurrency(
    cores: usize,
    minimum: usize,
    maximum: usize,
    per_core: usize,
) -> usize {
    cores
        .saturating_mul(per_core)
        .clamp(minimum, maximum.max(minimum))
}

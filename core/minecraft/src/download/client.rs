use std::sync::OnceLock;
use std::time::Duration;

const MIN_LIBRARY_DOWNLOAD_CONCURRENCY: usize = 4;
const MAX_LIBRARY_DOWNLOAD_CONCURRENCY: usize = 16;
const LIBRARY_DOWNLOADS_PER_CORE: usize = 2;
const MIN_ASSET_DOWNLOAD_CONCURRENCY: usize = 8;
const MAX_ASSET_DOWNLOAD_CONCURRENCY: usize = 32;
const ASSET_DOWNLOADS_PER_CORE: usize = 4;
const DOWNLOAD_CLIENT_MAX_IDLE_PER_HOST: usize = MAX_ASSET_DOWNLOAD_CONCURRENCY;
const DOWNLOAD_CLIENT_CONNECT_TIMEOUT_SECS: u64 = 20;
const DOWNLOAD_CLIENT_READ_TIMEOUT_SECS: u64 = 120;
const DOWNLOAD_CLIENT_POOL_IDLE_TIMEOUT_SECS: u64 = 120;
const DOWNLOAD_CLIENT_TCP_KEEPALIVE_SECS: u64 = 60;

pub(super) fn build_http_client(read_timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("croopor/0.3")
        .connect_timeout(Duration::from_secs(DOWNLOAD_CLIENT_CONNECT_TIMEOUT_SECS))
        .read_timeout(read_timeout)
        .pool_max_idle_per_host(DOWNLOAD_CLIENT_MAX_IDLE_PER_HOST)
        .pool_idle_timeout(Duration::from_secs(DOWNLOAD_CLIENT_POOL_IDLE_TIMEOUT_SECS))
        .tcp_keepalive(Duration::from_secs(DOWNLOAD_CLIENT_TCP_KEEPALIVE_SECS))
        .build()
        .expect("download HTTP client configuration should be valid")
}

pub(super) fn standard_minecraft_download_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| build_http_client(Duration::from_secs(DOWNLOAD_CLIENT_READ_TIMEOUT_SECS)))
        .clone()
}

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

use axial_api::app::{ApiServerShutdownError, ServerHandle};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct DesktopState {
    version: String,
}

impl DesktopState {
    pub fn new(version: String) -> Self {
        Self { version }
    }

    pub fn version(&self) -> &str {
        &self.version
    }
}

#[derive(Clone)]
pub struct ApiRuntimeState {
    server: Arc<ServerHandle>,
    exit_started: Arc<AtomicBool>,
}

impl ApiRuntimeState {
    pub fn new(server: ServerHandle) -> Self {
        Self {
            server: Arc::new(server),
            exit_started: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.server.addr
    }

    pub async fn wait(&self) -> Result<(), ApiServerShutdownError> {
        self.server.wait().await
    }

    pub async fn shutdown(&self) -> Result<(), ApiServerShutdownError> {
        self.server.shutdown().await
    }

    pub fn exit_started(&self) -> bool {
        self.exit_started.load(Ordering::Acquire)
    }

    pub fn mark_exit_started(&self) {
        self.exit_started.store(true, Ordering::Release);
    }
}

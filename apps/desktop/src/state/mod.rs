use axial_api::app::{ApiServerShutdownError, ServerHandle};
use std::net::SocketAddr;
use std::sync::Arc;

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
}

impl ApiRuntimeState {
    pub fn new(server: ServerHandle) -> Self {
        Self {
            server: Arc::new(server),
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
}

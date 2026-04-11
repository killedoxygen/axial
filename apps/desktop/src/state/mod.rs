use std::net::SocketAddr;

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

pub struct ApiRuntimeState {
    addr: SocketAddr,
}

impl ApiRuntimeState {
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

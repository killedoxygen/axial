pub const DESKTOP_API_STOPPED: &str = "croopor:desktop:api-stopped";

pub fn install_progress(install_id: &str) -> String {
    format!("croopor:install:{install_id}:progress")
}

pub fn loader_install_progress(install_id: &str) -> String {
    format!("croopor:loader-install:{install_id}:progress")
}

pub fn launch_status(session_id: &str) -> String {
    format!("croopor:launch:{session_id}:status")
}

pub fn launch_log(session_id: &str) -> String {
    format!("croopor:launch:{session_id}:log")
}

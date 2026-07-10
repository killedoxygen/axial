pub const DESKTOP_API_STOPPED: &str = "axial:desktop:api-stopped";
pub const DESKTOP_CLOSE_BLOCKED: &str = "axial:desktop:close-blocked";

pub fn install_progress(install_id: &str) -> String {
    format!("axial:install:{install_id}:progress")
}

pub fn loader_install_progress(install_id: &str) -> String {
    format!("axial:loader-install:{install_id}:progress")
}

pub fn launch_status(session_id: &str) -> String {
    format!("axial:launch:{session_id}:status")
}

pub fn launch_log(session_id: &str) -> String {
    format!("axial:launch:{session_id}:log")
}

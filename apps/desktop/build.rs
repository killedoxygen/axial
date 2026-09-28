fn main() {
    const COMMANDS: &[&str] = &[
        "app_version",
        "api_transport_bootstrap",
        "desktop_chrome",
        "window_minimize",
        "window_toggle_maximize",
        "window_is_maximized",
        "window_start_dragging",
        "window_set_resize_background",
        "window_close",
        "app_restart",
        "pending_interface_preferences",
        "complete_interface_preferences",
        "microsoft_sign_in",
        "pick_skin_file",
        "consume_skin_drop",
        "pick_import_profile",
        "pick_import_instance_source",
        "forget_import_profile",
        #[cfg(debug_assertions)]
        "app_reset",
    ];
    let manifest = tauri_build::AppManifest::new().commands(COMMANDS);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("desktop capability manifest must be valid");
}

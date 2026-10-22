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
        "export_saved_skin_png",
        #[cfg(debug_assertions)]
        "app_reset",
    ];
    // Clean Windows installs lack the VC++ redistributable; tauri-build 2.6 links its runtime
    // statically only when this variable is set.
    // SAFETY: the build script is single-threaded and nothing else reads the environment yet.
    unsafe { std::env::set_var("STATIC_VCRUNTIME", "true") };
    let manifest = tauri_build::AppManifest::new().commands(COMMANDS);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("desktop capability manifest must be valid");
}

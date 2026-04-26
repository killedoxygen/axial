pub fn uses_legacy_fml_support(minecraft_version: &str) -> bool {
    minecraft_version.starts_with("1.5.")
        || minecraft_version.starts_with("1.6.")
        || minecraft_version.starts_with("1.7.")
        || minecraft_version.starts_with("1.8.")
        || minecraft_version.starts_with("1.9.")
        || minecraft_version.starts_with("1.10.")
        || minecraft_version.starts_with("1.11.")
        || minecraft_version.starts_with("1.12.")
}

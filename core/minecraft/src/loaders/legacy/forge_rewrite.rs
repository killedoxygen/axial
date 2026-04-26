pub fn is_legacy_forge_coordinate(path: &str) -> bool {
    path.contains("minecraftforge") || path.contains(":forge:")
}

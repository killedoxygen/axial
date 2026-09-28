const platforms = Object.freeze(["linux", "windows", "macos"]);

function assetCapability(name, capabilityId, timeoutMs = 10_000) {
  return Object.freeze({
    scenario_id: `CP-OA-${name}`,
    proof_id: `CAP-OA-${name}`,
    capability_id: capabilityId,
    owner_domain: "offline-assets",
    toolchain_profile: "frontend",
    allowed_platforms: platforms,
    timeout_ms: timeoutMs,
    module_url: new URL(`./scenarios/${capabilityId}.mjs`, import.meta.url),
    evidence_path: `evidence/capabilities/CAP-OA-${name}.json`,
  });
}

export const capabilityRegistry = Object.freeze([
  assetCapability("FONTS", "asset-fonts"),
  assetCapability("ICONS", "asset-icons"),
  assetCapability("LOADER-MARKS", "asset-loader-marks"),
  assetCapability("PROVENANCE", "asset-provenance"),
  assetCapability("FRONTEND", "frontend-generation", 30_000),
  Object.freeze({
    scenario_id: "CP-ANCHORED-FILESYSTEM",
    proof_id: "CAP-ANCHORED-FILESYSTEM",
    capability_id: "anchored-filesystem",
    owner_domain: "anchored-filesystem",
    toolchain_profile: "rust",
    allowed_platforms: Object.freeze(["linux"]),
    timeout_ms: 300_000,
    module_url: new URL("./scenarios/anchored-filesystem.mjs", import.meta.url),
    evidence_path: "evidence/capabilities/CAP-ANCHORED-FILESYSTEM.json",
  }),
]);

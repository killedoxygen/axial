const platforms = Object.freeze(["linux", "windows", "macos"]);

function assetCapability(name, capabilityId, timeoutMs = 10_000) {
  return Object.freeze({
    scenario_id: `CP-OA-${name}`,
    proof_id: `CAP-OA-${name}`,
    capability_id: capabilityId,
    owner_phase: "P00",
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
    scenario_id: "CP-P01-B02-ANCHORED-FS",
    proof_id: "CAP-P01-B02-ANCHORED-FS",
    capability_id: "p01-b02-anchored-fs",
    owner_phase: "P01",
    toolchain_profile: "rust",
    allowed_platforms: Object.freeze(["linux"]),
    timeout_ms: 300_000,
    module_url: new URL("./scenarios/p01-b02-anchored-fs.mjs", import.meta.url),
    evidence_path: "evidence/capabilities/CAP-P01-B02-ANCHORED-FS.json",
  }),
]);

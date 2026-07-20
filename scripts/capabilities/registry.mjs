const platforms = Object.freeze(["linux", "windows", "macos"]);

function assetCapability(name, capabilityId) {
  return Object.freeze({
    scenario_id: `CP-OA-${name}`,
    proof_id: `CAP-OA-${name}`,
    capability_id: capabilityId,
    owner_phase: "P00",
    allowed_platforms: platforms,
    timeout_ms: 10_000,
    module_url: new URL(`./scenarios/${capabilityId}.mjs`, import.meta.url),
    evidence_path: `evidence/capabilities/CAP-OA-${name}.json`,
  });
}

export const capabilityRegistry = Object.freeze([
  assetCapability("FONTS", "asset-fonts"),
  assetCapability("ICONS", "asset-icons"),
  assetCapability("LOADER-MARKS", "asset-loader-marks"),
  assetCapability("PROVENANCE", "asset-provenance"),
]);

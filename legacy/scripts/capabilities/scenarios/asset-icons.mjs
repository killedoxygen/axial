import { currentReceipt, proveIcons, scenarioResult } from "./_asset-proof.mjs";

export const scenario = Object.freeze({
  scenario_id: "CP-OA-ICONS",
  proof_id: "CAP-OA-ICONS",
  capability_id: "asset-icons",
});

export async function runScenario(context) {
  return scenarioResult(
    "selected-assets",
    await proveIcons(context.repository_root),
  );
}

export async function readCurrentReceipts(context) {
  return currentReceipt(context, "selected-assets", proveIcons);
}

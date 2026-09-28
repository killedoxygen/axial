import { currentReceipt, proveFonts, scenarioResult } from "./_asset-proof.mjs";

export const scenario = Object.freeze({
  scenario_id: "CP-OA-FONTS",
  proof_id: "CAP-OA-FONTS",
  capability_id: "asset-fonts",
});

export async function runScenario(context) {
  return scenarioResult(
    "font-assets",
    await proveFonts(context.repository_root),
  );
}

export async function readCurrentReceipts(context) {
  return currentReceipt(context, "font-assets", proveFonts);
}

import {
  currentReceipt,
  proveProvenance,
  scenarioResult,
} from "./_asset-proof.mjs";

export const scenario = Object.freeze({
  scenario_id: "CP-OA-PROVENANCE",
  proof_id: "CAP-OA-PROVENANCE",
  capability_id: "asset-provenance",
});

export async function runScenario(context) {
  return scenarioResult(
    "strict-provenance",
    await proveProvenance(context.repository_root),
  );
}

export async function readCurrentReceipts(context) {
  return currentReceipt(context, "strict-provenance", proveProvenance);
}

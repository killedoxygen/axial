import {
  currentReceipt,
  proveLoaderMarks,
  scenarioResult,
} from "./_asset-proof.mjs";

export const scenario = Object.freeze({
  scenario_id: "CP-OA-LOADER-MARKS",
  proof_id: "CAP-OA-LOADER-MARKS",
  capability_id: "asset-loader-marks",
});

export async function runScenario(context) {
  return scenarioResult(
    "neutral-loader-marks",
    await proveLoaderMarks(context.repository_root),
  );
}

export async function readCurrentReceipts(context) {
  return currentReceipt(context, "neutral-loader-marks", proveLoaderMarks);
}

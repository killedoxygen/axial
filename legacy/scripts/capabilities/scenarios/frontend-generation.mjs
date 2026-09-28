import { createHash } from "node:crypto";
import { execFile as execFileCallback } from "node:child_process";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { promisify } from "node:util";

import { verifyFrontendGeneration } from "../../../frontend/build-generation.mjs";
import { currentReceipt, scenarioResult } from "./_asset-proof.mjs";

export const scenario = Object.freeze({
  scenario_id: "CP-OA-FRONTEND",
  proof_id: "CAP-OA-FRONTEND",
  capability_id: "frontend-generation",
});
const execFile = promisify(execFileCallback);

async function proveFrontend(repositoryRoot) {
  const frontendRoot = path.join(repositoryRoot, "frontend");
  const outputRoot = path.join(frontendRoot, "dist");
  const manifest = await verifyFrontendGeneration(outputRoot, {
    publicManifestPath: path.join(frontendRoot, "public-assets.json"),
    budgetPath: path.join(frontendRoot, "bundle-budgets.json"),
  });
  const manifestBytes = await readFile(
    path.join(outputRoot, "generation.json"),
  );
  return {
    file_count: manifest.files.length,
    generation_id: manifest.generation_id,
    manifest_sha256: createHash("sha256").update(manifestBytes).digest("hex"),
    packaged_payload_bytes: manifest.metrics.packaged_payload,
  };
}

export async function runScenario(context) {
  return scenarioResult(
    "atomic-frontend-generation",
    await rebuildAndProveFrontend(context.repository_root),
  );
}

export async function rebuildAndProveFrontend(
  repositoryRoot,
  buildHook = async (root) => {
    await execFile(
      process.execPath,
      [path.join(root, "frontend/esbuild.mjs")],
      {
        cwd: root,
        timeout: 25_000,
        killSignal: "SIGKILL",
        maxBuffer: 1024 * 1024,
      },
    );
  },
) {
  await buildHook(repositoryRoot);
  return proveFrontend(repositoryRoot);
}

export async function readCurrentReceipts(context) {
  return currentReceipt(context, "atomic-frontend-generation", proveFrontend);
}

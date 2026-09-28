import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { readFile } from 'node:fs/promises';

import { parseBundleBudgets, verifyFrontendGeneration } from './build-generation.mjs';

const frontendRoot = fileURLToPath(new URL('.', import.meta.url));

try {
  const manifest = await verifyFrontendGeneration(path.join(frontendRoot, 'dist'), {
    publicManifestPath: path.join(frontendRoot, 'public-assets.json'),
    budgetPath: path.join(frontendRoot, 'bundle-budgets.json'),
  });
  console.log(`verified frontend generation ${manifest.generation_id.slice(0, 12)}`);
  const maximum = parseBundleBudgets(await readFile(path.join(frontendRoot, 'bundle-budgets.json'), 'utf8'));
  for (const [metric, actual] of Object.entries(manifest.metrics)) {
    console.log(`  ${metric} ${actual}/${maximum[metric]}`);
  }
} catch (error) {
  console.error(error instanceof Error ? error.message : error);
  process.exitCode = 1;
}

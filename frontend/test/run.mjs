import { readdir, rm } from 'node:fs/promises';
import { isAbsolute, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { mkdtemp } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const frontendRoot = fileURLToPath(new URL('../', import.meta.url));
const testRoot = resolve(frontendRoot, 'test');
const outputRoot = await mkdtemp(resolve(tmpdir(), 'axial-frontend-test-'));

try {
  const entries = (await readdir(testRoot, { withFileTypes: true }))
    .filter((entry) => entry.isFile() && /\.test\.(?:mjs|ts)$/.test(entry.name))
    .map((entry) => resolve(testRoot, entry.name))
    .sort();

  if (entries.length === 0) throw new Error('No frontend tests found');

  const result = await build({
    absWorkingDir: frontendRoot,
    entryPoints: entries,
    bundle: true,
    define: {
      __AXIAL_ENABLE_DEV_LAB__: 'false',
      __AXIAL_MOCK_API__: 'false',
      __AXIAL_WEB_API_BASE__: '""',
    },
    entryNames: '[name]',
    format: 'esm',
    metafile: true,
    outExtension: { '.js': '.mjs' },
    outdir: outputRoot,
    platform: 'node',
    sourcemap: 'inline',
  });

  const outputs = Object.entries(result.metafile.outputs)
    .filter(([, metadata]) => metadata.entryPoint)
    .map(([path]) => (isAbsolute(path) ? path : resolve(frontendRoot, path)))
    .sort();
  const testRun = spawnSync(process.execPath, ['--test', ...outputs], { stdio: 'inherit' });
  if (testRun.error) throw testRun.error;
  process.exitCode = testRun.status ?? 1;
} finally {
  await rm(outputRoot, { force: true, recursive: true });
}

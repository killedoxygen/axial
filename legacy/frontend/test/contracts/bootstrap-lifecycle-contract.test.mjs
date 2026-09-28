import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { basename, resolve } from 'node:path';
import test from 'node:test';

const repositoryRoot = basename(process.cwd()) === 'frontend' ? resolve(process.cwd(), '..') : process.cwd();
/** @param {string} path */
const read = (path) => readFile(resolve(repositoryRoot, path), 'utf8');

/** @param {string} source @param {string[]} markers */
function ordered(source, markers) {
  let cursor = -1;
  for (const marker of markers) {
    const next = source.indexOf(marker, cursor + 1);
    assert.notEqual(next, -1, `missing ordered marker: ${marker}`);
    assert.ok(next > cursor, `out-of-order marker: ${marker}`);
    cursor = next;
  }
}

test('bootstrap-lifecycle frontend bootstrap gates Ready and retains an explicit retry terminal', async () => {
  const [bootstrap, main, splash] = await Promise.all([
    read('frontend/src/bootstrap.ts'),
    read('frontend/src/main.tsx'),
    read('frontend/src/shell/BootSplash.tsx'),
  ]);
  assert.equal((main.match(/startApplicationBootstrap\(\)/g) ?? []).length, 1);
  assert.match(bootstrap, /api\('GET', '\/status'\)\.then\(launcherStatusResponse\)/);
  assert.doesNotMatch(bootstrap, /api\('GET', '\/status'\)\.then\(launcherStatusResponse\)\s*\.catch/);
  assert.match(bootstrap, /if \(statusRes\.setup_required\)/);
  assert.match(bootstrap, /if \(setupError\) throw new Error\(setupError\)/);
  assert.match(bootstrap, /refreshInstallQueue[\s\S]*?\.catch\(/);
  ordered(bootstrap, [
    "api('GET', '/status').then(launcherStatusResponse)",
    "api('GET', '/versions').then(versionsResponse)",
    'instances.value = instancesRes.instances',
    "bootstrapState.value = 'ready'",
  ]);
  assert.match(splash, /role=\{state === 'error' \? 'alert' : 'status'\}/);
  assert.match(splash, /onClick=\{startApplicationBootstrap\}/);
  assert.match(splash, />\s*Retry\s*</);
});

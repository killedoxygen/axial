import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import { pathToFileURL } from 'node:url';

const repositoryRoot = basename(process.cwd()) === 'frontend' ? resolve(process.cwd(), '..') : process.cwd();
/** @param {string} filePath */
const read = (filePath) => readFile(resolve(repositoryRoot, filePath), 'utf8');
/** @param {string | Buffer} bytes */
const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

test('Worlds masks stay in one closed public, provenance, build, and rights inventory', async () => {
  const { parseProvenanceManifest, verifyAssetProvenance } = await import(
    pathToFileURL(resolve(repositoryRoot, 'scripts/verify-assets.mjs')).href
  );
  const [publicManifest, provenanceSource, esbuild, rightsDoc, base, accent] = await Promise.all([
    read('frontend/public-assets.json').then(JSON.parse),
    read('assets/provenance.json'),
    read('frontend/esbuild.mjs'),
    read('docs/ICON-ASSETS.md'),
    read('frontend/static/worlds-empty-base.svg'),
    read('frontend/static/worlds-empty-accent.svg'),
  ]);
  const parsed = parseProvenanceManifest(provenanceSource);
  const owner = parsed.manifest.assets.find(
    /** @param {{ id: string }} asset */
    (asset) => asset.id === 'worlds-empty-art',
  );
  assert.ok(owner);
  assert.equal(owner.source.kind, 'repository-owned');
  assert.equal(owner.rights.basis, 'repository-owned');
  assert.equal(owner.rights.evidence.locator, 'docs/ICON-ASSETS.md');
  assert.deepEqual(
    owner.files.map(
      /** @param {{ path: string, sha256: string }} file */
      (file) => [file.path, file.sha256],
    ),
    [
      ['frontend/static/worlds-empty-accent.svg', sha256(accent)],
      ['frontend/static/worlds-empty-base.svg', sha256(base)],
    ],
  );
  assert.deepEqual(publicManifest.files.slice(-2), ['worlds-empty-accent.svg', 'worlds-empty-base.svg']);
  assert.match(esbuild, /external: \['fonts\/\*', 'worlds-empty-accent\.svg', 'worlds-empty-base\.svg'\]/);
  assert.match(rightsDoc, /## Worlds empty art/);
  assert.match(rightsDoc, /separate base\/accent geometry/);
  await verifyAssetProvenance({ root: repositoryRoot });
});

test('the local static-check gate runs semantic lint once without duplicating its TypeScript check', async () => {
  const taskfile = await read('Taskfile.yml');
  const checkTask = /^  check:\n([\s\S]*?)(?=^  [a-z][a-z0-9:-]*:\n)/m.exec(taskfile)?.[1];
  const semanticTask = /^  frontend:lint:semantic:\n([\s\S]*?)(?=^  [a-z][a-z0-9:-]*:\n)/m.exec(taskfile)?.[1];
  assert.ok(checkTask);
  assert.ok(semanticTask);
  assert.equal((checkTask.match(/task: frontend:lint:semantic/g) ?? []).length, 1);
  assert.equal((checkTask.match(/task: frontend:test/g) ?? []).length, 1);
  assert.doesNotMatch(checkTask, /\btsc\b|pnpm --dir frontend run lint(?:\s|$)/);
  assert.match(semanticTask, /^    internal: true\n    cmds:\n      - pnpm --dir frontend run lint:semantic\n\n$/);
  assert.doesNotMatch(semanticTask, /\btsc\b|task: frontend:test/);
});

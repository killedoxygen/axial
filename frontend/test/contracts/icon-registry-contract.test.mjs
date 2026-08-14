import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { basename, resolve } from 'node:path';
import test from 'node:test';

const frontendRoot = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const repositoryRoot = resolve(frontendRoot, '..');
const expectedNames = [
  'activity',
  'archive',
  'arrow-left',
  'arrow-right',
  'arrow-up',
  'check',
  'check-circle',
  'chevron-left',
  'chevron-right',
  'chevron-down',
  'chevron-up',
  'circle-dashed',
  'clock',
  'compass',
  'copy',
  'cube',
  'dots',
  'download',
  'edit',
  'expand',
  'folder',
  'globe',
  'headphones',
  'home',
  'image',
  'info',
  'alert',
  'keyboard',
  'minus',
  'music',
  'music-off',
  'palette',
  'puzzle',
  'play',
  'player-skip',
  'plus',
  'rectangle',
  'refresh',
  'search',
  'settings',
  'sliders',
  'shield-check',
  'shield-person',
  'stack',
  'stop',
  'tag',
  'terminal',
  'trash',
  'user',
  'volume',
  'volume-off',
  'x',
];

test('icon source has one total 52-name registry and forwards stroke to Lucide', async () => {
  const source = await readFile(resolve(frontendRoot, 'src/ui/Icons.tsx'), 'utf8');
  const registry = /const REGISTRY = \{([\s\S]*?)\n\} as const satisfies Record<string, LucideIcon>;/.exec(source);
  assert.ok(registry);
  const names = [...registry[1].matchAll(/^\s+(?:'([^']+)'|([a-z][a-z-]*)): [A-Z][A-Za-z0-9]*,$/gm)].map(
    (match) => match[1] ?? match[2],
  );
  assert.deepEqual(names, expectedNames);
  assert.equal(new Set(names).size, 52);
  assert.match(source, /name: IconName;/);
  assert.match(source, /strokeWidth=\{stroke\}/);
  assert.doesNotMatch(source, /Sparkles|openai-icons-subset|@openai\/apps-sdk-ui/);
});

test('shipped Lucide notice is byte-exact and tied to immutable provenance', async () => {
  const manifest = JSON.parse(await readFile(resolve(frontendRoot, 'package.json'), 'utf8'));
  assert.equal(manifest.dependencies['lucide-preact'], '1.25.0');
  assert.equal(manifest.dependencies['@openai/apps-sdk-ui'], undefined);
  const notice = await readFile(resolve(frontendRoot, 'static/licenses/Lucide-ISC.txt'));
  const hash = createHash('sha256').update(notice).digest('hex');
  assert.equal(hash, 'b495047bd93a9b06913511076f504daba17d5bbeb3e0650f3bb53a4220329c57');
  /** @type {{ assets: Array<Record<string, any>> }} */
  const provenance = JSON.parse(await readFile(resolve(repositoryRoot, 'assets/provenance.json'), 'utf8'));
  const lucide = provenance.assets.find((asset) => asset.id === 'lucide-icons');
  assert.ok(lucide);
  assert.equal(lucide.source.revision, '5136572c10214634858fcf5f726b2a9d26683918');
  assert.equal(lucide.rights.basis, 'isc-and-mit');
  assert.equal(lucide.rights.evidence.sha256, hash);
  assert.equal(lucide.files[0].sha256, hash);
  assert.equal(lucide.files[0].upstream_sha256, hash);
});

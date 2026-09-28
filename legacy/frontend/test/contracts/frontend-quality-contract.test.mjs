import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { access, mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { basename, join, resolve } from 'node:path';
import test from 'node:test';

const frontendRoot = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const biomeConfigPath = resolve(frontendRoot, 'biome.json');
const biomeCli = resolve(frontendRoot, 'node_modules/@biomejs/biome/bin/biome');
const typeScriptCli = resolve(frontendRoot, 'node_modules/typescript/bin/tsc');
/** @param {string} filePath */
const read = (filePath) => readFile(resolve(frontendRoot, filePath), 'utf8');

/**
 * @param {string} cli
 * @param {string[]} args
 * @param {string} cwd
 */
function runNodeCli(cli, args, cwd) {
  return spawnSync(process.execPath, [cli, ...args], {
    cwd,
    encoding: 'utf8',
    timeout: 30_000,
  });
}

/**
 * @param {import('node:child_process').SpawnSyncReturns<string>} result
 * @returns {{ diagnostics: Array<{ category: string }> }}
 */
function biomeReport(result) {
  assert.equal(result.error, undefined);
  assert.ok(result.stdout, `Biome produced no JSON report: ${result.stderr}`);
  return JSON.parse(result.stdout);
}

test('status presentation is one exact private eight-state Lucide mapping', async () => {
  const [topbar, shell, base, discover] = await Promise.all([
    read('src/shell/Topbar.tsx'),
    read('src/shell/shell.css'),
    read('src/base.css'),
    read('src/views/discover/discover.css'),
  ]);
  const mapping = /const STATUS_ICON_BY_STATE = \{([\s\S]*?)\n\} as const satisfies Record<GlyphState, IconName>;/.exec(
    topbar,
  );
  assert.ok(mapping);
  assert.deepEqual(
    Object.fromEntries([...mapping[1].matchAll(/^\s+(\w+): '([^']+)',$/gm)].map((entry) => [entry[1], entry[2]])),
    {
      idle: 'circle-dashed',
      preparing: 'refresh',
      monitoring: 'activity',
      playing: 'play',
      stopping: 'stop',
      downloading: 'download',
      queued: 'clock',
      failed: 'alert',
    },
  );
  assert.match(topbar, /<Icon name=\{STATUS_ICON_BY_STATE\[state\]\} size=\{14\} stroke=\{2\} \/>/);
  assert.doesNotMatch(topbar, /export (?:type |const |function )?(?:GlyphState|STATUS_ICON_BY_STATE|StatusGlyph)/);
  const statusGlyph = /function StatusGlyph[^\n]*\{([\s\S]*?)\n\}/.exec(topbar);
  assert.ok(statusGlyph);
  assert.doesNotMatch(statusGlyph[1], /<span \/>/);
  assert.doesNotMatch(`${topbar}\n${shell}`, /cp-status-glyph|cp-glyph-/);
  assert.equal((`${base}\n${discover}\n${shell}`.match(/@keyframes cp-spin/g) ?? []).length, 1);
  assert.match(shell, /\.cp-status-icon\[data-state='preparing'\] svg \{\s*animation: cp-spin/);
  assert.match(shell, /prefers-reduced-motion:[\s\S]*?\.cp-status-icon\[data-state='preparing'\] svg/);
  assert.match(shell, /\.cp-topbar\[data-calm='true'\] \.cp-status-icon\[data-state='preparing'\] svg/);
});

test('Worlds art is a two-mask static projection with no inline component tree', async () => {
  const [pane, css, base, accent] = await Promise.all([
    read('src/views/instance/tabs/WorldsPane.tsx'),
    read('src/views/instance/instance.css'),
    read('static/worlds-empty-base.svg'),
    read('static/worlds-empty-accent.svg'),
  ]);
  await assert.rejects(access(resolve(frontendRoot, 'src/views/instance/components/worlds-empty-art.tsx')), /ENOENT/);
  assert.match(pane, /<div class="cp-worlds-empty-art" aria-hidden="true" \/>/);
  for (const file of ['worlds-empty-base.svg', 'worlds-empty-accent.svg']) {
    assert.match(css, new RegExp(`-webkit-mask-image: url\\('${file.replace('.', '\\.')}\\'\\)`));
    assert.match(css, new RegExp(`mask-image: url\\('${file.replace('.', '\\.')}\\'\\)`));
  }
  assert.match(css, /\.cp-worlds-empty-art \{[\s\S]*?width: 132px;[\s\S]*?aspect-ratio: 180 \/ 172\.3;/);
  assert.match(css, /\.cp-worlds-empty-art::before,[\s\S]*?background: var\(--text-mute\);/);
  assert.match(css, /\.cp-worlds-empty-art::after \{[\s\S]*?background: var\(--accent\);/);
  for (const svg of [base, accent]) {
    assert.match(svg, /^<svg[^>]*viewBox="0 0 180 172\.3"[^>]*>/);
    assert.doesNotMatch(svg, /<script|<foreignObject|(?:href|src)=|url\(|data:|currentColor/i);
  }
  /** @param {string} svg */
  const geometry = (svg) => [...svg.matchAll(/<(?:polygon|polyline|line|path)\b[^>]*\/>/g)].map((entry) => entry[0]);
  assert.equal(geometry(base).length, 37);
  assert.equal(geometry(accent).length, 3);
  assert.equal(
    geometry(base).some((node) => /122\.4 29\.7/.test(node)),
    false,
  );
  assert.equal(
    geometry(accent).some((node) => /122\.4 29\.7/.test(node)),
    true,
  );
});

test('dead facades and JavaScript-owned input focus state stay deleted', async () => {
  for (const file of ['src/machine.ts', 'src/instance.ts']) {
    await assert.rejects(access(resolve(frontendRoot, file)), /ENOENT/);
  }
  const [actions, store, utils, atoms, css, selection, loaderTypes, uiTypes, state] = await Promise.all([
    read('src/actions.ts'),
    read('src/store.ts'),
    read('src/utils.ts'),
    read('src/ui/Atoms.tsx'),
    read('src/ui/atoms.css'),
    read('src/ui/SelectionActionTray.tsx'),
    read('src/types-loader.ts'),
    read('src/types-ui.ts'),
    read('src/state.ts'),
  ]);
  assert.doesNotMatch(actions, /setVersions|setInstances|setCatalog|setSearch|setFilter|navigate\(/);
  assert.doesNotMatch(store, /catalog|currentPage|searchQuery|sidebarFilter|filteredInstances|versionMap/);
  assert.doesNotMatch(utils, /parseVersionDisplay|matchesVersionFilter|formatLoaderBuildLabel|setPage/);
  assert.doesNotMatch(`${atoms}\n${css}`, /cp-field--focused|setFocus\(/);
  assert.match(css, /\.cp-field:focus-within/);
  assert.match(selection, /selection: Pick<SelectionState<unknown>/);
  assert.doesNotMatch(selection, /shown\??:|count\??:|onClear\??:|onSelectAll\??:/);
  assert.doesNotMatch(
    loaderTypes,
    /LoaderBuildSubjectKind|LoaderType|LoaderTerm|LoaderSelection|LoaderBuildMetadata|LoaderBuildRecord|component_name|build_meta/,
  );
  assert.doesNotMatch(`${uiTypes}\n${state}`, /logHeight|collapsedGroups/);
  assert.match(store, /export const collapsedGroups = signal<Record<string, boolean>>\(\{\}\);/);
});

test('lint policy stays narrow and the production source has no configured diagnostics', async () => {
  const config = JSON.parse(await readFile(biomeConfigPath, 'utf8'));
  assert.deepEqual(config, {
    $schema: 'https://biomejs.dev/schemas/2.5.4/schema.json',
    files: { includes: ['src/**/*'] },
    formatter: { enabled: false },
    assist: { enabled: false },
    linter: {
      enabled: true,
      rules: {
        recommended: false,
        correctness: { useHookAtTopLevel: 'error' },
        nursery: { noFloatingPromises: 'error' },
      },
    },
  });

  const manifest = JSON.parse(await readFile(resolve(frontendRoot, 'package.json'), 'utf8'));
  assert.equal(manifest.scripts.lint, 'tsc --noEmit && pnpm run lint:semantic');
  assert.equal(manifest.scripts['lint:semantic'], 'biome lint src');
  assert.equal(
    manifest.scripts.verify,
    'pnpm run lint:semantic && pnpm run format:check && pnpm run test && pnpm run build',
  );
  assert.equal(manifest.scripts.verify.match(/lint:semantic/g)?.length, 1);
  assert.doesNotMatch(manifest.scripts.verify, /pnpm run lint(?:\s|&&|$)/);

  const result = runNodeCli(
    biomeCli,
    ['lint', 'src', `--config-path=${biomeConfigPath}`, '--reporter=json', '--max-diagnostics=none'],
    frontendRoot,
  );
  assert.equal(result.status, 0, result.stderr || result.stdout);
  assert.deepEqual(biomeReport(result).diagnostics, []);
});

test('unchanged lint config rejects floating promises and conditional hooks by exact rule ID', async () => {
  const root = await mkdtemp(join(tmpdir(), 'axial-lint-contract-'));
  try {
    const configSource = await readFile(biomeConfigPath, 'utf8');
    await mkdir(resolve(root, 'src'));
    await writeFile(resolve(root, 'biome.json'), configSource);
    await writeFile(
      resolve(root, 'src/invalid.tsx'),
      [
        "import { useEffect } from 'preact/hooks';",
        'async function backgroundWork(): Promise<void> {}',
        'export function Invalid({ ready }: { ready: boolean }) {',
        '  backgroundWork();',
        '  if (ready) useEffect(() => undefined, []);',
        '  return null;',
        '}',
        '',
      ].join('\n'),
    );

    const result = runNodeCli(
      biomeCli,
      ['lint', 'src', `--config-path=${resolve(root, 'biome.json')}`, '--reporter=json', '--max-diagnostics=none'],
      root,
    );
    assert.equal(result.status, 1, result.stderr || result.stdout);
    const categories = biomeReport(result)
      .diagnostics.map((diagnostic) => diagnostic.category)
      .sort();
    assert.deepEqual(categories, ['lint/correctness/useHookAtTopLevel', 'lint/nursery/noFloatingPromises']);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('unchanged TypeScript config rejects unused locals and parameters', async () => {
  const root = await mkdtemp(join(tmpdir(), 'axial-ts-unused-contract-'));
  try {
    const configSource = await readFile(resolve(frontendRoot, 'tsconfig.json'), 'utf8');
    const config = JSON.parse(configSource);
    assert.equal(config.compilerOptions.noUnusedLocals, true);
    assert.equal(config.compilerOptions.noUnusedParameters, true);
    await mkdir(resolve(root, 'src'));
    await writeFile(resolve(root, 'tsconfig.json'), configSource);
    await writeFile(
      resolve(root, 'src/invalid.ts'),
      [
        'const unusedLocal = 1;',
        'export function visible(unusedParameter: string): number {',
        '  return 1;',
        '}',
        '',
      ].join('\n'),
    );

    const result = runNodeCli(typeScriptCli, ['--project', resolve(root, 'tsconfig.json'), '--pretty', 'false'], root);
    assert.equal(result.status, 2, result.stderr || result.stdout);
    const diagnostics = `${result.stdout}\n${result.stderr}`.match(/error TS\d+:/g) ?? [];
    assert.deepEqual(diagnostics, ['error TS6133:', 'error TS6133:']);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

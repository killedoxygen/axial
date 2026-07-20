import assert from 'node:assert/strict';
import { execFile as execFileCallback, spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rename as renameFile,
  rm,
  symlink,
  writeFile,
} from 'node:fs/promises';
import { createRequire } from 'node:module';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { promisify } from 'node:util';
import { pathToFileURL } from 'node:url';

import {
  acquireFrontendGenerationLease,
  buildAndPublishFrontendGeneration,
  cleanFrontendGenerationOwned,
  computeFrontendGenerationId,
  deriveBundleMetrics,
  enforceBundleBudgets,
  frontendGenerationLeasePort,
  parseBuildInvocation,
  parseBundleBudgets,
  parsePublicAssetManifest,
  publishFrontendGeneration,
  measureBundle,
  reconcileFrontendPublicationOwned,
  replacePublishedDirectoryOwned,
  verifyFrontendGeneration,
  watchFrontendPublicationInputs,
} from '../../build-generation.mjs';
import { createFrontendResolverPlugins } from '../../build-config.mjs';

const repositoryRoot = path.basename(process.cwd()) === 'frontend' ? path.resolve(process.cwd(), '..') : process.cwd();
const frontendDependencyRoot = path.join(repositoryRoot, 'frontend');
const require = createRequire(path.join(frontendDependencyRoot, 'package.json'));
const { build } = require('esbuild');
const execFile = promisify(execFileCallback);
const generationModuleUrl = pathToFileURL(path.join(frontendDependencyRoot, 'build-generation.mjs')).href;

async function unusedLoopbackPort() {
  const server = net.createServer();
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => resolve(undefined));
  });
  const address = server.address();
  assert.ok(address && typeof address === 'object');
  await new Promise((resolve, reject) => server.close((error) => (error ? reject(error) : resolve(undefined))));
  return address.port;
}

/** @typedef {import('../../build-generation.mjs').BundleMetricKey} BundleMetricKey */
/** @typedef {import('../../build-generation.mjs').BundleMetrics} BundleMetrics */
/** @typedef {import('../../build-generation.mjs').GenerationManifest} GenerationManifest */
/** @typedef {import('../../build-generation.mjs').GenerationReport} GenerationReport */

/** @type {BundleMetricKey[]} */
const budgetKeys = [
  'initial_javascript',
  'initial_css',
  'lazy_total',
  'public_assets',
  'largest_public_asset',
  'largest_generated_output',
  'generated_total',
  'packaged_payload',
];

/** @param {unknown} value */
const json = (value) => `${JSON.stringify(value, null, 2)}\n`;

/** @param {number} [maximum] */
function budgets(maximum = 100_000) {
  return {
    schema_version: 1,
    maximum_bytes: /** @type {BundleMetrics} */ (Object.fromEntries(budgetKeys.map((key) => [key, maximum]))),
  };
}

async function fixture() {
  const root = await mkdtemp(path.join(os.tmpdir(), 'axial-p00-b05-'));
  await mkdir(path.join(root, 'src'), { recursive: true });
  await mkdir(path.join(root, 'static'), { recursive: true });
  await writeFile(
    path.join(root, 'src/app.js'),
    "import './app.css'; import('./lazy.js').then(({ value }) => console.log(value));\n",
  );
  await writeFile(path.join(root, 'src/app.css'), 'body { color: #123456; }\n');
  await writeFile(path.join(root, 'src/lazy.js'), "export const value = 'lazy';\n");
  await writeFile(path.join(root, 'static/asset.txt'), 'asset\n');
  await writeFile(
    path.join(root, 'static/index.html'),
    '<!doctype html><link rel="stylesheet" href="/app.css"><script type="module" src="/app.js"></script>\n',
  );
  await writeFile(path.join(root, 'static/unlisted-residue.txt'), 'must not ship\n');
  await writeFile(
    path.join(root, 'public-assets.json'),
    json({ schema_version: 1, files: ['asset.txt', 'index.html'] }),
  );
  await writeFile(path.join(root, 'bundle-budgets.json'), json(budgets()));
  return { root, outputRoot: path.join(root, 'dist') };
}

/** @param {string} root @param {string} outputRoot @returns {import('esbuild').BuildOptions} */
function buildOptions(root, outputRoot) {
  return {
    absWorkingDir: root,
    entryPoints: { app: 'src/app.js' },
    bundle: true,
    outdir: outputRoot,
    format: 'esm',
    splitting: true,
    chunkNames: 'chunks/[name]-[hash]',
    minify: true,
    metafile: true,
    write: false,
  };
}

/** @param {string} root @param {string} outputRoot */
async function buildFixture(root, outputRoot) {
  return buildAndPublishFrontendGeneration({
    buildFunction: build,
    buildOptions: buildOptions(root, outputRoot),
    frontendRoot: root,
    outputRoot,
    scriptEntryPoint: 'src/app.js',
  });
}

/** @param {string} root @param {string} [prefix] @returns {Promise<string[]>} */
async function fileInventory(root, prefix = '') {
  const rows = [];
  for (const entry of await readdir(path.join(root, prefix), { withFileTypes: true })) {
    const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isDirectory()) rows.push(...(await fileInventory(root, relative)));
    else if (entry.isFile()) {
      const bytes = await readFile(path.join(root, relative));
      rows.push(`${relative}\t${bytes.length}\t${createHash('sha256').update(bytes).digest('hex')}`);
    } else {
      rows.push(`${relative}\tinvalid`);
    }
  }
  return rows.sort();
}

test('build modes reject unknown or misplaced arguments before publication', () => {
  assert.deepEqual(parseBuildInvocation([]), { mode: 'build', strictPort: false, mock: false });
  assert.deepEqual(parseBuildInvocation(['serve', '--strict-port', '--mock']), {
    mode: 'serve',
    strictPort: true,
    mock: true,
  });
  assert.deepEqual(parseBuildInvocation(['watch']), { mode: 'watch', strictPort: false, mock: false });
  assert.deepEqual(parseBuildInvocation(['clean']), { mode: 'clean', strictPort: false, mock: false });
  for (const args of [['production'], ['watch', '--mock'], ['clean', '--strict-port'], ['serve', '--mock', '--mock']]) {
    assert.throws(() => parseBuildInvocation(args), /frontend_generation:invalid_/);
  }
});

test('asset and budget manifests are canonical closed inputs', () => {
  assert.deepEqual(parsePublicAssetManifest(json({ schema_version: 1, files: ['a', 'b/c'] })), ['a', 'b/c']);
  for (const files of [
    ['b', 'a'],
    ['a', 'a'],
    ['A', 'a'],
    ['../a'],
    ['/a'],
    ['a//b'],
    ['a/'],
    ['con.txt'],
    ['name.'],
    ['generation.json'],
    ['Generation.json'],
    ['generation.json/x'],
    ['GENERATION.JSON/x'],
  ]) {
    assert.throws(() => parsePublicAssetManifest(json({ schema_version: 1, files })), /frontend_generation:/);
  }
  assert.deepEqual(parseBundleBudgets(json(budgets())).packaged_payload, 100_000);
  assert.throws(() => parseBundleBudgets(json({ ...budgets(), unknown: true })), /invalid_bundle_budget_keys/);
  assert.throws(
    () => parsePublicAssetManifest(json({ files: ['a'], schema_version: 1 })),
    /noncanonical_public_asset_manifest/,
  );
  const reversedMaximum = Object.fromEntries([...Object.entries(budgets().maximum_bytes)].reverse());
  assert.throws(
    () => parseBundleBudgets(json({ schema_version: 1, maximum_bytes: reversedMaximum })),
    /noncanonical_bundle_budget/,
  );
});

test('graph projection deduplicates imports with static reachability winning', () => {
  const root = path.join(os.tmpdir(), 'axial-p00-b05-graph');
  const outputRoot = path.join(root, 'dist');
  const app = path.join(outputRoot, 'app.js');
  const shared = path.join(outputRoot, 'chunks/shared.js');
  const result = measureBundle({
    metafile: {
      inputs: {},
      outputs: {
        [app]: {
          bytes: 1,
          inputs: {},
          exports: [],
          entryPoint: path.join(root, 'src/app.js'),
          imports: [
            { path: shared, kind: 'dynamic-import', external: false },
            { path: shared, kind: 'import-statement', external: false },
          ],
        },
        [shared]: { bytes: 1, inputs: {}, exports: [], imports: [] },
      },
    },
    outputFiles: [
      { path: app, contents: Uint8Array.of(1), hash: '', text: '' },
      { path: shared, contents: Uint8Array.of(2), hash: '', text: '' },
    ],
    outputRoot,
    workingDirectory: root,
    publicFiles: [],
    scriptEntryPoint: path.join(root, 'src/app.js'),
  });
  assert.deepEqual(result.graph[0].imports, [{ path: 'chunks/shared.js', dynamic: false }]);
});

test('React aliases retain esbuild import conditions and one ESM Preact graph', async () => {
  const result = await build({
    stdin: {
      contents: "import { h } from 'preact'; import React from 'react'; console.log(h, React);",
      resolveDir: frontendDependencyRoot,
    },
    bundle: true,
    format: 'esm',
    metafile: true,
    plugins: createFrontendResolverPlugins({ dependencyRoot: frontendDependencyRoot }),
    write: false,
  });
  const preactInputs = Object.keys(result.metafile.inputs).filter((filePath) => filePath.includes('/preact/'));
  assert.ok(preactInputs.some((filePath) => filePath.endsWith('/preact/dist/preact.module.js')));
  assert.ok(preactInputs.some((filePath) => filePath.endsWith('/preact/compat/dist/compat.module.js')));
  assert.ok(!preactInputs.some((filePath) => filePath.endsWith('/preact/compat/dist/compat.js')));
});

test('one graph-derived metric function follows static CSS and counts the receipt', () => {
  const metrics = deriveBundleMetrics({
    files: [
      { path: 'app.js', bytes: 2 },
      { path: 'app.css', bytes: 3 },
      { path: 'nested.css', bytes: 4 },
      { path: 'lazy.js', bytes: 5 },
      { path: 'index.html', bytes: 6 },
    ],
    publicPaths: ['index.html'],
    graph: [
      {
        path: 'app.css',
        css_bundle: null,
        imports: [{ path: 'nested.css', dynamic: false }],
      },
      { path: 'app.js', css_bundle: 'app.css', imports: [{ path: 'lazy.js', dynamic: true }] },
      { path: 'lazy.js', css_bundle: null, imports: [] },
      { path: 'nested.css', css_bundle: null, imports: [] },
    ],
    scriptEntry: 'app.js',
    receiptBytes: 7,
  });
  assert.deepEqual(metrics, {
    initial_javascript: 2,
    initial_css: 7,
    lazy_total: 5,
    public_assets: 6,
    largest_public_asset: 6,
    largest_generated_output: 5,
    generated_total: 14,
    packaged_payload: 27,
  });
});

test('every measured cost has an independent fail-closed budget', () => {
  const maximum = /** @type {BundleMetrics} */ (Object.fromEntries(budgetKeys.map((key) => [key, 1])));
  const measured = { ...maximum };
  enforceBundleBudgets(measured, maximum);
  for (const key of budgetKeys) {
    assert.throws(() => enforceBundleBudgets({ ...measured, [key]: 2 }, maximum), new RegExp(`${key}_budget_exceeded`));
  }
});

test('clean, repeated, failed, and residue-heavy builds retain one exact generation', async () => {
  const { root, outputRoot } = await fixture();
  try {
    await mkdir(path.join(outputRoot, 'chunks'), { recursive: true });
    await writeFile(path.join(outputRoot, 'stale.js'), 'stale\n');
    await writeFile(path.join(outputRoot, 'chunks/stale.js'), 'stale\n');

    const first = await buildFixture(root, outputRoot);
    const firstInventory = await fileInventory(outputRoot);
    assert.ok(first.metrics.initial_javascript > 0);
    assert.ok(first.metrics.initial_css > 0);
    assert.ok(first.metrics.lazy_total > 0);
    assert.equal(first.metrics.public_assets, 106);
    assert.ok(!firstInventory.some((row) => row.includes('stale')));
    assert.ok(!firstInventory.some((row) => row.includes('unlisted-residue')));

    /** @type {GenerationManifest} */
    const manifest = JSON.parse(await readFile(path.join(outputRoot, 'generation.json'), 'utf8'));
    const manifestBytes = (await readFile(path.join(outputRoot, 'generation.json'))).length;
    assert.equal(manifest.generation_id, first.generation_id);
    assert.equal(
      first.metrics.packaged_payload,
      manifest.files.reduce((total, file) => total + file.bytes, 0) + manifestBytes,
    );
    assert.deepEqual(
      manifest.files.map(({ path: filePath }) => filePath),
      [...manifest.files.map(({ path: filePath }) => filePath)].sort(),
    );
    assert.equal(manifest.files.length + 1, firstInventory.length);

    const second = await buildFixture(root, outputRoot);
    assert.equal(second.generation_id, first.generation_id);
    assert.deepEqual(await fileInventory(outputRoot), firstInventory);

    await writeFile(path.join(root, 'src/app.js'), 'export const broken = ;\n');
    await assert.rejects(() => buildFixture(root, outputRoot));
    assert.deepEqual(await fileInventory(outputRoot), firstInventory);

    await writeFile(
      path.join(root, 'src/app.js'),
      "import './app.css'; import('./lazy.js').then(({ value }) => console.log(value));\n",
    );
    const zeroInitial = budgets();
    zeroInitial.maximum_bytes.initial_javascript = 0;
    await writeFile(path.join(root, 'bundle-budgets.json'), json(zeroInitial));
    await assert.rejects(() => buildFixture(root, outputRoot), /initial_javascript_budget_exceeded/);
    assert.deepEqual(await fileInventory(outputRoot), firstInventory);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('verification rejects authority, graph, and generation identity tampering', async () => {
  const { root, outputRoot } = await fixture();
  try {
    await buildFixture(root, outputRoot);
    const manifestPath = path.join(outputRoot, 'generation.json');
    const original = await readFile(manifestPath, 'utf8');

    /** @type {GenerationManifest} */
    let tampered = JSON.parse(original);
    assert.ok(!('metrics' in tampered));
    assert.ok(!('maximum_bytes' in tampered));
    tampered.graph.pop();
    tampered.generation_id = computeFrontendGenerationId(tampered);
    await writeFile(manifestPath, json(tampered));
    await assert.rejects(() => verifyFrontendGeneration(outputRoot), /generation_file_authority_drift/);

    tampered = JSON.parse(original);
    tampered.graph[0].css_bundle = 'missing.css';
    tampered.generation_id = computeFrontendGenerationId(tampered);
    await writeFile(manifestPath, json(tampered));
    await assert.rejects(() => verifyFrontendGeneration(outputRoot), /invalid_generation_css_bundle/);

    const zeroPublic = budgets();
    zeroPublic.maximum_bytes.public_assets = 0;
    await writeFile(path.join(root, 'bundle-budgets.json'), json(zeroPublic));
    await writeFile(manifestPath, original);
    await assert.rejects(() => verifyFrontendGeneration(outputRoot), /public_assets_budget_exceeded/);
    await writeFile(path.join(root, 'bundle-budgets.json'), json(budgets()));

    tampered = JSON.parse(original);
    tampered.generation_id = '0'.repeat(64);
    await writeFile(manifestPath, json(tampered));
    await assert.rejects(() => verifyFrontendGeneration(outputRoot), /generation_identity_drift/);

    tampered = JSON.parse(original);
    const firstFile = tampered.files[0];
    tampered.files[0] = {
      sha256: firstFile.sha256,
      bytes: firstFile.bytes,
      path: firstFile.path,
    };
    tampered.generation_id = computeFrontendGenerationId(tampered);
    await writeFile(manifestPath, json(tampered));
    await assert.rejects(() => verifyFrontendGeneration(outputRoot), /noncanonical_generation_manifest/);

    await writeFile(manifestPath, original);
    await verifyFrontendGeneration(outputRoot);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('independent staged verification rejects malformed output before replacement', async () => {
  const { root, outputRoot } = await fixture();
  try {
    await buildFixture(root, outputRoot);
    const inventory = await fileInventory(outputRoot);
    const result = await build(buildOptions(root, outputRoot));
    const { metafile: sourceMetafile, outputFiles } = result;
    if (!sourceMetafile || !outputFiles) throw new Error('incomplete esbuild result');
    const metafile = structuredClone(sourceMetafile);
    const entry = Object.entries(metafile.outputs).find(([outputPath]) => path.basename(outputPath) === 'app.js');
    assert.ok(entry);
    entry[1].imports.push({
      path: path.join(outputRoot, 'chunks/missing.js'),
      kind: 'dynamic-import',
      external: false,
    });

    await assert.rejects(
      () =>
        publishFrontendGeneration({
          buildResult: { metafile, outputFiles },
          frontendRoot: root,
          outputRoot,
          scriptEntryPoint: 'src/app.js',
        }),
      /invalid_generation_graph_edge/,
    );
    assert.deepEqual(await fileInventory(outputRoot), inventory);
    assert.deepEqual(
      (await readdir(root)).filter((name) => name.startsWith('dist.')),
      [],
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('capability proof rebuilds a stale valid generation before attestation', async () => {
  const existing = await fixture();
  const repository = await mkdtemp(path.join(os.tmpdir(), 'axial-p00-b05-capability-'));
  const frontendRoot = path.join(repository, 'frontend');
  try {
    await renameFile(existing.root, frontendRoot);
    const outputRoot = path.join(frontendRoot, 'dist');
    const stale = await buildFixture(frontendRoot, outputRoot);
    await writeFile(path.join(frontendRoot, 'src/lazy.js'), "export const value = 'fresh';\n");
    let buildInvoked = false;
    const scenarioModule = await import(
      pathToFileURL(path.join(repositoryRoot, 'scripts/capabilities/scenarios/frontend-generation.mjs')).href
    );
    const receipt = await scenarioModule.rebuildAndProveFrontend(repository, async () => {
      buildInvoked = true;
      await buildFixture(frontendRoot, outputRoot);
    });
    assert.equal(buildInvoked, true);
    assert.notEqual(receipt.generation_id, stale.generation_id);
    assert.equal(receipt.generation_id, (await verifyFrontendGeneration(outputRoot)).generation_id);
  } finally {
    await rm(repository, { recursive: true, force: true });
    await rm(existing.root, { recursive: true, force: true });
  }
});

test('clean removes current generations and retired generated static outputs only', async () => {
  const { root, outputRoot } = await fixture();
  try {
    const publicRoot = path.join(root, 'static');
    await buildFixture(root, outputRoot);
    await mkdir(path.join(publicRoot, 'chunks'));
    await writeFile(path.join(publicRoot, 'app.js'), 'retired');
    await writeFile(path.join(publicRoot, 'app.css'), 'retired');
    await writeFile(path.join(publicRoot, 'chunks/retired.js'), 'retired');

    await cleanFrontendGenerationOwned(outputRoot, publicRoot);

    await assert.rejects(() => readFile(path.join(outputRoot, 'generation.json')), /ENOENT/);
    await assert.rejects(() => readFile(path.join(publicRoot, 'app.js')), /ENOENT/);
    await assert.rejects(() => readFile(path.join(publicRoot, 'app.css')), /ENOENT/);
    await assert.rejects(() => readFile(path.join(publicRoot, 'chunks/retired.js')), /ENOENT/);
    assert.equal(
      await readFile(path.join(publicRoot, 'index.html'), 'utf8'),
      '<!doctype html><link rel="stylesheet" href="/app.css"><script type="module" src="/app.js"></script>\n',
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('clean remains dependency-free when node_modules is absent', async () => {
  const frontendRoot = await mkdtemp(path.join(os.tmpdir(), 'axial-p00-b05-clean-'));
  try {
    for (const script of ['build-config.mjs', 'build-generation.mjs', 'esbuild.mjs']) {
      await copyFile(path.join(frontendDependencyRoot, script), path.join(frontendRoot, script));
    }
    await mkdir(path.join(frontendRoot, 'dist'));
    await mkdir(path.join(frontendRoot, 'dist.stage-stale'));
    await mkdir(path.join(frontendRoot, 'static/chunks'), { recursive: true });
    await writeFile(path.join(frontendRoot, 'dist/stale.js'), 'stale');
    await writeFile(path.join(frontendRoot, 'static/app.js'), 'retired');
    await writeFile(path.join(frontendRoot, 'static/app.css'), 'retired');
    await writeFile(path.join(frontendRoot, 'static/chunks/stale.js'), 'retired');
    await writeFile(path.join(frontendRoot, 'static/index.html'), 'source');

    await execFile(process.execPath, [path.join(frontendRoot, 'esbuild.mjs'), 'clean']);

    await assert.rejects(() => readFile(path.join(frontendRoot, 'dist/stale.js')), /ENOENT/);
    await assert.rejects(() => readFile(path.join(frontendRoot, 'dist.stage-stale')), /ENOENT/);
    await assert.rejects(() => readFile(path.join(frontendRoot, 'static/app.js')), /ENOENT/);
    assert.equal(await readFile(path.join(frontendRoot, 'static/index.html'), 'utf8'), 'source');
  } finally {
    await rm(frontendRoot, { recursive: true, force: true });
  }
});

test('the real development server starts and serves the frontend within a bound', async () => {
  const port = await unusedLoopbackPort();
  const child = spawn(process.execPath, [path.join(frontendDependencyRoot, 'esbuild.mjs'), 'serve', '--strict-port'], {
    cwd: frontendDependencyRoot,
    env: { ...process.env, PORT: String(port) },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let output = '';
  child.stdout.setEncoding('utf8');
  child.stderr.setEncoding('utf8');
  child.stdout.on('data', (chunk) => {
    output += chunk;
  });
  child.stderr.on('data', (chunk) => {
    output += chunk;
  });
  try {
    await Promise.race([
      new Promise((resolve, reject) => {
        const poll = () => {
          if (output.includes(`dev -> http://localhost:${port}`)) resolve(undefined);
          else if (child.exitCode !== null) reject(new Error(`dev server exited ${child.exitCode}: ${output}`));
          else setTimeout(poll, 25);
        };
        poll();
      }),
      new Promise((_, reject) => {
        setTimeout(() => reject(new Error(`dev server startup timeout: ${output}`)), 20_000).unref();
      }),
    ]);
    const response = await fetch(`http://127.0.0.1:${port}/`);
    assert.equal(response.status, 200);
    assert.match(await response.text(), /src="\/?app\.js"/);
    for (const [asset, expectedType] of /** @type {Array<[string, RegExp]>} */ ([
      ['app.js', /^(?:application|text)\/javascript/],
      ['app.css', /^text\/css/],
    ])) {
      const assetResponse = await fetch(`http://127.0.0.1:${port}/${asset}`);
      assert.equal(assetResponse.status, 200);
      assert.match(assetResponse.headers.get('content-type') ?? '', expectedType);
      assert.ok((await assetResponse.arrayBuffer()).byteLength > 0);
    }
  } finally {
    if (child.exitCode === null) {
      child.kill('SIGTERM');
      await Promise.race([
        new Promise((resolve) => child.once('exit', resolve)),
        new Promise((resolve) => {
          setTimeout(() => {
            child.kill('SIGKILL');
            resolve(undefined);
          }, 2_000).unref();
        }),
      ]);
    }
  }
});

test('promotion faults preserve or recover exactly one complete generation', async () => {
  for (const failure of ['move-current', 'promote-stage', 'restore-current', 'remove-previous']) {
    const root = await mkdtemp(path.join(os.tmpdir(), `axial-p00-b05-${failure}-`));
    const outputRoot = path.join(root, 'dist');
    const stage = path.join(root, 'dist.stage-test');
    try {
      await mkdir(outputRoot);
      await mkdir(stage);
      await writeFile(path.join(outputRoot, 'value'), 'old');
      await writeFile(path.join(stage, 'value'), 'new');
      let renameCount = 0;
      /** @param {string} source @param {string} destination */
      const renamePath = async (source, destination) => {
        renameCount += 1;
        if (
          (failure === 'move-current' && renameCount === 1) ||
          (failure === 'promote-stage' && renameCount === 2) ||
          (failure === 'restore-current' && (renameCount === 2 || renameCount === 3))
        ) {
          throw Object.assign(new Error(`injected ${failure}`), { code: 'EACCES' });
        }
        await renameFile(source, destination);
      };
      /** @param {string} target @param {{ recursive: true }} options */
      const removePath = async (target, options) => {
        if (failure === 'remove-previous') {
          throw Object.assign(new Error('injected remove-previous'), { code: 'EACCES' });
        }
        await rm(target, options);
      };

      if (failure === 'restore-current') {
        await assert.rejects(
          () => replacePublishedDirectoryOwned(stage, outputRoot, { renamePath, removePath }),
          /frontend generation restore failed/,
        );
        await reconcileFrontendPublicationOwned(outputRoot);
        assert.equal(await readFile(path.join(outputRoot, 'value'), 'utf8'), 'old');
      } else if (failure === 'remove-previous') {
        const result = await replacePublishedDirectoryOwned(stage, outputRoot, { renamePath, removePath });
        assert.equal(result.cleanup_pending, true);
        assert.equal(await readFile(path.join(outputRoot, 'value'), 'utf8'), 'new');
        await reconcileFrontendPublicationOwned(outputRoot);
        assert.equal(await readFile(path.join(outputRoot, 'value'), 'utf8'), 'new');
      } else {
        await assert.rejects(() => replacePublishedDirectoryOwned(stage, outputRoot, { renamePath, removePath }));
        assert.equal(await readFile(path.join(outputRoot, 'value'), 'utf8'), 'old');
      }
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  }
});

test('hard exits at both promotion boundaries reconcile without a partial tree', async () => {
  for (const [boundary, expected] of [
    [1, 'old'],
    [2, 'new'],
  ]) {
    const root = await mkdtemp(path.join(os.tmpdir(), `axial-p00-b05-crash-${boundary}-`));
    const outputRoot = path.join(root, 'dist');
    const stage = path.join(root, 'dist.stage-test');
    try {
      await mkdir(outputRoot);
      await mkdir(stage);
      await writeFile(path.join(outputRoot, 'value'), 'old');
      await writeFile(path.join(stage, 'value'), 'new');
      const child = `
        import { rename } from 'node:fs/promises';
        import { replacePublishedDirectoryOwned } from ${JSON.stringify(generationModuleUrl)};
        let count = 0;
        await replacePublishedDirectoryOwned(${JSON.stringify(stage)}, ${JSON.stringify(outputRoot)}, {
          renamePath: async (source, destination) => {
            await rename(source, destination);
            count += 1;
            if (count === ${boundary}) process.exit(70 + count);
          },
        });
      `;
      await assert.rejects(execFile(process.execPath, ['--input-type=module', '-e', child]));
      await reconcileFrontendPublicationOwned(outputRoot);
      assert.equal(await readFile(path.join(outputRoot, 'value'), 'utf8'), expected);
      assert.deepEqual(
        (await readdir(root)).filter((name) => name.startsWith('dist.')),
        [],
      );
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  }
});

test('crash residue is reconciled before a failing rebuild can return', async () => {
  const { root, outputRoot } = await fixture();
  try {
    await buildFixture(root, outputRoot);
    const previous = `${outputRoot}.previous-crash`;
    const stage = `${outputRoot}.stage-crash`;
    await renameFile(outputRoot, previous);
    await mkdir(stage);
    await writeFile(path.join(stage, 'partial.js'), 'partial');
    let sawRecoveredGeneration = false;
    await assert.rejects(
      () =>
        buildAndPublishFrontendGeneration({
          buildFunction: async () => {
            sawRecoveredGeneration = await readFile(path.join(outputRoot, 'generation.json'))
              .then(() => true)
              .catch(() => false);
            throw new Error('injected compilation failure');
          },
          buildOptions: {},
          frontendRoot: root,
          outputRoot,
        }),
      /injected compilation failure/,
    );
    assert.equal(sawRecoveredGeneration, true);
    await verifyFrontendGeneration(outputRoot);
    assert.deepEqual(
      (await readdir(root)).filter((name) => name.startsWith('dist.')),
      [],
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('reconciliation never promotes a linked previous generation', async (context) => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'axial-p00-b05-linked-previous-'));
  const outputRoot = path.join(root, 'dist');
  const previous = `${outputRoot}.previous-forged`;
  const target = path.join(root, 'target');
  try {
    await mkdir(target);
    try {
      await symlink(target, previous, process.platform === 'win32' ? 'junction' : 'dir');
    } catch (error) {
      const code = error instanceof Error && 'code' in error ? error.code : undefined;
      if (code === 'EPERM' || code === 'EACCES' || code === 'ENOSYS') {
        context.skip(`directory links unavailable: ${String(code)}`);
        return;
      }
      throw error;
    }
    await assert.rejects(() => reconcileFrontendPublicationOwned(outputRoot), /invalid_previous_generation/);
    await assert.rejects(() => readFile(path.join(outputRoot, 'generation.json')), /ENOENT/);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('an asset-only watch event publishes the next exact generation', async () => {
  const { root, outputRoot } = await fixture();
  let watcher;
  try {
    const first = await buildFixture(root, outputRoot);
    /** @type {(value: GenerationReport) => void} */
    let resolveChange = () => {};
    /** @type {(error: unknown) => void} */
    let rejectChange = () => {};
    /** @type {Promise<GenerationReport>} */
    const changed = new Promise((resolve, reject) => {
      resolveChange = resolve;
      rejectChange = reject;
    });
    watcher = await watchFrontendPublicationInputs({
      frontendRoot: root,
      onChange: async () => resolveChange(await buildFixture(root, outputRoot)),
      onError: rejectChange,
    });
    await writeFile(path.join(root, 'static/asset.txt'), 'asset changed\n');
    const second = await Promise.race([
      changed,
      new Promise((_, reject) => setTimeout(() => reject(new Error('asset watch timeout')), 5_000)),
    ]);
    assert.notEqual(second.generation_id, first.generation_id);
    assert.equal(second.metrics.public_assets, 114);
    await verifyFrontendGeneration(outputRoot);
  } finally {
    await watcher?.close();
    await rm(root, { recursive: true, force: true });
  }
});

test('the loopback lease admits one owner and blocks mutation by every loser', async () => {
  const { root, outputRoot } = await fixture();
  try {
    await buildFixture(root, outputRoot);
    const inventory = await fileInventory(outputRoot);
    const port = await frontendGenerationLeasePort(outputRoot);
    const release = await acquireFrontendGenerationLease(outputRoot);
    try {
      let buildStarted = false;
      await assert.rejects(
        () =>
          buildAndPublishFrontendGeneration({
            buildFunction: async () => {
              buildStarted = true;
              throw new Error('losing build started');
            },
            buildOptions: {},
            frontendRoot: root,
            outputRoot,
          }),
        new RegExp(`publication_lease_contended:.*:${port}$`),
      );
      assert.equal(buildStarted, false);
      assert.deepEqual(await fileInventory(outputRoot), inventory);
    } finally {
      await release();
      await release();
    }

    for (let attempt = 0; attempt < 10; attempt += 1) {
      const contenders = await Promise.allSettled([
        acquireFrontendGenerationLease(outputRoot),
        acquireFrontendGenerationLease(outputRoot),
      ]);
      const winners = contenders.filter(
        /** @returns {result is PromiseFulfilledResult<() => Promise<void>>} */
        (result) => result.status === 'fulfilled',
      );
      assert.equal(winners.length, 1);
      assert.equal(contenders.filter((result) => result.status === 'rejected').length, 1);
      await winners[0].value();
    }
    assert.deepEqual(
      (await readdir(root)).filter((name) => name.startsWith('dist.lock')),
      [],
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('a child hard exit releases the OS lease for immediate reacquisition', async () => {
  const { root, outputRoot } = await fixture();
  try {
    const child = `
      import { acquireFrontendGenerationLease } from ${JSON.stringify(generationModuleUrl)};
      await acquireFrontendGenerationLease(${JSON.stringify(outputRoot)});
      process.exit(73);
    `;
    await assert.rejects(execFile(process.execPath, ['--input-type=module', '-e', child]));
    const release = await acquireFrontendGenerationLease(outputRoot);
    await release();
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('accepted local connections cannot delay lease release or reacquisition', async () => {
  const { root, outputRoot } = await fixture();
  /** @type {import('node:net').Socket | undefined} */
  let client;
  try {
    const port = await frontendGenerationLeasePort(outputRoot);
    const release = await acquireFrontendGenerationLease(outputRoot);
    const socket = net.createConnection({ host: '127.0.0.1', port });
    client = socket;
    await new Promise((resolve, reject) => {
      socket.once('connect', () => resolve(undefined));
      socket.once('error', reject);
    });
    const released = await Promise.race([
      release().then(() => true),
      new Promise((resolve) => setTimeout(() => resolve(false), 1_000)),
    ]);
    assert.equal(released, true);
    const releaseAgain = await acquireFrontendGenerationLease(outputRoot);
    await releaseAgain();
  } finally {
    client?.destroy();
    await rm(root, { recursive: true, force: true });
  }
});

test('filesystem aliases resolve to the same lease identity when supported', async (context) => {
  const container = await mkdtemp(path.join(os.tmpdir(), 'axial-p00-b05-alias-'));
  const realRoot = path.join(container, 'real');
  const aliasRoot = path.join(container, 'alias');
  /** @type {(() => Promise<void>) | undefined} */
  let release;
  try {
    await mkdir(realRoot);
    try {
      await symlink(realRoot, aliasRoot, process.platform === 'win32' ? 'junction' : 'dir');
    } catch (error) {
      const code = error instanceof Error && 'code' in error ? error.code : undefined;
      if (code === 'EPERM' || code === 'EACCES' || code === 'ENOSYS') {
        context.skip(`directory aliases unavailable: ${String(code)}`);
        return;
      }
      throw error;
    }
    const outputRoot = path.join(realRoot, 'dist');
    const aliasOutputRoot = path.join(aliasRoot, 'dist');
    assert.equal(await frontendGenerationLeasePort(aliasOutputRoot), await frontendGenerationLeasePort(outputRoot));
    release = await acquireFrontendGenerationLease(outputRoot);
    await assert.rejects(() => acquireFrontendGenerationLease(aliasOutputRoot), /publication_lease_contended/);
  } finally {
    await release?.();
    await rm(container, { recursive: true, force: true });
  }
});

test('portable case variants map to one publication lease', async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'axial-p00-b05-case-'));
  try {
    assert.equal(
      await frontendGenerationLeasePort(path.join(root, 'dist')),
      await frontendGenerationLeasePort(path.join(root, 'DIST')),
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('an unrelated loopback listener collision fails closed at the exact lease port', async () => {
  const { root, outputRoot } = await fixture();
  const port = await frontendGenerationLeasePort(outputRoot);
  const blocker = net.createServer();
  try {
    await new Promise((resolve, reject) => {
      blocker.once('error', reject);
      blocker.listen({ host: '127.0.0.1', port, exclusive: true }, () => resolve(undefined));
    });
    await assert.rejects(
      () => acquireFrontendGenerationLease(outputRoot),
      new RegExp(`publication_lease_contended:.*:${port}$`),
    );
  } finally {
    if (blocker.listening) {
      await new Promise((resolve, reject) => {
        blocker.close((error) => (error ? reject(error) : resolve(undefined)));
      });
    }
    await rm(root, { recursive: true, force: true });
  }
});

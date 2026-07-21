import { createHash, randomUUID } from 'node:crypto';
import { lstat, mkdir, readFile, readdir, rename, rm, writeFile } from 'node:fs/promises';
import { watch as watchFileSystem } from 'node:fs';
import path from 'node:path';

import {
  acquireExclusiveLoopbackPort,
  portablePathLeaseIdentity,
  privateLoopbackLeasePort,
} from '../scripts/loopback-lease.mjs';

const GENERATION_MANIFEST = 'generation.json';
const BUDGET_KEYS = Object.freeze([
  'initial_javascript',
  'initial_css',
  'lazy_total',
  'public_assets',
  'largest_public_asset',
  'largest_generated_output',
  'generated_total',
  'packaged_payload',
]);

function fail(message) {
  throw new Error(`frontend_generation:${message}`);
}

function exactKeys(value, expected, label) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) fail(`invalid_${label}`);
  const actual = Object.keys(value).sort();
  const wanted = [...expected].sort();
  if (actual.join('\0') !== wanted.join('\0')) fail(`invalid_${label}_keys`);
}

function canonicalRelativePath(value, label) {
  if (
    typeof value !== 'string' ||
    !value ||
    !/^[A-Za-z0-9._/-]+$/.test(value) ||
    value.includes('\\') ||
    value.includes('\0')
  ) {
    fail(`invalid_${label}_path`);
  }
  if (
    path.posix.normalize(value) !== value ||
    value.startsWith('/') ||
    value
      .split('/')
      .some(
        (segment) =>
          segment === '' ||
          segment === '.' ||
          segment === '..' ||
          segment.endsWith('.') ||
          segment.endsWith(' ') ||
          /^(?:con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)/i.test(segment),
      ) ||
    value.split('/')[0].toLowerCase() === GENERATION_MANIFEST
  ) {
    fail(`invalid_${label}_path`);
  }
  return value;
}

function rejectPortableCollisions(paths, label) {
  const folded = new Set();
  for (const filePath of paths) {
    const key = filePath.toLowerCase();
    if (folded.has(key)) fail(`${label}_path_collision`);
    folded.add(key);
  }
}

function canonicalJson(value) {
  return `${JSON.stringify(value, null, 2)}\n`;
}

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

export function parseBuildInvocation(argv) {
  if (!Array.isArray(argv)) fail('invalid_arguments');
  if (argv.length === 0) return Object.freeze({ mode: 'build', strictPort: false, mock: false });
  const [mode, ...flags] = argv;
  if (!['serve', 'watch', 'clean'].includes(mode)) fail(`invalid_mode:${String(mode)}`);
  const allowed = mode === 'serve' ? new Set(['--strict-port', '--mock']) : new Set();
  if (new Set(flags).size !== flags.length || flags.some((flag) => !allowed.has(flag))) {
    fail('invalid_arguments');
  }
  return Object.freeze({
    mode,
    strictPort: flags.includes('--strict-port'),
    mock: flags.includes('--mock'),
  });
}

export function parsePublicAssetManifest(source) {
  let parsed;
  try {
    parsed = JSON.parse(source);
  } catch {
    fail('invalid_public_asset_manifest_json');
  }
  exactKeys(parsed, ['schema_version', 'files'], 'public_asset_manifest');
  if (parsed.schema_version !== 1 || !Array.isArray(parsed.files)) {
    fail('invalid_public_asset_manifest');
  }
  const files = parsed.files.map((value) => canonicalRelativePath(value, 'public_asset'));
  if (new Set(files).size !== files.length || files.join('\0') !== [...files].sort().join('\0')) {
    fail('noncanonical_public_asset_manifest');
  }
  rejectPortableCollisions(files, 'public_asset');
  if (source !== canonicalJson({ schema_version: 1, files })) {
    fail('noncanonical_public_asset_manifest');
  }
  return Object.freeze(files);
}

export function parseBundleBudgets(source) {
  let parsed;
  try {
    parsed = JSON.parse(source);
  } catch {
    fail('invalid_bundle_budget_json');
  }
  exactKeys(parsed, ['schema_version', 'maximum_bytes'], 'bundle_budget');
  exactKeys(parsed.maximum_bytes, BUDGET_KEYS, 'bundle_budget_maximum_bytes');
  if (parsed.schema_version !== 1) fail('invalid_bundle_budget_schema');
  for (const key of BUDGET_KEYS) {
    if (!Number.isSafeInteger(parsed.maximum_bytes[key]) || parsed.maximum_bytes[key] < 0) {
      fail(`invalid_${key}_budget`);
    }
  }
  const maximumBytes = Object.fromEntries(BUDGET_KEYS.map((key) => [key, parsed.maximum_bytes[key]]));
  if (source !== canonicalJson({ schema_version: 1, maximum_bytes: maximumBytes })) {
    fail('noncanonical_bundle_budget');
  }
  return Object.freeze(maximumBytes);
}

export function enforceBundleBudgets(metrics, maximumBytes) {
  for (const key of BUDGET_KEYS) {
    const actual = metrics[key];
    const maximum = maximumBytes[key];
    if (!Number.isSafeInteger(actual) || actual < 0) fail(`invalid_${key}_measurement`);
    if (!Number.isSafeInteger(maximum) || maximum < 0) fail(`invalid_${key}_budget`);
    if (actual > maximum) fail(`${key}_budget_exceeded:${actual}>${maximum}`);
  }
}

function relativeOutputPath(outputPath, outputRoot, workingDirectory, label) {
  const absolute = path.isAbsolute(outputPath) ? outputPath : path.resolve(workingDirectory, outputPath);
  const relative = path.relative(outputRoot, absolute).split(path.sep).join('/');
  return canonicalRelativePath(relative, label);
}

export function measureBundle({ metafile, outputFiles, outputRoot, workingDirectory, publicFiles, scriptEntryPoint }) {
  if (!metafile?.outputs || !Array.isArray(outputFiles)) fail('missing_esbuild_output');
  const metadata = new Map();
  for (const [outputPath, value] of Object.entries(metafile.outputs)) {
    const relative = relativeOutputPath(outputPath, outputRoot, workingDirectory, 'generated');
    if (metadata.has(relative)) fail('duplicate_generated_output');
    metadata.set(relative, value);
  }
  const generated = new Map();
  for (const file of outputFiles) {
    const relative = relativeOutputPath(file.path, outputRoot, workingDirectory, 'generated');
    if (generated.has(relative)) fail('duplicate_generated_output');
    generated.set(relative, Buffer.from(file.contents));
  }
  if ([...metadata.keys()].sort().join('\0') !== [...generated.keys()].sort().join('\0')) {
    fail('metafile_output_mismatch');
  }
  rejectPortableCollisions([...generated.keys()], 'generated');

  const configuredEntryPoint = metadata.get('app.js')?.entryPoint;
  if (
    !configuredEntryPoint ||
    path.resolve(workingDirectory, configuredEntryPoint) !== path.resolve(workingDirectory, scriptEntryPoint)
  ) {
    fail('invalid_entry_output');
  }
  const publicPaths = new Set(publicFiles.map(({ path: filePath }) => filePath));
  for (const value of metadata.values()) {
    for (const imported of value.imports ?? []) {
      if (!imported.external) continue;
      const externalPath = canonicalRelativePath(imported.path, 'external_asset');
      if (imported.kind !== 'url-token' || !publicPaths.has(externalPath)) {
        fail(`unowned_external_asset:${externalPath}`);
      }
    }
  }
  const graph = [...metadata]
    .map(([filePath, value]) => {
      const importsByPath = new Map();
      for (const imported of value.imports ?? []) {
        if (imported.external) continue;
        const importedPath = relativeOutputPath(imported.path, outputRoot, workingDirectory, 'import');
        const dynamic = imported.kind === 'dynamic-import';
        importsByPath.set(importedPath, (importsByPath.get(importedPath) ?? true) && dynamic);
      }
      return {
        path: filePath,
        css_bundle: value.cssBundle
          ? relativeOutputPath(value.cssBundle, outputRoot, workingDirectory, 'css_bundle')
          : null,
        imports: [...importsByPath]
          .map(([importedPath, dynamic]) => ({ path: importedPath, dynamic }))
          .sort((left, right) => (left.path < right.path ? -1 : left.path > right.path ? 1 : 0)),
      };
    })
    .sort((left, right) => (left.path < right.path ? -1 : left.path > right.path ? 1 : 0));
  return { generated, graph };
}

export function deriveBundleMetrics({ files, publicPaths, graph, scriptEntry, receiptBytes }) {
  if (!Number.isSafeInteger(receiptBytes) || receiptBytes < 0) fail('invalid_generation_receipt_bytes');
  const fileByPath = new Map(files.map((file) => [file.path, file]));
  const graphByPath = new Map(graph.map((record) => [record.path, record]));
  const reachable = new Set();
  const pending = [scriptEntry];
  while (pending.length) {
    const current = pending.pop();
    if (reachable.has(current)) continue;
    const record = graphByPath.get(current);
    if (!record) fail('invalid_generation_initial_graph');
    reachable.add(current);
    if (record.css_bundle !== null) pending.push(record.css_bundle);
    for (const imported of record.imports) {
      if (!imported.dynamic) pending.push(imported.path);
    }
  }

  const recordsFor = (paths) =>
    [...paths].map((filePath) => {
      const record = fileByPath.get(filePath);
      if (!record) fail(`missing_generation_metric_file:${filePath}`);
      return record;
    });
  const totalBytes = (records) => records.reduce((total, record) => total + record.bytes, 0);
  const largestBytes = (records) => records.reduce((maximum, record) => Math.max(maximum, record.bytes), 0);
  const publicRecords = recordsFor(publicPaths);
  const generatedRecords = recordsFor(graphByPath.keys());
  const initialJavaScript = recordsFor([...reachable].filter((filePath) => path.posix.extname(filePath) === '.js'));
  const initialCss = recordsFor([...reachable].filter((filePath) => path.posix.extname(filePath) === '.css'));
  const lazy = generatedRecords.filter(({ path: filePath }) => !reachable.has(filePath));
  const publicAssets = totalBytes(publicRecords);
  const generatedTotal = totalBytes(generatedRecords);
  return Object.freeze({
    initial_javascript: totalBytes(initialJavaScript),
    initial_css: totalBytes(initialCss),
    lazy_total: totalBytes(lazy),
    public_assets: publicAssets,
    largest_public_asset: largestBytes(publicRecords),
    largest_generated_output: largestBytes(generatedRecords),
    generated_total: generatedTotal,
    packaged_payload: generatedTotal + publicAssets + receiptBytes,
  });
}

async function assertRegularPath(root, relative) {
  const rootMetadata = await lstat(root);
  if (!rootMetadata.isDirectory() || rootMetadata.isSymbolicLink()) fail('invalid_source_root');
  let current = root;
  for (const segment of relative.split('/')) {
    current = path.join(current, segment);
    const metadata = await lstat(current);
    if (metadata.isSymbolicLink()) fail(`source_path_is_symlink:${relative}`);
  }
  const metadata = await lstat(current);
  if (!metadata.isFile()) fail(`source_path_not_regular:${relative}`);
}

async function readPublicFiles(sourceRoot, files) {
  const result = [];
  for (const filePath of files) {
    const absolute = path.join(sourceRoot, ...filePath.split('/'));
    await assertRegularPath(sourceRoot, filePath);
    result.push({ path: filePath, bytes: await readFile(absolute) });
  }
  return result;
}

function canonicalGenerationIdentity(manifest) {
  return {
    schema_version: manifest.schema_version,
    document_entry: manifest.document_entry,
    script_entry: manifest.script_entry,
    files: manifest.files.map((file) => ({
      path: file.path,
      bytes: file.bytes,
      sha256: file.sha256,
    })),
    graph: manifest.graph.map((output) => ({
      path: output.path,
      css_bundle: output.css_bundle,
      imports: output.imports.map((imported) => ({
        path: imported.path,
        dynamic: imported.dynamic,
      })),
    })),
  };
}

function canonicalGenerationManifest(manifest) {
  const identity = canonicalGenerationIdentity(manifest);
  return {
    schema_version: identity.schema_version,
    generation_id: manifest.generation_id,
    document_entry: identity.document_entry,
    script_entry: identity.script_entry,
    files: identity.files,
    graph: identity.graph,
  };
}

export function computeFrontendGenerationId(manifest) {
  const identity = canonicalGenerationIdentity(manifest);
  return sha256(Buffer.from(canonicalJson(identity)));
}

async function listFiles(root, prefix = '') {
  const files = [];
  for (const entry of await readdir(path.join(root, prefix), { withFileTypes: true })) {
    const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isDirectory()) files.push(...(await listFiles(root, relative)));
    else if (entry.isFile()) files.push(relative);
    else fail(`invalid_staged_entry:${relative}`);
  }
  return files.sort();
}

async function writeGeneration(stage, files, manifest) {
  for (const file of files) {
    const destination = path.join(stage, ...file.path.split('/'));
    await mkdir(path.dirname(destination), { recursive: true });
    await writeFile(destination, file.bytes, { flag: 'wx' });
  }
  await writeFile(path.join(stage, GENERATION_MANIFEST), canonicalJson(manifest), { flag: 'wx' });
  const actual = await listFiles(stage);
  const expected = [...files.map(({ path: filePath }) => filePath), GENERATION_MANIFEST].sort();
  if (actual.join('\0') !== expected.join('\0')) fail('staged_generation_mismatch');
}

export async function replacePublishedDirectoryOwned(stage, outputRoot, { renamePath = rename, removePath = rm } = {}) {
  const previous = `${outputRoot}.previous-${randomUUID()}`;
  let previousExists = false;
  try {
    await renamePath(outputRoot, previous);
    previousExists = true;
  } catch (error) {
    if (error?.code !== 'ENOENT') throw error;
  }
  try {
    await renamePath(stage, outputRoot);
  } catch (error) {
    if (previousExists) {
      try {
        await renamePath(previous, outputRoot);
      } catch (restoreError) {
        throw new AggregateError([error, restoreError], 'frontend generation restore failed');
      }
    }
    throw error;
  }
  if (!previousExists) return Object.freeze({ cleanup_pending: false });
  try {
    await removePath(previous, { recursive: true });
    return Object.freeze({ cleanup_pending: false });
  } catch {
    return Object.freeze({ cleanup_pending: true });
  }
}

export async function frontendGenerationLeasePort(outputRoot) {
  return privateLoopbackLeasePort(await portablePathLeaseIdentity(outputRoot));
}

export async function acquireFrontendGenerationLease(outputRoot) {
  const identity = await portablePathLeaseIdentity(outputRoot);
  const port = privateLoopbackLeasePort(identity);
  try {
    return await acquireExclusiveLoopbackPort(port, { unref: true });
  } catch (error) {
    if (error?.code === 'EADDRINUSE') {
      throw new Error(`frontend_generation:publication_lease_contended:${identity}:${port}`);
    }
    throw error;
  }
}

export async function reconcileFrontendPublicationOwned(outputRoot) {
  const parent = path.dirname(outputRoot);
  const base = path.basename(outputRoot);
  const names = await readdir(parent).catch((error) => {
    if (error?.code === 'ENOENT') return [];
    throw error;
  });
  const stages = names.filter((name) => name.startsWith(`${base}.stage-`));
  const previous = names.filter((name) => name.startsWith(`${base}.previous-`));
  const outputMetadata = await lstat(outputRoot).catch((error) => {
    if (error?.code === 'ENOENT') return undefined;
    throw error;
  });
  if (outputMetadata?.isSymbolicLink() || (outputMetadata && !outputMetadata.isDirectory())) {
    fail('invalid_output_root');
  }
  if (!outputMetadata && previous.length > 1) fail('ambiguous_previous_generation');
  if (!outputMetadata && previous.length === 1) {
    const previousPath = path.join(parent, previous[0]);
    const previousMetadata = await lstat(previousPath);
    if (previousMetadata.isSymbolicLink() || !previousMetadata.isDirectory()) {
      fail('invalid_previous_generation');
    }
    await rename(previousPath, outputRoot);
    previous.length = 0;
  }
  await Promise.all([...stages, ...previous].map((name) => rm(path.join(parent, name), { recursive: true })));
}

export async function reconcileFrontendPublication(outputRoot) {
  const release = await acquireFrontendGenerationLease(outputRoot);
  try {
    await reconcileFrontendPublicationOwned(outputRoot);
  } finally {
    await release();
  }
}

export async function buildAndPublishFrontendGeneration({
  buildFunction,
  buildOptions,
  frontendRoot,
  outputRoot,
  publicRoot,
  publicManifestPath,
  budgetPath,
  scriptEntryPoint,
}) {
  if (typeof buildFunction !== 'function') fail('invalid_build_function');
  const release = await acquireFrontendGenerationLease(outputRoot);
  try {
    await reconcileFrontendPublicationOwned(outputRoot);
    const buildResult = await buildFunction(buildOptions);
    return await publishFrontendGenerationOwned({
      buildResult,
      frontendRoot,
      outputRoot,
      publicRoot,
      publicManifestPath,
      budgetPath,
      scriptEntryPoint,
    });
  } finally {
    await release();
  }
}

async function publishFrontendGenerationOwned({
  buildResult,
  frontendRoot,
  outputRoot,
  publicRoot = path.join(frontendRoot, 'static'),
  publicManifestPath = path.join(frontendRoot, 'public-assets.json'),
  budgetPath = path.join(frontendRoot, 'bundle-budgets.json'),
  scriptEntryPoint = 'src/main.tsx',
}) {
  const publicManifestSource = await readFile(publicManifestPath, 'utf8');
  const publicFiles = await readPublicFiles(publicRoot, parsePublicAssetManifest(publicManifestSource));
  const { generated, graph } = measureBundle({
    metafile: buildResult.metafile,
    outputFiles: buildResult.outputFiles,
    outputRoot,
    workingDirectory: frontendRoot,
    publicFiles,
    scriptEntryPoint,
  });
  const generatedFiles = [...generated].map(([filePath, bytes]) => ({
    path: filePath,
    bytes,
  }));
  const files = [...publicFiles, ...generatedFiles].sort((left, right) =>
    left.path < right.path ? -1 : left.path > right.path ? 1 : 0,
  );
  if (new Set(files.map(({ path: filePath }) => filePath)).size !== files.length) {
    fail('public_generated_path_collision');
  }
  rejectPortableCollisions(
    files.map(({ path: filePath }) => filePath),
    'packaged',
  );
  const manifestIdentity = {
    schema_version: 1,
    document_entry: 'index.html',
    script_entry: 'app.js',
    files: files.map((file) => ({
      path: file.path,
      bytes: file.bytes.length,
      sha256: sha256(file.bytes),
    })),
    graph,
  };
  const manifest = {
    schema_version: manifestIdentity.schema_version,
    generation_id: computeFrontendGenerationId(manifestIdentity),
    document_entry: manifestIdentity.document_entry,
    script_entry: manifestIdentity.script_entry,
    files: manifestIdentity.files,
    graph: manifestIdentity.graph,
  };
  const stage = `${outputRoot}.stage-${randomUUID()}`;
  await mkdir(stage, { recursive: false });
  try {
    await writeGeneration(stage, files, manifest);
    const verified = await verifyFrontendGeneration(stage, { publicManifestPath, budgetPath });
    const publication = await replacePublishedDirectoryOwned(stage, outputRoot);
    return Object.freeze({
      generation_id: manifest.generation_id,
      metrics: verified.metrics,
      cleanup_pending: publication.cleanup_pending,
    });
  } catch (error) {
    await rm(stage, { recursive: true, force: true });
    throw error;
  }
}

export async function publishFrontendGeneration(options) {
  const release = await acquireFrontendGenerationLease(options.outputRoot);
  try {
    await reconcileFrontendPublicationOwned(options.outputRoot);
    return await publishFrontendGenerationOwned(options);
  } finally {
    await release();
  }
}

export async function verifyFrontendGeneration(
  outputRoot,
  {
    publicManifestPath = path.join(path.dirname(outputRoot), 'public-assets.json'),
    budgetPath = path.join(path.dirname(outputRoot), 'bundle-budgets.json'),
  } = {},
) {
  await assertRegularPath(outputRoot, GENERATION_MANIFEST);
  const [source, publicManifestSource, budgetSource] = await Promise.all([
    readFile(path.join(outputRoot, GENERATION_MANIFEST), 'utf8'),
    readFile(publicManifestPath, 'utf8'),
    readFile(budgetPath, 'utf8'),
  ]);
  const publicAuthority = parsePublicAssetManifest(publicManifestSource);
  const budgetAuthority = parseBundleBudgets(budgetSource);
  let manifest;
  try {
    manifest = JSON.parse(source);
  } catch {
    fail('invalid_generation_manifest_json');
  }
  exactKeys(
    manifest,
    ['schema_version', 'generation_id', 'document_entry', 'script_entry', 'files', 'graph'],
    'generation_manifest',
  );
  if (
    manifest.schema_version !== 1 ||
    manifest.document_entry !== 'index.html' ||
    manifest.script_entry !== 'app.js' ||
    !/^[0-9a-f]{64}$/.test(manifest.generation_id) ||
    !Array.isArray(manifest.files) ||
    !Array.isArray(manifest.graph)
  ) {
    fail('invalid_generation_manifest');
  }
  const fileRecords = [];
  for (const record of manifest.files) {
    exactKeys(record, ['path', 'bytes', 'sha256'], 'generation_file');
    const filePath = canonicalRelativePath(record.path, 'generation');
    if (!Number.isSafeInteger(record.bytes) || record.bytes < 0 || !/^[0-9a-f]{64}$/.test(record.sha256)) {
      fail(`invalid_generation_file:${filePath}`);
    }
    await assertRegularPath(outputRoot, filePath);
    const bytes = await readFile(path.join(outputRoot, ...filePath.split('/')));
    if (bytes.length !== record.bytes || sha256(bytes) !== record.sha256) {
      fail(`generation_file_drift:${filePath}`);
    }
    fileRecords.push({ path: filePath, bytes: record.bytes });
  }
  const paths = fileRecords.map(({ path: filePath }) => filePath);
  if (
    paths.join('\0') !== [...paths].sort().join('\0') ||
    !paths.includes(manifest.document_entry) ||
    !paths.includes(manifest.script_entry)
  ) {
    fail('noncanonical_generation_files');
  }
  rejectPortableCollisions(paths, 'generation');

  const graph = [];
  for (const record of manifest.graph) {
    exactKeys(record, ['path', 'css_bundle', 'imports'], 'generation_graph_output');
    const outputPath = canonicalRelativePath(record.path, 'graph_output');
    if (!['.js', '.css'].includes(path.posix.extname(outputPath)) || !Array.isArray(record.imports)) {
      fail(`invalid_generation_graph_output:${outputPath}`);
    }
    const cssBundle = record.css_bundle === null ? null : canonicalRelativePath(record.css_bundle, 'graph_css_bundle');
    const imports = record.imports.map((imported) => {
      exactKeys(imported, ['path', 'dynamic'], 'generation_graph_import');
      const importedPath = canonicalRelativePath(imported.path, 'graph_import');
      if (typeof imported.dynamic !== 'boolean') fail('invalid_generation_graph_import');
      return { path: importedPath, dynamic: imported.dynamic };
    });
    const importOrder = imports.map(({ path: importedPath, dynamic }) => `${importedPath}\0${dynamic ? '1' : '0'}`);
    if (
      importOrder.join('\0') !== [...importOrder].sort().join('\0') ||
      new Set(imports.map(({ path: importedPath }) => importedPath)).size !== imports.length
    ) {
      fail(`noncanonical_generation_graph_imports:${outputPath}`);
    }
    graph.push({ path: outputPath, css_bundle: cssBundle, imports });
  }
  const graphPaths = graph.map(({ path: outputPath }) => outputPath);
  if (graphPaths.join('\0') !== [...graphPaths].sort().join('\0') || new Set(graphPaths).size !== graphPaths.length) {
    fail('noncanonical_generation_graph');
  }
  rejectPortableCollisions(graphPaths, 'generation_graph');
  const pathSet = new Set(paths);
  const publicSet = new Set(publicAuthority);
  const graphSet = new Set(graphPaths);
  if (
    publicAuthority.some((filePath) => !pathSet.has(filePath) || graphSet.has(filePath)) ||
    graphPaths.some((filePath) => !pathSet.has(filePath) || publicSet.has(filePath)) ||
    publicSet.size + graphSet.size !== pathSet.size ||
    !publicSet.has(manifest.document_entry) ||
    !graphSet.has(manifest.script_entry)
  ) {
    fail('generation_file_authority_drift');
  }
  const graphByPath = new Map(graph.map((record) => [record.path, record]));
  for (const record of graph) {
    const extension = path.posix.extname(record.path);
    if (record.css_bundle !== null) {
      if (
        extension !== '.js' ||
        path.posix.extname(record.css_bundle) !== '.css' ||
        !graphByPath.has(record.css_bundle)
      ) {
        fail(`invalid_generation_css_bundle:${record.path}`);
      }
    }
    for (const imported of record.imports) {
      if (
        path.posix.extname(imported.path) !== extension ||
        !graphByPath.has(imported.path) ||
        (imported.dynamic && extension !== '.js')
      ) {
        fail(`invalid_generation_graph_edge:${record.path}`);
      }
    }
  }

  const actual = await listFiles(outputRoot);
  const expected = [...paths, GENERATION_MANIFEST].sort();
  if (actual.join('\0') !== expected.join('\0')) fail('generation_inventory_drift');
  if (computeFrontendGenerationId(manifest) !== manifest.generation_id) {
    fail('generation_identity_drift');
  }
  if (source !== canonicalJson(canonicalGenerationManifest(manifest))) {
    fail('noncanonical_generation_manifest');
  }
  const derivedMetrics = deriveBundleMetrics({
    files: fileRecords,
    publicPaths: publicAuthority,
    graph,
    scriptEntry: manifest.script_entry,
    receiptBytes: Buffer.byteLength(source),
  });
  enforceBundleBudgets(derivedMetrics, budgetAuthority);
  return Object.freeze({ ...manifest, metrics: Object.freeze(derivedMetrics) });
}

async function publicationWatchPaths(frontendRoot) {
  const manifestPath = path.join(frontendRoot, 'public-assets.json');
  const files = parsePublicAssetManifest(await readFile(manifestPath, 'utf8'));
  return new Set(['public-assets.json', 'bundle-budgets.json', ...files.map((filePath) => `static/${filePath}`)]);
}

export async function watchFrontendPublicationInputs({ frontendRoot, onChange, onError }) {
  let watched = await publicationWatchPaths(frontendRoot);
  let running = false;
  let pending = false;
  let closed = false;
  let idle = Promise.resolve();
  let resolveIdle = () => {};
  const run = async () => {
    if (closed) return;
    if (running) {
      pending = true;
      return;
    }
    running = true;
    idle = new Promise((resolve) => {
      resolveIdle = resolve;
    });
    do {
      pending = false;
      try {
        watched = await publicationWatchPaths(frontendRoot);
        await onChange();
      } catch (error) {
        onError(error);
      }
    } while (pending);
    running = false;
    resolveIdle();
  };
  const watcher = watchFileSystem(frontendRoot, { recursive: true }, (_event, filename) => {
    if (!filename) return;
    const relative = String(filename).split(path.sep).join('/');
    if (watched.has(relative)) void run();
  });
  watcher.on('error', onError);
  return Object.freeze({
    close: async () => {
      closed = true;
      watcher.close();
      await idle;
    },
  });
}

export async function cleanFrontendGenerationOwned(outputRoot, publicRoot) {
  const parent = path.dirname(outputRoot);
  const base = path.basename(outputRoot);
  const names = await readdir(parent).catch((error) => {
    if (error?.code === 'ENOENT') return [];
    throw error;
  });
  await Promise.all(
    names
      .filter((name) => name === base || name.startsWith(`${base}.stage-`) || name.startsWith(`${base}.previous-`))
      .map((name) => rm(path.join(parent, name), { recursive: true, force: true })),
  );
  if (publicRoot) {
    await Promise.all(
      ['app.js', 'app.css', 'chunks'].map((name) => rm(path.join(publicRoot, name), { recursive: true, force: true })),
    );
  }
}

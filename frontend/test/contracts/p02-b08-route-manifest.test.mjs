import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { basename, posix, resolve } from 'node:path';
import test from 'node:test';

/**
 * @typedef {object} RegisteredRoute
 * @property {string} method
 * @property {string} registration_source
 * @property {string} template
 */
/**
 * @typedef {RegisteredRoute & {
 *   probe: string,
 *   audience: string,
 *   auth: string,
 *   origin: string,
 *   consumer_source: string,
 *   consumer_fragment: string,
 * }} ManifestRoute
 */

const repositoryRoot = basename(process.cwd()) === 'frontend' ? resolve(process.cwd(), '..') : process.cwd();
const manifestPath = 'apps/api/src/routes/P02_B08_ROUTES.tsv';
const manifestHeader = [
  'method',
  'template',
  'probe',
  'audience',
  'auth',
  'origin',
  'registration_source',
  'consumer_source',
  'consumer_fragment',
];
const methods = new Set(['DELETE', 'GET', 'PATCH', 'POST', 'PUT']);
const audiences = new Set(['developer-diagnostic', 'internal-runtime', 'product-ui', 'transport-internal']);
const authModes = new Set([
  'capability',
  'capability-or-media-ticket',
  'capability-or-origin-bootstrap',
  'capability-or-stream-ticket',
]);
const originPolicies = new Set(['allowed-local-origin-required-for-bootstrap', 'allowed-local-origin-when-present']);
const designations = new Map([
  ['@developer-diagnostic', 'developer-diagnostic'],
  ['@internal-runtime', 'internal-runtime'],
]);

/** @param {string} path */
const read = (path) => readFile(resolve(repositoryRoot, path), 'utf8');

/** @param {string} directory @returns {Promise<string[]>} */
async function rustSourcesBelow(directory) {
  /** @type {string[]} */
  const result = [];
  const entries = await readdir(resolve(repositoryRoot, directory), { withFileTypes: true });
  entries.sort((left, right) => left.name.localeCompare(right.name));
  for (const entry of entries) {
    const relative = posix.join(directory, entry.name);
    if (entry.isDirectory()) result.push(...(await rustSourcesBelow(relative)));
    else if (entry.isFile() && entry.name.endsWith('.rs') && entry.name !== 'tests.rs') result.push(relative);
  }
  return result;
}

/**
 * Route modules keep test-only items after their production definitions. The root
 * module has one test convenience function before the production router, so bind
 * that file to the explicit production entry point before removing its test module.
 * @param {string} path
 * @param {string} source
 */
function productionRouteSource(path, source) {
  if (path === 'apps/api/src/routes/mod.rs') {
    const start = source.indexOf('pub(crate) fn router_with_authority');
    assert.notEqual(start, -1, 'root route module must retain the production router entry point');
    const tests = source.indexOf('\n#[cfg(test)]\nmod tests {', start);
    assert.notEqual(tests, -1, 'root route module must retain a bounded test module');
    return source.slice(start, tests);
  }
  return source.split('\n#[cfg(test)]')[0];
}

/** @param {string} source @returns {string[]} */
function routeCalls(source) {
  /** @type {string[]} */
  const result = [];
  let cursor = 0;
  while ((cursor = source.indexOf('.route(', cursor)) !== -1) {
    const bodyStart = cursor + '.route('.length;
    let depth = 1;
    let index = bodyStart;
    let quoted = false;
    let escaped = false;
    for (; index < source.length && depth > 0; index += 1) {
      const character = source[index];
      if (quoted) {
        if (escaped) escaped = false;
        else if (character === '\\') escaped = true;
        else if (character === '"') quoted = false;
      } else if (character === '"') quoted = true;
      else if (character === '(') depth += 1;
      else if (character === ')') depth -= 1;
    }
    assert.equal(depth, 0, 'production route registration must have balanced parentheses');
    result.push(source.slice(bodyStart, index - 1));
    cursor = index;
  }
  return result;
}

/** @param {string} source @param {string} path @returns {RegisteredRoute[]} */
function registeredRoutes(source, path) {
  /** @type {RegisteredRoute[]} */
  const result = [];
  for (const call of routeCalls(source)) {
    const parsed = /^\s*"([^"]+)"\s*,([\s\S]*)$/.exec(call);
    if (!parsed || !parsed[1].startsWith('/api/v1/')) continue;
    const template = parsed[1];
    for (const match of parsed[2].matchAll(/(?:^|\.)\s*(get|post|put|patch|delete)\s*\(/g)) {
      result.push({ method: match[1].toUpperCase(), registration_source: path, template });
    }
  }
  return result;
}

/** @param {string} source @returns {ManifestRoute[]} */
function parseManifest(source) {
  assert.ok(source.endsWith('\n'), 'route manifest must end with a newline');
  const lines = source.trimEnd().split('\n');
  const header = lines.shift();
  assert.ok(header, 'route manifest must contain its schema header');
  assert.deepEqual(header.split('\t'), manifestHeader, 'route manifest schema changed without its contract');
  return lines.map((line, index) => {
    const fields = line.split('\t');
    assert.equal(fields.length, manifestHeader.length, `manifest row ${index + 2} must have nine TSV fields`);
    assert.ok(
      fields.every((field) => field.length > 0),
      `manifest row ${index + 2} contains an empty field`,
    );
    return /** @type {ManifestRoute} */ (
      Object.fromEntries(manifestHeader.map((field, fieldIndex) => [field, fields[fieldIndex]]))
    );
  });
}

/** @param {{ method: string, template: string }} route */
const routeKey = (route) => `${route.method} ${route.template}`;

/** @param {{ method: string, template: string }} left @param {{ method: string, template: string }} right */
function compareRoutes(left, right) {
  return left.template.localeCompare(right.template) || left.method.localeCompare(right.method);
}

/** @param {{ method: string, template: string }} route */
function expectedAuth(route) {
  if (route.template === '/api/v1/transport/bootstrap') return 'capability-or-origin-bootstrap';
  if (
    route.method === 'GET' &&
    ['/api/v1/install/{id}/events', '/api/v1/launch/{id}/events', '/api/v1/loaders/install/{id}/events'].includes(
      route.template,
    )
  ) {
    return 'capability-or-stream-ticket';
  }
  if (
    route.method === 'GET' &&
    [
      '/api/v1/instances/{id}/screenshots/{name}/file',
      '/api/v1/music/track',
      '/api/v1/skin/cape/file',
      '/api/v1/skin/head',
      '/api/v1/skin/lookup/cape',
      '/api/v1/skin/lookup/file',
      '/api/v1/skin/lookup/head',
      '/api/v1/skin/profile/file',
      '/api/v1/skins/{texture_key}/file',
    ].includes(route.template)
  ) {
    return 'capability-or-media-ticket';
  }
  return 'capability';
}

test('P02-B08 manifest exactly freezes every non-fallback production API method and template', async () => {
  const [manifestSource, rustPaths] = await Promise.all([read(manifestPath), rustSourcesBelow('apps/api/src/routes')]);
  const manifest = parseManifest(manifestSource);
  /** @type {RegisteredRoute[]} */
  const registered = [];
  for (const path of rustPaths) {
    const source = productionRouteSource(path, await read(path));
    registered.push(...registeredRoutes(source, path));
  }
  registered.sort(compareRoutes);

  assert.equal(manifest.length, 122, 'the frozen P02-B08 production surface must contain 122 method/template pairs');
  assert.deepEqual(
    manifest.map(({ method, registration_source, template }) => ({ method, registration_source, template })),
    registered,
    'manifest and production route registrations must have exact set and owner equality',
  );
  assert.deepEqual(
    manifest,
    [...manifest].sort(compareRoutes),
    'manifest rows must retain deterministic route ordering',
  );
  assert.equal(new Set(manifest.map(routeKey)).size, manifest.length, 'manifest method/template pairs must be unique');

  for (const route of manifest) {
    assert.ok(methods.has(route.method), `unsupported method in manifest: ${routeKey(route)}`);
    assert.ok(audiences.has(route.audience), `unsupported audience in manifest: ${routeKey(route)}`);
    assert.ok(authModes.has(route.auth), `unsupported auth mode in manifest: ${routeKey(route)}`);
    assert.ok(originPolicies.has(route.origin), `unsupported origin policy in manifest: ${routeKey(route)}`);
    assert.equal(route.auth, expectedAuth(route), `incorrect auth classification: ${routeKey(route)}`);
    assert.equal(
      route.origin,
      route.template === '/api/v1/transport/bootstrap'
        ? 'allowed-local-origin-required-for-bootstrap'
        : 'allowed-local-origin-when-present',
      `incorrect origin classification: ${routeKey(route)}`,
    );
    assert.ok(route.template.startsWith('/api/v1/'), `route template escapes v1: ${route.template}`);
    assert.ok(route.probe.startsWith('/api/v1/'), `route probe escapes v1: ${route.probe}`);
    assert.doesNotMatch(route.probe, /[{}]/, `route probe is not concrete: ${route.probe}`);
    assert.equal(
      new URL(route.probe, 'http://127.0.0.1').pathname,
      route.probe,
      `route probe has a query: ${route.probe}`,
    );
  }
});

test('P02-B08 manifest binds every route to an exact caller fragment or explicit internal designation', async () => {
  const manifest = parseManifest(await read(manifestPath));
  /** @type {Map<string, string>} */
  const cachedSources = new Map();
  for (const route of manifest) {
    const designationAudience = designations.get(route.consumer_source);
    if (designationAudience) {
      assert.equal(route.audience, designationAudience, `designation/audience mismatch: ${routeKey(route)}`);
      assert.ok(route.consumer_fragment.length >= 24, `internal designation is not specific: ${routeKey(route)}`);
      continue;
    }

    assert.match(route.consumer_source, /^frontend\/src\/.+\.(?:ts|tsx)$/, `invalid caller source: ${routeKey(route)}`);
    let consumer = cachedSources.get(route.consumer_source);
    if (consumer === undefined) {
      consumer = await read(route.consumer_source);
      cachedSources.set(route.consumer_source, consumer);
    }
    assert.ok(
      consumer.includes(route.consumer_fragment),
      `caller fragment missing for ${routeKey(route)} in ${route.consumer_source}: ${route.consumer_fragment}`,
    );
    assert.ok(route.consumer_fragment.length >= 12, `caller fragment is too weak: ${routeKey(route)}`);
  }

  const bootstrap = manifest.find((route) => route.template === '/api/v1/transport/bootstrap');
  const tickets = manifest.find((route) => route.template === '/api/v1/transport/tickets');
  assert.equal(bootstrap?.audience, 'transport-internal');
  assert.equal(tickets?.audience, 'transport-internal');

  const advancedSettings = await read('frontend/src/views/settings/AdvancedSettingsSection.tsx');
  assert.match(advancedSettings, /const loadPerformanceLabCard = __AXIAL_ENABLE_DEV_LAB__/);
  assert.match(advancedSettings, /__AXIAL_ENABLE_DEV_LAB__ && isDev && <PerformanceLabSlot \/>/);
});

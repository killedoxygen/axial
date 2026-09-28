import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { basename, resolve } from 'node:path';
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
const manifestPath = 'apps/api/src/routes/route-manifest.tsv';
const baselineManifestPath = 'legacy/apps/api/src/routes/route-manifest.tsv';
const parityReportPath = 'docs/rewrite/results/wire-parity-review.md';
// These remain release gaps, not exemptions from retained behavior. Closing or
// adding a gap requires updating this inventory and the source-review report.
/** @type {string[]} */
const missingBaselineRoutes = [];
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

/**
 * This is a bounded source inventory, not a Rust interpreter or runtime proof.
 * The composed top-level functions use rustfmt's unindented closing brace. Read
 * only the named function: a test-only helper elsewhere in transport.rs must not
 * hide production definitions, and an unmounted router must not count as live.
 * @param {string} source
 * @param {string} name
 */
function functionSource(source, name) {
  const declaration = new RegExp(`^(?:pub(?:\\(crate\\))?\\s+)?(?:async\\s+)?fn ${name}\\s*\\(`, 'm');
  const start = declaration.exec(source)?.index;
  assert.notEqual(start, undefined, `composed function ${name} must exist at module scope`);
  const end = source.indexOf('\n}', start);
  assert.notEqual(end, -1, `composed function ${name} must have a bounded body`);
  return source.slice(start, end + 2);
}

/** @param {string} source */
function productionCompositionSource(source) {
  const wrapper = functionSource(source, 'start_profile');
  assert.match(
    wrapper.slice(wrapper.indexOf('{')),
    /^\{\s*start_profile_inner\(\s*profile_root,\s*extra_origin,\s*native_login,\s*collector,\s*(?:#\[cfg\(test\)\]\s*None,\s*){2}\)\s*\.await\s*\}$/,
    'production startup must delegate directly to the shared composition without test input injection',
  );
  return functionSource(source, 'start_profile_inner');
}

/** @returns {Promise<RegisteredRoute[]>} */
async function composedRoutes() {
  const entryPath = 'apps/api/src/lib.rs';
  const transportPath = 'apps/api/src/transport.rs';
  const entry = productionCompositionSource(await read(entryPath));
  const modules = await read('apps/api/src/routes/mod.rs');
  assert.match(entry, /let router = transport::protected_router\(router, authority\.clone\(\)\);/);
  assert.doesNotMatch(entry, /\.nest(?:_service)?\s*\(/, 'extend the inventory for nested route prefixes');
  const merges = [...entry.matchAll(/\.merge\(\s*(routes(?:::[a-z_][a-z0-9_]*)+)\s*\(/g)];
  assert.ok(merges.length > 0, 'production composition must mount feature routers');
  assert.equal(
    merges.length,
    [...entry.matchAll(/\.merge\s*\(/g)].length,
    'every merged router must use an explicitly inventoried feature function',
  );
  const registered = registeredRoutes(entry, entryPath);
  for (const [, qualifiedName] of merges) {
    const parts = qualifiedName.split('::').slice(1);
    const name = parts.pop();
    assert.ok(name);
    assert.ok(parts.length <= 1, `extend the inventory for nested module ${qualifiedName}`);
    const module = parts[0];
    if (module) assert.match(modules, new RegExp(`^pub mod ${module};$`, 'm'));
    const path = `apps/api/src/routes/${module ?? 'mod'}.rs`;
    const body = functionSource(await read(path), name);
    assertInlineComposition(body, path);
    registered.push(...registeredRoutes(body, path));
  }
  const transport = functionSource(await read(transportPath), 'protected_router');
  registered.push(...registeredRoutes(transport, transportPath));
  return registered.sort(compareRoutes);
}

/** @param {string} body @param {string} path */
function assertInlineComposition(body, path) {
  assert.doesNotMatch(body, /\.(?:nest|nest_service)\s*\(/, `inventory nested prefixes in ${path}`);
  const merges = [...body.matchAll(/\.merge\s*\(/g)];
  const inline = [...body.matchAll(/\.merge\s*\(\s*Router::new\s*\(\s*\)/g)];
  assert.equal(merges.length, inline.length, `inventory non-inline merged routers in ${path}`);
}

test('route inventory accepts inline state-specific routers but rejects hidden routes and prefixes', () => {
  assert.doesNotThrow(() =>
    assertInlineComposition(
      'Router::new().merge(Router::new().route("/api/v1/retry", post(retry)).with_state(setup))',
      'fixture',
    ),
  );
  assert.throws(() => assertInlineComposition('Router::new().merge(other_router())', 'fixture'));
  assert.throws(() => assertInlineComposition('Router::new().nest("/hidden", Router::new())', 'fixture'));
  assert.throws(() => assertInlineComposition('Router::new().nest_service("/hidden", service)', 'fixture'));
});

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
    assert.ok(parsed, `route registration must use an inventoried literal path in ${path}`);
    if (path === 'apps/api/src/transport.rs' && ['/api', '/api/', '/api/{*path}'].includes(parsed[1])) {
      // Authenticated generic 404 reservations must never count as retained
      // feature implementations. Only this exact not-found adapter is excluded.
      assert.match(parsed[2], /^\s*any\s*\(\s*api_not_found\s*\)\s*,?\s*$/);
      continue;
    }
    assert.ok(parsed[1].startsWith('/api/v1/'), `route registration escapes v1 in ${path}: ${parsed[1]}`);
    const template = parsed[1];
    const matches = [...parsed[2].matchAll(/(?:^|\.)\s*(?:axum::routing::)?(get|post|put|patch|delete)\s*\(/g)];
    assert.ok(matches.length > 0, `route methods must be explicitly inventoried: ${path} ${template}`);
    for (const match of matches) {
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

// Placeholder names are Rust-local labels, not a change to the public path.
// Methods and literal path segments are never normalized away.
/** @param {{ method: string, template: string }} route */
const structuralRouteKey = (route) => `${route.method} ${route.template.replace(/\{[^/{}]+\}/g, '{}')}`;

/** @param {{ method: string, template: string }} left @param {{ method: string, template: string }} right */
function compareRoutes(left, right) {
  return left.template.localeCompare(right.template) || left.method.localeCompare(right.method);
}

/** @param {{ method: string, template: string }} route */
function expectedAuth(route) {
  if (route.template === '/api/v1/transport/bootstrap') return 'capability-or-origin-bootstrap';
  if (
    route.method === 'GET' &&
    [
      '/api/v1/install/{id}/events',
      '/api/v1/install/queue/events',
      '/api/v1/launch/{id}/events',
      '/api/v1/loaders/install/{id}/events',
    ].includes(route.template)
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

test('route-manifest freezes every explicitly composed API method, template and registration owner', async () => {
  const [manifestSource, registered] = await Promise.all([read(manifestPath), composedRoutes()]);
  const manifest = parseManifest(manifestSource);
  assert.ok(manifest.length > 0, 'the current route inventory must not be empty');
  const registrationKey = (/** @type {RegisteredRoute} */ route) => `${routeKey(route)} ${route.registration_source}`;
  const manifestKeys = new Set(manifest.map(registrationKey));
  const registeredKeys = new Set(registered.map(registrationKey));
  assert.deepEqual(
    [...manifestKeys].filter((key) => !registeredKeys.has(key)),
    [],
    'manifest contains unregistered routes or stale owners',
  );
  assert.deepEqual(
    [...registeredKeys].filter((key) => !manifestKeys.has(key)),
    [],
    'composed routes must be frozen with their exact registration owners',
  );
  assert.equal(
    new Set(registered.map(routeKey)).size,
    registered.length,
    'composed method/template pairs must be unique',
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

test('route-manifest manifest binds every route to an exact caller fragment or explicit internal designation', async () => {
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

test('route inventory selects mounted functions and includes qualified and chained methods', () => {
  const source = `
#[cfg(test)]
fn test_helper() {
    Router::new().route("/api/v1/test-only", get(test_only))
}
pub fn router() -> Router {
    Router::new().route("/api/v1/selected/{id}", axum::routing::put(update).delete(remove))
}
pub fn unmounted_router() -> Router {
    Router::new().route("/api/v1/not-mounted", get(not_mounted))
}
`;
  assert.deepEqual(registeredRoutes(functionSource(source, 'router'), 'fixture.rs'), [
    { method: 'PUT', template: '/api/v1/selected/{id}', registration_source: 'fixture.rs' },
    { method: 'DELETE', template: '/api/v1/selected/{id}', registration_source: 'fixture.rs' },
  ]);
  assert.throws(() => functionSource(source, 'missing_router'), /must exist/);
  assert.throws(() => registeredRoutes('.route(DYNAMIC_PATH, get(handler))', 'fixture.rs'), /literal path/);
  assert.throws(
    () => registeredRoutes('.route("/api/v1/hidden", any(handler))', 'fixture.rs'),
    /explicitly inventoried/,
  );
  assert.deepEqual(registeredRoutes('.route("/api/{*path}", any(api_not_found))', 'apps/api/src/transport.rs'), []);
  assert.throws(() => registeredRoutes('.route("/api/{*path}", any(handler))', 'apps/api/src/transport.rs'));
  assert.equal(
    structuralRouteKey({ method: 'GET', template: '/api/v1/launch/preflight/{id}' }),
    structuralRouteKey({ method: 'GET', template: '/api/v1/launch/preflight/{instance_id}' }),
  );
  assert.notEqual(
    structuralRouteKey({ method: 'GET', template: '/api/v1/launch/preflight/{id}' }),
    structuralRouteKey({ method: 'POST', template: '/api/v1/launch/preflight/{id}' }),
  );
});

test('route inventory follows production delegation without including test-only startup mounts', () => {
  const source = `
async fn start_profile() {
    start_profile_inner(
        profile_root,
        extra_origin,
        native_login,
        collector,
        #[cfg(test)]
        None,
        #[cfg(test)]
        None,
    ).await
}
#[cfg(test)]
async fn start_profile_with_test_endpoints() {
    Router::new().route("/api/v1/test-only-mount", get(test_only))
}
async fn start_profile_inner() {
    Router::new().route("/api/v1/production", get(production))
}
`;
  assert.deepEqual(registeredRoutes(productionCompositionSource(source), 'fixture.rs'), [
    { method: 'GET', template: '/api/v1/production', registration_source: 'fixture.rs' },
  ]);
  const slots = [...source.matchAll(/#\[cfg\(test\)\]\s*None,/g)];
  assert.equal(slots.length, 2);
  for (const slot of slots) {
    for (const replacement of ['#[cfg(test)] Some(injected),', 'None,']) {
      const changed = source.slice(0, slot.index) + replacement + source.slice(slot.index + slot[0].length);
      assert.throws(() => productionCompositionSource(changed), /without test input injection/);
    }
  }
  assert.throws(
    () => productionCompositionSource(source.replace('    start_profile_inner(', '    test_startup(')),
    /delegate directly/,
  );
});

test('retained baseline route coverage reports explicit release gaps, not behavior parity', async (context) => {
  const [baselineSource, currentSource, report, updates, content] = await Promise.all([
    read(baselineManifestPath),
    read(manifestPath),
    read(parityReportPath),
    read('apps/api/src/routes/update.rs'),
    read('apps/api/src/routes/content.rs'),
  ]);
  const baseline = parseManifest(baselineSource);
  assert.equal(baseline.length, 122, 'the preserved baseline is 122 routes, not the replacement route count');
  assert.equal(new Set(baseline.map(structuralRouteKey)).size, baseline.length);
  const retained = baseline.filter((route) => !route.template.split('/').includes('guardian'));
  const current = parseManifest(currentSource);
  const currentKeys = new Set(current.map(structuralRouteKey));
  const missing = retained.filter((route) => !currentKeys.has(structuralRouteKey(route)));
  assert.deepEqual(
    missing.map(routeKey).sort(),
    [...missingBaselineRoutes].sort(),
    'retained route coverage changed: review the current manifest and document every remaining release gap',
  );
  const gapReport = /## Current route inventory release gaps\n([\s\S]*?)(?=\n## |$)/.exec(report)?.[1];
  assert.ok(gapReport, 'the source review must distinguish the current route gaps from historical findings');
  const documentedGaps = [...gapReport.matchAll(/^\| (DELETE|GET|PATCH|POST|PUT) \| `([^`]+)` \|$/gm)].map(
    ([, method, template]) => `${method} ${template}`,
  );
  assert.deepEqual(documentedGaps.sort(), missing.map(routeKey).sort(), 'current documented route gaps must be exact');
  assert.match(gapReport, /not (?:a )?(?:runtime|behavior) parity/i);
  context.diagnostic(
    `Source registration only: ${retained.length - missing.length}/${retained.length} retained routes covered; ` +
      `${current.length} current routes. This is not behavior parity or a release gate pass.`,
  );
  for (const route of missing) context.diagnostic(`INCOMPLETE retained route: ${routeKey(route)}`);
  // A registered 501 adapter is deliberately not counted as successful feature
  // behavior. This inventory must keep that known semantic release gap visible.
  if (updates.includes('"update_unsupported"')) {
    assert.ok(report.includes('update_unsupported'), 'registered-but-unavailable updates must remain documented');
    context.diagnostic(
      'Installed update parity remains separate: unsupported packages/configurations return update_unsupported.',
    );
  }
  if (content.includes('Full modpack and override installation is not available yet.')) {
    assert.ok(
      gapReport.includes('/api/v1/content/modpack/install') && gapReport.includes('overrides'),
      'registered-but-incomplete full pack installation must remain in the current release-gap report',
    );
    context.diagnostic('INCOMPLETE retained behavior: full modpack installation and overrides remain unavailable.');
  }
});

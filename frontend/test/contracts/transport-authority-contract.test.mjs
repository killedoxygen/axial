import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import { apiFetch, initializeApiBase, setApiBaseUrl } from '../../src/api';

const repositoryRoot = basename(process.cwd()) === 'frontend' ? resolve(process.cwd(), '..') : process.cwd();
/** @param {string} path */
const read = (path) => readFile(resolve(repositoryRoot, path), 'utf8');

test('production API authority wraps every domain before body handling and binds only loopback', async () => {
  const [composition, transport, main] = await Promise.all([
    read('apps/api/src/lib.rs'),
    read('apps/api/src/transport.rs'),
    read('apps/api/src/main.rs'),
  ]);
  const protectedRouter = transport.match(/pub fn protected_router\([\s\S]*?\n\}/)?.[0] ?? '';
  assert.match(
    protectedRouter,
    /\.layer\(cors\)\s*\.layer\(middleware::from_fn_with_state\(\s*authority,\s*authenticate_request,/,
  );
  const protection = composition.indexOf('let router = transport::protected_router(router, authority.clone())');
  assert.ok(protection > composition.lastIndexOf('.merge(routes::'), 'all domains must be merged before authority');
  assert.ok(
    protection < composition.indexOf('serve(listener, router, receiver)'),
    'only the protected router is served',
  );
  assert.match(transport, /const MAX_LIVE_TICKETS: usize = 256;/);
  assert.match(transport, /TicketKind::Stream \{ target \}[\s\S]*?tickets\.remove\(&ticket\)/);
  assert.match(transport, /TicketKind::Media[\s\S]*?is_media_path\(uri\.path\(\)\)/);
  assert.match(transport, /request\.uri\(\)\.path\(\) == "\/api\/v1\/transport\/bootstrap"/);
  assert.match(transport, /if !origin_allowed \{[\s\S]*?StatusCode::FORBIDDEN/);
  assert.match(transport, /if !authority\.authenticate\(&request\) \{[\s\S]*?StatusCode::UNAUTHORIZED/);
  assert.match(transport, /if let Some\(uri\) = without_ticket\(request\.uri\(\)\)/);
  assert.doesNotMatch(transport, /derive\([^)]*Debug[^)]*\)[\s\S]{0,80}ApiTransportBootstrap/);
  assert.match(main, /start_browser\(origin\.as_deref\(\)\)/);
  assert.match(composition, /TcpListener::bind\(\(std::net::Ipv4Addr::LOCALHOST, 0\)\)/);
  assert.match(composition, /LocalApiAuthority::new\([\s\S]*?admit_profile\(&profile_root\)/);
  assert.match(transport, /if !addr\.ip\(\)\.is_loopback\(\)/);
});

test('frontend fetch, SSE, media, and restart paths carry bounded authority', async () => {
  const [api, native, launch, loaders, downloads, events, music, build] = await Promise.all([
    read('frontend/src/api.ts'),
    read('frontend/src/native.ts'),
    read('frontend/src/launch.ts'),
    read('frontend/src/loaders/api.ts'),
    read('frontend/src/machines/downloads.ts'),
    read('frontend/src/backend/events.ts'),
    read('frontend/src/music.ts'),
    read('frontend/esbuild.mjs'),
  ]);
  assert.match(native, /invoke\(cmd: string, args\?: Record<string, unknown>\): Promise<unknown>/);
  assert.match(
    native,
    /invoke\('api_transport_bootstrap'\)[\s\S]*?dtoRecord\(value, 'Native API transport bootstrap'\)/,
  );
  assert.doesNotMatch(native, /invoke<[^>]+>/);
  assert.doesNotMatch(api, /catch[\s\S]{0,120}getNativeApiTransportBootstrap/);
  assert.match(api, /headers\.set\('X-Axial-Capability', requireApiCapability\(\)\)/);
  assert.match(api, /response\.status === 401 && \(await recoverBrowserTransport\(\)\)/);
  assert.match(api, /response\.status === 401 && readOnly && \(await recoverBrowserTransport\(\)\)/);
  assert.match(api, /mintTicket\('stream', target\)/);
  assert.match(api, /withMediaTicket/);
  assert.doesNotMatch(`${api}\n${build}`, /AXIAL_WEB_API_CAPABILITY/);
  assert.doesNotMatch(build, /process\.env\.AXIAL_.*CAPABILITY/);
  assert.doesNotMatch(`${launch}\n${loaders}\n${downloads}`, /new EventSource\(apiUrl/);
  for (const source of [launch, loaders]) assert.match(source, /subscribeApiEvents/);
  assert.match(downloads, /connectInstallQueueSSE\(/);
  assert.match(loaders, /function connectInstallQueueSSE\([\s\S]*?subscribeApiEvents\('\/install\/queue\/events'/);
  assert.match(events, /await apiEventSourceUrl\(path\)[\s\S]*?new EventSource\(url\)/);
  assert.match(music, /apiResourceUrl\(`\/music\/track/);
});

test('production Tauri authority is local and development is exact-origin', async () => {
  const [cargo, capability, config, bootstrap, window] = await Promise.all([
    read('Cargo.toml'),
    read('apps/desktop/capabilities/main.json').then(JSON.parse),
    read('apps/desktop/tauri.conf.json').then(JSON.parse),
    read('apps/desktop/src/bootstrap.rs'),
    read('apps/desktop/src/window.rs'),
  ]);
  assert.match(cargo, /tauri = \{ version = "=2\.11\.2" \}/);
  assert.deepEqual(capability.permissions.slice(0, 2), ['core:event:allow-listen', 'core:event:allow-unlisten']);
  assert.equal('remote' in capability, false);
  assert.ok(!capability.permissions.includes('core:default'));
  assert.deepEqual(capability.windows, ['main']);
  assert.deepEqual(config.app.security.capabilities, ['main-capability']);
  assert.match(window, /\.on_navigation\(move \|url\| bootstrap\.allows_main_url\(url\)\)/);
  assert.match(window, /\.on_new_window\(\|_, _\| NewWindowResponse::Deny\)/);
  assert.match(window, /window\.label\(\) != MAIN_WINDOW[\s\S]*?bootstrap\.allows_main_url\(&url\)/);
  assert.match(bootstrap, /url\.origin\(\)\.ascii_serialization\(\) == origin/);
  assert.match(bootstrap, /dev_origin\.map\(admit_development_origin\)\.transpose\(\)/);
  assert.match(bootstrap, /url\.scheme\(\) != "http"[\s\S]*?!loopback/);
  assert.match(bootstrap, /pub fn api_transport_bootstrap\([\s\S]*?require_main_window\(&window, &state\)\?/);
});

test('authenticated fetch refuses off-origin targets and never retries a rejected domain mutation', async () => {
  const originalFetch = globalThis.fetch;
  let requests = 0;
  try {
    await initializeApiBase();
    setApiBaseUrl('http://127.0.0.1:38471');
    globalThis.fetch = async (_input, init) => {
      requests += 1;
      assert.equal(new Headers(init?.headers).get('X-Axial-Capability'), 'frontend-test-capability');
      assert.equal(init?.redirect, 'error');
      return new Response('', { status: 401 });
    };
    for (const target of [
      'https://example.com/api/v1/config',
      'http://127.0.0.1:38472/api/v1/config',
      'http://127.0.0.1:38471/elsewhere',
      'http://127.0.0.1:38471/api/v10/config',
    ])
      await assert.rejects(apiFetch(target), /outside the local API/);
    assert.equal(requests, 0);
    const response = await apiFetch('http://127.0.0.1:38471/api/v1/config', { method: 'PUT', body: '{}' });
    assert.equal(response.status, 401);
    assert.equal(requests, 1);
    for (const base of [
      'https://127.0.0.1:38471',
      'http://example.com',
      'http://user@127.0.0.1:38471',
      'http://127.0.0.1:38471/private',
    ]) {
      assert.throws(() => setApiBaseUrl(base), /loopback HTTP origin/);
    }
  } finally {
    globalThis.fetch = originalFetch;
    setApiBaseUrl('');
  }
});

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { basename, resolve } from 'node:path';
import test from 'node:test';

const repositoryRoot = basename(process.cwd()) === 'frontend' ? resolve(process.cwd(), '..') : process.cwd();
/** @param {string} path */
const read = (path) => readFile(resolve(repositoryRoot, path), 'utf8');

test('production API authority precedes lifecycle and body handling', async () => {
  const [routes, transport, main, app] = await Promise.all([
    read('apps/api/src/routes/mod.rs'),
    read('apps/api/src/transport.rs'),
    read('apps/api/src/main.rs'),
    read('apps/api/src/app.rs'),
  ]);
  assert.match(routes, /lifecycle_admission,[\s\S]*?local_cors_layer\(authority\)[\s\S]*?authenticate_request,/);
  assert.match(transport, /const MAX_LIVE_TICKETS: usize = 256;/);
  assert.match(transport, /TicketKind::Stream \{ target \}[\s\S]*?tickets\.remove\(&ticket\)/);
  assert.match(transport, /TicketKind::Media[\s\S]*?is_media_path\(uri\.path\(\)\)/);
  assert.match(transport, /request\.uri\(\)\.path\(\) == "\/api\/v1\/transport\/bootstrap"/);
  assert.doesNotMatch(transport, /derive\([^)]*Debug[^)]*\)[\s\S]{0,80}ApiTransportBootstrap/);
  assert.match(main, /api_addr_from_environment\(\)\?[\s\S]*?open_app_root_session/);
  assert.match(main, /if !addr\.ip\(\)\.is_loopback\(\)/);
  assert.match(app, /LocalApiAuthority::new\(addr, origin\)/);
});

test('frontend fetch, SSE, media, and restart paths carry bounded authority', async () => {
  const [api, native, launch, loaders, downloads, music, build] = await Promise.all([
    read('frontend/src/api.ts'),
    read('frontend/src/native.ts'),
    read('frontend/src/launch.ts'),
    read('frontend/src/loaders/api.ts'),
    read('frontend/src/machines/downloads.ts'),
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
  assert.match(api, /mintTicket\('stream', target\)/);
  assert.match(api, /withMediaTicket/);
  assert.doesNotMatch(`${api}\n${build}`, /AXIAL_WEB_API_CAPABILITY/);
  assert.doesNotMatch(build, /process\.env\.AXIAL_.*CAPABILITY/);
  assert.doesNotMatch(`${launch}\n${loaders}\n${downloads}`, /new EventSource\(apiUrl/);
  assert.match(`${launch}\n${loaders}\n${downloads}`, /apiEventSourceUrl/);
  assert.match(music, /apiResourceUrl\(`\/music\/track/);
});

test('production Tauri authority is local and development is exact-origin', async () => {
  const [cargo, capability, taskfile, desktop] = await Promise.all([
    read('Cargo.toml'),
    read('apps/desktop/capabilities/main.json').then(JSON.parse),
    read('Taskfile.yml'),
    read('apps/desktop/src/main.rs'),
  ]);
  assert.match(cargo, /tauri = \{ version = "=2\.11\.2" \}/);
  assert.deepEqual(capability.permissions.slice(0, 2), ['core:event:allow-listen', 'core:event:allow-unlisten']);
  assert.equal('remote' in capability, false);
  assert.ok(!capability.permissions.includes('core:default'));
  assert.match(taskfile, /"devUrl":"http:\/\/localhost:\{\{\.DEV_PORT\}\}"/);
  assert.match(taskfile, /"remote":\{"urls":\["http:\/\/localhost:\{\{\.DEV_PORT\}\}\/\*"\]\}/);
  assert.match(desktop, /\.on_navigation\(move \|url\|/);
  assert.match(desktop, /url\.origin\(\)\.ascii_serialization\(\) == origin/);
});

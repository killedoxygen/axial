import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import { basename, resolve } from 'node:path';
import test from 'node:test';

const repositoryRoot = basename(process.cwd()) === 'frontend' ? resolve(process.cwd(), '..') : process.cwd();

/** @param {string} filePath */
const read = (filePath) => readFile(resolve(repositoryRoot, filePath), 'utf8');

/** @param {string} source @param {string} name */
function taskBody(source, name) {
  const escaped = name.split(':').join('\\:');
  const match = source.match(new RegExp(`^  ${escaped}:\\n([\\s\\S]*?)(?=^  [a-zA-Z0-9:_-]+:|\\Z)`, 'm'));
  assert.ok(match, `missing Task ${name}`);
  return match[1];
}

test('the public asset manifest exactly owns tracked frontend source assets', async () => {
  const manifest = JSON.parse(await read('frontend/public-assets.json'));
  const tracked = execFileSync('git', ['ls-files', '-z', 'frontend/static'], {
    cwd: repositoryRoot,
    encoding: 'utf8',
    maxBuffer: 1024 * 1024,
    timeout: 10_000,
  })
    .split('\0')
    .filter(Boolean)
    .map((filePath) => filePath.slice('frontend/static/'.length));
  assert.deepEqual(manifest, { schema_version: 1, files: tracked.sort() });
  assert.ok(!manifest.files.some((filePath) => /^(?:app\.(?:js|css)|chunks\/)/.test(filePath)));
});

test('frozen graph budgets cannot exceed the reviewed P00 baseline', async () => {
  const policy = JSON.parse(await read('frontend/bundle-budgets.json'));
  assert.deepEqual(policy, {
    schema_version: 1,
    maximum_bytes: {
      initial_javascript: 224726,
      initial_css: 191681,
      lazy_total: 1200989,
      public_assets: 1124651,
      largest_public_asset: 929167,
      largest_generated_output: 728258,
      generated_total: 1617396,
      packaged_payload: 2742047,
    },
  });
});

test('Task, Tauri, and release consume the same complete generation', async () => {
  const [taskfile, tauri, release, ignore, prettierIgnore, packageManifest, esbuildScript, generation, loopbackLease] =
    await Promise.all([
      read('Taskfile.yml'),
      read('apps/desktop/tauri.conf.json').then(JSON.parse),
      read('.github/workflows/release.yml'),
      read('.gitignore'),
      read('frontend/.prettierignore'),
      read('frontend/package.json').then(JSON.parse),
      read('frontend/esbuild.mjs'),
      read('frontend/build-generation.mjs'),
      read('scripts/loopback-lease.mjs'),
    ]);
  assert.equal(tauri.build.frontendDist, '../../frontend/dist');
  assert.equal(packageManifest.scripts.clean, 'node esbuild.mjs clean');
  assert.match(taskBody(taskfile, 'frontend:build-budget'), /task: frontend:build/);
  assert.match(taskBody(taskfile, 'frontend:build-budget'), /node frontend\/verify-generation\.mjs/);
  assert.equal((taskBody(taskfile, 'api').match(/task: frontend:build/g) ?? []).length, 1);
  assert.doesNotMatch(taskBody(taskfile, 'api'), /assets:verify/);
  assert.equal((taskBody(taskfile, 'frontend:build').match(/assets:verify/g) ?? []).length, 1);
  assert.match(taskBody(taskfile, 'clean'), /pnpm --dir frontend run clean/);
  assert.match(taskBody(taskfile, 'frontend:build'), /pnpm --dir frontend run build/);
  assert.equal((release.match(/path: frontend\/dist/g) ?? []).length, 3);
  assert.equal((release.match(/name: Verify frontend generation/g) ?? []).length, 2);
  assert.equal((release.match(/node frontend\/verify-generation\.mjs/g) ?? []).length, 2);
  assert.doesNotMatch(release, /frontend\/static\/(?:app\.js|app\.css|chunks)|path: frontend\/static/);
  assert.match(ignore, /^frontend\/dist\/$/m);
  assert.doesNotMatch(ignore, /^frontend\/dist\.lock/m);
  assert.doesNotMatch(ignore, /^frontend\/static\/(?:app\.js|app\.css|chunks\/)/m);
  assert.match(prettierIgnore, /^dist\/$/m);
  assert.match(prettierIgnore, /^dist\.stage-\*\/$/m);
  assert.match(prettierIgnore, /^dist\.previous-\*\/$/m);
  assert.match(esbuildScript, /const enableDevLab = invocation\.mode === 'serve';/);
  assert.match(esbuildScript, /cleanFrontendGenerationOwned\(outputRoot, publicRoot\)/);
  assert.match(
    esbuildScript,
    /invocation\.mode === 'watch'[\s\S]*?await reconcileFrontendPublication\(outputRoot\);[\s\S]*?context\(/,
  );
  assert.match(generation, /\['app\.js', 'app\.css', 'chunks'\]/);
  assert.match(generation, /from '\.\.\/scripts\/loopback-lease\.mjs'/);
  assert.match(
    generation,
    /const identity = await portablePathLeaseIdentity\(outputRoot\);\s+const port = privateLoopbackLeasePort\(identity\);/,
  );
  assert.match(generation, /publication_lease_contended/);
  assert.doesNotMatch(generation, /net\.createServer|server\.listen/);
  assert.match(loopbackLease, /export async function portablePathLeaseIdentity\(candidate\)/);
  assert.match(loopbackLease, /return path[\s\S]*?\.toLowerCase\(\)/);
  assert.match(loopbackLease, /export function privateLoopbackLeasePort\(identity\)/);
  assert.match(loopbackLease, /server\.listen\(\{ host: ["']127\.0\.0\.1["'], port, exclusive: true \}\)/);
});

test('standalone API embeds only manifest files and desktop has one Tauri payload', async () => {
  const [apiCargo, desktopCargo, app, buildScript, buildSupport] = await Promise.all([
    read('apps/api/Cargo.toml'),
    read('apps/desktop/Cargo.toml'),
    read('apps/api/src/app.rs'),
    read('apps/api/build.rs'),
    read('apps/api/src/frontend_build_support.rs'),
  ]);
  assert.match(apiCargo, /default = \["embedded-frontend"\]/);
  assert.match(apiCargo, /include_dir = \{ workspace = true, optional = true \}/);
  assert.match(desktopCargo, /axial-api = \{ path = "\.\.\/api", default-features = false \}/);
  assert.match(app, /include_dir!\("\$OUT_DIR\/embedded-frontend"\)/);
  const runtimeApp = app.split('#[cfg(test)]\nmod tests')[0];
  assert.doesNotMatch(runtimeApp, /ServeDir|ServeFile|AXIAL_FRONTEND_STATIC_DIR|frontend_dir|frontend\/dist/);
  assert.doesNotMatch(app, /include_dir!\([^\n]*frontend\/static/);
  assert.match(buildScript, /manifest\.files/);
  assert.match(buildScript, /Sha256::digest/);
  assert.match(buildScript, /symlink_metadata/);
  assert.match(buildScript, /reset_frontend_destination\(&destination\)/);
  assert.match(buildSupport, /match fs::remove_dir_all\(destination\)/);
  assert.match(buildSupport, /ErrorKind::NotFound/);
  assert.match(buildSupport, /Err\(error\) => return Err\(error\)/);
  assert.match(buildScript, /embedded frontend generation is absent; run task frontend:build/);
  assert.match(buildScript, /frontend generation manifest must be a real file/);
  assert.match(buildScript, /manifest_bytes\.len\(\) as u64/);
  assert.doesNotMatch(buildScript, /read_dir\(&source\)/);

  const desktopTree = execFileSync('cargo', ['tree', '--locked', '-p', 'axial-desktop', '-e', 'features'], {
    cwd: repositoryRoot,
    encoding: 'utf8',
    maxBuffer: 1024 * 1024,
    timeout: 30_000,
  });
  assert.doesNotMatch(desktopTree, /include_dir/);
});

test('every default-feature Rust Task establishes the generation exactly once', async () => {
  const taskfile = await read('Taskfile.yml');
  assert.match(taskfile, /^  NATIVE_VERIFY_TAURI_CONFIG: '\{"build":\{"devUrl":"http:\/\/localhost:1420"\}\}'$/m);
  /** @type {Array<[string, RegExp]>} */
  const defaultFeatureTasks = [
    ['check', /cargo clippy --workspace --all-targets --all-features/],
    ['test', /cargo test --workspace --locked/],
    ['verify:rust', /task: verify:rust:ready/],
  ];
  for (const [task, cargo] of defaultFeatureTasks) {
    const body = taskBody(taskfile, task);
    assert.equal((body.match(/task: frontend:build/g) ?? []).length, 1);
    assert.match(body, cargo);
  }
  const delivery = taskBody(taskfile, 'verify:delivery');
  assert.match(delivery, /task: verify:frontend[\s\S]*task: verify:rust:ready/);
  assert.doesNotMatch(delivery, /task: verify:rust\n/);
  for (const task of ['verify:native:windows', 'verify:native:macos']) {
    const body = taskBody(taskfile, task);
    assert.match(body, /TAURI_CONFIG: "\{\{\.NATIVE_VERIFY_TAURI_CONFIG\}\}"/);
    assert.match(body, /cargo check --workspace --all-targets --no-default-features --locked/);
    assert.doesNotMatch(body, /frontend:build|all-features/);
  }
});

test('the closed capability registry owns the portable frontend generation proof', async () => {
  const [registry, scenario] = await Promise.all([
    read('scripts/capabilities/registry.mjs'),
    read('scripts/capabilities/scenarios/frontend-generation.mjs'),
  ]);
  assert.match(registry, /assetCapability\("FRONTEND", "frontend-generation", 30_000\)/);
  assert.match(scenario, /scenario_id: "CP-OA-FRONTEND"/);
  assert.match(scenario, /proof_id: "CAP-OA-FRONTEND"/);
  assert.match(scenario, /verifyFrontendGeneration/);
});

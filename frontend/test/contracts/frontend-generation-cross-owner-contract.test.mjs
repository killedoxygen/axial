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

test('frozen graph budgets cannot exceed the reviewed baseline', async () => {
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

test('Task, Tauri, and release consume the verified dist generation exactly once', async () => {
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
  // The CLI runs build hooks from apps/, not the Rust crate at apps/desktop/.
  // Verified by the actual debug bundle build, not inferred from frontendDist.
  assert.equal(tauri.build.beforeBuildCommand, 'cd ../frontend && pnpm run build');
  assert.equal(resolve(repositoryRoot, 'apps', '../frontend'), resolve(repositoryRoot, 'frontend'));
  assert.equal(packageManifest.scripts.clean, 'node esbuild.mjs clean');
  assert.match(taskBody(taskfile, 'frontend:build'), /pnpm --dir frontend run build/);
  assert.equal((release.match(/pnpm --dir frontend run build/g) ?? []).length, 1);
  assert.equal((release.match(/node frontend\/verify-generation\.mjs/g) ?? []).length, 1);
  assert.match(
    release,
    /pnpm --dir frontend run build[\s\S]*node frontend\/verify-generation\.mjs[\s\S]*cargo tauri build/,
  );
  assert.match(release, /--config '\{"build":\{"beforeBuildCommand":""\}\}'/);
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

test('standalone API embeds only verified manifest files and desktop disables its optional payload', async () => {
  const [apiCargo, desktopCargo, app, composition, buildScript, buildSupport] = await Promise.all([
    read('apps/api/Cargo.toml'),
    read('apps/desktop/Cargo.toml'),
    read('apps/api/src/frontend.rs'),
    read('apps/api/src/lib.rs'),
    read('apps/api/build.rs'),
    read('apps/api/src/frontend_build_support.rs'),
  ]);
  assert.match(apiCargo, /default = \["embedded-frontend"\]/);
  assert.match(apiCargo, /include_dir = \{ workspace = true, optional = true \}/);
  assert.match(desktopCargo, /axial-api = \{ path = "\.\.\/api", default-features = false \}/);
  assert.match(app, /include_dir!\("\$OUT_DIR\/embedded-frontend"\)/);
  assert.match(composition, /#\[cfg\(feature = "embedded-frontend"\)\]\nmod frontend;/);
  assert.match(
    composition,
    /transport::protected_router\(router, authority\.clone\(\)\)[\s\S]*frontend::router\(router\)/,
  );
  assert.match(composition, /let router = if native_login \{\s*router\s*\} else \{\s*frontend::router\(router\)/);
  const runtimeApp = app.split('#[cfg(test)]\nmod tests')[0];
  assert.doesNotMatch(runtimeApp, /ServeDir|ServeFile|AXIAL_FRONTEND_STATIC_DIR|frontend_dir|frontend\/dist/);
  assert.doesNotMatch(app, /include_dir!\([^\n]*frontend\/static/);
  assert.match(buildScript, /manifest\.files/);
  assert.match(buildScript, /Sha256::digest/);
  assert.match(buildScript, /symlink_metadata/);
  assert.match(buildScript, /reset_frontend_destination\(&destination\)/);
  assert.match(buildScript, /if env::var_os\("CARGO_FEATURE_EMBEDDED_FRONTEND"\)\.is_none\(\) \{\s*return;/);
  assert.match(buildSupport, /match fs::remove_dir_all\(destination\)/);
  assert.match(buildSupport, /ErrorKind::NotFound/);
  assert.match(buildSupport, /Err\(error\) => return Err\(error\)/);
  assert.match(buildScript, /embedded frontend generation is absent; run task frontend:build/);
  assert.match(buildScript, /frontend generation manifest must be a real file/);
  assert.match(buildScript, /manifest_bytes\.len\(\) as u64/);
  assert.doesNotMatch(buildScript, /read_dir\(&source\)/);
});

test('Task target writers retain the Cargo lease and desktop consumers establish frontend output once', async () => {
  const [taskfile, ci] = await Promise.all([read('Taskfile.yml'), read('.github/workflows/ci.yml')]);
  assert.match(
    ci,
    /pnpm --dir frontend run build[\s\S]*node frontend\/verify-generation\.mjs[\s\S]*cargo test[^\n]*-p axial-api/,
  );
  const cargoWriters = taskfile
    .split('\n')
    .filter((line) => /\bcargo (?:build|check|clippy|clean|run|test)\b/.test(line));
  assert.ok(cargoWriters.length > 0);
  for (const command of cargoWriters) {
    assert.match(command, /node scripts\/cargo-target\.mjs run -- cargo /);
  }
  for (const task of ['check', 'test', 'test:api', 'test:desktop', 'dev:api', 'dev:desktop']) {
    const body = taskBody(taskfile, task);
    assert.equal((body.match(/task: frontend:build/g) ?? []).length, 1);
    assert.match(body, /task: frontend:build[\s\S]*node scripts\/cargo-target\.mjs run -- cargo /);
  }
  for (const task of ['test:app', 'contracts:generate']) {
    const body = taskBody(taskfile, task);
    assert.match(body, /node scripts\/cargo-target\.mjs run -- cargo /);
    assert.doesNotMatch(body, /cargo (?:tauri|build).*axial-desktop/);
  }
  const desktop = taskBody(taskfile, 'dev:desktop');
  assert.match(desktop, /PORT: '1420'/);
  assert.match(desktop, /cargo tauri dev --config/);
  const configuration = desktop.match(/'(\{"build":.*\})'/);
  assert.ok(configuration);
  const development = JSON.parse(configuration[1]);
  assert.deepEqual(development, {
    build: {
      beforeDevCommand: { script: 'pnpm run dev:desktop', cwd: '{{toSlash .ROOT_DIR}}/frontend' },
      devUrl: 'http://127.0.0.1:1420',
    },
  });
});

test('build, watch and clean retain the same atomic generation owner and standalone verifier', async () => {
  const [manifest, build, verifier] = await Promise.all([
    read('frontend/package.json').then(JSON.parse),
    read('frontend/esbuild.mjs'),
    read('frontend/verify-generation.mjs'),
  ]);
  assert.equal(manifest.scripts.build, 'node esbuild.mjs');
  assert.equal(manifest.scripts.watch, 'node esbuild.mjs watch');
  assert.equal(manifest.scripts.clean, 'node esbuild.mjs clean');
  assert.match(build, /buildAndPublishFrontendGeneration/);
  assert.match(build, /acquireFrontendGenerationLease\(outputRoot\)/);
  assert.match(build, /publishFrontendGeneration/);
  assert.match(verifier, /verifyFrontendGeneration\(path\.join\(frontendRoot, 'dist'\), \{/);
  assert.match(verifier, /publicManifestPath: path\.join\(frontendRoot, 'public-assets\.json'\)/);
  assert.match(verifier, /budgetPath: path\.join\(frontendRoot, 'bundle-budgets\.json'\)/);
  assert.match(verifier, /process\.exitCode = 1/);
});

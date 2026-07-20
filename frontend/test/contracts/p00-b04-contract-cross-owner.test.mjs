import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const read = (path) => readFile(path, 'utf8');

function taskBody(source, name) {
  const escaped = name.replaceAll(':', '\\:');
  const match = source.match(new RegExp(`^  ${escaped}:\\n([\\s\\S]*?)(?=^  [a-zA-Z0-9:_-]+:|\\Z)`, 'm'));
  assert.ok(match, `missing Task ${name}`);
  return match[1];
}

test('the exact cargo-deny archive is mirrored by the pinned Linux image', async () => {
  const identity = JSON.parse(await read('toolchain.json'));
  const dockerfile = await read('.github/docker/linux-ci/Dockerfile');
  const { release, linux_archive: archive } = identity.cargo_deny;
  assert.match(
    dockerfile,
    new RegExp(`ADD --checksum=sha256:${archive.sha256}[\\s\\S]*?cargo-deny-${release}-${archive.target}\\.tar\\.gz`),
  );
  assert.match(dockerfile, new RegExp(`cargo-deny --version\\)" = "cargo-deny ${release}"`));
});

test('one Linux dependency gate is inherited by CI and release without native duplication', async () => {
  const [taskfile, ci, release] = await Promise.all([
    read('Taskfile.yml'),
    read('.github/workflows/ci.yml'),
    read('.github/workflows/release.yml'),
  ]);
  const dependencyGate = taskBody(taskfile, 'dependencies:check');
  assert.match(dependencyGate, /ensure:cargo-deny/);
  assert.match(dependencyGate, /--profile dependencies/);
  assert.equal((dependencyGate.match(/dependency-policy\.mjs check/g) ?? []).length, 1);
  assert.match(taskBody(taskfile, 'verify:linux'), /task: dependencies:check/);
  assert.doesNotMatch(
    `${taskBody(taskfile, 'verify:native:windows')}\n${taskBody(taskfile, 'verify:native:macos')}`,
    /dependencies:check|dependency-policy|cargo deny|pnpm .*audit/,
  );
  for (const workflow of [ci, release]) {
    assert.equal((workflow.match(/task verify:linux/g) ?? []).length, 1);
    assert.doesNotMatch(workflow, /cargo (?:audit|deny)|pnpm .*audit|dependency-policy/);
  }
});

test('cross-platform setup verifies the dependency tool only on Linux', async () => {
  const taskfile = await read('Taskfile.yml');
  for (const task of ['setup', 'toolchain:verify']) {
    const body = taskBody(taskfile, task);
    assert.match(body, /task: ensure:cargo-deny/);
    assert.match(
      body,
      /cmd: node scripts\/toolchain\.mjs verify --profile dependencies\n\s+platforms: \[linux\]/,
    );
  }
});

test('the policy uses the exact structured YAML dependency supplied by the frontend graph', async () => {
  const [packageManifest, implementation] = await Promise.all([
    read('frontend/package.json').then(JSON.parse),
    read('scripts/dependency-policy.mjs'),
  ]);
  assert.equal(packageManifest.devDependencies.yaml, '2.9.0');
  assert.match(implementation, /createRequire/);
  assert.match(implementation, /yaml\.parseDocument/);
  assert.match(implementation, /yamlVersion !== "2\.9\.0"/);
  assert.doesNotMatch(implementation, /split\([^\n]*pnpm-lock|match\([^\n]*pnpm-lock/);
});

test('Cargo and pnpm receive bounded weekly dependency updates', async () => {
  const dependabot = await read('.github/dependabot.yml');
  for (const [ecosystem, directory] of [
    ['cargo', '/'],
    ['npm', '/frontend'],
  ]) {
    assert.match(
      dependabot,
      new RegExp(
        `package-ecosystem: ${ecosystem}[\\s\\S]*?directory: ${directory.replace('/', '\\/')}[\\s\\S]*?interval: weekly[\\s\\S]*?open-pull-requests-limit: 3`,
      ),
    );
  }
});

test('safe lock updates remove every actionable advisory without an exception', async () => {
  const [lock, policy] = await Promise.all([read('Cargo.lock'), read('dependency-policy.json').then(JSON.parse)]);
  for (const [name, version] of [
    ['quick-xml', '0.41.0'],
    ['plist', '1.10.0'],
    ['rustls-webpki', '0.103.13'],
    ['rand', '0.8.7'],
  ]) {
    assert.match(lock, new RegExp(`name = "${name}"\\nversion = "${version.replaceAll('.', '\\.')}"`));
  }
  assert.deepEqual(policy.advisory_exceptions, []);
});

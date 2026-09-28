import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';

const repositoryRoot = basename(process.cwd()) === 'frontend' ? resolve(process.cwd(), '..') : process.cwd();
const require = createRequire(resolve(repositoryRoot, 'frontend/package.json'));
const { parse: parseYaml } = /** @type {typeof import('yaml')} */ (require('yaml'));

/** @param {string} path */
const read = (path) => readFile(resolve(repositoryRoot, path), 'utf8');

/** @param {string} source @param {string} name */
function taskBody(source, name) {
  const escaped = name.split(':').join('\\:');
  const match = source.match(new RegExp(`^  ${escaped}:\\n([\\s\\S]*?)(?=^  [a-zA-Z0-9:_-]+:|\\Z)`, 'm'));
  assert.ok(match, `missing Task ${name}`);
  return match[1];
}

/** @typedef {{'runs-on': string, needs?: string | string[], steps: Array<{run?: string, uses?: string, with?: Record<string, string>, 'continue-on-error'?: boolean}>}} WorkflowJob */
/** @param {string} source @returns {{jobs: Record<string, WorkflowJob>}} */
function workflow(source) {
  return parseYaml(source);
}

test('CI and release install the exact auditor with locked dependencies before checking policy', async () => {
  const identity = JSON.parse(await read('toolchain.json'));
  for (const file of ['.github/workflows/ci.yml', '.github/workflows/release.yml']) {
    const jobs = Object.values(workflow(await read(file)).jobs);
    const gates = jobs.filter((job) =>
      job.steps.some((step) => step.run?.includes('scripts/dependency-policy.mjs check')),
    );
    assert.equal(gates.length, 1, `${file} must own one dependency gate`);
    assert.equal(gates[0]['runs-on'], 'ubuntu-24.04');
    const commands = gates[0].steps.map((step) => step.run ?? '').join('\n');
    const version = identity.cargo_deny.release.replaceAll('.', '\\.');
    assert.match(commands, new RegExp(`cargo install cargo-deny --version ['"]=(${version})['"] --locked`));
    assert.match(commands, new RegExp(`cargo deny --version\\)" = ['"]cargo-deny ${version}['"]`));
    assert.match(
      commands,
      /cargo install cargo-deny[\s\S]*cargo deny --version[\s\S]*node scripts\/dependency-policy\.mjs check/,
    );
    for (const step of gates[0].steps.filter((step) => step.run?.includes('scripts/dependency-policy.mjs check'))) {
      assert.notEqual(step['continue-on-error'], true);
      assert.match(step.run ?? '', /exit "\$status"/);
    }
  }
});

test('local and workflow dependency gates share the retained policy without native duplication', async () => {
  const [taskfile, ci, release] = await Promise.all([
    read('Taskfile.yml'),
    read('.github/workflows/ci.yml'),
    read('.github/workflows/release.yml'),
  ]);
  const dependencyGate = taskBody(taskfile, 'dependencies:check');
  const identity = JSON.parse(await read('toolchain.json'));
  assert.match(dependencyGate, /preconditions:/);
  assert.ok(dependencyGate.includes(`cargo-deny ${identity.cargo_deny.release}`));
  assert.equal((dependencyGate.match(/dependency-policy\.mjs check/g) ?? []).length, 1);
  for (const source of [ci, release]) {
    assert.equal((source.match(/node scripts\/dependency-policy\.mjs check/g) ?? []).length, 1);
    assert.doesNotMatch(source, /cargo audit|cargo deny (?:check|--format)|pnpm .*audit/);
  }
  const releaseJobs = workflow(release).jobs;
  assert.equal(releaseJobs.package.needs, 'source');
  assert.ok(releaseJobs.source.steps.some((step) => step.run?.includes('scripts/dependency-policy.mjs check')));
  assert.ok(releaseJobs.package.steps.every((step) => !step.run?.includes('scripts/dependency-policy.mjs check')));
});

test('every frontend workflow uses the same pinned Node and package-manager identity', async () => {
  const identity = JSON.parse(await read('toolchain.json'));
  const manifest = JSON.parse(await read('frontend/package.json'));
  assert.equal(manifest.engines.node, identity.node);
  assert.equal(manifest.packageManager, `pnpm@${identity.pnpm}`);
  assert.equal(manifest.devDependencies['@types/node'], identity.node_types);
  for (const file of ['.github/workflows/ci.yml', '.github/workflows/release.yml']) {
    const source = await read(file);
    for (const job of Object.values(workflow(source).jobs)) {
      for (const step of job.steps.filter((step) => step.uses?.startsWith('actions/setup-node@'))) {
        assert.equal(step.with?.['node-version'], identity.node);
      }
      for (const step of job.steps.filter((step) => step.uses?.startsWith('pnpm/action-setup@'))) {
        assert.equal(step.with?.version, identity.pnpm);
      }
    }
    for (const match of source.matchAll(/npm install --global pnpm@([^\s]+)/g)) {
      assert.equal(match[1], identity.pnpm);
    }
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

test('lock updates retain reviewed minimum fixes and no advisory exceptions', async () => {
  const [lock, policy] = await Promise.all([read('Cargo.lock'), read('dependency-policy.json').then(JSON.parse)]);
  const packages = [...lock.matchAll(/\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"/g)];
  for (const [name, minimum] of [
    ['quick-xml', '0.41.0'],
    ['plist', '1.10.0'],
    ['rustls-webpki', '0.103.13'],
    ['rand', '0.8.7'],
  ]) {
    const versions = packages.filter((entry) => entry[1] === name).map((entry) => entry[2]);
    assert.ok(versions.length > 0, `review the removed ${name} dependency before changing its fix floor`);
    for (const version of versions) {
      assert.match(version, /^\d+\.\d+\.\d+$/, `${name} requires review for a nonstable version`);
      const actual = version.split('.').map(Number);
      const floor = minimum.split('.').map(Number);
      const difference = actual.map((part, index) => part - floor[index]).find((part) => part !== 0) ?? 0;
      assert.ok(difference >= 0, `${name} ${version} regresses below reviewed fix ${minimum}`);
    }
  }
  assert.deepEqual(policy.advisory_exceptions, []);
});

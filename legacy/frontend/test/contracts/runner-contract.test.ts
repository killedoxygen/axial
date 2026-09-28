import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readdir, readFile, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';
import {
  discoverFrontendTests,
  frontendDependencyRoot,
  runBoundedChild,
  runFrontendTests,
  selectFrontendTests,
  validateFrontendTestInventory,
} from '../runner.mjs';

const dependencyRoot = frontendDependencyRoot;

interface Fixture {
  root: string;
  receipts: string;
  testRoot: string;
  tsconfigPath: string;
}

async function write(path: string, contents: string): Promise<void> {
  await mkdir(resolve(path, '..'), { recursive: true });
  await writeFile(path, contents);
}

async function createFixture(): Promise<Fixture> {
  const root = await mkdtemp(join(tmpdir(), 'axial-runner-contract-'));
  const receipts = join(root, 'receipts');
  const testRoot = join(root, 'test');
  const tsconfigPath = join(root, 'tsconfig.json');
  await mkdir(receipts, { recursive: true });
  await writeFile(
    tsconfigPath,
    `${JSON.stringify(
      {
        compilerOptions: {
          baseUrl: '.',
          forceConsistentCasingInFileNames: true,
          isolatedModules: true,
          jsx: 'react-jsx',
          jsxImportSource: 'preact',
          module: 'ESNext',
          moduleResolution: 'bundler',
          noEmit: true,
          paths: {
            '@/*': ['./src/*'],
            react: [join(dependencyRoot, 'node_modules/preact/compat')],
          },
          resolveJsonModule: true,
          skipLibCheck: true,
          strict: true,
          target: 'ES2020',
        },
      },
      null,
      2,
    )}\n`,
  );
  await write(join(root, 'src/build-flags.d.ts'), 'declare const __AXIAL_MOCK_API__: boolean;\n');
  await write(join(root, 'src/alias.ts'), "export const marker = 'shared-alias';\n");
  return { receipts, root, testRoot, tsconfigPath };
}

function successfulTypeScriptTest(receipt: string): string {
  return `
import assert from 'node:assert/strict';
import { writeFile } from 'node:fs/promises';
import test from 'node:test';
import { createElement } from 'react';
import { marker } from '@/alias';

test('nested TypeScript fixture', async () => {
  assert.equal(marker, 'shared-alias');
  assert.equal(typeof createElement, 'function');
  await writeFile(${JSON.stringify(receipt)}, 'ts', { flag: 'wx' });
});
`;
}

function successfulMjsTest(receipt: string): string {
  return `
// @ts-check
import assert from 'node:assert/strict';
import { writeFile } from 'node:fs/promises';
import test from 'node:test';

test('nested MJS fixture', async () => {
  assert.equal(__AXIAL_MOCK_API__, false);
  await writeFile(${JSON.stringify(receipt)}, 'mjs', { flag: 'wx' });
});
`;
}

function descendantHangingTest(statePath: string, heartbeatPath: string): string {
  const descendant = `
const { appendFileSync } = require('node:fs');
appendFileSync(${JSON.stringify(heartbeatPath)}, 'start\\n');
setInterval(() => appendFileSync(${JSON.stringify(heartbeatPath)}, 'beat\\n'), 25);
`;
  return `
import { spawn } from 'node:child_process';
import { writeFileSync } from 'node:fs';

process.on('SIGTERM', () => {});
const descendant = spawn(process.execPath, ['-e', ${JSON.stringify(descendant)}], { stdio: 'ignore' });
writeFileSync(
  ${JSON.stringify(statePath)},
  JSON.stringify({ descendant: descendant.pid, leader: process.pid }),
);
setInterval(() => {}, 60_000);
`;
}

async function runFixture(fixture: Fixture, selectors: string[] = []): Promise<number> {
  return runFrontendTests({
    dependencyRoot,
    frontendRoot: fixture.root,
    graceMs: 100,
    settlementMs: 500,
    selectors,
    stdio: 'ignore',
    testRoot: fixture.testRoot,
    testTimeoutMs: 1_000,
    tsconfigPath: fixture.tsconfigPath,
    typecheckTimeoutMs: 5_000,
    wholeRunTimeoutMs: 2_000,
  });
}

async function processIsAlive(pid: number): Promise<boolean | null> {
  if (process.platform === 'linux') {
    try {
      const processStat = await readFile(`/proc/${pid}/stat`, 'utf8');
      const state = processStat.slice(processStat.lastIndexOf(')') + 2, processStat.lastIndexOf(')') + 3);
      if (state === 'X' || state === 'Z') return false;
    } catch (error) {
      if (error instanceof Error && 'code' in error && error.code === 'ENOENT') return false;
    }
  }
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    if (error instanceof Error && 'code' in error) {
      if (error.code === 'ESRCH') return false;
      if (['EACCES', 'EINVAL', 'ENOSYS', 'EPERM'].includes(String(error.code))) return null;
    }
    throw error;
  }
}

async function waitForProcessExit(pid: number): Promise<boolean | null> {
  for (let attempt = 0; attempt < 20; attempt += 1) {
    const alive = await processIsAlive(pid);
    if (alive !== true) return alive === false ? true : null;
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 50));
  }
  return false;
}

async function receiptNames(fixture: Fixture): Promise<string[]> {
  return (await readdir(fixture.receipts)).sort();
}

async function runnerTemporaryTrees(): Promise<string[]> {
  return (await readdir(tmpdir())).filter((name) => name.startsWith('axial-frontend-test-')).sort();
}

test('recursive mixed inventory preserves equal basenames, aliases, and exact targets', async () => {
  const fixture = await createFixture();
  try {
    await write(
      join(fixture.testRoot, 'nested/equal.test.ts'),
      successfulTypeScriptTest(join(fixture.receipts, 'typescript')),
    );
    await write(join(fixture.testRoot, 'other/equal.test.mjs'), successfulMjsTest(join(fixture.receipts, 'mjs')));
    await write(
      join(fixture.testRoot, 'mixed/same.test.ts'),
      successfulTypeScriptTest(join(fixture.receipts, 'same-typescript')),
    );
    await write(join(fixture.testRoot, 'mixed/same.test.mjs'), successfulMjsTest(join(fixture.receipts, 'same-mjs')));

    const inventory = await discoverFrontendTests({
      frontendRoot: fixture.root,
      testRoot: fixture.testRoot,
    });
    assert.deepEqual(
      inventory.map((entry) => entry.identity),
      ['test/mixed/same.test.mjs', 'test/mixed/same.test.ts', 'test/nested/equal.test.ts', 'test/other/equal.test.mjs'],
    );
    assert.equal(await runFixture(fixture), 0);
    assert.deepEqual(await receiptNames(fixture), ['mjs', 'same-mjs', 'same-typescript', 'typescript']);

    await rm(fixture.receipts, { recursive: true });
    await mkdir(fixture.receipts);
    assert.equal(await runFixture(fixture, ['test/nested/equal.test.ts']), 0);
    assert.deepEqual(await receiptNames(fixture), ['typescript']);

    assert.throws(() => selectFrontendTests(inventory, ['nested']), /Unknown frontend test target/);
    assert.throws(
      () => selectFrontendTests(inventory, ['test/nested/equal.test.ts', 'test/other/equal.test.mjs']),
      /at most one/,
    );
    assert.throws(() => selectFrontendTests(inventory, ['test/**/*.test.ts']), /normalized repository-relative path/);
  } finally {
    await rm(fixture.root, { force: true, recursive: true });
  }
});

test('inventory rejects case-fold aliases and symlinks', async () => {
  assert.throws(
    () =>
      validateFrontendTestInventory([
        { identity: 'test/A.test.ts', path: '/test/A.test.ts' },
        { identity: 'test/a.test.ts', path: '/test/a.test.ts' },
      ]),
    /Case-insensitive frontend test identity collision/,
  );

  const fixture = await createFixture();
  try {
    const target = join(fixture.root, 'outside.test.ts');
    await write(target, "throw new Error('must not run');\n");
    await mkdir(fixture.testRoot, { recursive: true });
    try {
      await symlink(target, join(fixture.testRoot, 'linked.test.ts'));
    } catch (error) {
      if (process.platform === 'win32' && error instanceof Error && 'code' in error) return;
      throw error;
    }
    await assert.rejects(
      discoverFrontendTests({ frontendRoot: fixture.root, testRoot: fixture.testRoot }),
      /Symlinks are not allowed/,
    );
  } finally {
    await rm(fixture.root, { force: true, recursive: true });
  }
});

test('empty and statically invalid inventories fail before execution', async () => {
  const empty = await createFixture();
  try {
    await mkdir(empty.testRoot, { recursive: true });
    await assert.rejects(runFixture(empty), /No frontend tests found/);
  } finally {
    await rm(empty.root, { force: true, recursive: true });
  }

  const invalid = await createFixture();
  try {
    await write(
      join(invalid.testRoot, 'invalid.test.mjs'),
      '/** @type {string} */\nconst invalidValue = 42;\nvoid invalidValue;\n',
    );
    await assert.rejects(runFixture(invalid), /typecheck failed/);
    assert.deepEqual(await receiptNames(invalid), []);
  } finally {
    await rm(invalid.root, { force: true, recursive: true });
  }
});

test('runtime failures are returned and top-level hangs are killed and cleaned', async () => {
  const failing = await createFixture();
  try {
    await write(
      join(failing.testRoot, 'failure.test.ts'),
      "import test from 'node:test';\ntest('fails', () => { throw new Error('expected'); });\n",
    );
    assert.equal(await runFixture(failing), 1);
  } finally {
    await rm(failing.root, { force: true, recursive: true });
  }

  const hanging = await createFixture();
  const temporaryTreesBefore = await runnerTemporaryTrees();
  try {
    const processState = join(hanging.receipts, 'processes.json');
    const heartbeat = join(hanging.receipts, 'descendant-heartbeat');
    await write(join(hanging.testRoot, 'hang.test.mjs'), descendantHangingTest(processState, heartbeat));
    const started = Date.now();
    await assert.rejects(
      runFrontendTests({
        dependencyRoot,
        frontendRoot: hanging.root,
        graceMs: 100,
        settlementMs: 500,
        stdio: 'ignore',
        testRoot: hanging.testRoot,
        testTimeoutMs: 200,
        tsconfigPath: hanging.tsconfigPath,
        typecheckTimeoutMs: 5_000,
        wholeRunTimeoutMs: 700,
      }),
      /execution exceeded its deadline/,
    );
    assert.ok(Date.now() - started < 8_000);

    const processes = JSON.parse(await readFile(processState, 'utf8')) as {
      descendant: unknown;
      leader: unknown;
    };
    assert.equal(Number.isInteger(processes.leader), true);
    assert.equal(Number.isInteger(processes.descendant), true);
    const heartbeatBefore = await readFile(heartbeat, 'utf8');
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 200));
    assert.equal(await readFile(heartbeat, 'utf8'), heartbeatBefore);

    for (const pid of [processes.leader, processes.descendant] as number[]) {
      const exited = await waitForProcessExit(pid);
      if (exited !== null) assert.equal(exited, true, `process ${pid} survived runner cleanup`);
    }
  } finally {
    await rm(hanging.root, { force: true, recursive: true });
  }
  assert.deepEqual(await runnerTemporaryTrees(), temporaryTreesBefore);
});

test('CLI environment targeting is exact and does not evaluate shell input', async () => {
  const markerRoot = await mkdtemp(join(tmpdir(), 'axial-runner-injection-'));
  const marker = join(markerRoot, 'must-not-exist');
  try {
    const targeted = await runBoundedChild(process.execPath, [join(dependencyRoot, 'test/run.mjs')], {
      cwd: dependencyRoot,
      env: { ...process.env, AXIAL_FRONTEND_TEST: 'test/look-guardian.test.mjs' },
      stdio: 'ignore',
      timeoutMs: 10_000,
    });
    assert.deepEqual({ code: targeted.code, timedOut: targeted.timedOut }, { code: 0, timedOut: false });

    const hostile = await runBoundedChild(process.execPath, [join(dependencyRoot, 'test/run.mjs')], {
      cwd: dependencyRoot,
      env: {
        ...process.env,
        AXIAL_FRONTEND_TEST: `test/look-guardian.test.mjs;touch ${marker}`,
      },
      stdio: 'ignore',
      timeoutMs: 10_000,
    });
    assert.deepEqual({ code: hostile.code, timedOut: hostile.timedOut }, { code: 1, timedOut: false });
    await assert.rejects(readFile(marker), /ENOENT/);
  } finally {
    await rm(markerRoot, { force: true, recursive: true });
  }
});

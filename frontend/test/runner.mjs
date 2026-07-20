import { spawn, spawnSync } from 'node:child_process';
import { lstat, mkdtemp, opendir, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { isAbsolute, join, posix, relative, resolve, sep } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { build } from 'esbuild';
import { createFrontendBuildSemantics } from '../build-config.mjs';

/** @typedef {{ identity: string, path: string }} FrontendTestEntry */
/** @typedef {{ frontendRoot?: string, testRoot?: string }} DiscoveryOptions */
/** @typedef {{ ok: boolean, reason: string }} TreeControlResult */
/**
 * @typedef {object} ChildOptions
 * @property {string} [cwd]
 * @property {NodeJS.ProcessEnv} [env]
 * @property {number} [graceMs]
 * @property {number} [settlementMs]
 * @property {import('node:child_process').StdioOptions} [stdio]
 * @property {number} timeoutMs
 */
/**
 * @typedef {object} TypeScriptProjectOptions
 * @property {string} dependencyRoot
 * @property {FrontendTestEntry[]} entries
 * @property {string} frontendRoot
 * @property {string} outputRoot
 * @property {string} tsconfigPath
 */
/**
 * @typedef {object} RunOptions
 * @property {string} [dependencyRoot]
 * @property {string} [frontendRoot]
 * @property {number} [graceMs]
 * @property {number} [settlementMs]
 * @property {string[]} [selectors]
 * @property {import('node:child_process').StdioOptions} [stdio]
 * @property {string} [testRoot]
 * @property {number} [testTimeoutMs]
 * @property {string} [tsconfigPath]
 * @property {number} [typecheckTimeoutMs]
 * @property {number} [wholeRunTimeoutMs]
 */

const moduleFrontendRoot = fileURLToPath(new URL('../', import.meta.url));
export const frontendDependencyRoot = moduleFrontendRoot;
const testPattern = /\.test\.(?:mjs|ts)$/;
const globPattern = /[*?\[\]{}]/;

/** @param {string} root @param {string} path */
function normalizedRelativePath(root, path) {
  const pathFromRoot = relative(root, path);
  if (pathFromRoot === '' || isAbsolute(pathFromRoot) || pathFromRoot.startsWith(`..${sep}`)) {
    throw new Error(`Test path escapes the frontend root: ${path}`);
  }
  return pathFromRoot.split(sep).join('/').normalize('NFC');
}

/** @param {string} left @param {string} right */
function stableCompare(left, right) {
  return left < right ? -1 : left > right ? 1 : 0;
}

/** @param {DiscoveryOptions} [options] @returns {Promise<FrontendTestEntry[]>} */
export async function discoverFrontendTests({
  frontendRoot = moduleFrontendRoot,
  testRoot = resolve(frontendRoot, 'test'),
} = {}) {
  const rootInfo = await lstat(testRoot);
  if (rootInfo.isSymbolicLink() || !rootInfo.isDirectory()) {
    throw new Error(`Frontend test root must be a real directory: ${testRoot}`);
  }

  /** @type {FrontendTestEntry[]} */
  const discovered = [];
  /** @param {string} directory */
  async function walk(directory) {
    const entries = [];
    const handle = await opendir(directory);
    for await (const entry of handle) entries.push(entry);
    entries.sort((left, right) => stableCompare(left.name, right.name));

    for (const entry of entries) {
      const path = join(directory, entry.name);
      if (entry.isSymbolicLink()) {
        throw new Error(`Symlinks are not allowed in the frontend test inventory: ${path}`);
      }
      if (entry.isDirectory()) {
        await walk(path);
      } else if (entry.isFile() && testPattern.test(entry.name)) {
        discovered.push({ identity: normalizedRelativePath(frontendRoot, path), path });
      }
    }
  }

  await walk(testRoot);
  discovered.sort((left, right) => stableCompare(left.identity, right.identity));
  return validateFrontendTestInventory(discovered);
}

/** @param {FrontendTestEntry[]} discovered @returns {FrontendTestEntry[]} */
export function validateFrontendTestInventory(discovered) {
  if (discovered.length === 0) throw new Error('No frontend tests found');

  const exact = new Set();
  const caseFolded = new Map();
  for (const entry of discovered) {
    if (exact.has(entry.identity)) {
      throw new Error(`Duplicate normalized frontend test identity: ${entry.identity}`);
    }
    exact.add(entry.identity);

    const folded = entry.identity.toLowerCase();
    const conflicting = caseFolded.get(folded);
    if (conflicting) {
      throw new Error(`Case-insensitive frontend test identity collision: ${conflicting} and ${entry.identity}`);
    }
    caseFolded.set(folded, entry.identity);
  }

  return discovered;
}

/**
 * @param {FrontendTestEntry[]} inventory
 * @param {string[]} [selectors]
 * @returns {FrontendTestEntry[]}
 */
export function selectFrontendTests(inventory, selectors = []) {
  if (selectors.length === 0) return inventory;
  if (selectors.length !== 1) throw new Error('Expected at most one exact frontend test path');

  const selector = selectors[0];
  if (
    !selector ||
    isAbsolute(selector) ||
    selector.includes('\\') ||
    globPattern.test(selector) ||
    selector !== selector.normalize('NFC') ||
    selector !== posix.normalize(selector) ||
    selector.startsWith('../')
  ) {
    throw new Error(`Frontend test target must be a normalized repository-relative path: ${selector}`);
  }

  const selected = inventory.find((entry) => entry.identity === selector);
  if (!selected) throw new Error(`Unknown frontend test target: ${selector}`);
  return [selected];
}

/**
 * @param {number | undefined} pid
 * @param {NodeJS.Signals} signal
 * @param {boolean} force
 * @returns {TreeControlResult}
 */
function signalProcessTree(pid, signal, force) {
  if (pid == null) {
    return { ok: false, reason: 'child PID was unavailable' };
  }
  if (process.platform === 'win32') {
    const args = ['/pid', String(pid), '/t'];
    if (force) args.push('/f');
    const result = spawnSync('taskkill', args, {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      timeout: 1_000,
      windowsHide: true,
    });
    if (result.error) {
      const detail = 'code' in result.error ? String(result.error.code) : result.error.name;
      return { ok: false, reason: `taskkill failed with ${detail}` };
    }
    if (result.signal) return { ok: false, reason: `taskkill ended from ${result.signal}` };
    if (result.status !== 0) return { ok: false, reason: `taskkill exited with status ${result.status}` };
    return { ok: true, reason: 'taskkill confirmed tree termination' };
  }

  try {
    process.kill(-pid, signal);
    return { ok: true, reason: `${signal} was delivered to process group ${pid}` };
  } catch (error) {
    if (error instanceof Error && 'code' in error && error.code === 'ESRCH') {
      return { ok: true, reason: `process group ${pid} was already absent` };
    }
    const reason = error instanceof Error && 'code' in error ? String(error.code) : 'unknown error';
    return { ok: false, reason: `${signal} process-group control failed with ${reason}` };
  }
}

/** @param {number} processGroup @returns {Promise<'active' | 'settled' | 'unprovable'>} */
async function probePosixProcessGroup(processGroup) {
  try {
    process.kill(-processGroup, 0);
  } catch (error) {
    if (error instanceof Error && 'code' in error && error.code === 'ESRCH') return 'settled';
    if (process.platform !== 'linux') return 'unprovable';
  }

  if (process.platform !== 'linux') return 'active';

  let unreadable = false;
  const proc = await opendir('/proc');
  for await (const entry of proc) {
    if (!entry.isDirectory() || !/^\d+$/.test(entry.name)) continue;
    try {
      const stat = await readFile(join('/proc', entry.name, 'stat'), 'utf8');
      const fields = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
      const state = fields[0];
      const group = Number(fields[2]);
      if (group === processGroup && state !== 'X' && state !== 'Z') return 'active';
    } catch (error) {
      if (!(error instanceof Error) || !('code' in error) || error.code !== 'ENOENT') unreadable = true;
    }
  }
  return unreadable ? 'unprovable' : 'settled';
}

/** @param {number | undefined} processGroup @param {number} timeoutMs */
async function waitForPosixProcessGroupSettlement(processGroup, timeoutMs) {
  if (processGroup == null) return { settled: false, reason: 'process group ID was unavailable' };
  const deadline = Date.now() + timeoutMs;
  do {
    const state = await probePosixProcessGroup(processGroup);
    if (state === 'settled') return { settled: true, reason: `process group ${processGroup} has no live members` };
    if (state === 'unprovable') {
      return { settled: false, reason: `process group ${processGroup} could not be inspected` };
    }
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 25));
  } while (Date.now() < deadline);
  return { settled: false, reason: `process group ${processGroup} remained live` };
}

/**
 * @param {string} command
 * @param {string[]} args
 * @param {ChildOptions} options
 * @returns {Promise<{ code: number, signal: NodeJS.Signals | null, timedOut: boolean }>}
 */
export function runBoundedChild(
  command,
  args,
  { cwd, env = process.env, graceMs = 1_000, settlementMs = 1_000, stdio = 'inherit', timeoutMs },
) {
  if (!Number.isInteger(timeoutMs) || timeoutMs <= 0) {
    throw new Error(`Child timeout must be a positive integer, received ${timeoutMs}`);
  }
  if (!Number.isInteger(graceMs) || graceMs < 0) {
    throw new Error(`Child termination grace must be a non-negative integer, received ${graceMs}`);
  }
  if (!Number.isInteger(settlementMs) || settlementMs <= 0) {
    throw new Error(`Child settlement timeout must be a positive integer, received ${settlementMs}`);
  }

  return new Promise((resolveChild, rejectChild) => {
    /** @type {import('node:child_process').ChildProcess} */
    const child = spawn(command, args, {
      cwd,
      detached: process.platform !== 'win32',
      env,
      stdio,
      windowsHide: true,
    });
    const pid = child.pid;
    let settled = false;
    let timedOut = false;
    let forceSent = false;
    let treeSettled = false;
    let treeSettlementReason = 'tree settlement was not checked';
    /** @type {TreeControlResult | undefined} */
    let gracefulControl;
    /** @type {TreeControlResult | undefined} */
    let forcedControl;
    /** @type {{ code: number, signal: NodeJS.Signals | null } | undefined} */
    let closeResult;
    /** @type {NodeJS.Timeout | undefined} */
    let forceTimer;
    /** @type {NodeJS.Timeout | undefined} */
    let settlementTimer;

    /** @param {() => void} complete */
    function settle(complete) {
      if (settled) return;
      settled = true;
      clearTimeout(deadline);
      clearTimeout(forceTimer);
      clearTimeout(settlementTimer);
      complete();
    }

    const deadline = setTimeout(() => {
      timedOut = true;
      gracefulControl = signalProcessTree(pid, 'SIGTERM', false);
      forceTimer = setTimeout(() => {
        forceSent = true;
        forcedControl = signalProcessTree(pid, 'SIGKILL', true);
        settlementTimer = setTimeout(() => {
          child.stdin?.destroy();
          child.stdout?.destroy();
          child.stderr?.destroy();
          child.unref();
          const controlFailures = [gracefulControl, forcedControl]
            .filter((result) => result && !result.ok)
            .map((result) => result?.reason)
            .join('; ');
          settle(() =>
            rejectChild(
              new Error(
                `Child process tree did not provably settle within ${settlementMs}ms after forced termination: ${treeSettlementReason}${controlFailures ? `; ${controlFailures}` : ''}`,
              ),
            ),
          );
        }, settlementMs);

        if (process.platform === 'win32') {
          treeSettled = forcedControl.ok || Boolean(gracefulControl?.ok);
          treeSettlementReason = treeSettled
            ? forcedControl.ok
              ? forcedControl.reason
              : (gracefulControl?.reason ?? 'taskkill result unavailable')
            : `no taskkill tree-control attempt succeeded (${forcedControl.reason})`;
          const result = closeResult;
          if (treeSettled && result) settle(() => resolveChild({ ...result, timedOut: true }));
          return;
        }

        void waitForPosixProcessGroupSettlement(pid, settlementMs)
          .then((proof) => {
            if (settled) return;
            treeSettled = proof.settled;
            treeSettlementReason = proof.reason;
            const result = closeResult;
            if (treeSettled && result) settle(() => resolveChild({ ...result, timedOut: true }));
          })
          .catch((error) => {
            if (settled) return;
            treeSettlementReason = `process-group inspection failed with ${error instanceof Error ? error.name : 'unknown error'}`;
          });
      }, graceMs);
    }, timeoutMs);

    child.once('error', (error) => {
      settle(() => rejectChild(error));
    });
    child.once('close', (code, signal) => {
      const result = { code: code ?? 1, signal };
      closeResult = result;
      if (!timedOut) settle(() => resolveChild({ ...result, timedOut: false }));
      else if (forceSent && treeSettled) settle(() => resolveChild({ ...result, timedOut: true }));
    });
  });
}

/** @param {TypeScriptProjectOptions} options */
async function writeTypeScriptProject({ dependencyRoot, entries, frontendRoot, outputRoot, tsconfigPath }) {
  const buildFlags = resolve(frontendRoot, 'src/build-flags.d.ts');
  const files = [buildFlags, ...entries.map((entry) => entry.path)];
  const generatedPath = resolve(outputRoot, 'tsconfig.json');
  await writeFile(
    generatedPath,
    `${JSON.stringify(
      {
        extends: tsconfigPath,
        compilerOptions: {
          allowJs: true,
          allowImportingTsExtensions: true,
          checkJs: true,
          noEmit: true,
          typeRoots: [resolve(dependencyRoot, 'node_modules/@types')],
          types: ['node'],
        },
        files,
        include: [
          resolve(frontendRoot, 'src/**/*.d.ts'),
          resolve(frontendRoot, 'src/**/*.ts'),
          resolve(frontendRoot, 'src/**/*.tsx'),
        ],
      },
      null,
      2,
    )}\n`,
  );
  return generatedPath;
}

/** @param {TypeScriptProjectOptions} options @returns {Promise<string[]>} */
async function compileTests({ dependencyRoot, entries, frontendRoot, outputRoot, tsconfigPath }) {
  const semantics = createFrontendBuildSemantics({
    dependencyRoot,
    enableDevLab: false,
    enableMockApi: false,
    webApiBase: '',
  });
  const runnerModule = resolve(dependencyRoot, 'test/runner.mjs');
  const result = await build({
    absWorkingDir: frontendRoot,
    bundle: true,
    entryNames: '[dir]/[name]',
    entryPoints: Object.fromEntries(entries.map((entry) => [entry.identity, entry.path])),
    format: 'esm',
    metafile: true,
    outExtension: { '.js': '.mjs' },
    outbase: frontendRoot,
    outdir: outputRoot,
    platform: 'node',
    sourcemap: 'inline',
    tsconfig: tsconfigPath,
    ...semantics,
    plugins: [
      ...(semantics.plugins ?? []),
      {
        name: 'external-test-runner-module',
        /** @param {import('esbuild').PluginBuild} pluginBuild */
        setup(pluginBuild) {
          pluginBuild.onResolve({ filter: /^\.\.\/runner\.mjs$/ }, (args) => {
            if (resolve(args.resolveDir, args.path) !== runnerModule) return;
            return { external: true, path: pathToFileURL(runnerModule).href };
          });
        },
      },
    ],
  });

  if (!result.metafile) throw new Error('Frontend test compiler did not return output metadata');
  return Object.entries(result.metafile.outputs)
    .filter(([, metadata]) => metadata.entryPoint)
    .map(([path]) => (isAbsolute(path) ? path : resolve(frontendRoot, path)))
    .sort(stableCompare);
}

/** @param {RunOptions} [options] @returns {Promise<number>} */
export async function runFrontendTests({
  dependencyRoot = moduleFrontendRoot,
  frontendRoot = moduleFrontendRoot,
  graceMs = 1_000,
  settlementMs = 1_000,
  selectors = [],
  stdio = 'inherit',
  testRoot = resolve(frontendRoot, 'test'),
  testTimeoutMs = 30_000,
  tsconfigPath = resolve(frontendRoot, 'tsconfig.json'),
  typecheckTimeoutMs = 30_000,
  wholeRunTimeoutMs = 45_000,
} = {}) {
  const inventory = await discoverFrontendTests({ frontendRoot, testRoot });
  const selected = selectFrontendTests(inventory, selectors);
  const outputRoot = await mkdtemp(resolve(tmpdir(), 'axial-frontend-test-'));

  try {
    const generatedTsconfig = await writeTypeScriptProject({
      dependencyRoot,
      entries: selected,
      frontendRoot,
      outputRoot,
      tsconfigPath,
    });
    const typescriptCli = resolve(dependencyRoot, 'node_modules/typescript/bin/tsc');
    const typecheck = await runBoundedChild(
      process.execPath,
      [typescriptCli, '--pretty', 'false', '--project', generatedTsconfig],
      {
        cwd: frontendRoot,
        graceMs,
        settlementMs,
        stdio,
        timeoutMs: typecheckTimeoutMs,
      },
    );
    if (typecheck.timedOut) throw new Error('Frontend test typecheck exceeded its deadline');
    if (typecheck.code !== 0) throw new Error(`Frontend test typecheck failed with code ${typecheck.code}`);

    const outputs = await compileTests({
      dependencyRoot,
      entries: selected,
      frontendRoot,
      outputRoot,
      tsconfigPath,
    });
    if (outputs.length !== selected.length) {
      throw new Error(`Expected ${selected.length} compiled frontend tests, received ${outputs.length}`);
    }

    const testEnvironment = { ...process.env };
    delete testEnvironment.NODE_TEST_CONTEXT;
    const testRun = await runBoundedChild(process.execPath, ['--test', `--test-timeout=${testTimeoutMs}`, ...outputs], {
      cwd: frontendRoot,
      env: testEnvironment,
      graceMs,
      settlementMs,
      stdio,
      timeoutMs: wholeRunTimeoutMs,
    });
    if (testRun.timedOut) throw new Error('Frontend test execution exceeded its deadline');
    return testRun.code;
  } finally {
    await rm(outputRoot, { force: true, recursive: true });
  }
}

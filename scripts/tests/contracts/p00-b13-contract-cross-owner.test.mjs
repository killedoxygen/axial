import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { EventEmitter, once } from "node:events";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test, { after } from "node:test";

import {
  acquireCargoTargetLease,
  CargoTargetError,
  cargoTargetContainment,
  cargoTargetLeasePort,
  cargoTargetQuiescence,
  parseCargoTargetInvocation,
  runCargoTarget,
  settleCargoProcessGroupAfterClose,
  terminateCargoProcessTree,
} from "../../cargo-target.mjs";
import {
  BuildStorageError,
  main as buildStorageMain,
} from "../../build-storage.mjs";
import { acquireExclusiveLoopbackPort } from "../../loopback-lease.mjs";

const repositoryRoot = path.resolve(".");
const cargoRunner = "node scripts/cargo-target.mjs run -- cargo";
const focusedContract = "scripts/tests/contracts/p00-b13-contract.test.mjs";
const crossOwnerContract =
  "scripts/tests/contracts/p00-b13-contract-cross-owner.test.mjs";
const temporaryRoots = [];

after(async () => {
  await Promise.all(
    temporaryRoots.map((root) => rm(root, { recursive: true, force: true })),
  );
});

async function temporaryRoot(label) {
  const root = await mkdtemp(path.join(os.tmpdir(), `axial-${label}-`));
  temporaryRoots.push(root);
  return root;
}

async function waitFor(check, timeout = 5_000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const value = await check();
    if (value !== undefined) return value;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error("timed out waiting for fixture state");
}

function runTask(args) {
  const result = spawnSync("task", args, {
    cwd: repositoryRoot,
    encoding: "utf8",
    env: { ...process.env, NO_COLOR: "1" },
    timeout: 10_000,
  });
  assert.equal(result.error, undefined, result.error?.message);
  assert.equal(result.status, 0, result.stderr || result.stdout);
  return `${result.stdout}${result.stderr}`;
}

function occurrences(source, value) {
  return source.split(value).length - 1;
}

function expectCargoError(argv, code) {
  assert.throws(
    () => parseCargoTargetInvocation(argv),
    (error) => error instanceof CargoTargetError && error.code === code,
  );
}

const rawCargoNonwriters = Object.freeze([
  /^- cargo fetch --locked$/,
  /^- cargo fmt --all(?: --check)?$/,
  /^echo "(?:cargo|rustfmt|clippy|tauri|deny)\s+\$\(cargo (?:--version|fmt --version|clippy --version|tauri --version|deny --version) 2>\/dev\/null \|\| echo .+\)"$/,
  /^cargo binstall -y --force --locked tauri-cli --version "=\$tauri_cli_version"$/,
  /^cargo install tauri-cli --version "=\$tauri_cli_version" --locked --force$/,
  /^tauri_version_output="\$\(cargo tauri --version 2>\/dev\/null \|\| true\)"$/,
  /^test "\$\(cargo (?:tauri|deny) --version\)" = "(?:tauri-cli|cargo-deny) \$[a-z_]+"$/,
  /^if \[ "\$\(cargo deny --version 2>\/dev\/null \|\| true\)" = "cargo-deny \$cargo_deny_version" \]; then$/,
]);

function assertClosedTaskCargoInvocations(source) {
  const cargoLines = source
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => !line.startsWith("#") && /\bcargo(?:\s|$)/.test(line));
  assert.ok(cargoLines.length > 0);
  for (const line of cargoLines) {
    if (line.includes(cargoRunner)) {
      assert.equal(occurrences(line, cargoRunner), 1);
      assert.match(
        line.slice(line.indexOf(cargoRunner) + cargoRunner.length),
        /^ (?:build|check|clippy|clean|run|test|tauri (?:dev|build))\b/,
      );
      continue;
    }
    assert.ok(
      rawCargoNonwriters.some((pattern) => pattern.test(line)),
      `unclassified raw Cargo invocation: ${line}`,
    );
  }
  return cargoLines;
}

test("Cargo target invocation is closed around fixed writer commands", () => {
  assert.deepEqual(
    parseCargoTargetInvocation([
      "run",
      "--",
      "cargo",
      "build",
      "--locked",
      "-p",
      "axial-api",
    ]),
    {
      cargoArgs: ["build", "--locked", "-p", "axial-api"],
      cwd: ".",
    },
  );
  assert.deepEqual(
    parseCargoTargetInvocation([
      "run",
      "--",
      "cargo",
      "tauri",
      "dev",
      "--config",
      '{"build":{"devUrl":"http://localhost:1420"}}',
      "--",
      "--locked",
    ]),
    {
      cargoArgs: [
        "tauri",
        "dev",
        "--config",
        '{"build":{"devUrl":"http://localhost:1420"}}',
        "--",
        "--locked",
      ],
      cwd: "apps/desktop",
    },
  );

  for (const [argv, code] of [
    [["run", "--", "cargo", "metadata"], "command_not_allowed"],
    [["run", "cargo", "build"], "invalid_invocation"],
    [["run", "--", "rustc", "build"], "invalid_invocation"],
    [
      ["run", "--", "cargo", "build", "--target-dir=/tmp/out"],
      "target_dir_override_forbidden",
    ],
    [
      ["run", "--", "cargo", "test", "--manifest-path", "/tmp/Cargo.toml"],
      "manifest_path_forbidden",
    ],
    [
      ["run", "--", "cargo", "check", "--config", "net.retry=3"],
      "cargo_config_forbidden",
    ],
    [
      ["run", "--", "cargo", "tauri", "dev", "--", "--config=net.retry=3"],
      "cargo_config_forbidden",
    ],
    [
      ["run", "--", "cargo", "tauri", "dev", "--config", "tauri.local.json"],
      "invalid_tauri_config",
    ],
    [
      ["run", "--", "cargo", "tauri", "build", "--config", "[]"],
      "invalid_tauri_config",
    ],
    [
      [
        "run",
        "--",
        "cargo",
        "tauri",
        "build",
        `--config={"value":"${"x".repeat(33 * 1024)}"}`,
      ],
      "invalid_tauri_config",
    ],
    ...[
      "--artifact-dir=/tmp/artifacts",
      "--build-dir=/tmp/build",
      "--lockfile-path=/tmp/Cargo.lock",
      "--out-dir=/tmp/out",
    ].map((option) => [
      ["run", "--", "cargo", "build", option],
      "external_output_forbidden",
    ]),
    [["run", "--", "cargo", "tauri", "serve"], "command_not_allowed"],
    [["run", "--", "cargo", "build\0hidden"], "invalid_invocation"],
  ]) {
    expectCargoError(argv, code);
  }
});

test("Cargo target wrapper fixes environment, cwd, settlement, and release order", async () => {
  const root = await temporaryRoot("cargo-wrapper");
  await mkdir(path.join(root, "target"));
  await mkdir(path.join(root, "apps", "desktop"), { recursive: true });
  const signalSource = new EventEmitter();
  const events = [];
  const controlledSignals = [];
  const spawnImpl = (command, args, options) => {
    assert.equal(command, "cargo");
    assert.deepEqual(args, ["build", "--locked"]);
    assert.equal(options.cwd, root);
    assert.equal(options.shell, false);
    assert.equal(options.detached, true);
    assert.equal(options.stdio, "inherit");
    assert.equal(options.env.CARGO_TARGET_DIR, path.join(root, "target"));
    assert.equal(Object.hasOwn(options.env, "CARGO_BUILD_TARGET_DIR"), false);
    assert.equal(Object.hasOwn(options.env, "CARGO_BUILD_BUILD_DIR"), false);
    assert.equal(Object.hasOwn(options.env, "cargo_target_dir"), false);
    events.push("spawn");
    const child = new EventEmitter();
    child.pid = 321;
    queueMicrotask(() => {
      signalSource.emit("SIGTERM");
    });
    setImmediate(() => {
      events.push("close");
      child.emit("close", null, "SIGKILL");
    });
    return child;
  };
  const status = await runCargoTarget(
    ["run", "--", "cargo", "build", "--locked"],
    {
      repositoryRoot: root,
      signalSource,
      spawnImpl,
      env: {
        CARGO_TARGET_DIR: "/tmp/untrusted",
        CARGO_BUILD_TARGET_DIR: "/tmp/untrusted-build",
        CARGO_BUILD_BUILD_DIR: "/tmp/untrusted-build-dir",
        cargo_target_dir: "/tmp/untrusted-case",
      },
      terminateTreeImpl: async (_child, signal) => {
        controlledSignals.push(signal);
        events.push("tree");
        return true;
      },
      acquireLeaseImpl: async () => {
        events.push("acquire");
        return async () => events.push("release");
      },
    },
  );
  assert.equal(status, 128 + os.constants.signals.SIGTERM);
  assert.deepEqual(controlledSignals, ["SIGTERM"]);
  assert.deepEqual(events, ["acquire", "spawn", "tree", "close", "release"]);

  let symlinkRelease = false;
  await assert.rejects(
    runCargoTarget(["run", "--", "cargo", "clean"], {
      repositoryRoot: root,
      signalSource: new EventEmitter(),
      acquireLeaseImpl: async () => async () => {
        symlinkRelease = true;
      },
      lstatImpl: async () => ({
        isSymbolicLink: () => true,
        isDirectory: () => false,
      }),
      spawnImpl: () => assert.fail("symlink target must not reach Cargo"),
    }),
    (error) =>
      error instanceof CargoTargetError &&
      error.code === "target_root_is_symlink",
  );
  assert.equal(symlinkRelease, true);

  await runCargoTarget(
    ["run", "--", "cargo", "tauri", "build", "--", "--locked"],
    {
      repositoryRoot: root,
      signalSource: new EventEmitter(),
      acquireLeaseImpl: async () => async () => {},
      settleNaturalTreeImpl: async () => true,
      spawnImpl: (_command, _args, options) => {
        assert.equal(options.cwd, path.join(root, "apps", "desktop"));
        const tauriChild = new EventEmitter();
        tauriChild.kill = () => {};
        queueMicrotask(() => tauriChild.emit("close", 0, null));
        return tauriChild;
      },
    },
  );
});

test("child errors are recorded without releasing the lease before close", async () => {
  const root = await temporaryRoot("cargo-error-close");
  await mkdir(path.join(root, "target"));
  const events = [];
  await assert.rejects(
    runCargoTarget(["run", "--", "cargo", "check"], {
      repositoryRoot: root,
      signalSource: new EventEmitter(),
      acquireLeaseImpl: async () => {
        events.push("acquire");
        return async () => events.push("release");
      },
      spawnImpl: () => {
        const child = new EventEmitter();
        queueMicrotask(() => {
          events.push("error");
          child.emit("error", new Error("fixture spawn error"));
          setImmediate(() => {
            events.push("close");
            child.emit("close", -2, null);
          });
        });
        return child;
      },
    }),
    (error) =>
      error instanceof CargoTargetError && error.code === "spawn_failed",
  );
  assert.deepEqual(events, ["acquire", "error", "close", "release"]);
});

test("tree control is platform-aware, bounded, and preserves the initiating signal", async () => {
  const posixSignals = [];
  let probes = 0;
  assert.equal(
    await terminateCargoProcessTree({ pid: 412 }, "SIGINT", {
      platform: "linux",
      graceMilliseconds: 0,
      settlementMilliseconds: 0,
      processKillImpl: (pid, signal) => posixSignals.push([pid, signal]),
      linuxGroupProbeImpl: async () => {
        probes += 1;
        return probes < 3;
      },
    }),
    true,
  );
  assert.deepEqual(posixSignals, [
    [-412, "SIGINT"],
    [-412, "SIGTERM"],
    [-412, "SIGKILL"],
  ]);

  const rollbackSignals = [];
  const readings = [100, 90, 100, 90];
  let sleeps = 0;
  assert.equal(
    await settleCargoProcessGroupAfterClose(
      { pid: 513 },
      {
        platform: "linux",
        graceMilliseconds: 1_000,
        settlementMilliseconds: 2_000,
        monotonicNowImpl: () => readings.shift() ?? 0,
        sleepImpl: async () => {
          sleeps += 1;
        },
        processKillImpl: (pid, signal) => rollbackSignals.push([pid, signal]),
        linuxGroupProbeImpl: async () => true,
      },
    ),
    false,
  );
  assert.deepEqual(rollbackSignals, [
    [-513, "SIGTERM"],
    [-513, "SIGKILL"],
  ]);
  assert.equal(sleeps, 0, "clock rollback must fail the bound immediately");

  const windowsCalls = [];
  assert.equal(
    await terminateCargoProcessTree({ pid: 731 }, "SIGHUP", {
      platform: "win32",
      taskkillPath: "C:\\Windows\\System32\\taskkill.exe",
      spawnSyncImpl: (command, args, options) => {
        windowsCalls.push({ command, args, options });
        return { error: undefined, signal: null, status: 0 };
      },
    }),
    true,
  );
  assert.deepEqual(windowsCalls[0].args, ["/PID", "731", "/T", "/F"]);
  assert.equal(windowsCalls[0].command, "C:\\Windows\\System32\\taskkill.exe");
  assert.equal(windowsCalls[0].options.shell, false);
  assert.equal(windowsCalls[0].options.timeout, 2_000);
  assert.deepEqual(windowsCalls[0].options.stdio, [
    "ignore",
    "ignore",
    "ignore",
  ]);

  const root = await temporaryRoot("tree-control-boundary");
  await mkdir(path.join(root, "target"));
  const signalSource = new EventEmitter();
  const events = [];
  let child;
  const running = runCargoTarget(["run", "--", "cargo", "test"], {
    repositoryRoot: root,
    signalSource,
    acquireLeaseImpl: async () => async () => events.push("release"),
    terminateTreeImpl: async () => {
      events.push("tree-unsettled");
      return false;
    },
    spawnImpl: () => {
      child = new EventEmitter();
      child.pid = 811;
      queueMicrotask(() => signalSource.emit("SIGINT"));
      return child;
    },
  });
  await waitFor(() => (events.includes("tree-unsettled") ? true : undefined));
  assert.deepEqual(events, ["tree-unsettled"]);
  const earlySettlement = await Promise.race([
    running.then(
      () => true,
      () => true,
    ),
    new Promise((resolve) => setTimeout(() => resolve(false), 50)),
  ]);
  assert.equal(earlySettlement, false);
  events.push("close");
  child.emit("close", null, "SIGKILL");
  await assert.rejects(running, (error) => {
    assert.ok(error instanceof CargoTargetError);
    assert.equal(error.code, "process_tree_unsettled");
    assert.equal(error.exitCode, 128 + os.constants.signals.SIGINT);
    return true;
  });
  assert.deepEqual(events, ["tree-unsettled", "close", "release"]);
  assert.equal(
    cargoTargetContainment.windows_boundary,
    "taskkill_snapshot_survivors_unobserved",
  );
});

test("Cargo and reporter contention is fail-fast and path-free", async () => {
  const root = await temporaryRoot("cargo-contention-secret");
  const port = await cargoTargetLeasePort(root);
  const release = await acquireExclusiveLoopbackPort(port);
  try {
    await assert.rejects(acquireCargoTargetLease(root), (error) => {
      assert.ok(error instanceof CargoTargetError);
      assert.equal(error.code, "lease_contended");
      assert.equal(error.exitCode, 75);
      assert.doesNotMatch(error.message, /cargo-contention-secret/i);
      return true;
    });
    await assert.rejects(
      buildStorageMain(["report"], { repositoryRoot: root }),
      (error) => {
        assert.ok(error instanceof BuildStorageError);
        assert.equal(error.code, "target_lease_contended");
        assert.doesNotMatch(error.message, /cargo-contention-secret/i);
        return true;
      },
    );
  } finally {
    await release();
  }

  let leaseAttempted = false;
  await assert.rejects(
    buildStorageMain(["report", "--target", "secret"], {
      repositoryRoot: root,
      acquireLeaseImpl: async () => {
        leaseAttempted = true;
        return async () => {};
      },
    }),
    (error) =>
      error instanceof BuildStorageError && error.code === "invalid_command",
  );
  assert.equal(leaseAttempted, false);
});

test("storage output settles before the report lease is released", async () => {
  const root = await temporaryRoot("storage-output");
  await mkdir(path.join(root, "target"));
  for (const name of [
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
    "toolchain.json",
  ]) {
    await writeFile(path.join(root, name), `${name}\n`);
  }

  const events = [];
  let settleWrite;
  let markWriteStarted;
  const writeSettled = new Promise((resolve) => {
    settleWrite = resolve;
  });
  const writeStarted = new Promise((resolve) => {
    markWriteStarted = resolve;
  });
  const running = buildStorageMain(["report"], {
    repositoryRoot: root,
    metadata: {
      workspace_root: root,
      target_directory: path.join(root, "target"),
    },
    commit: "a".repeat(40),
    stdout: {},
    acquireLeaseImpl: async () => {
      events.push("acquire");
      return async () => events.push("release");
    },
    writeOutputImpl: async (_destination, output) => {
      events.push("write");
      markWriteStarted();
      assert.match(output, /"state": "exclusive_lease_held_during_report"/);
      await writeSettled;
      events.push("written");
    },
  });
  await Promise.race([
    writeStarted,
    new Promise((_, reject) =>
      setTimeout(() => reject(new Error("report write did not start")), 5_000),
    ),
  ]);
  assert.deepEqual(events, ["acquire", "write"]);
  settleWrite();
  await running;
  assert.deepEqual(events, ["acquire", "write", "written", "release"]);

  let synchronousOutput = "";
  await buildStorageMain(["report"], {
    repositoryRoot: root,
    metadata: {
      workspace_root: root,
      target_directory: path.join(root, "target"),
    },
    commit: "a".repeat(40),
    stdout: { write: (output) => (synchronousOutput = output) },
    acquireLeaseImpl: async () => async () => {},
  });
  assert.match(synchronousOutput, /"schema": "axial.build-storage.v1"/);
});

test("a hard-killed supervisor releases only its lease, leaving orphan Cargo unobserved", async (t) => {
  if (process.platform === "win32") {
    t.skip("POSIX SIGKILL fixture; the contract remains encoded on Windows");
    return;
  }

  const root = await temporaryRoot("cargo-crash-boundary");
  const scripts = path.join(root, "scripts");
  const fakeBin = path.join(root, "bin");
  const pidFile = path.join(root, "cargo.pid");
  await mkdir(scripts);
  await mkdir(fakeBin);
  await Promise.all([
    writeFile(
      path.join(scripts, "cargo-target.mjs"),
      await readFile("scripts/cargo-target.mjs", "utf8"),
    ),
    writeFile(
      path.join(scripts, "loopback-lease.mjs"),
      await readFile("scripts/loopback-lease.mjs", "utf8"),
    ),
  ]);
  const fakeCargo = path.join(fakeBin, "cargo");
  await writeFile(
    fakeCargo,
    `#!/usr/bin/env node\nrequire('node:fs').writeFileSync(process.env.AXIAL_FAKE_CARGO_PID, String(process.pid));\nsetInterval(() => {}, 1000);\n`,
  );
  await chmod(fakeCargo, 0o755);

  const supervisor = spawn(
    process.execPath,
    [path.join(scripts, "cargo-target.mjs"), "run", "--", "cargo", "build"],
    {
      cwd: root,
      env: {
        ...process.env,
        PATH: `${fakeBin}${path.delimiter}${process.env.PATH ?? ""}`,
        AXIAL_FAKE_CARGO_PID: pidFile,
      },
      stdio: "ignore",
    },
  );
  const supervisorClosed = once(supervisor, "close");
  let cargoPid;
  try {
    cargoPid = await waitFor(async () => {
      try {
        const value = Number.parseInt(await readFile(pidFile, "utf8"), 10);
        return Number.isSafeInteger(value) && value > 0 ? value : undefined;
      } catch (error) {
        if (error?.code === "ENOENT") return undefined;
        throw error;
      }
    });
    supervisor.kill("SIGKILL");
    const [_status, signal] = await supervisorClosed;
    assert.equal(signal, "SIGKILL");
    assert.doesNotThrow(() => process.kill(cargoPid, 0));

    const release = await waitFor(async () => {
      try {
        return await acquireCargoTargetLease(root);
      } catch (error) {
        if (
          error instanceof CargoTargetError &&
          error.code === "lease_contended"
        ) {
          return undefined;
        }
        throw error;
      }
    });
    await release();
    assert.equal(cargoTargetQuiescence.direct_or_orphaned_cargo, "unobserved");
  } finally {
    if (supervisor.exitCode === null && supervisor.signalCode === null) {
      supervisor.kill("SIGKILL");
      await supervisorClosed.catch(() => {});
    }
    if (cargoPid) {
      try {
        process.kill(cargoPid, "SIGTERM");
      } catch (error) {
        if (error?.code !== "ESRCH") throw error;
      }
      const stopped = async () => {
        if (process.platform === "linux") {
          try {
            const source = await readFile(`/proc/${cargoPid}/stat`, "utf8");
            const state = source
              .slice(source.lastIndexOf(") ") + 2)
              .split(/\s+/)[0];
            return state === "X" || state === "Z" ? true : undefined;
          } catch (error) {
            if (error?.code === "ENOENT") return true;
            throw error;
          }
        }
        try {
          process.kill(cargoPid, 0);
          return undefined;
        } catch (error) {
          if (error?.code === "ESRCH") return true;
          throw error;
        }
      };
      try {
        await waitFor(stopped, 1_000);
      } catch {
        try {
          process.kill(cargoPid, "SIGKILL");
        } catch (error) {
          if (error?.code !== "ESRCH") throw error;
        }
        await waitFor(stopped, 2_000);
      }
    }
  }
});

test("natural POSIX Cargo close retains the lease until its process group settles", async (t) => {
  if (process.platform === "win32") {
    t.skip(
      "POSIX process-group fixture; Windows has an explicit snapshot boundary",
    );
    return;
  }

  const root = await temporaryRoot("cargo-natural-tree");
  const scripts = path.join(root, "scripts");
  const fakeBin = path.join(root, "bin");
  const descendantScript = path.join(root, "descendant.cjs");
  const descendantPidFile = path.join(root, "descendant.pid");
  const descendantReadyFile = path.join(root, "descendant.ready");
  const descendantTermFile = path.join(root, "descendant.term");
  await mkdir(scripts);
  await mkdir(fakeBin);
  await Promise.all([
    writeFile(
      path.join(scripts, "cargo-target.mjs"),
      await readFile("scripts/cargo-target.mjs", "utf8"),
    ),
    writeFile(
      path.join(scripts, "loopback-lease.mjs"),
      await readFile("scripts/loopback-lease.mjs", "utf8"),
    ),
  ]);
  await writeFile(
    descendantScript,
    `const fs = require('node:fs');\nfs.writeFileSync(process.env.AXIAL_DESCENDANT_READY, 'ready');\nprocess.on('SIGTERM', () => fs.writeFileSync(process.env.AXIAL_DESCENDANT_TERM, 'term'));\nsetInterval(() => {}, 1000);\n`,
  );
  const fakeCargo = path.join(fakeBin, "cargo");
  await writeFile(
    fakeCargo,
    `#!/usr/bin/env node\nconst fs = require('node:fs');\nconst { spawn } = require('node:child_process');\nconst child = spawn(process.execPath, [process.env.AXIAL_DESCENDANT_SCRIPT], { stdio: 'ignore' });\nfs.writeFileSync(process.env.AXIAL_DESCENDANT_PID, String(child.pid));\nchild.unref();\nconst deadline = Date.now() + 3000;\n(function waitReady() {\n  if (fs.existsSync(process.env.AXIAL_DESCENDANT_READY)) process.exit(0);\n  if (Date.now() >= deadline) process.exit(70);\n  setTimeout(waitReady, 10);\n})();\n`,
  );
  await chmod(fakeCargo, 0o755);

  const wrapper = spawn(
    process.execPath,
    [path.join(scripts, "cargo-target.mjs"), "run", "--", "cargo", "build"],
    {
      cwd: root,
      env: {
        ...process.env,
        PATH: `${fakeBin}${path.delimiter}${process.env.PATH ?? ""}`,
        AXIAL_DESCENDANT_SCRIPT: descendantScript,
        AXIAL_DESCENDANT_PID: descendantPidFile,
        AXIAL_DESCENDANT_READY: descendantReadyFile,
        AXIAL_DESCENDANT_TERM: descendantTermFile,
      },
      stdio: "ignore",
    },
  );
  const wrapperClosed = once(wrapper, "close");
  let descendantPid;
  try {
    await waitFor(async () => {
      try {
        await readFile(descendantReadyFile);
        return true;
      } catch (error) {
        if (error?.code === "ENOENT") return undefined;
        throw error;
      }
    });
    descendantPid = Number.parseInt(
      await readFile(descendantPidFile, "utf8"),
      10,
    );
    assert.ok(Number.isSafeInteger(descendantPid) && descendantPid > 0);
    await waitFor(async () => {
      try {
        await readFile(descendantTermFile);
        return true;
      } catch (error) {
        if (error?.code === "ENOENT") return undefined;
        throw error;
      }
    });
    await assert.rejects(acquireCargoTargetLease(root), (error) => {
      assert.ok(error instanceof CargoTargetError);
      assert.equal(error.code, "lease_contended");
      return true;
    });

    const [status, signal] = await Promise.race([
      wrapperClosed,
      new Promise((_, reject) =>
        setTimeout(
          () => reject(new Error("Cargo process group did not settle")),
          5_000,
        ),
      ),
    ]);
    assert.equal(status, 0);
    assert.equal(signal, null);
    const release = await acquireCargoTargetLease(root);
    await release();
    await waitFor(async () => {
      if (process.platform === "linux") {
        try {
          const source = await readFile(`/proc/${descendantPid}/stat`, "utf8");
          const fields = source
            .slice(source.lastIndexOf(") ") + 2)
            .split(/\s+/);
          return fields[0] === "X" || fields[0] === "Z" ? true : undefined;
        } catch (error) {
          if (error?.code === "ENOENT") return true;
          throw error;
        }
      }
      try {
        process.kill(descendantPid, 0);
        return undefined;
      } catch (error) {
        if (error?.code === "ESRCH") return true;
        throw error;
      }
    });
  } finally {
    if (wrapper.exitCode === null && wrapper.signalCode === null)
      wrapper.kill("SIGKILL");
    if (descendantPid) {
      try {
        process.kill(descendantPid, "SIGKILL");
      } catch (error) {
        if (error?.code !== "ESRCH") throw error;
      }
    }
  }
});

test("Task exposes bounded reporting and fixed Cargo-owned cleanup tiers", () => {
  const listed = JSON.parse(runTask(["--list", "--json"]));
  const names = new Set(listed.tasks.map(({ name }) => name));
  for (const name of [
    "storage:report",
    "clean:cargo:release",
    "clean:cargo:windows",
    "clean",
  ]) {
    assert.ok(names.has(name), `missing Task entry point ${name}`);
  }

  const storage = runTask(["--summary", "storage:report"]);
  assert.match(storage, /^ - node scripts\/build-storage\.mjs report$/m);

  const callerOverrides = [
    "PROFILE=caller",
    "WINDOWS_TARGET=caller",
    "TARGET_DIR=caller",
  ];
  const release = runTask([
    "--summary",
    "clean:cargo:release",
    ...callerOverrides,
  ]);
  assert.match(
    release,
    new RegExp(
      `^ - ${cargoRunner.replaceAll(/[.*+?^${}()|[\]\\]/g, "\\$&")} clean --profile release$`,
      "m",
    ),
  );

  const windows = runTask([
    "--summary",
    "clean:cargo:windows",
    ...callerOverrides,
  ]);
  assert.match(
    windows,
    new RegExp(
      `^ - ${cargoRunner.replaceAll(/[.*+?^${}()|[\]\\]/g, "\\$&")} clean --target x86_64-pc-windows-gnu$`,
      "m",
    ),
  );

  const clean = runTask(["--summary", "clean", ...callerOverrides]);
  assert.match(
    clean,
    new RegExp(
      `^ - ${cargoRunner.replaceAll(/[.*+?^${}()|[\]\\]/g, "\\$&")} clean$`,
      "m",
    ),
  );
  assert.ok(
    clean.indexOf(`${cargoRunner} clean`) <
      clean.indexOf("pnpm --dir frontend run clean"),
    "Cargo lease must fail before other cleanup mutates outputs",
  );

  const cleanup = `${release}\n${windows}\n${clean}`;
  assert.doesNotMatch(cleanup, /\brm\s+-rf\s+[^\n]*target/i);
  assert.doesNotMatch(cleanup, /\bfind\b[^\n]*target|\b(?:m|a|c)time\b/i);
  assert.doesNotMatch(cleanup, /--(?:profile|target(?:-dir)?) caller/);

  const bypass = runTask([
    "--summary",
    "build",
    "CARGO_TARGET_RUNNER=echo bypassed",
    "ROOT_DIR=/tmp/untrusted-root",
    "TASKFILE_DIR=/tmp/untrusted-taskfile",
  ]);
  assert.match(
    bypass,
    new RegExp(cargoRunner.replaceAll(/[.*+?^${}()|[\]\\]/g, "\\$&")),
  );
  assert.doesNotMatch(bypass, /^ - echo bypassed/m);
  assert.doesNotMatch(bypass, /^ - node \/tmp\/untrusted/m);
});

test("canonical and native verification inventories execute B13 once", () => {
  for (const [label, inventory] of [
    ["canonical", runTask(["--dry", "capability:self-test"])],
    ["Windows", runTask(["--summary", "verify:native:windows"])],
    ["macOS", runTask(["--summary", "verify:native:macos"])],
  ]) {
    assert.equal(
      occurrences(inventory, focusedContract),
      1,
      `${label} must execute the B13 focused contract exactly once`,
    );
    assert.equal(
      occurrences(inventory, crossOwnerContract),
      1,
      `${label} must execute the B13 cross-owner contract exactly once`,
    );
  }

  const host = runTask(["--summary", "host:launch-evidence"]);
  assert.match(
    host,
    /powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File scripts\/host-launch-evidence\.ps1/,
  );
  assert.match(
    host,
    /powershell\.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "\$\(wslpath -w scripts\/host-launch-evidence\.ps1\)"/,
  );
  assert.doesNotMatch(
    host,
    /host-launch-evidence\.ps1[^\n]*(?:--path|-Path\s)/i,
  );
});

test("ambiguous storage and host-evidence compatibility paths are gone", async () => {
  const [host, storage, cargoTarget, taskfile, gitignore, conventions] =
    await Promise.all([
      readFile("scripts/host-launch-evidence.ps1", "utf8"),
      readFile("scripts/build-storage.mjs", "utf8"),
      readFile("scripts/cargo-target.mjs", "utf8"),
      readFile("Taskfile.yml", "utf8"),
      readFile(".gitignore", "utf8"),
      readFile("docs/CONVENTIONS.md", "utf8"),
    ]);

  assert.doesNotMatch(host, /SilentlyContinue/);
  assert.doesNotMatch(
    host,
    /^function\s+(?:Child|LocationState|DirectoryCount)\b/im,
  );
  assert.doesNotMatch(host, /\bstate\s*=\s*['"]present['"]/i);
  assert.doesNotMatch(storage, /['"]--(?:path|root|target-dir)['"]/);
  assert.match(storage, /acquireCargoTargetLease/);
  assert.match(storage, /quiescence: cargoTargetQuiescence/);
  assert.match(cargoTarget, /shell: false/);
  assert.match(
    cargoTarget,
    /environment\.CARGO_TARGET_DIR = path\.join\(repositoryRoot, ["']target["']\)/,
  );
  assert.doesNotMatch(taskfile, /CARGO_TARGET_RUNNER:/);
  assert.doesNotMatch(
    taskfile,
    /^\s*-\s+cargo\s+(?:build|check|clippy|clean|run|test|tauri)\b/m,
  );
  const cargoLines = assertClosedTaskCargoInvocations(taskfile);
  assert.equal(
    cargoLines.filter((line) => line.includes(cargoRunner)).length,
    occurrences(taskfile, cargoRunner),
  );
  for (const subcommand of ["bench", "doc", "fix", "rustc"]) {
    assert.throws(
      () =>
        assertClosedTaskCargoInvocations(
          `tasks:\n  future:\n    cmds:\n      - cargo ${subcommand} --workspace\n`,
        ),
      /unclassified raw Cargo invocation/,
    );
  }
  assert.match(taskfile, /^\s*- cargo fetch --locked$/m);
  assert.match(taskfile, /^\s*- cargo fmt --all(?: --check)?$/m);
  assert.match(taskfile, /cargo install tauri-cli/);
  assert.match(taskfile, /cargo (?:clippy|tauri|deny) --version/);
  assert.doesNotMatch(
    taskfile,
    /cargo-target\.mjs[^\n]*cargo (?:fetch|fmt|install)\b/,
  );
  assert.match(conventions, /same network namespace/);
  assert.match(conventions, /Windows `taskkill` is snapshot-based/);
  assert.match(conventions, /failed tree-control proof/);
  assert.doesNotMatch(taskfile, /\brm\s+-rf\s+[^\n]*target/i);
  assert.doesNotMatch(taskfile, /\bfind\b[^\n]*target|\b(?:m|a|c)time\b/i);
  assert.equal(
    gitignore.split(/\r?\n/).filter((line) => line === "target/").length,
    1,
  );
  assert.deepEqual(cargoTargetQuiescence, {
    scope: "cooperating_task_owned_cargo",
    state: "exclusive_lease_held_during_report",
    coordination_domain: "same_loopback_network_namespace",
    direct_or_orphaned_cargo: "unobserved",
  });
  assert.deepEqual(cargoTargetContainment, {
    child_boundary: "detached_process_group",
    ordinary_signal: "bounded_full_tree_termination",
    natural_posix_close: "bounded_process_group_settlement",
    windows_boundary: "taskkill_snapshot_survivors_unobserved",
    settlement_failure:
      "original_signal_status_with_unobserved_orphan_boundary",
    supervisor_hard_kill: "orphaned_cargo_unobserved",
  });
});

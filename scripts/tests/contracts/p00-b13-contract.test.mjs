import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { performance } from "node:perf_hooks";
import { pathToFileURL } from "node:url";
import test, { after } from "node:test";

import {
  BuildStorageError,
  collectBuildStorage,
  createBuildStorageReport,
  formatBuildStorage,
  resolveCanonicalTarget,
} from "../../build-storage.mjs";

const COMMIT = "a".repeat(40);
const INPUT_NAMES = [
  "Cargo.lock",
  "Cargo.toml",
  "rust-toolchain.toml",
  "toolchain.json",
];
const SCRIPT = path.resolve("scripts/build-storage.mjs");
const SCRIPT_URL = pathToFileURL(SCRIPT).href;
const POWERSHELL_SCRIPT = path.resolve("scripts/host-launch-evidence.ps1");
const HOST_PROCESS_PROBE = path.resolve("scripts/host-process-probe.cs");
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

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

async function expectCode(promise, code, secret = "") {
  await assert.rejects(promise, (error) => {
    assert.ok(error instanceof BuildStorageError, error?.stack);
    assert.equal(error.code, code);
    if (secret) assert.doesNotMatch(error.message, new RegExp(secret, "i"));
    return true;
  });
}

function stats(
  kind,
  {
    size = 0n,
    blocks = 0n,
    dev = 7n,
    ino = 11n,
    nlink = 1n,
    ctimeNs = 13n,
    mtimeNs = 17n,
  } = {},
) {
  return {
    size,
    blocks,
    dev,
    ino,
    nlink,
    ctimeNs,
    mtimeNs,
    isDirectory: () => kind === "directory",
    isFile: () => kind === "file",
    isSymbolicLink: () => kind === "symlink",
  };
}

function statsWithBlocks(value) {
  return async (candidate) => {
    const actual = await lstat(candidate, { bigint: true });
    return {
      size: actual.size,
      blocks: value,
      dev: actual.dev,
      ino: actual.ino,
      nlink: actual.nlink,
      ctimeNs: actual.ctimeNs,
      mtimeNs: actual.mtimeNs,
      isDirectory: () => actual.isDirectory(),
      isFile: () => actual.isFile(),
      isSymbolicLink: () => actual.isSymbolicLink(),
    };
  };
}

async function* directoryEntries(names) {
  for (const name of names) yield name;
}

async function sourceFixture({ target = true } = {}) {
  const root = await temporaryRoot("storage-report");
  const inputs = new Map();
  for (const name of INPUT_NAMES) {
    const contents = Buffer.from(`fixture:${name}\n`);
    inputs.set(name, contents);
    await writeFile(path.join(root, name), contents);
  }
  if (target) {
    await mkdir(path.join(root, "target"));
    await writeFile(path.join(root, "target", "artifact"), "payload");
  }
  return { root, inputs };
}

function metadata(root, target = path.join(root, "target")) {
  return { workspace_root: root, target_directory: target };
}

const fixedStatfs = async () => ({
  type: 0xef53n,
  bsize: 4096n,
  bavail: 12_345n,
});
const acquireFixtureLease = async () => async () => {};

test("storage traversal is deterministic and never follows nested links", async () => {
  const root = await temporaryRoot("storage-tree");
  const target = path.join(root, "target");
  await mkdir(path.join(target, "nested"), { recursive: true });
  await writeFile(path.join(target, "empty"), "");
  await writeFile(path.join(target, "nested", "payload"), "abc");

  const first = await collectBuildStorage({ targetDirectory: target });
  const reverseOpen = async (directory) =>
    directoryEntries((await readdir(directory)).reverse());
  const reversed = await collectBuildStorage({
    targetDirectory: target,
    openDirectoryImpl: reverseOpen,
  });
  assert.deepEqual(reversed, first);
  assert.equal(first.state, "present");
  assert.equal(first.files, 2);
  assert.equal(first.directories, 2);
  assert.equal(first.symlinks, 0);
  assert.ok(BigInt(first.apparent_bytes) >= 3n);

  const formatted = formatBuildStorage(first);
  assert.equal(formatted, formatBuildStorage(reversed));
  assert.ok(formatted.endsWith("\n"));
  assert.doesNotMatch(formatted, /storage-tree|nested|payload|empty/i);

  const virtualRoot = path.join(root, "virtual-target");
  let followedLink = false;
  const virtual = await collectBuildStorage({
    targetDirectory: virtualRoot,
    lstatImpl: async (candidate) => {
      if (candidate === virtualRoot) return stats("directory", { size: 9n });
      if (path.basename(candidate) === "artifact") {
        return stats("file", { size: 3n, blocks: 1n });
      }
      return stats("symlink", { size: 12n, blocks: 1n });
    },
    openDirectoryImpl: async (directory) => {
      if (directory !== virtualRoot) {
        followedLink = true;
        throw new Error("link followed");
      }
      return directoryEntries(["outside-link", "artifact"]);
    },
  });
  assert.equal(followedLink, false);
  assert.deepEqual(
    {
      files: virtual.files,
      directories: virtual.directories,
      symlinks: virtual.symlinks,
    },
    { files: 1, directories: 1, symlinks: 1 },
  );
});

test("storage traversal fails closed at roots, limits, and unknown allocation", async (t) => {
  const root = await temporaryRoot("storage-limits-secret");
  const target = path.join(root, "target");
  await mkdir(path.join(target, "nested"), { recursive: true });
  await writeFile(path.join(target, "nested", "artifact"), "nonempty");

  await t.test("a target-root link is rejected", async () => {
    await expectCode(
      collectBuildStorage({
        targetDirectory: target,
        lstatImpl: async () => stats("symlink"),
      }),
      "target_root_is_symlink",
      "storage-limits-secret",
    );
  });

  await t.test(
    "entry limits stop and close streaming enumeration",
    async () => {
      let pulled = 0;
      let returned = false;
      const iterator = {
        [Symbol.asyncIterator]() {
          return this;
        },
        async next() {
          pulled += 1;
          return { value: `entry-${pulled}`, done: false };
        },
        async return() {
          returned = true;
          return { value: undefined, done: true };
        },
      };
      let probes = 0;
      await expectCode(
        collectBuildStorage({
          targetDirectory: target,
          maxEntries: 2,
          lstatImpl: async (candidate) => {
            probes += 1;
            return candidate === target
              ? stats("directory")
              : stats("file", { size: 1n });
          },
          openDirectoryImpl: async () => iterator,
        }),
        "entry_limit_exceeded",
      );
      assert.equal(pulled, 2, "the stream must stop at the cap-plus-one entry");
      assert.equal(probes, 2, "the rejected entry must not be probed");
      assert.equal(returned, true, "the bounded iterator must be closed");
    },
  );

  await t.test(
    "depth and aggregate limits do not publish partial facts",
    async () => {
      await expectCode(
        collectBuildStorage({ targetDirectory: target, maxDepth: 0 }),
        "depth_limit_exceeded",
      );
      for (const [label, rootStats, limit] of [
        ["apparent", stats("directory", { size: 2n }), 1n],
        ["allocated", stats("directory", { blocks: 2n }), 511n],
      ]) {
        let childProbes = 0;
        let directoryOpens = 0;
        await expectCode(
          collectBuildStorage({
            targetDirectory: target,
            maxAggregateBytes: limit,
            lstatImpl: async (candidate) => {
              if (candidate !== target) childProbes += 1;
              return rootStats;
            },
            openDirectoryImpl: async () => {
              directoryOpens += 1;
              return directoryEntries(["must-not-probe"]);
            },
          }),
          "aggregate_limit_exceeded",
        );
        assert.equal(
          childProbes,
          0,
          `${label} root limit must precede child stat`,
        );
        assert.equal(
          directoryOpens,
          0,
          `${label} root limit must precede directory enumeration`,
        );
      }
    },
  );

  await t.test("missing allocation facts stay unavailable", async () => {
    const unavailable = await collectBuildStorage({
      targetDirectory: target,
      lstatImpl: statsWithBlocks(undefined),
    });
    assert.equal(unavailable.allocated_bytes, null);
    assert.equal(unavailable.allocated_state, "unavailable");

    const windowsUnavailable = await collectBuildStorage({
      targetDirectory: target,
      lstatImpl: statsWithBlocks(9n),
      platform: "win32",
    });
    assert.equal(windowsUnavailable.allocated_bytes, null);
    assert.equal(windowsUnavailable.allocated_state, "unavailable");
  });

  await t.test(
    "hardlinks retain path counts without double-counting storage",
    async () => {
      const virtualRoot = path.join(root, "hardlink-target");
      const hardlinkStats = stats("file", {
        size: 100n,
        blocks: 2n,
        dev: 19n,
        ino: 23n,
        nlink: 2n,
      });
      const result = await collectBuildStorage({
        targetDirectory: virtualRoot,
        lstatImpl: async (candidate) =>
          candidate === virtualRoot
            ? stats("directory", { size: 5n, blocks: 1n })
            : hardlinkStats,
        openDirectoryImpl: async () => directoryEntries(["first", "second"]),
        platform: "linux",
      });
      assert.deepEqual(
        {
          files: result.files,
          directories: result.directories,
          apparent: result.apparent_bytes,
          allocated: result.allocated_bytes,
        },
        { files: 2, directories: 1, apparent: "105", allocated: "1536" },
      );
    },
  );

  await t.test(
    "the monotonic traversal deadline fails with one exact code",
    async () => {
      const readings = [0, 0, 0, 0, 30_001];
      let directoryOpened = false;
      await expectCode(
        collectBuildStorage({
          targetDirectory: path.join(root, "deadline-target"),
          monotonicNowImpl: () => readings.shift() ?? 30_001,
          lstatImpl: async () => stats("directory"),
          openDirectoryImpl: async () => {
            directoryOpened = true;
            return directoryEntries([]);
          },
        }),
        "traversal_deadline_exceeded",
      );
      assert.equal(directoryOpened, false);
    },
  );
});

test("storage reports bind canonical source and path-free filesystem facts", async (t) => {
  const { root, inputs } = await sourceFixture();
  const options = {
    repositoryRoot: root,
    metadata: metadata(root),
    commit: COMMIT,
    platform: "linux",
    statfsImpl: fixedStatfs,
    acquireLeaseImpl: acquireFixtureLease,
  };
  const report = await createBuildStorageReport(options);
  assert.deepEqual(Object.keys(report), [
    "schema",
    "quiescence",
    "source",
    "target",
  ]);
  assert.equal(report.schema, "axial.build-storage.v1");
  assert.deepEqual(report.quiescence, {
    scope: "cooperating_task_owned_cargo",
    state: "exclusive_lease_held_during_report",
    coordination_domain: "same_loopback_network_namespace",
    direct_or_orphaned_cargo: "unobserved",
  });
  assert.equal(report.source.commit, COMMIT);
  assert.deepEqual(
    report.source.inputs,
    Object.fromEntries(
      [...inputs].map(([name, contents]) => [name, sha256(contents)]),
    ),
  );
  assert.equal(report.target.relative_path, "target");
  assert.equal(report.target.state, "present");
  assert.equal(report.target.files, 1);
  assert.equal(report.target.filesystem.available_bytes, "50565120");
  assert.equal(report.target.filesystem.availability_state, "available");
  assert.match(report.target.filesystem.identity, /^sha256:[0-9a-f]{64}$/);
  assert.equal(Object.hasOwn(report.target, "inventory"), false);

  const repeated = await createBuildStorageReport(options);
  assert.deepEqual(repeated, report);
  const formatted = formatBuildStorage(report);
  assert.equal(formatted, formatBuildStorage(repeated));
  assert.doesNotMatch(formatted, new RegExp(path.basename(root), "i"));
  assert.doesNotMatch(
    formatted,
    new RegExp(root.replaceAll("\\", "\\\\"), "i"),
  );

  await t.test(
    "missing targets use the workspace parent filesystem",
    async () => {
      const fixture = await sourceFixture({ target: false });
      const missing = await createBuildStorageReport({
        repositoryRoot: fixture.root,
        metadata: metadata(fixture.root),
        commit: COMMIT,
        platform: "linux",
        statfsImpl: fixedStatfs,
        acquireLeaseImpl: acquireFixtureLease,
      });
      assert.deepEqual(
        {
          state: missing.target.state,
          apparent: missing.target.apparent_bytes,
          allocated: missing.target.allocated_bytes,
          allocation: missing.target.allocated_state,
          files: missing.target.files,
          directories: missing.target.directories,
          free: missing.target.filesystem.available_bytes,
          availability: missing.target.filesystem.availability_state,
        },
        {
          state: "missing",
          apparent: "0",
          allocated: "0",
          allocation: "available",
          files: 0,
          directories: 0,
          free: "50565120",
          availability: "available",
        },
      );
    },
  );

  await t.test(
    "Windows keeps free bytes without trusting zero filesystem identity facts",
    async () => {
      const zeroIdentityStats = async (candidate) => {
        const actual = await lstat(candidate, { bigint: true });
        return {
          size: actual.size,
          blocks: 9n,
          dev: 0n,
          ino: actual.ino,
          nlink: actual.nlink,
          ctimeNs: actual.ctimeNs,
          mtimeNs: actual.mtimeNs,
          isDirectory: () => actual.isDirectory(),
          isFile: () => actual.isFile(),
          isSymbolicLink: () => actual.isSymbolicLink(),
        };
      };
      const windows = await createBuildStorageReport({
        ...options,
        platform: "win32",
        lstatImpl: zeroIdentityStats,
        statfsImpl: async () => ({
          type: 0n,
          bsize: 4096n,
          bavail: 12_345n,
        }),
      });
      assert.equal(windows.target.allocated_bytes, null);
      assert.equal(windows.target.allocated_state, "unavailable");
      assert.equal(windows.target.filesystem.identity, null);
      assert.equal(windows.target.filesystem.available_bytes, "50565120");
      assert.equal(windows.target.filesystem.availability_state, "available");
    },
  );

  assert.equal(
    resolveCanonicalTarget(metadata(root), root),
    path.join(root, "target"),
  );
  assert.throws(
    () =>
      resolveCanonicalTarget(
        metadata(root, path.join(root, "elsewhere")),
        root,
      ),
    (error) => error?.code === "noncanonical_target_directory",
  );

  await t.test(
    "source hashing closes at the exact input byte ceiling",
    async () => {
      const fixture = await sourceFixture();
      let pulled = 0;
      let returned = false;
      const oversized = {
        [Symbol.asyncIterator]() {
          return this;
        },
        async next() {
          pulled += 1;
          if (pulled <= 128) {
            return { value: Buffer.alloc(64 * 1024), done: false };
          }
          if (pulled === 129) return { value: Buffer.alloc(1), done: false };
          return { value: Buffer.from("must-not-read"), done: false };
        },
        async return() {
          returned = true;
          return { value: undefined, done: true };
        },
      };
      await expectCode(
        createBuildStorageReport({
          repositoryRoot: fixture.root,
          metadata: metadata(fixture.root),
          commit: COMMIT,
          platform: "linux",
          statfsImpl: fixedStatfs,
          acquireLeaseImpl: acquireFixtureLease,
          createReadStreamImpl: (_candidate, streamOptions) => {
            assert.equal(streamOptions.highWaterMark, 64 * 1024);
            return oversized;
          },
        }),
        "source_input_too_large",
      );
      assert.equal(pulled, 129);
      assert.equal(
        returned,
        true,
        "the oversized source stream must be closed",
      );
    },
  );

  await t.test(
    "source hashing admits only regular no-link inputs",
    async () => {
      const fixture = await sourceFixture();
      let opened = false;
      await expectCode(
        createBuildStorageReport({
          repositoryRoot: fixture.root,
          metadata: metadata(fixture.root),
          commit: COMMIT,
          statfsImpl: fixedStatfs,
          acquireLeaseImpl: acquireFixtureLease,
          platform: "win32",
          sourceLstatImpl: async () => stats("symlink"),
          openSourceImpl: () => {
            opened = true;
            throw new Error("must not open a rejected source input");
          },
          createReadStreamImpl: () => {
            opened = true;
            throw new Error("must not stream a rejected source input");
          },
        }),
        "source_input_not_regular_file",
      );
      assert.equal(opened, false);

      let descriptorClosed = false;
      let streamed = false;
      await expectCode(
        createBuildStorageReport({
          repositoryRoot: fixture.root,
          metadata: metadata(fixture.root),
          commit: COMMIT,
          platform: "win32",
          statfsImpl: fixedStatfs,
          acquireLeaseImpl: acquireFixtureLease,
          sourceLstatImpl: async () => stats("file", { size: 1n }),
          openSourceImpl: async () => ({
            stat: async () => stats("file", { size: 1n, ino: 12n }),
            close: async () => {
              descriptorClosed = true;
            },
          }),
          createReadStreamImpl: () => {
            streamed = true;
            throw new Error("must not stream a replaced source input");
          },
        }),
        "source_input_changed",
      );
      assert.equal(descriptorClosed, true);
      assert.equal(streamed, false);

      if (process.platform !== "win32") {
        const linked = await sourceFixture();
        const cargoLock = path.join(linked.root, "Cargo.lock");
        const source = path.join(linked.root, "Cargo.lock.source");
        await writeFile(source, "linked input");
        await rm(cargoLock);
        await symlink(source, cargoLock);
        await expectCode(
          createBuildStorageReport({
            repositoryRoot: linked.root,
            metadata: metadata(linked.root),
            commit: COMMIT,
            platform: process.platform,
            statfsImpl: fixedStatfs,
            acquireLeaseImpl: acquireFixtureLease,
          }),
          "source_input_not_regular_file",
        );
      }
    },
  );

  await t.test("FIFO source inputs fail before a blocking open", async () => {
    if (process.platform === "win32") return;
    const fixture = await sourceFixture();
    const cargoLock = path.join(fixture.root, "Cargo.lock");
    await rm(cargoLock);
    const fifo = spawnSync("mkfifo", [cargoLock], {
      encoding: "utf8",
      timeout: 5_000,
    });
    assert.equal(fifo.error, undefined, fifo.error?.message);
    assert.equal(fifo.status, 0, fifo.stderr);

    const started = performance.now();
    await expectCode(
      createBuildStorageReport({
        repositoryRoot: fixture.root,
        metadata: metadata(fixture.root),
        commit: COMMIT,
        platform: process.platform,
        statfsImpl: fixedStatfs,
        acquireLeaseImpl: acquireFixtureLease,
      }),
      "source_input_not_regular_file",
    );
    assert.ok(
      performance.now() - started < 1_000,
      "FIFO rejection must be bounded without attempting to open the pipe",
    );
  });

  await t.test(
    "a regular-to-FIFO race is bounded outside the reporter process",
    () => {
      if (process.platform === "win32") return;
      const childSource = String.raw`
import { spawnSync } from "node:child_process";
import { lstat, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const { createBuildStorageReport } = await import(process.argv[1]);
const root = await mkdtemp(path.join(os.tmpdir(), "axial-source-race-"));
try {
  for (const name of ["Cargo.lock", "Cargo.toml", "rust-toolchain.toml", "toolchain.json"]) {
    await writeFile(path.join(root, name), name);
  }
  await mkdir(path.join(root, "target"));
  const cargoLock = path.join(root, "Cargo.lock");
  let replaced = false;
  const sourceLstatImpl = async (candidate) => {
    const admitted = await lstat(candidate, { bigint: true });
    if (!replaced && candidate === cargoLock) {
      replaced = true;
      await rm(candidate);
      const replacement = spawnSync("mkfifo", [candidate], { encoding: "utf8" });
      if (replacement.status !== 0) throw new Error("mkfifo fixture failed");
    }
    return admitted;
  };
  try {
    await createBuildStorageReport({
      repositoryRoot: root,
      metadata: { workspace_root: root, target_directory: path.join(root, "target") },
      commit: "${COMMIT}",
      acquireLeaseImpl: async () => async () => {},
      sourceLstatImpl,
    });
    throw new Error("raced FIFO source was accepted");
  } catch (error) {
    if (error?.code !== "source_input_not_regular_file") throw error;
    process.stdout.write(error.code);
  }
} finally {
  await rm(root, { recursive: true, force: true });
}
`;
      const raced = spawnSync(
        process.execPath,
        ["--input-type=module", "--eval", childSource, SCRIPT_URL],
        {
          encoding: "utf8",
          killSignal: "SIGKILL",
          timeout: 3_000,
        },
      );
      assert.equal(raced.error, undefined, raced.error?.message);
      assert.equal(raced.status, 0, raced.stderr);
      assert.equal(raced.stdout, "source_input_not_regular_file");
    },
  );

  await t.test("source descriptor reads have an abort deadline", async () => {
    const fixture = await sourceFixture();
    let observedSignal = false;
    const started = performance.now();
    await expectCode(
      createBuildStorageReport({
        repositoryRoot: fixture.root,
        metadata: metadata(fixture.root),
        commit: COMMIT,
        platform: "linux",
        statfsImpl: fixedStatfs,
        acquireLeaseImpl: acquireFixtureLease,
        sourceReadTimeoutMilliseconds: 25,
        createReadStreamImpl: (_handle, streamOptions) => {
          assert.equal(streamOptions.autoClose, false);
          assert.equal(streamOptions.highWaterMark, 64 * 1024);
          observedSignal = streamOptions.signal instanceof AbortSignal;
          return {
            [Symbol.asyncIterator]() {
              return this;
            },
            next() {
              return new Promise((_, reject) => {
                const abort = () => reject(new Error("fixture aborted"));
                if (streamOptions.signal.aborted) abort();
                else
                  streamOptions.signal.addEventListener("abort", abort, {
                    once: true,
                  });
              });
            },
          };
        },
      }),
      "source_input_timeout",
    );
    assert.equal(observedSignal, true);
    assert.ok(
      performance.now() - started < 1_000,
      "the source read deadline must settle the report attempt",
    );
  });
});

test("the CLI refuses caller paths before metadata or target access", () => {
  const secret = "DO_NOT_ECHO_CALLER_TARGET";
  const result = spawnSync(
    process.execPath,
    [SCRIPT, "report", "--target", secret],
    {
      encoding: "utf8",
      timeout: 10_000,
    },
  );
  assert.equal(result.error, undefined, result.error?.message);
  assert.equal(result.status, 1);
  assert.equal(result.stdout, "");
  assert.equal(
    result.stderr.replaceAll("\r\n", "\n"),
    "build-storage: invalid_command\n",
  );
  assert.doesNotMatch(result.stderr, new RegExp(secret, "i"));
});

function invokePowerShellSelfTest() {
  const common = [
    "-NoProfile",
    "-NonInteractive",
    "-ExecutionPolicy",
    "Bypass",
    "-File",
  ];
  const options = {
    encoding: "utf8",
    timeout: 15_000,
    env: {
      ...process.env,
      APPDATA: "AXIAL_SELF_TEST_MUST_NOT_READ",
      LOCALAPPDATA: "AXIAL_SELF_TEST_MUST_NOT_READ",
    },
  };
  if (process.platform === "win32") {
    return spawnSync(
      "powershell.exe",
      [...common, POWERSHELL_SCRIPT, "-SelfTest"],
      options,
    );
  }

  const pwsh = spawnSync(
    "pwsh",
    [...common, POWERSHELL_SCRIPT, "-SelfTest"],
    options,
  );
  if (pwsh.error?.code !== "ENOENT") return pwsh;

  const converted = spawnSync("wslpath", ["-w", POWERSHELL_SCRIPT], {
    encoding: "utf8",
    timeout: 5_000,
  });
  if (converted.error || converted.status !== 0) return null;
  const windowsPath = converted.stdout.trim();
  return spawnSync(
    "powershell.exe",
    [...common, windowsPath, "-SelfTest"],
    options,
  );
}

test("Windows host evidence executes bounded process and path probes", async (t) => {
  const [source, probe] = await Promise.all([
    readFile(POWERSHELL_SCRIPT, "utf8"),
    readFile(HOST_PROCESS_PROBE, "utf8"),
  ]);
  assert.doesNotMatch(source, /&\s*\$java(?:\.Source)?\s+-version/);
  assert.match(source, /Join-Path \$PSScriptRoot 'host-process-probe\.cs'/);
  assert.match(source, /Add-Type -Path \$probeSource/);
  assert.doesNotMatch(
    source,
    /TypeDefinition|CreateJobObjectW|taskkill|Stop-BoundedProcessTree/i,
  );
  assert.doesNotMatch(source, /ComSpec/);
  assert.match(source, /'timed_out'/);
  assert.match(source, /'output_limit_exceeded'/);
  assert.match(source, /child_pid/);
  assert.match(source, /early_exit/);
  assert.match(source, /oversized_stderr/);
  assert.match(source, /mixed/);

  assert.match(probe, /STARTUPINFOEX/);
  assert.match(probe, /PROC_THREAD_ATTRIBUTE_HANDLE_LIST/);
  assert.match(probe, /EXTENDED_STARTUPINFO_PRESENT/);
  assert.match(probe, /InitializeProcThreadAttributeList/);
  assert.match(probe, /UpdateProcThreadAttribute/);
  assert.match(probe, /DeleteProcThreadAttributeList/);
  assert.match(
    probe,
    /IntPtr\[\] inheritedHandles = new IntPtr\[\] \{\s*nullInput\.DangerousGetHandle\(\),\s*outputPipe\.Write\.DangerousGetHandle\(\),\s*errorPipe\.Write\.DangerousGetHandle\(\)\s*\}/,
  );
  assert.match(probe, /SafeFileHandle/);
  assert.match(probe, /TakeRead/);
  assert.match(probe, /CREATE_SUSPENDED/);
  assert.match(probe, /AssignProcessToJobObject/);
  assert.match(
    probe,
    /runtime = Stopwatch\.StartNew\(\);\s*uint resumeResult = ResumeThread/,
  );
  assert.match(probe, /TerminateJobObject/);
  assert.match(probe, /QueryInformationJobObject/);
  assert.match(probe, /ActiveProcesses/);
  assert.match(probe, /private static ProbeResult Complete/);
  assert.match(probe, /jobEmpty && rootExited && capturesSettled/);
  assert.match(probe, /Task\.WaitAll/);
  assert.doesNotMatch(probe, /taskkill|Stop-BoundedProcessTree|CloseHandle/i);

  const result = invokePowerShellSelfTest();
  if (result === null || result.error?.code === "ENOENT") {
    t.skip(
      "PowerShell is unavailable; native Windows verification is authoritative",
    );
    return;
  }
  assert.equal(result.error, undefined, result.error?.message);
  assert.equal(result.status, 0, result.stderr || result.stdout);
  assert.equal(result.stdout.replaceAll("\r\n", "\n"), "self_test ok\n");
  assert.equal(result.stderr, "");
  assert.doesNotMatch(result.stdout, /AXIAL_SELF_TEST_MUST_NOT_READ/);
});

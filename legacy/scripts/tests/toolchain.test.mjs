import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, test } from "node:test";

import {
  parseToolchainManifest,
  readToolchainIdentity,
  resolvePnpmInvocation,
  verifyToolchain,
} from "../toolchain.mjs";

const repositoryRoot = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "../..",
);
const manifestSource = readFileSync(
  resolve(repositoryRoot, "toolchain.json"),
  "utf8",
);
const validManifest = JSON.parse(manifestSource);
const temporaryRoots = [];

afterEach(() => {
  for (const root of temporaryRoots.splice(0))
    rmSync(root, { recursive: true, force: true });
});

function mutated(mutator) {
  const manifest = structuredClone(validManifest);
  mutator(manifest);
  return JSON.stringify(manifest);
}

function temporaryRepository(
  identity = readToolchainIdentity({ repositoryRoot }),
) {
  const root = mkdtempSync(resolve(tmpdir(), "axial-toolchain-"));
  temporaryRoots.push(root);
  mkdirSync(resolve(root, "frontend"));
  writeFileSync(
    resolve(root, "frontend/package.json"),
    JSON.stringify({
      packageManager: `pnpm@${identity.pnpm}`,
      engines: { node: identity.node },
      devDependencies: { "@types/node": identity.node_types },
    }),
  );
  writeFileSync(
    resolve(root, "rust-toolchain.toml"),
    `[toolchain]\nchannel = "${identity.rust.release}"\nprofile = "minimal"\ncomponents = ["clippy", "rustfmt"]\n`,
  );
  return root;
}

function exactRunner(calls = []) {
  const outputs = new Map([
    ["node --version", "v24.13.1"],
    ["task --version", "3.52.0"],
    ["pnpm --version", "11.1.3"],
    [
      "rustc --version --verbose",
      "release: 1.93.1\ncommit-hash: 01f6ddf7588f42ae2d7eb0a2f21d44e8e96674cf",
    ],
    [
      "cargo --version --verbose",
      "release: 1.93.1\ncommit-hash: 083ac5135f967fd9dc906ab057a2315861c7a80d",
    ],
    ["cargo tauri --version", "tauri-cli 2.11.2"],
    ["cargo deny --version", "cargo-deny 0.20.2"],
  ]);
  return (command, args) => {
    const key = `${command} ${args.join(" ")}`;
    calls.push(key);
    assert.ok(outputs.has(key), `unexpected command: ${key}`);
    return outputs.get(key);
  };
}

test("parses and normalizes the exact reviewed manifest", () => {
  assert.deepEqual(parseToolchainManifest(manifestSource), validManifest);
});

test("returns normalized identity without probing executables", () => {
  const identity = readToolchainIdentity({ repositoryRoot });
  assert.equal(
    identity.manifest_sha256,
    createHash("sha256").update(manifestSource).digest("hex"),
  );
  assert.equal(identity.node, "24.13.1");
  assert.equal(identity.ubuntu_apt_snapshot, "20260719T000000Z");
});

test("rejects unknown and missing manifest fields", () => {
  assert.throws(
    () =>
      parseToolchainManifest(mutated((manifest) => (manifest.extra = true))),
    /keys must be exactly/,
  );
  assert.throws(
    () => parseToolchainManifest(mutated((manifest) => delete manifest.pnpm)),
    /keys must be exactly/,
  );
});

test("rejects ranges, aliases, and malformed immutable identities", () => {
  const cases = [
    [
      (manifest) => (manifest.node = ">=24.13.1"),
      /node has an invalid exact identity/,
    ],
    [
      (manifest) => (manifest.rust.release = "stable"),
      /rust\.release has an invalid/,
    ],
    [
      (manifest) => (manifest.tauri_cli = "^2.11.2"),
      /tauri_cli has an invalid/,
    ],
    [
      (manifest) => (manifest.cargo_deny.release = "latest"),
      /cargo_deny\.release has an invalid/,
    ],
    [
      (manifest) => (manifest.cargo_deny.linux_archive.sha256 = "unverified"),
      /cargo_deny\.linux_archive\.sha256 has an invalid/,
    ],
    [
      (manifest) =>
        (manifest.linux_ci_image.reference =
          "ghcr.io/mateoltd/axial-linux-ci:latest"),
      /linux_ci_image\.reference has an invalid/,
    ],
    [
      (manifest) => (manifest.ubuntu_base.reference = "ubuntu:24.04"),
      /ubuntu_base\.reference has an invalid/,
    ],
    [
      (manifest) => (manifest.ubuntu_apt_snapshot = "latest"),
      /ubuntu_apt_snapshot has an invalid/,
    ],
  ];
  for (const [mutation, expected] of cases) {
    assert.throws(() => parseToolchainManifest(mutated(mutation)), expected);
  }
});

test("rejects duplicate keys in the tracked canonical manifest", () => {
  const root = temporaryRepository();
  const duplicate = manifestSource.replace(
    '  "task": "3.52.0",',
    '  "task": "3.52.0",\n  "task": "3.52.0",',
  );
  const manifestPath = resolve(root, "toolchain.json");
  writeFileSync(manifestPath, duplicate);
  assert.throws(
    () => readToolchainIdentity({ manifestPath }),
    /canonical JSON/,
  );
});

test("Rust profile excludes pnpm and Tauri while retaining exact native tools", () => {
  const calls = [];
  const report = verifyToolchain({
    repositoryRoot: temporaryRepository(),
    identity: readToolchainIdentity({ repositoryRoot }),
    profiles: ["rust"],
    runExecutable: exactRunner(calls),
  });
  assert.deepEqual(calls.sort(), [
    "cargo --version --verbose",
    "node --version",
    "rustc --version --verbose",
    "task --version",
  ]);
  assert.deepEqual(report.profiles, ["rust"]);
  assert.deepEqual(report.mirrors.rust_toolchain, {
    channel: "1.93.1",
    profile: "minimal",
    components: ["clippy", "rustfmt"],
  });
  assert.equal(report.executables.tauri_cli, undefined);
});

test("desktop profile verifies every exact mirror and executable", () => {
  const report = verifyToolchain({
    repositoryRoot: temporaryRepository(),
    identity: readToolchainIdentity({ repositoryRoot }),
    profiles: ["desktop"],
    runExecutable: exactRunner(),
  });
  assert.equal(report.mirrors.frontend_package.node_types, "24.13.3");
  assert.equal(report.executables.tauri_cli.release, "2.11.2");
  assert.equal(
    report.executables.cargo.commit,
    validManifest.rust.cargo_commit,
  );
});

test("dependency profile verifies the policy tool without widening desktop startup", () => {
  const calls = [];
  const report = verifyToolchain({
    repositoryRoot: temporaryRepository(),
    identity: readToolchainIdentity({ repositoryRoot }),
    profiles: ["dependencies"],
    runExecutable: exactRunner(calls),
  });
  assert.deepEqual(calls.sort(), [
    "cargo --version --verbose",
    "cargo deny --version",
    "node --version",
    "pnpm --version",
    "task --version",
  ]);
  assert.equal(report.executables.cargo_deny.release, "0.20.2");
  assert.equal(
    report.identity.cargo_deny.linux_archive.sha256,
    "9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f",
  );
});

test("rejects exact-version drift in an executable", () => {
  const runner = exactRunner();
  assert.throws(
    () =>
      verifyToolchain({
        repositoryRoot: temporaryRepository(),
        identity: readToolchainIdentity({ repositoryRoot }),
        profiles: ["frontend"],
        runExecutable: (command, args) =>
          command === "pnpm" ? "11.1.4" : runner(command, args),
      }),
    /pnpm is "11\.1\.4"; expected "11\.1\.3"/,
  );
});

test("rejects frontend and Rust mirror drift", () => {
  const identity = readToolchainIdentity({ repositoryRoot });
  const frontendRoot = temporaryRepository(identity);
  writeFileSync(
    resolve(frontendRoot, "frontend/package.json"),
    JSON.stringify({
      packageManager: "pnpm@11.1.4",
      engines: { node: identity.node },
      devDependencies: { "@types/node": identity.node_types },
    }),
  );
  assert.throws(
    () =>
      verifyToolchain({
        repositoryRoot: frontendRoot,
        identity,
        profiles: ["frontend"],
        runExecutable: exactRunner(),
      }),
    /pnpm mirror is "pnpm@11\.1\.4"/,
  );

  const rustRoot = temporaryRepository(identity);
  writeFileSync(
    resolve(rustRoot, "rust-toolchain.toml"),
    '[toolchain]\nchannel = "stable"\n',
  );
  assert.throws(
    () =>
      verifyToolchain({
        repositoryRoot: rustRoot,
        identity,
        profiles: ["rust"],
        runExecutable: exactRunner(),
      }),
    /canonical exact manifest projection/,
  );
});

test("rejects unknown profiles instead of broadening verification", () => {
  assert.throws(
    () =>
      verifyToolchain({
        repositoryRoot: temporaryRepository(),
        identity: readToolchainIdentity({ repositoryRoot }),
        profiles: ["native"],
        runExecutable: exactRunner(),
      }),
    /unknown profile: native/,
  );
});

test("uses the direct pnpm executable outside Windows", () => {
  assert.deepEqual(
    resolvePnpmInvocation(["--version"], {
      platform: "linux",
      environment: {},
      isRegularFile: () => {
        assert.fail("non-Windows invocation must not probe the filesystem");
      },
    }),
    {
      command: "pnpm",
      args: ["--version"],
      spawnOptions: { shell: false },
    },
  );
});

test("resolves the Windows pnpm cmd shim without an ambient shell", () => {
  const files = new Set(
    [
      "D:\\setup pnpm\\pnpm.cmd",
      "C:\\Windows\\System32\\cmd.exe",
    ].map((path) => path.toLowerCase()),
  );
  const invocation = resolvePnpmInvocation(["--version"], {
    platform: "win32",
    environment: {
      Path: 'C:\\missing;"D:\\setup pnpm";relative\\ignored-after-match',
      COMSPEC: "C:\\Windows\\System32\\cmd.exe",
    },
    isRegularFile: (path) => files.has(path.toLowerCase()),
  });

  assert.deepEqual(invocation, {
    command: "C:\\Windows\\System32\\cmd.exe",
    args: [
      "/d",
      "/s",
      "/v:off",
      "/c",
      '""D:\\setup pnpm\\pnpm.cmd" --version"',
    ],
    spawnOptions: {
      shell: false,
      windowsVerbatimArguments: true,
    },
  });
});

test("executes a resolved Windows pnpm binary directly", () => {
  const files = new Set(["D:\\pnpm\\pnpm.exe"].map((path) => path.toLowerCase()));
  assert.deepEqual(
    resolvePnpmInvocation(["--version"], {
      platform: "win32",
      environment: { PATH: "D:\\pnpm" },
      isRegularFile: (path) => files.has(path.toLowerCase()),
    }),
    {
      command: "D:\\pnpm\\pnpm.exe",
      args: ["--version"],
      spawnOptions: { shell: false },
    },
  );
});

test("fails closed on unsafe or ambiguous Windows pnpm resolution", () => {
  assert.throws(
    () =>
      resolvePnpmInvocation(["install"], {
        platform: "win32",
        environment: {},
        isRegularFile: () => false,
      }),
    /accepts only --version/,
  );
  assert.throws(
    () =>
      resolvePnpmInvocation(["--version"], {
        platform: "win32",
        environment: {
          PATH: "D:\\pnpm",
          Path: "E:\\other",
        },
        isRegularFile: () => false,
      }),
    /ambiguous PATH keys/,
  );
  assert.throws(
    () =>
      resolvePnpmInvocation(["--version"], {
        platform: "win32",
        environment: {
          PATH: "C:\\shadow;D:\\pnpm",
          PATHEXT: ".COM;.EXE;.BAT;.CMD",
        },
        isRegularFile: (path) =>
          [
            "c:\\shadow\\pnpm.bat",
            "d:\\pnpm\\pnpm.cmd",
          ].includes(path.toLowerCase()),
      }),
    /launcher type is unsupported: \.bat/,
  );
  assert.throws(
    () =>
      resolvePnpmInvocation(["--version"], {
        platform: "win32",
        environment: {
          PATH: "D:\\pnpm",
          ComSpec: "cmd.exe",
        },
        isRegularFile: (path) =>
          path.toLowerCase() === "d:\\pnpm\\pnpm.cmd",
      }),
    /ComSpec must name an absolute executable file/,
  );
  assert.throws(
    () =>
      resolvePnpmInvocation(["--version"], {
        platform: "win32",
        environment: {
          PATH: "D:\\100%pnpm",
          ComSpec: "C:\\Windows\\System32\\cmd.exe",
        },
        isRegularFile: (path) =>
          [
            "d:\\100%pnpm\\pnpm.cmd",
            "c:\\windows\\system32\\cmd.exe",
          ].includes(path.toLowerCase()),
      }),
    /unsafe cmd\.exe characters/,
  );
});

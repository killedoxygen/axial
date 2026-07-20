import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const scriptPath = fileURLToPath(import.meta.url);
const defaultRepositoryRoot = resolve(dirname(scriptPath), "..");
const exactVersionPattern = /^(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)$/;
const commitPattern = /^[0-9a-f]{40}$/;
const maximumManifestBytes = 16 * 1024;

const profileTools = Object.freeze({
  orchestration: ["node", "task"],
  frontend: ["node", "task", "pnpm"],
  rust: ["node", "task", "rustc", "cargo"],
  desktop: ["node", "task", "pnpm", "rustc", "cargo", "tauri_cli"],
});

function fail(message) {
  throw new Error(`toolchain: ${message}`);
}

function requireRecord(value, location) {
  if (value === null || Array.isArray(value) || typeof value !== "object") {
    fail(`${location} must be an object`);
  }
  return value;
}

function requireKeys(value, expected, location) {
  const actual = Object.keys(requireRecord(value, location)).sort();
  const wanted = [...expected].sort();
  if (actual.join("\0") !== wanted.join("\0")) {
    fail(`${location} keys must be exactly: ${wanted.join(", ")}`);
  }
}

function requireString(value, location, pattern) {
  if (typeof value !== "string" || !pattern.test(value)) {
    fail(`${location} has an invalid exact identity`);
  }
  return value;
}

function requireExactVersion(value, location) {
  return requireString(value, location, exactVersionPattern);
}

export function parseToolchainManifest(source) {
  if (
    typeof source !== "string" ||
    Buffer.byteLength(source) > maximumManifestBytes
  ) {
    fail(
      `manifest must be UTF-8 JSON no larger than ${maximumManifestBytes} bytes`,
    );
  }
  let parsed;
  try {
    parsed = JSON.parse(source);
  } catch (error) {
    fail(`manifest is not valid JSON: ${error.message}`);
  }

  requireKeys(
    parsed,
    [
      "schema_version",
      "task",
      "node",
      "node_types",
      "pnpm",
      "rust",
      "tauri_cli",
      "linux_ci_image",
      "ubuntu_base",
      "ubuntu_apt_snapshot",
    ],
    "manifest",
  );
  if (parsed.schema_version !== 1) fail("schema_version must be 1");

  requireKeys(parsed.rust, ["release", "rustc_commit", "cargo_commit"], "rust");
  requireKeys(
    parsed.linux_ci_image,
    ["reference", "source_revision"],
    "linux_ci_image",
  );
  requireKeys(parsed.ubuntu_base, ["reference"], "ubuntu_base");

  const normalized = {
    schema_version: 1,
    task: requireExactVersion(parsed.task, "task"),
    node: requireExactVersion(parsed.node, "node"),
    node_types: requireExactVersion(parsed.node_types, "node_types"),
    pnpm: requireExactVersion(parsed.pnpm, "pnpm"),
    rust: {
      release: requireExactVersion(parsed.rust.release, "rust.release"),
      rustc_commit: requireString(
        parsed.rust.rustc_commit,
        "rust.rustc_commit",
        commitPattern,
      ),
      cargo_commit: requireString(
        parsed.rust.cargo_commit,
        "rust.cargo_commit",
        commitPattern,
      ),
    },
    tauri_cli: requireExactVersion(parsed.tauri_cli, "tauri_cli"),
    linux_ci_image: {
      reference: requireString(
        parsed.linux_ci_image.reference,
        "linux_ci_image.reference",
        /^ghcr\.io\/mateoltd\/axial-linux-ci@sha256:[0-9a-f]{64}$/,
      ),
      source_revision: requireString(
        parsed.linux_ci_image.source_revision,
        "linux_ci_image.source_revision",
        commitPattern,
      ),
    },
    ubuntu_base: {
      reference: requireString(
        parsed.ubuntu_base.reference,
        "ubuntu_base.reference",
        /^ubuntu:24\.04@sha256:[0-9a-f]{64}$/,
      ),
    },
    ubuntu_apt_snapshot: requireString(
      parsed.ubuntu_apt_snapshot,
      "ubuntu_apt_snapshot",
      /^\d{8}T\d{6}Z$/,
    ),
  };

  return normalized;
}

export function readToolchainIdentity(options = {}) {
  const repositoryRoot = options.repositoryRoot ?? defaultRepositoryRoot;
  const manifestPath =
    options.manifestPath ?? resolve(repositoryRoot, "toolchain.json");
  const source = readFileSync(manifestPath, "utf8");
  const manifest = parseToolchainManifest(source);
  if (source !== `${JSON.stringify(manifest, null, 2)}\n`) {
    fail("manifest must use canonical JSON without duplicate keys");
  }
  return {
    manifest_sha256: createHash("sha256").update(source).digest("hex"),
    ...manifest,
  };
}

function selectProfiles(profiles) {
  const selected = profiles?.length ? profiles : ["desktop"];
  const unknown = selected.filter((profile) => !(profile in profileTools));
  if (unknown.length) fail(`unknown profile: ${unknown.join(", ")}`);
  return [...new Set(selected)].sort();
}

function parsePackageMirror(repositoryRoot, identity) {
  const packageJson = JSON.parse(
    readFileSync(resolve(repositoryRoot, "frontend/package.json"), "utf8"),
  );
  const actual = {
    node: packageJson.engines?.node,
    node_types: packageJson.devDependencies?.["@types/node"],
    pnpm: packageJson.packageManager,
  };
  const expected = {
    node: identity.node,
    node_types: identity.node_types,
    pnpm: `pnpm@${identity.pnpm}`,
  };
  for (const key of Object.keys(expected)) {
    if (actual[key] !== expected[key]) {
      fail(
        `frontend/package.json ${key} mirror is ${JSON.stringify(actual[key])}; expected ${JSON.stringify(expected[key])}`,
      );
    }
  }
  return actual;
}

function parseRustMirror(repositoryRoot, identity) {
  const source = readFileSync(
    resolve(repositoryRoot, "rust-toolchain.toml"),
    "utf8",
  );
  const expected = `[toolchain]\nchannel = "${identity.rust.release}"\nprofile = "minimal"\ncomponents = ["clippy", "rustfmt"]\n`;
  if (source !== expected) {
    fail("rust-toolchain.toml must be the canonical exact manifest projection");
  }
  return {
    channel: identity.rust.release,
    profile: "minimal",
    components: ["clippy", "rustfmt"],
  };
}

function runExecutable(command, args) {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    timeout: 10_000,
    windowsHide: true,
    env: { ...process.env, NO_COLOR: "1" },
  });
  if (result.error)
    fail(`could not execute ${command}: ${result.error.message}`);
  if (result.signal) fail(`${command} was terminated by ${result.signal}`);
  if (result.status !== 0)
    fail(
      `${command} exited with status ${result.status}: ${result.stderr.trim()}`,
    );
  return result.stdout.trim();
}

function exactObservedVersion(
  name,
  output,
  expected,
  pattern = exactVersionPattern,
) {
  const match = output.match(pattern);
  const actual = match?.[1] ?? match?.[0];
  if (actual !== expected)
    fail(
      `${name} is ${JSON.stringify(actual)}; expected ${JSON.stringify(expected)}`,
    );
  return actual;
}

function inspectExecutable(tool, identity, runner) {
  if (tool === "node") {
    return {
      release: exactObservedVersion(
        "node",
        runner("node", ["--version"]),
        identity.node,
        /^v(\d+\.\d+\.\d+)$/,
      ),
    };
  }
  if (tool === "task") {
    return {
      release: exactObservedVersion(
        "task",
        runner("task", ["--version"]),
        identity.task,
        /^(?:Task version:\s*v?)?(\d+\.\d+\.\d+)$/,
      ),
    };
  }
  if (tool === "pnpm") {
    return {
      release: exactObservedVersion(
        "pnpm",
        runner("pnpm", ["--version"]),
        identity.pnpm,
      ),
    };
  }
  if (tool === "rustc" || tool === "cargo") {
    const output = runner(tool, ["--version", "--verbose"]);
    const release = output.match(/^release:\s*(\S+)$/m)?.[1];
    const commit = output.match(/^commit-hash:\s*([0-9a-f]{40})$/m)?.[1];
    const expectedCommit = identity.rust[`${tool}_commit`];
    if (release !== identity.rust.release || commit !== expectedCommit) {
      fail(
        `${tool} identity mismatch; expected ${identity.rust.release} (${expectedCommit})`,
      );
    }
    return { release, commit };
  }
  if (tool === "tauri_cli") {
    return {
      release: exactObservedVersion(
        "tauri-cli",
        runner("cargo", ["tauri", "--version"]),
        identity.tauri_cli,
        /^tauri-cli\s+(\d+\.\d+\.\d+)$/,
      ),
    };
  }
  fail(`unsupported executable ${tool}`);
}

export function verifyToolchain(options = {}) {
  const repositoryRoot = options.repositoryRoot ?? defaultRepositoryRoot;
  const identity =
    options.identity ?? readToolchainIdentity({ repositoryRoot });
  const profiles = selectProfiles(options.profiles);
  const tools = [
    ...new Set(profiles.flatMap((profile) => profileTools[profile])),
  ].sort();
  const mirrors = {};
  if (profiles.includes("frontend") || profiles.includes("desktop")) {
    mirrors.frontend_package = parsePackageMirror(repositoryRoot, identity);
  }
  if (profiles.includes("rust") || profiles.includes("desktop")) {
    mirrors.rust_toolchain = parseRustMirror(repositoryRoot, identity);
  }

  const runner = options.runExecutable ?? runExecutable;
  const executables = Object.fromEntries(
    tools.map((tool) => [tool, inspectExecutable(tool, identity, runner)]),
  );
  return { identity, profiles, mirrors, executables };
}

function parseArguments(argv) {
  const command = argv.shift();
  if (command !== "verify" && command !== "report") {
    fail("usage: toolchain.mjs <verify|report> [--profile <name>] [--json]");
  }
  const profiles = [];
  let json = false;
  while (argv.length) {
    const argument = argv.shift();
    if (argument === "--profile") {
      const profile = argv.shift();
      if (!profile) fail("--profile requires a value");
      profiles.push(profile);
    } else if (argument === "--json") {
      json = true;
    } else {
      fail(`unknown argument ${argument}`);
    }
  }
  return { command, profiles, json };
}

function main() {
  const { command, profiles, json } = parseArguments(process.argv.slice(2));
  const report = verifyToolchain({ profiles });
  if (json || command === "report") {
    process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
  } else {
    process.stdout.write(
      `toolchain verified (${report.profiles.join(", ")})\n`,
    );
  }
}

if (process.argv[1] && resolve(process.argv[1]) === scriptPath) {
  try {
    main();
  } catch (error) {
    process.stderr.write(`${error.message}\n`);
    process.exitCode = 1;
  }
}

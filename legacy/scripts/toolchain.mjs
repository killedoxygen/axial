import { createHash } from "node:crypto";
import { readFileSync, statSync } from "node:fs";
import { dirname, resolve, win32 } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const scriptPath = fileURLToPath(import.meta.url);
const defaultRepositoryRoot = resolve(dirname(scriptPath), "..");
const exactVersionPattern = /^(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)$/;
const commitPattern = /^[0-9a-f]{40}$/;
const sha256Pattern = /^[0-9a-f]{64}$/;
const maximumManifestBytes = 16 * 1024;

const profileTools = Object.freeze({
  orchestration: ["node", "task"],
  frontend: ["node", "task", "pnpm"],
  rust: ["node", "task", "rustc", "cargo"],
  dependencies: ["node", "task", "pnpm", "cargo", "cargo_deny"],
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
      "cargo_deny",
      "linux_ci_image",
      "ubuntu_base",
      "ubuntu_apt_snapshot",
    ],
    "manifest",
  );
  if (parsed.schema_version !== 1) fail("schema_version must be 1");

  requireKeys(parsed.rust, ["release", "rustc_commit", "cargo_commit"], "rust");
  requireKeys(parsed.cargo_deny, ["release", "linux_archive"], "cargo_deny");
  requireKeys(
    parsed.cargo_deny.linux_archive,
    ["target", "sha256"],
    "cargo_deny.linux_archive",
  );
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
    cargo_deny: {
      release: requireExactVersion(
        parsed.cargo_deny.release,
        "cargo_deny.release",
      ),
      linux_archive: {
        target: requireString(
          parsed.cargo_deny.linux_archive.target,
          "cargo_deny.linux_archive.target",
          /^x86_64-unknown-linux-musl$/,
        ),
        sha256: requireString(
          parsed.cargo_deny.linux_archive.sha256,
          "cargo_deny.linux_archive.sha256",
          sha256Pattern,
        ),
      },
    },
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

function environmentValue(environment, name) {
  const matches = Object.entries(environment).filter(
    ([key, value]) =>
      key.toLowerCase() === name.toLowerCase() && typeof value === "string",
  );
  if (matches.length === 0) return undefined;
  const values = new Set(matches.map(([, value]) => value));
  if (values.size !== 1) fail(`Windows environment has ambiguous ${name} keys`);
  return matches[0][1];
}

function isRegularFile(path) {
  try {
    return statSync(path).isFile();
  } catch {
    return false;
  }
}

function* windowsPathDirectories(environment) {
  const path = environmentValue(environment, "PATH");
  if (!path) fail("Windows PATH is unavailable");
  for (let entry of path.split(win32.delimiter)) {
    entry = entry.trim();
    if (!entry) continue;
    if (entry.startsWith('"') || entry.endsWith('"')) {
      if (!(entry.startsWith('"') && entry.endsWith('"'))) {
        fail("Windows PATH contains an unbalanced quoted entry");
      }
      entry = entry.slice(1, -1);
    }
    if (!win32.isAbsolute(entry)) {
      fail(`Windows PATH entry is not absolute: ${JSON.stringify(entry)}`);
    }
    yield entry;
  }
}

function windowsExecutableExtensions(environment) {
  const configured =
    environmentValue(environment, "PATHEXT") ?? ".COM;.EXE;.BAT;.CMD";
  const extensions = configured
    .split(win32.delimiter)
    .map((extension) => extension.trim().toLowerCase())
    .filter(Boolean);
  if (
    extensions.length === 0 ||
    extensions.some((extension) => !/^\.[a-z0-9]+$/.test(extension))
  ) {
    fail("Windows PATHEXT contains an invalid executable extension");
  }
  return [...new Set(extensions)];
}

function resolveWindowsPnpm(environment, fileProbe) {
  const extensions = windowsExecutableExtensions(environment);
  for (const directory of windowsPathDirectories(environment)) {
    for (const extension of extensions) {
      const candidate = win32.join(directory, `pnpm${extension}`);
      if (!fileProbe(candidate)) continue;
      if (![".com", ".exe", ".cmd"].includes(extension)) {
        fail(`Windows pnpm launcher type is unsupported: ${extension}`);
      }
      return candidate;
    }
  }
  fail("could not resolve pnpm from Windows PATH");
}

export function resolvePnpmInvocation(args, options = {}) {
  const platform = options.platform ?? process.platform;
  if (platform !== "win32") {
    return {
      command: "pnpm",
      args: [...args],
      spawnOptions: { shell: false },
    };
  }
  if (args.length !== 1 || args[0] !== "--version") {
    fail("Windows pnpm identity probing accepts only --version");
  }

  const environment = options.environment ?? process.env;
  const fileProbe = options.isRegularFile ?? isRegularFile;
  const pnpm = resolveWindowsPnpm(environment, fileProbe);
  if (win32.extname(pnpm).toLowerCase() !== ".cmd") {
    return {
      command: pnpm,
      args: [...args],
      spawnOptions: { shell: false },
    };
  }
  if (/[\r\n"%]/.test(pnpm)) {
    fail("Windows pnpm command shim path contains unsafe cmd.exe characters");
  }

  const commandProcessor = environmentValue(environment, "ComSpec");
  if (
    !commandProcessor ||
    !win32.isAbsolute(commandProcessor) ||
    win32.extname(commandProcessor).toLowerCase() !== ".exe" ||
    !fileProbe(commandProcessor)
  ) {
    fail("Windows ComSpec must name an absolute executable file");
  }

  return {
    command: commandProcessor,
    // /S removes the outer quotes and retains the quoted batch path.
    args: ["/d", "/s", "/v:off", "/c", `""${pnpm}" --version"`],
    spawnOptions: {
      shell: false,
      windowsVerbatimArguments: true,
    },
  };
}

function runExecutable(command, args) {
  const invocation =
    command === "pnpm"
      ? resolvePnpmInvocation(args)
      : { command, args, spawnOptions: { shell: false } };
  const result = spawnSync(invocation.command, invocation.args, {
    encoding: "utf8",
    timeout: 10_000,
    windowsHide: true,
    env: { ...process.env, NO_COLOR: "1" },
    ...invocation.spawnOptions,
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
  if (tool === "cargo_deny") {
    return {
      release: exactObservedVersion(
        "cargo-deny",
        runner("cargo", ["deny", "--version"]),
        identity.cargo_deny.release,
        /^cargo-deny\s+(\d+\.\d+\.\d+)$/,
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
  if (
    profiles.includes("frontend") ||
    profiles.includes("dependencies") ||
    profiles.includes("desktop")
  ) {
    mirrors.frontend_package = parsePackageMirror(repositoryRoot, identity);
  }
  if (
    profiles.includes("rust") ||
    profiles.includes("dependencies") ||
    profiles.includes("desktop")
  ) {
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

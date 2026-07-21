import { fork, execFile as execFileCallback } from "node:child_process";
import { promisify } from "node:util";
import { lstat, readFile, readdir, realpath, rm } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { fileURLToPath, pathToFileURL } from "node:url";

import { capabilityRegistry } from "./capabilities/registry.mjs";
import {
  EvidenceError,
  aggregateCapabilityEvidence,
  canonicalJson,
  sealScenarioResult,
  validateEvidenceDocument,
  writeCanonicalAtomic,
  writeEvidenceAtomic,
} from "./capabilities/evidence.mjs";

const execFile = promisify(execFileCallback);
const scriptPath = fileURLToPath(import.meta.url);
const repositoryRoot = path.resolve(path.dirname(scriptPath), "..");
const workerPath = fileURLToPath(new URL("./capabilities/worker.mjs", import.meta.url));
const concretePlatforms = Object.freeze(["linux", "windows", "macos", "browser"]);
const scenarioPattern = /^(?:CP|PM)-[A-Z0-9]+(?:-[A-Z0-9]+)*$/;
const proofPattern = /^(?:CAP|PM)-[A-Z0-9]+(?:-[A-Z0-9]+)*$/;
const capabilityPattern = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;
const phasePattern = /^P(?:0[0-9]|1[0-4])$/;
const commitPattern = /^[0-9a-f]{40}$/;
const hashPattern = /^[0-9a-f]{64}$/;
const recordKeys = Object.freeze([
  "scenario_id",
  "proof_id",
  "capability_id",
  "owner_phase",
  "allowed_platforms",
  "timeout_ms",
  "module_url",
  "evidence_path",
]);
const settlementGraceMs = 50;
const settlementDeadlineMs = 2_000;
const workerEnvironmentNames = Object.freeze([
  "PATH",
  "HOME",
  "USERPROFILE",
  "CARGO_HOME",
  "RUSTUP_HOME",
  "PNPM_HOME",
  "XDG_CACHE_HOME",
  "TMPDIR",
  "TEMP",
  "TMP",
  "SYSTEMROOT",
  "WINDIR",
  "COMSPEC",
  "PATHEXT",
  "LANG",
  "LC_ALL",
  "TZ",
  "SSL_CERT_FILE",
  "SSL_CERT_DIR",
]);

export class CapabilityError extends Error {
  constructor(code) {
    super(code);
    this.name = "CapabilityError";
    this.code = code;
  }
}

function fail(code) {
  throw new CapabilityError(code);
}

function isPlainObject(value) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

function exactKeys(value, expected, code) {
  if (!isPlainObject(value)) fail(code);
  const actual = Object.keys(value).sort();
  const wanted = [...expected].sort();
  if (actual.length !== wanted.length || actual.some((key, index) => key !== wanted[index])) fail(code);
}

function validateBoundedId(value, pattern, code) {
  if (typeof value !== "string" || value.length > 96 || !pattern.test(value)) fail(code);
}

function isInside(parent, child) {
  return child.startsWith(`${parent}${path.sep}`);
}

export function capabilityWorkerEnvironment(context, source = process.env) {
  const accepted = new Set(workerEnvironmentNames);
  const environment = {};
  for (const [name, value] of Object.entries(source)) {
    if (typeof value === "string" && accepted.has(name.toUpperCase())) {
      environment[name] = value;
    }
  }
  if (!Object.keys(environment).some((name) => name.toUpperCase() === "PATH")) {
    fail("worker_path_unavailable");
  }
  if (context !== null) environment.AXIAL_CAPABILITY_CONTEXT = JSON.stringify(context);
  return Object.freeze(environment);
}

async function validateRegistryStructure(registry, options) {
  if (!Array.isArray(registry)) fail("invalid_registry");
  if (registry.length > 256) fail("invalid_registry");
  if (registry.length === 0) return [];

  const rootPath = path.resolve(options.scenarioRoot);
  const rootReal = await realpath(rootPath).catch(() => fail("invalid_scenario_root"));
  const scenarioIds = new Set();
  const proofIds = new Set();
  const evidencePaths = new Set();
  const records = [];

  for (const source of registry) {
    exactKeys(source, recordKeys, "invalid_registry_record");
    validateBoundedId(source.scenario_id, scenarioPattern, "invalid_scenario_id");
    validateBoundedId(source.proof_id, proofPattern, "invalid_proof_id");
    validateBoundedId(source.capability_id, capabilityPattern, "invalid_capability_id");
    if (!phasePattern.test(source.owner_phase)) fail("invalid_owner_phase");
    if (!Number.isSafeInteger(source.timeout_ms) || source.timeout_ms < 25 || source.timeout_ms > 300_000) {
      fail("invalid_timeout");
    }
    if (
      !Array.isArray(source.allowed_platforms) ||
      source.allowed_platforms.length === 0 ||
      source.allowed_platforms.some((platform) => !concretePlatforms.includes(platform)) ||
      new Set(source.allowed_platforms).size !== source.allowed_platforms.length
    ) {
      fail("invalid_allowed_platforms");
    }
    const platformScenario = source.scenario_id.startsWith("PM-");
    if (platformScenario !== source.proof_id.startsWith("PM-")) fail("invalid_proof_id");
    const expectedEvidence = platformScenario
      ? `evidence/platform/${source.proof_id}.json`
      : `evidence/capabilities/${source.proof_id}.json`;
    if (source.evidence_path !== expectedEvidence) fail("invalid_evidence_path");
    if (scenarioIds.has(source.scenario_id) || proofIds.has(source.proof_id) || evidencePaths.has(source.evidence_path)) {
      fail("duplicate_registry_identity");
    }
    scenarioIds.add(source.scenario_id);
    proofIds.add(source.proof_id);
    evidencePaths.add(source.evidence_path);

    let moduleUrl;
    try {
      moduleUrl = source.module_url instanceof URL ? source.module_url : new URL(source.module_url);
    } catch {
      fail("invalid_module_url");
    }
    if (moduleUrl.protocol !== "file:" || moduleUrl.search || moduleUrl.hash) fail("invalid_module_url");
    const modulePath = path.resolve(fileURLToPath(moduleUrl));
    if (!isInside(rootPath, modulePath) || path.extname(modulePath) !== ".mjs") {
      fail("invalid_module_url");
    }
    const moduleReal = await realpath(modulePath).catch(() => fail("implementation_absent"));
    const metadata = await lstat(modulePath);
    if (!metadata.isFile() || metadata.isSymbolicLink() || !isInside(rootReal, moduleReal)) {
      fail("invalid_module_url");
    }

    records.push(
      Object.freeze({
        scenario_id: source.scenario_id,
        proof_id: source.proof_id,
        capability_id: source.capability_id,
        owner_phase: source.owner_phase,
        allowed_platforms: Object.freeze([...source.allowed_platforms].sort()),
        timeout_ms: source.timeout_ms,
        module_path: moduleReal,
        evidence_path: source.evidence_path,
        evidence_destination: path.join(options.repositoryRoot, ...source.evidence_path.split("/")),
      }),
    );
  }
  return records;
}

function sleep(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function waitForCondition(predicate, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  do {
    if (await predicate()) return true;
    await sleep(20);
  } while (Date.now() < deadline);
  return Boolean(await predicate());
}

function signalPosixGroup(pid, signal) {
  try {
    process.kill(-pid, signal);
  } catch (error) {
    if (error.code !== "ESRCH") throw error;
  }
}

function signalPosixProcess(pid, signal) {
  try {
    process.kill(pid, signal);
  } catch (error) {
    if (error.code !== "ESRCH") throw error;
  }
}

export function parseLinuxProcessStat(pid, source) {
  if (!Number.isSafeInteger(pid) || pid <= 0 || typeof source !== "string") {
    throw new Error("invalid Linux process stat");
  }
  const prefix = `${pid} (`;
  if (!source.startsWith(prefix)) throw new Error("invalid Linux process stat");
  const commandEnd = source.lastIndexOf(") ");
  if (commandEnd < prefix.length) throw new Error("invalid Linux process stat");
  const fields = source.slice(commandEnd + 2).trim().split(/\s+/);
  if (fields.length < 3) throw new Error("invalid Linux process stat");
  const member = { pid, processGroup: Number(fields[2]), state: fields[0] };
  if (member.state.length !== 1) throw new Error("invalid Linux process stat");
  livePosixGroupMembers([member]);
  return member;
}

export function livePosixGroupMembers(members) {
  return members.filter(({ pid, processGroup, state }) => {
    if (
      !Number.isSafeInteger(pid) ||
      pid <= 0 ||
      !Number.isSafeInteger(processGroup) ||
      processGroup < 0 ||
      typeof state !== "string" ||
      !/^[A-Za-z][!-~]{0,15}$/.test(state)
    ) {
      throw new Error("invalid POSIX process group member");
    }
    return !state.startsWith("Z");
  });
}

export async function listLinuxProcessGroup(groupId, procRoot = "/proc") {
  if (!Number.isSafeInteger(groupId) || groupId <= 0) throw new Error("invalid POSIX process group id");
  const entries = (await readdir(procRoot, { withFileTypes: true }))
    .filter((entry) => entry.isDirectory() && /^[1-9][0-9]*$/.test(entry.name))
    .sort((left, right) => Number(left.name) - Number(right.name));
  const members = [];
  for (const entry of entries) {
    try {
      const member = parseLinuxProcessStat(
        Number(entry.name),
        await readFile(path.join(procRoot, entry.name, "stat"), "utf8"),
      );
      if (member.processGroup === groupId) members.push(member);
    } catch (error) {
      if (error.code === "ENOENT") continue;
      throw error;
    }
  }
  return members;
}

async function listPosixProcessGroup(groupId) {
  if (process.platform === "linux") return listLinuxProcessGroup(groupId);
  const result = await execFile("ps", ["-A", "-o", "pid=", "-o", "pgid=", "-o", "stat="], {
    encoding: "utf8",
    timeout: settlementDeadlineMs,
    windowsHide: true,
    maxBuffer: 1024 * 1024,
  });
  const members = result.stdout
    .split(/\r?\n/)
    .filter((line) => line.trim() !== "")
    .map((line) => line.trim().split(/\s+/))
    .map((fields) => {
      if (fields.length !== 3) throw new Error("invalid POSIX process list");
      return { pid: Number(fields[0]), processGroup: Number(fields[1]), state: fields[2] };
    });
  livePosixGroupMembers(members);
  return members.filter(({ processGroup }) => processGroup === groupId);
}

async function terminateWindowsTree(pid) {
  try {
    await execFile("taskkill.exe", ["/PID", String(pid), "/T", "/F"], {
      encoding: "utf8",
      timeout: settlementDeadlineMs,
      windowsHide: true,
      maxBuffer: 64 * 1024,
    });
    return true;
  } catch (error) {
    if (!Number.isInteger(error.code)) throw error;
    return false;
  }
}

async function settleWorkerTree(child, closeState) {
  if (process.platform === "win32") {
    const treeControlled = await terminateWindowsTree(child.pid);
    const leaderGone = await waitForCondition(() => closeState.closed, settlementDeadlineMs);
    return treeControlled && leaderGone;
  }

  // Freeze the group so no live member can fork past exact PGID enumeration.
  try {
    signalPosixGroup(child.pid, "SIGSTOP");
    await sleep(20);
    const initialDescendants = livePosixGroupMembers(await listPosixProcessGroup(child.pid))
      .filter(({ pid }) => pid !== child.pid)
      .map(({ pid }) => pid);
    for (const pid of initialDescendants) signalPosixProcess(pid, "SIGTERM");
    for (const pid of initialDescendants) signalPosixProcess(pid, "SIGCONT");
    await sleep(settlementGraceMs);
    const forcedDescendants = livePosixGroupMembers(await listPosixProcessGroup(child.pid))
      .filter(({ pid }) => pid !== child.pid)
      .map(({ pid }) => pid);
    for (const pid of forcedDescendants) signalPosixProcess(pid, "SIGKILL");
    signalPosixProcess(child.pid, "SIGCONT");
    const descendantsGone = await waitForCondition(async () => {
      const members = livePosixGroupMembers(await listPosixProcessGroup(child.pid));
      return members.every(({ pid }) => pid === child.pid);
    }, settlementDeadlineMs);
    signalPosixProcess(child.pid, "SIGTERM");
    await sleep(settlementGraceMs);
    if (!closeState.closed) signalPosixProcess(child.pid, "SIGKILL");
    const leaderGone = await waitForCondition(() => closeState.closed, settlementDeadlineMs);
    if (livePosixGroupMembers(await listPosixProcessGroup(child.pid)).length > 0) {
      signalPosixGroup(child.pid, "SIGKILL");
    }
    const groupSettled = await waitForCondition(
      async () => livePosixGroupMembers(await listPosixProcessGroup(child.pid)).length === 0,
      settlementDeadlineMs,
    );
    return descendantsGone && leaderGone && groupSettled;
  } catch (error) {
    try { signalPosixGroup(child.pid, "SIGCONT"); } catch {}
    try { signalPosixGroup(child.pid, "SIGKILL"); } catch {}
    await waitForCondition(() => closeState.closed, settlementDeadlineMs);
    throw error;
  }
}

function workerInvocation(mode, modulePath, context, timeoutMs) {
  return new Promise((resolve, reject) => {
    let child;
    try {
      child = fork(workerPath, [mode, modulePath], {
        cwd: repositoryRoot,
        env: capabilityWorkerEnvironment(context),
        detached: true,
        execArgv: [],
        serialization: "json",
        stdio: ["ignore", "ignore", "ignore", "ipc"],
        windowsHide: true,
      });
    } catch {
      reject(new CapabilityError("worker_start_failed"));
      return;
    }

    const closeState = { closed: false };
    let message;
    let messageCount = 0;
    let completionReason = null;
    let finalizing = false;
    const timer = setTimeout(() => void finalize("timeout"), timeoutMs);

    async function finalize(reason) {
      if (finalizing) return;
      finalizing = true;
      completionReason = reason;
      clearTimeout(timer);
      let settled = false;
      try {
        settled = await settleWorkerTree(child, closeState);
      } catch {
        settled = false;
      }
      if (!settled) {
        reject(new CapabilityError("process_tree_unsettled"));
      } else if (completionReason === "timeout") {
        reject(new CapabilityError("scenario_timeout"));
      } else if (messageCount !== 1 || !isPlainObject(message)) {
        reject(new CapabilityError("worker_failed"));
      } else if (message.ok !== true) {
        const closedWorkerCodes = new Set([
          "worker_failed",
          "implementation_load_failed",
          "scenario_failed",
          "malformed_scenario_result",
        ]);
        reject(new CapabilityError(closedWorkerCodes.has(message.code) ? message.code : "worker_failed"));
      } else {
        resolve(message);
      }
    }

    child.on("message", (value) => {
      messageCount += 1;
      if (messageCount === 1) message = value;
      void finalize("message");
    });
    child.once("error", () => void finalize("error"));
    child.once("close", () => {
      closeState.closed = true;
      if (!finalizing) void finalize("close");
    });
  });
}

async function inspectRegistry(records) {
  const inspections = [];
  for (const record of records) {
    const message = await workerInvocation("inspect", record.module_path, null, record.timeout_ms);
    exactKeys(
      message,
      ["ok", "declaration", "has_implementation", "has_receipt_revalidator"],
      "invalid_implementation_declaration",
    );
    if (message.ok !== true || message.has_implementation !== true) fail("implementation_absent");
    exactKeys(message.declaration, ["scenario_id", "proof_id", "capability_id"], "invalid_implementation_declaration");
    if (
      message.declaration.scenario_id !== record.scenario_id ||
      message.declaration.proof_id !== record.proof_id ||
      message.declaration.capability_id !== record.capability_id
    ) {
      fail("implementation_identity_mismatch");
    }
    inspections.push(message);
  }
  return inspections;
}

async function readCurrentReceipts(record, documents, root) {
  const receipts = new Map();
  for (const document of documents) {
    const observationIds = document.observations.map((observation) => observation.id);
    const message = await workerInvocation(
      "receipts",
      record.module_path,
      {
        scenario_id: record.scenario_id,
        proof_id: record.proof_id,
        capability_id: record.capability_id,
        owner_phase: record.owner_phase,
        platform: document.platform.os,
        repository_root: root,
        observations: observationIds,
      },
      record.timeout_ms,
    );
    exactKeys(message, ["ok", "result"], "malformed_receipt_revalidation");
    exactKeys(message.result, ["observations"], "malformed_receipt_revalidation");
    if (!Array.isArray(message.result.observations)) fail("malformed_receipt_revalidation");
    const returned = new Set();
    for (const observation of message.result.observations) {
      exactKeys(observation, ["id", "receipt"], "malformed_receipt_revalidation");
      if (!observationIds.includes(observation.id) || returned.has(observation.id)) {
        fail("malformed_receipt_revalidation");
      }
      returned.add(observation.id);
      receipts.set(`${document.platform.os}\0${observation.id}`, observation.receipt);
    }
    if (returned.size !== observationIds.length) fail("malformed_receipt_revalidation");
  }
  return (document, observationId) => receipts.get(`${document.platform.os}\0${observationId}`);
}

async function cleanStaleEvidence(record) {
  const evidenceRoot = path.dirname(record.evidence_destination);
  const rootMetadata = await lstat(evidenceRoot).catch((error) => {
    if (error.code === "ENOENT") return null;
    throw error;
  });
  if (rootMetadata === null) return;
  if (!rootMetadata.isDirectory() || rootMetadata.isSymbolicLink()) fail("unsafe_evidence_root");
  const rootReal = await realpath(evidenceRoot);
  if (rootReal !== path.resolve(evidenceRoot)) fail("unsafe_evidence_root");

  await rm(record.evidence_destination, { force: true });
  const temporaryPrefix = `.${path.basename(record.evidence_destination)}.`;
  for (const entry of await readdir(rootReal, { withFileTypes: true })) {
    if (entry.isFile() && entry.name.startsWith(temporaryPrefix) && entry.name.endsWith(".tmp")) {
      await rm(path.join(rootReal, entry.name), { force: true });
    }
  }
}

function normalizeActualPlatform(value) {
  exactKeys(value, ["os", "arch", "runner_image_os", "runner_image_version"], "invalid_actual_platform");
  if (!concretePlatforms.includes(value.os) || !["x64", "arm64"].includes(value.arch)) fail("invalid_actual_platform");
  for (const key of ["runner_image_os", "runner_image_version"]) {
    if (value[key] !== null && (typeof value[key] !== "string" || !/^[A-Za-z0-9_.-]{1,128}$/.test(value[key]))) {
      fail("invalid_actual_platform");
    }
  }
  return Object.freeze({ ...value });
}

export function hostPlatform() {
  const hostOs = { linux: "linux", win32: "windows", darwin: "macos" }[process.platform];
  const arch = { x64: "x64", arm64: "arm64" }[process.arch];
  if (!hostOs || !arch) fail("unsupported_host_platform");
  const boundedEnvironment = (name) => {
    const value = process.env[name];
    return typeof value === "string" && /^[A-Za-z0-9_.-]{1,128}$/.test(value) ? value : null;
  };
  return {
    os: hostOs,
    arch,
    runner_image_os: boundedEnvironment("ImageOS"),
    runner_image_version: boundedEnvironment("ImageVersion"),
  };
}

function requireBrowserExecutorReceipt(result, trustedReceipt) {
  exactKeys(trustedReceipt, ["engine", "version"], "browser_executor_unverified");
  const receipt = result?.observations?.find((observation) => observation?.id === "browser-executor")?.receipt;
  exactKeys(receipt, ["engine", "version"], "browser_executor_unverified");
  if (!["chromium", "firefox", "webkit"].includes(receipt.engine)) fail("browser_executor_unverified");
  if (typeof receipt.version !== "string" || !/^\d+(?:\.\d+){1,3}$/.test(receipt.version)) {
    fail("browser_executor_unverified");
  }
  if (canonicalJson(receipt) !== canonicalJson(trustedReceipt)) fail("browser_executor_unverified");
}

async function readSourceIdentity(root) {
  const git = async (...args) => {
    const result = await execFile("git", ["-C", root, ...args], {
      encoding: "utf8",
      timeout: 10_000,
      windowsHide: true,
      maxBuffer: 1024 * 1024,
    });
    return result.stdout.trim();
  };
  const [commit, tree, status] = await Promise.all([
    git("rev-parse", "HEAD"),
    git("rev-parse", "HEAD^{tree}"),
    git("status", "--porcelain=v1", "--untracked-files=normal"),
  ]);
  return { commit, tree, dirty: status.length > 0 };
}

function normalizeSourceIdentity(source) {
  exactKeys(source, ["commit", "tree", "dirty"], "invalid_source_identity");
  if (!commitPattern.test(source.commit) || !commitPattern.test(source.tree) || typeof source.dirty !== "boolean") {
    fail("invalid_source_identity");
  }
  if (source.dirty) fail("dirty_source");
  return Object.freeze({ commit: source.commit, tree: source.tree });
}

async function readToolIdentity(root, platform) {
  try {
    const { verifyToolchain } = await import("./toolchain.mjs");
    const report = verifyToolchain({
      repositoryRoot: root,
      profiles: [platform.os === "browser" ? "frontend" : "desktop"],
    });
    const { manifest_sha256, ...manifest } = report.identity;
    return {
      manifest_sha256,
      identity: {
        manifest,
        profiles: report.profiles,
        mirrors: report.mirrors,
        executables: report.executables,
      },
    };
  } catch {
    fail("toolchain_unverified");
  }
}

async function readManifestToolIdentity(root) {
  try {
    const { readToolchainIdentity } = await import("./toolchain.mjs");
    const { manifest_sha256, ...manifest } = readToolchainIdentity({ repositoryRoot: root });
    return { manifest_sha256, identity: manifest };
  } catch {
    fail("toolchain_unverified");
  }
}

function validateObservedToolIdentity(toolchain, platform, current) {
  normalizeToolIdentity(toolchain);
  const identity = toolchain.identity;
  exactKeys(identity, ["manifest", "profiles", "mirrors", "executables"], "invalid_observed_toolchain");
  if (canonicalJson(identity.manifest) !== canonicalJson(current.identity)) fail("mixed_toolchain_identity");
  const profile = platform === "browser" ? "frontend" : "desktop";
  if (!Array.isArray(identity.profiles) || canonicalJson(identity.profiles) !== canonicalJson([profile])) {
    fail("invalid_observed_toolchain");
  }
  const expectedTools =
    profile === "frontend"
      ? ["node", "pnpm", "task"]
      : ["cargo", "node", "pnpm", "rustc", "task", "tauri_cli"];
  exactKeys(identity.executables, expectedTools, "invalid_observed_toolchain");
  const manifest = current.identity;
  const expectedMirrors =
    profile === "frontend"
      ? ["frontend_package"]
      : ["frontend_package", "rust_toolchain"];
  exactKeys(identity.mirrors, expectedMirrors, "invalid_observed_toolchain");
  exactKeys(identity.mirrors.frontend_package, ["node", "node_types", "pnpm"], "invalid_observed_toolchain");
  if (
    identity.mirrors.frontend_package.node !== manifest.node ||
    identity.mirrors.frontend_package.node_types !== manifest.node_types ||
    identity.mirrors.frontend_package.pnpm !== `pnpm@${manifest.pnpm}`
  ) {
    fail("invalid_observed_toolchain");
  }
  if (profile === "desktop") {
    exactKeys(
      identity.mirrors.rust_toolchain,
      ["channel", "profile", "components"],
      "invalid_observed_toolchain",
    );
    if (
      identity.mirrors.rust_toolchain.channel !== manifest.rust.release ||
      identity.mirrors.rust_toolchain.profile !== "minimal" ||
      canonicalJson(identity.mirrors.rust_toolchain.components) !== canonicalJson(["clippy", "rustfmt"])
    ) {
      fail("invalid_observed_toolchain");
    }
  }
  for (const tool of expectedTools) {
    exactKeys(
      identity.executables[tool],
      tool === "rustc" || tool === "cargo" ? ["release", "commit"] : ["release"],
      "invalid_observed_toolchain",
    );
  }
  if (
    identity.executables.node?.release !== manifest.node ||
    identity.executables.task?.release !== manifest.task ||
    identity.executables.pnpm?.release !== manifest.pnpm
  ) {
    fail("invalid_observed_toolchain");
  }
  if (profile === "desktop") {
    if (
      identity.executables.rustc?.release !== manifest.rust.release ||
      identity.executables.rustc?.commit !== manifest.rust.rustc_commit ||
      identity.executables.cargo?.release !== manifest.rust.release ||
      identity.executables.cargo?.commit !== manifest.rust.cargo_commit ||
      identity.executables.tauri_cli?.release !== manifest.tauri_cli
    ) {
      fail("invalid_observed_toolchain");
    }
  }
}

function normalizeToolIdentity(toolchain) {
  exactKeys(toolchain, ["manifest_sha256", "identity"], "invalid_toolchain_identity");
  if (!hashPattern.test(toolchain.manifest_sha256) || !isPlainObject(toolchain.identity)) fail("invalid_toolchain_identity");
  try {
    JSON.stringify(toolchain.identity);
  } catch {
    fail("invalid_toolchain_identity");
  }
  return Object.freeze({ manifest_sha256: toolchain.manifest_sha256, identity: toolchain.identity });
}

function normalizeRequest(request) {
  exactKeys(request, ["scenario", "platform", "capability"], "invalid_request");
  validateBoundedId(request.scenario, scenarioPattern, "invalid_scenario_id");
  if (![...concretePlatforms, "matrix"].includes(request.platform)) fail("invalid_requested_platform");
  if (request.capability !== null) validateBoundedId(request.capability, capabilityPattern, "invalid_capability_id");
  return request;
}

function concreteEvidenceRecord(record, platform) {
  if (record.allowed_platforms.length === 1) return record;
  const destination = path.join(
    path.dirname(record.evidence_destination),
    record.proof_id,
    `${platform}.json`,
  );
  return Object.freeze({ ...record, evidence_destination: destination });
}

async function resolveCapabilityRoots(overrides) {
  const requestedRoot = path.resolve(overrides.repositoryRoot ?? repositoryRoot);
  const root = await realpath(requestedRoot).catch(() => fail("invalid_repository_root"));
  const scenarioRoot = path.resolve(
    overrides.scenarioRoot ?? path.join(requestedRoot, "scripts/capabilities/scenarios"),
  );
  const requestedEvidenceRoot = path.resolve(
    overrides.evidenceRoot ?? path.join(requestedRoot, "evidence/capabilities"),
  );
  if (requestedEvidenceRoot !== path.join(requestedRoot, "evidence", "capabilities")) {
    fail("invalid_evidence_root");
  }
  return {
    root,
    scenarioRoot,
    evidenceRoot: path.join(root, "evidence", "capabilities"),
  };
}

async function aggregateRegisteredEvidence(record, root, overrides) {
  if (record.allowed_platforms.length < 2) fail("matrix_not_required");
  await cleanStaleEvidence(record);
  const documents = [];
  for (const platform of record.allowed_platforms) {
    const concrete = concreteEvidenceRecord(record, platform);
    let source;
    try {
      const metadata = await lstat(concrete.evidence_destination);
      if (!metadata.isFile() || metadata.isSymbolicLink() || metadata.size > 8 * 1024 * 1024) {
        fail("invalid_evidence");
      }
      source = await readFile(concrete.evidence_destination, "utf8");
    } catch {
      fail("incomplete_evidence_matrix");
    }
    let document;
    try {
      document = JSON.parse(source);
    } catch {
      fail("invalid_evidence");
    }
    if (source !== canonicalJson(document)) fail("noncanonical_evidence");
    validateEvidenceDocument(document);
    documents.push(document);
  }

  const source = normalizeSourceIdentity(await (overrides.sourceHook ?? readSourceIdentity)(root));
  const manifestToolchain = normalizeToolIdentity(
    await (overrides.matrixManifestHook ?? readManifestToolIdentity)(root),
  );
  const receiptProvider =
    overrides.receiptProvider ?? (await readCurrentReceipts(record, documents, root));
  const aggregate = await aggregateCapabilityEvidence(documents, {
    record,
    required_platforms: record.allowed_platforms,
    repository_root: root,
    source,
    manifest_sha256: manifestToolchain.manifest_sha256,
    manifest_identity: manifestToolchain.identity,
    tool_identity_validator:
      overrides.toolIdentityValidator ??
      ((toolchain, platform) => validateObservedToolIdentity(toolchain, platform, manifestToolchain)),
    receipt_provider: receiptProvider,
  });
  await (overrides.matrixWriter ?? writeCanonicalAtomic)(record.evidence_destination, aggregate);
  return Object.freeze({ evidence_path: record.evidence_path, evidence: aggregate });
}

export async function runCapability(request, overrides = {}) {
  const normalizedRequest = normalizeRequest(request);
  const { root, scenarioRoot, evidenceRoot } = await resolveCapabilityRoots(overrides);

  const records = await validateRegistryStructure(overrides.registry ?? capabilityRegistry, {
    scenarioRoot,
    evidenceRoot,
    repositoryRoot: root,
  });
  const record = records.find((candidate) => candidate.scenario_id === normalizedRequest.scenario);
  if (!record) fail("unknown_scenario");
  if (normalizedRequest.platform !== "matrix") {
    await cleanStaleEvidence(concreteEvidenceRecord(record, normalizedRequest.platform));
  }
  await cleanStaleEvidence(record);
  if (normalizedRequest.capability !== null && normalizedRequest.capability !== record.capability_id) {
    fail("capability_mismatch");
  }
  if (
    normalizedRequest.platform !== "matrix" &&
    !record.allowed_platforms.includes(normalizedRequest.platform)
  ) {
    fail("platform_not_allowed");
  }

  let inspection;
  try {
    [inspection] = await inspectRegistry([record]);
  } catch (error) {
    await cleanStaleEvidence(record);
    throw error;
  }

  if (normalizedRequest.platform === "matrix") {
    if (!inspection.has_receipt_revalidator) fail("receipt_revalidator_absent");
    return aggregateRegisteredEvidence(record, root, overrides);
  }

  const observedHost = normalizeActualPlatform(await (overrides.platformHook ?? hostPlatform)());
  if (normalizedRequest.platform !== "browser" && observedHost.os !== normalizedRequest.platform) {
    fail("platform_mismatch");
  }
  const actualPlatform =
    normalizedRequest.platform === "browser"
      ? Object.freeze({ ...observedHost, os: "browser" })
      : observedHost;
  const browserExecutorReceipt =
    normalizedRequest.platform === "browser"
      ? await (overrides.browserExecutorHook ?? (() => fail("browser_executor_unavailable")))()
      : null;
  const source = normalizeSourceIdentity(await (overrides.sourceHook ?? readSourceIdentity)(root));
  const toolchain = normalizeToolIdentity(
    await (overrides.toolchainHook ?? readToolIdentity)(root, actualPlatform),
  );

  const context = {
    scenario_id: record.scenario_id,
    proof_id: record.proof_id,
    capability_id: record.capability_id,
    owner_phase: record.owner_phase,
    platform: actualPlatform.os,
    repository_root: root,
  };
  const startedAt = Date.now();
  const startedMonotonic = process.hrtime.bigint();
  const message = await workerInvocation("run", record.module_path, context, record.timeout_ms);
  exactKeys(message, ["ok", "result"], "malformed_scenario_result");
  if (normalizedRequest.platform === "browser") {
    requireBrowserExecutorReceipt(message.result, browserExecutorReceipt);
  }
  const sealed = await sealScenarioResult(message.result, root).catch((error) => {
    if (error instanceof EvidenceError) fail(error.code);
    throw error;
  });
  const durationMs = Number((process.hrtime.bigint() - startedMonotonic) / 1_000_000n);
  const document = {
    schema_version: 1,
    result: "verified",
    proof_id: record.proof_id,
    scenario_id: record.scenario_id,
    capability_id: record.capability_id,
    owner_phase: record.owner_phase,
    source,
    platform: actualPlatform,
    toolchain,
    timing: {
      started_at: new Date(startedAt).toISOString(),
      completed_at: new Date(startedAt + durationMs).toISOString(),
      duration_ms: durationMs,
    },
    observations: sealed.observations,
    artifacts: sealed.artifacts,
  };
  validateEvidenceDocument(document);

  try {
    const concreteRecord = concreteEvidenceRecord(record, normalizedRequest.platform);
    await (overrides.evidenceWriter ?? writeEvidenceAtomic)(concreteRecord.evidence_destination, document);
  } catch (error) {
    await cleanStaleEvidence(concreteEvidenceRecord(record, normalizedRequest.platform));
    if (error instanceof CapabilityError) throw error;
    if (error instanceof EvidenceError) fail(error.code);
    fail("evidence_write_failed");
  }
  const concreteRecord = concreteEvidenceRecord(record, normalizedRequest.platform);
  const evidencePath = path.relative(root, concreteRecord.evidence_destination).split(path.sep).join("/");
  return Object.freeze({ evidence_path: evidencePath, evidence: document });
}

export async function auditCapabilityRegistry(overrides = {}) {
  const { root, scenarioRoot, evidenceRoot } = await resolveCapabilityRoots(overrides);
  const records = await validateRegistryStructure(overrides.registry ?? capabilityRegistry, {
    scenarioRoot,
    evidenceRoot,
    repositoryRoot: root,
  });
  await inspectRegistry(records);
  return Object.freeze({ records: records.length });
}

function parseCli(argv) {
  const command = argv.shift();
  if (command === "audit") {
    if (argv.length > 0 || ["SCENARIO", "PLATFORM", "CAPABILITY"].some((name) => process.env[name] !== undefined)) {
      fail("invalid_audit_input");
    }
    return { command: "audit" };
  }
  if (command !== "run" && command !== "platform") fail("invalid_command");
  const values = new Map();
  while (argv.length) {
    const option = argv.shift();
    if (!["--scenario", "--platform", "--capability"].includes(option) || values.has(option)) fail("invalid_argument");
    const value = argv.shift();
    if (!value) fail("invalid_argument");
    values.set(option, value);
  }
  const environmentMappings = [
    ["--scenario", "SCENARIO"],
    ["--platform", "PLATFORM"],
    ["--capability", "CAPABILITY"],
  ];
  for (const [option, environmentName] of environmentMappings) {
    const environmentValue = process.env[environmentName];
    if (values.has(option) && environmentValue !== undefined) fail("conflicting_argument_source");
    if (!values.has(option) && environmentValue !== undefined) {
      if (environmentValue.length === 0) fail("invalid_argument");
      values.set(option, environmentValue);
    }
  }
  if (!values.has("--scenario") || !values.has("--platform")) fail("invalid_argument");
  if (command === "platform" && !values.has("--capability")) fail("invalid_argument");
  if (command === "run" && values.has("--capability")) fail("invalid_argument");
  return {
    command,
    request: {
      scenario: values.get("--scenario"),
      platform: values.get("--platform"),
      capability: values.get("--capability") ?? null,
    },
  };
}

async function main() {
  const parsed = parseCli(process.argv.slice(2));
  if (parsed.command === "audit") {
    const result = await auditCapabilityRegistry();
    process.stdout.write(`capability_registry_audited:${result.records}\n`);
    return;
  }
  const result = await runCapability(parsed.request);
  process.stdout.write(`capability_verified:${result.evidence.scenario_id}\n`);
}

if (process.argv[1] && pathToFileURL(path.resolve(process.argv[1])).href === import.meta.url) {
  main().catch((error) => {
    const code = error instanceof CapabilityError || error instanceof EvidenceError ? error.code : "internal_failure";
    process.stderr.write(`capability_failed:${code}\n`);
    process.exitCode = 1;
  });
}

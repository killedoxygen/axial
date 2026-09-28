#!/usr/bin/env node

import { spawn, spawnSync } from "node:child_process";
import { lstat, opendir, readFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { performance } from "node:perf_hooks";
import { fileURLToPath } from "node:url";

import { isDirectInvocation } from "./direct-invocation.mjs";
import {
  acquireExclusiveLoopbackPort,
  portablePathLeaseIdentity,
  privateLoopbackLeasePort,
} from "./loopback-lease.mjs";

const modulePath = fileURLToPath(import.meta.url);
const defaultRepositoryRoot = path.resolve(path.dirname(modulePath), "..");
const leaseNamespace = "axial.cargo-target.v1";
const allowedCommands = new Set([
  "build",
  "check",
  "clippy",
  "clean",
  "run",
  "test",
]);
const forwardedSignals = ["SIGINT", "SIGTERM", "SIGHUP"];
const treeGraceMilliseconds = 1_000;
const treeSettlementMilliseconds = 2_000;
const maximumTauriConfigBytes = 32 * 1024;
const externalOutputOptions = Object.freeze([
  "--artifact-dir",
  "--build-dir",
  "--lockfile-path",
  "--out-dir",
]);
const nativeMonotonicNow = () => performance.now();

export const cargoTargetQuiescence = Object.freeze({
  scope: "cooperating_task_owned_cargo",
  state: "exclusive_lease_held_during_report",
  coordination_domain: "same_loopback_network_namespace",
  direct_or_orphaned_cargo: "unobserved",
});

export const cargoTargetContainment = Object.freeze({
  child_boundary: "posix_detached_group_windows_attached_tree",
  ordinary_signal: "bounded_full_tree_termination",
  natural_posix_close: "bounded_process_group_settlement",
  windows_boundary: "taskkill_snapshot_survivors_unobserved",
  settlement_failure: "original_signal_status_with_unobserved_orphan_boundary",
  supervisor_hard_kill: "orphaned_cargo_unobserved",
});

export class CargoTargetError extends Error {
  constructor(code, exitCode = 1) {
    super(`cargo-target: ${code}`);
    this.name = "CargoTargetError";
    this.code = code;
    this.exitCode = exitCode;
  }
}

function fail(code, exitCode) {
  throw new CargoTargetError(code, exitCode);
}

function isOption(args, name) {
  return args.some(
    (argument) => argument === name || argument.startsWith(`${name}=`),
  );
}

function tauriConfigValue(argument, following) {
  if (argument === "--config") return following;
  if (argument.startsWith("--config="))
    return argument.slice("--config=".length);
  return undefined;
}

function validateTauriConfig(value) {
  if (
    typeof value !== "string" ||
    !value ||
    Buffer.byteLength(value, "utf8") > maximumTauriConfigBytes
  ) {
    fail("invalid_tauri_config");
  }
  let parsed;
  try {
    parsed = JSON.parse(value);
  } catch {
    fail("invalid_tauri_config");
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    fail("invalid_tauri_config");
  }
}

export function parseCargoTargetInvocation(argv) {
  if (
    !Array.isArray(argv) ||
    argv.some(
      (argument) =>
        typeof argument !== "string" || !argument || argument.includes("\0"),
    ) ||
    argv.length < 4 ||
    argv[0] !== "run" ||
    argv[1] !== "--" ||
    argv[2] !== "cargo"
  ) {
    fail("invalid_invocation");
  }

  const cargoArgs = argv.slice(3);
  const command = cargoArgs[0];
  const tauri = command === "tauri";
  if (
    (!tauri && !allowedCommands.has(command)) ||
    (tauri && !["dev", "build"].includes(cargoArgs[1]))
  ) {
    fail("command_not_allowed");
  }
  if (isOption(cargoArgs, "--target-dir"))
    fail("target_dir_override_forbidden");
  if (isOption(cargoArgs, "--manifest-path")) fail("manifest_path_forbidden");
  if (externalOutputOptions.some((option) => isOption(cargoArgs, option))) {
    fail("external_output_forbidden");
  }

  if (tauri) {
    const separator = cargoArgs.indexOf("--", 2);
    const tauriArgs = cargoArgs.slice(
      2,
      separator === -1 ? undefined : separator,
    );
    const cargoTail = separator === -1 ? [] : cargoArgs.slice(separator + 1);
    let configCount = 0;
    for (let index = 0; index < tauriArgs.length; index += 1) {
      const argument = tauriArgs[index];
      const value = tauriConfigValue(argument, tauriArgs[index + 1]);
      if (value !== undefined) {
        configCount += 1;
        if (configCount > 1) fail("invalid_tauri_config");
        validateTauriConfig(value);
      }
      if (argument === "--config") {
        index += 1;
      }
    }
    if (isOption(cargoTail, "--config")) fail("cargo_config_forbidden");
  } else if (isOption(cargoArgs, "--config")) {
    fail("cargo_config_forbidden");
  }

  return Object.freeze({
    cargoArgs: Object.freeze(cargoArgs),
    cwd: tauri ? "apps/desktop" : ".",
  });
}

export async function cargoTargetLeasePort(
  repositoryRoot = defaultRepositoryRoot,
) {
  try {
    const targetIdentity = await portablePathLeaseIdentity(
      path.join(repositoryRoot, "target"),
    );
    return privateLoopbackLeasePort(`${leaseNamespace}\0${targetIdentity}`);
  } catch (error) {
    if (error instanceof CargoTargetError) throw error;
    fail("lease_unavailable");
  }
}

export async function acquireCargoTargetLease(
  repositoryRoot = defaultRepositoryRoot,
) {
  try {
    const port = await cargoTargetLeasePort(repositoryRoot);
    return await acquireExclusiveLoopbackPort(port);
  } catch (error) {
    if (error instanceof CargoTargetError) throw error;
    if (error?.code === "EADDRINUSE") fail("lease_contended", 75);
    fail("lease_unavailable");
  }
}

async function validateTargetRoot(targetDirectory, lstatImpl) {
  try {
    const metadata = await lstatImpl(targetDirectory);
    if (metadata.isSymbolicLink()) fail("target_root_is_symlink");
    if (!metadata.isDirectory()) fail("target_root_not_directory");
  } catch (error) {
    if (error instanceof CargoTargetError) throw error;
    if (error?.code !== "ENOENT") fail("target_probe_failed");
  }
}

export function cargoTargetEnvironment(repositoryRoot, overrides = {}) {
  const environment = { ...process.env, ...overrides };
  for (const name of Object.keys(environment)) {
    const folded = name.toUpperCase();
    if (
      folded === "CARGO_TARGET_DIR" ||
      folded === "CARGO_BUILD_TARGET_DIR" ||
      folded === "CARGO_BUILD_BUILD_DIR"
    ) {
      delete environment[name];
    }
  }
  environment.CARGO_TARGET_DIR = path.join(repositoryRoot, "target");
  return environment;
}

function sleep(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function linuxProcessGroupHasLiveMembers(groupId, procRoot = "/proc") {
  const entries = await opendir(procRoot);
  for await (const entry of entries) {
    if (!entry.isDirectory() || !/^[1-9][0-9]*$/.test(entry.name)) continue;
    try {
      const source = await readFile(
        path.join(procRoot, entry.name, "stat"),
        "utf8",
      );
      const commandEnd = source.lastIndexOf(") ");
      if (commandEnd < 0) return true;
      const fields = source
        .slice(commandEnd + 2)
        .trim()
        .split(/\s+/);
      if (fields.length < 3) return true;
      const state = fields[0];
      const processGroup = Number(fields[2]);
      if (processGroup === groupId && state !== "X" && state !== "Z")
        return true;
    } catch (error) {
      if (error?.code !== "ENOENT") return true;
    }
  }
  return false;
}

async function posixProcessGroupSettled(groupId, options) {
  if (options.platform === "linux") {
    try {
      return !(await (
        options.linuxGroupProbeImpl ?? linuxProcessGroupHasLiveMembers
      )(groupId));
    } catch {
      return false;
    }
  }
  try {
    (options.processKillImpl ?? process.kill)(-groupId, 0);
    return false;
  } catch (error) {
    return error?.code === "ESRCH";
  }
}

async function waitForPosixSettlement(groupId, timeout, options) {
  const monotonicNowImpl = options.monotonicNowImpl ?? nativeMonotonicNow;
  const sleepImpl = options.sleepImpl ?? sleep;
  let previous = monotonicNowImpl();
  if (!Number.isFinite(previous) || previous < 0) return false;
  const deadline = previous + timeout;
  while (true) {
    if (await posixProcessGroupSettled(groupId, options)) return true;
    const current = monotonicNowImpl();
    if (
      !Number.isFinite(current) ||
      current < previous ||
      current >= deadline
    ) {
      return false;
    }
    previous = current;
    await sleepImpl(Math.min(20, deadline - current));
  }
}

function boundedTreeDuration(value, fallback, maximum) {
  const selected = value ?? fallback;
  return Number.isInteger(selected) && selected >= 0 && selected <= maximum
    ? selected
    : fallback;
}

function signalPosixGroup(groupId, signal, processKillImpl) {
  try {
    processKillImpl(-groupId, signal);
    return true;
  } catch (error) {
    return error?.code === "ESRCH";
  }
}

function windowsTaskkillPath(options) {
  if (options.taskkillPath !== undefined) {
    const candidate = options.taskkillPath;
    return typeof candidate === "string" &&
      !candidate.includes("\0") &&
      path.win32.isAbsolute(candidate) &&
      path.win32.basename(candidate).toLowerCase() === "taskkill.exe"
      ? path.win32.normalize(candidate)
      : null;
  }

  const roots = Object.entries(options.environment ?? process.env)
    .filter(([name]) => name.toUpperCase() === "SYSTEMROOT")
    .map(([, value]) => value);
  if (roots.length !== 1 || typeof roots[0] !== "string") return null;
  const root = path.win32.normalize(roots[0]);
  if (!/^[A-Za-z]:\\Windows$/i.test(root)) return null;
  return path.win32.join(root, "System32", "taskkill.exe");
}

export async function terminateCargoProcessTree(child, signal, options = {}) {
  const platform = options.platform ?? process.platform;
  const pid = child?.pid;
  if (!Number.isSafeInteger(pid) || pid <= 0) return false;

  if (platform === "win32") {
    const executable = windowsTaskkillPath(options);
    if (!executable) return false;
    const result = (options.spawnSyncImpl ?? spawnSync)(
      executable,
      ["/PID", String(pid), "/T", "/F"],
      {
        stdio: ["ignore", "ignore", "ignore"],
        timeout: treeSettlementMilliseconds,
        windowsHide: true,
        shell: false,
      },
    );
    return !result.error && result.signal === null && result.status === 0;
  }

  const processKillImpl = options.processKillImpl ?? process.kill;
  const grace = boundedTreeDuration(
    options.graceMilliseconds,
    treeGraceMilliseconds,
    treeGraceMilliseconds,
  );
  const settlement = boundedTreeDuration(
    options.settlementMilliseconds,
    treeSettlementMilliseconds,
    treeSettlementMilliseconds,
  );
  signalPosixGroup(pid, signal, processKillImpl);
  const probeOptions = { ...options, platform, processKillImpl };
  if (await waitForPosixSettlement(pid, grace, probeOptions)) {
    return true;
  }
  if (signal !== "SIGTERM") {
    signalPosixGroup(pid, "SIGTERM", processKillImpl);
    if (await waitForPosixSettlement(pid, grace, probeOptions)) return true;
  }
  signalPosixGroup(pid, "SIGKILL", processKillImpl);
  return waitForPosixSettlement(pid, settlement, probeOptions);
}

export async function settleCargoProcessGroupAfterClose(child, options = {}) {
  const platform = options.platform ?? process.platform;
  if (platform === "win32") return true;
  const pid = child?.pid;
  if (!Number.isSafeInteger(pid) || pid <= 0) return false;

  const processKillImpl = options.processKillImpl ?? process.kill;
  const probeOptions = { ...options, platform, processKillImpl };
  if (await posixProcessGroupSettled(pid, probeOptions)) return true;

  const grace = boundedTreeDuration(
    options.graceMilliseconds,
    treeGraceMilliseconds,
    treeGraceMilliseconds,
  );
  const settlement = boundedTreeDuration(
    options.settlementMilliseconds,
    treeSettlementMilliseconds,
    treeSettlementMilliseconds,
  );
  signalPosixGroup(pid, "SIGTERM", processKillImpl);
  if (await waitForPosixSettlement(pid, grace, probeOptions)) return true;
  signalPosixGroup(pid, "SIGKILL", processKillImpl);
  return waitForPosixSettlement(pid, settlement, probeOptions);
}

function signalExitStatus(signal) {
  const number = os.constants.signals?.[signal];
  return Number.isInteger(number) ? 128 + number : 1;
}

function childSettlement(
  child,
  signalSource,
  terminateTreeImpl,
  settleNaturalTreeImpl,
) {
  return new Promise((resolve, reject) => {
    const handlers = new Map();
    let childError = false;
    let settled = false;
    let originalSignal;
    let treeControl;
    const detach = () => {
      for (const [signal, handler] of handlers)
        signalSource.removeListener(signal, handler);
    };
    const settle = (complete) => {
      if (settled) return;
      settled = true;
      detach();
      complete();
    };
    for (const signal of forwardedSignals) {
      const handler = () => {
        if (originalSignal) return;
        originalSignal = signal;
        treeControl = Promise.resolve()
          .then(() => terminateTreeImpl(child, signal))
          .then((controlled) => controlled === true)
          .catch(() => false);
      };
      handlers.set(signal, handler);
      signalSource.on(signal, handler);
    }
    child.once("error", () => {
      childError = true;
    });
    child.once("close", (status, signal) => {
      void (async () => {
        const treeControlled = treeControl ? await treeControl : true;
        if (originalSignal && !treeControlled) {
          settle(() =>
            reject(
              new CargoTargetError(
                "process_tree_unsettled",
                signalExitStatus(originalSignal),
              ),
            ),
          );
        } else if (originalSignal) {
          settle(() => resolve(signalExitStatus(originalSignal)));
        } else if (childError) {
          settle(() => reject(new CargoTargetError("spawn_failed")));
        } else {
          const groupSettled = await Promise.resolve()
            .then(() => settleNaturalTreeImpl(child))
            .then((controlled) => controlled === true)
            .catch(() => false);
          if (!groupSettled) {
            settle(() =>
              reject(new CargoTargetError("process_tree_unsettled")),
            );
          } else if (Number.isInteger(status) && status >= 0) {
            settle(() => resolve(status));
          } else if (typeof signal === "string") {
            settle(() => resolve(signalExitStatus(signal)));
          } else {
            settle(() => reject(new CargoTargetError("invalid_child_status")));
          }
        }
      })();
    });
  });
}

export async function runCargoTarget(argv, options = {}) {
  const invocation = parseCargoTargetInvocation(argv);
  const repositoryRoot = options.repositoryRoot ?? defaultRepositoryRoot;
  if (typeof repositoryRoot !== "string" || !path.isAbsolute(repositoryRoot)) {
    fail("invalid_repository_root");
  }

  const release = await (options.acquireLeaseImpl ?? acquireCargoTargetLease)(
    repositoryRoot,
  );
  try {
    const targetDirectory = path.join(repositoryRoot, "target");
    await validateTargetRoot(targetDirectory, options.lstatImpl ?? lstat);
    let child;
    try {
      child = (options.spawnImpl ?? spawn)("cargo", invocation.cargoArgs, {
        cwd: path.resolve(repositoryRoot, invocation.cwd),
        env: cargoTargetEnvironment(repositoryRoot, options.env),
        detached: (options.platform ?? process.platform) !== "win32",
        shell: false,
        stdio: "inherit",
        windowsHide: true,
      });
    } catch {
      fail("spawn_failed");
    }
    const terminateTreeImpl =
      options.terminateTreeImpl ??
      ((ownedChild, signal) =>
        terminateCargoProcessTree(ownedChild, signal, {
          platform: options.platform,
          processKillImpl: options.processKillImpl,
          spawnSyncImpl: options.spawnSyncImpl,
          taskkillPath: options.taskkillPath,
          environment: options.env,
          linuxGroupProbeImpl: options.linuxGroupProbeImpl,
          monotonicNowImpl: options.monotonicNowImpl,
          sleepImpl: options.sleepImpl,
          graceMilliseconds: options.treeGraceMilliseconds,
          settlementMilliseconds: options.treeSettlementMilliseconds,
        }));
    const settleNaturalTreeImpl =
      options.settleNaturalTreeImpl ??
      ((ownedChild) =>
        settleCargoProcessGroupAfterClose(ownedChild, {
          platform: options.platform,
          processKillImpl: options.processKillImpl,
          linuxGroupProbeImpl: options.linuxGroupProbeImpl,
          monotonicNowImpl: options.monotonicNowImpl,
          sleepImpl: options.sleepImpl,
          graceMilliseconds: options.treeGraceMilliseconds,
          settlementMilliseconds: options.treeSettlementMilliseconds,
        }));
    return await childSettlement(
      child,
      options.signalSource ?? process,
      terminateTreeImpl,
      settleNaturalTreeImpl,
    );
  } finally {
    await release();
  }
}

export async function main(argv = process.argv.slice(2), options = {}) {
  const status = await runCargoTarget(argv, options);
  process.exitCode = status;
  return status;
}

if (isDirectInvocation(import.meta.url)) {
  main().catch((error) => {
    const message =
      error instanceof CargoTargetError
        ? error.message
        : "cargo-target: unexpected_error";
    process.stderr.write(`${message}\n`);
    process.exitCode = error instanceof CargoTargetError ? error.exitCode : 1;
  });
}

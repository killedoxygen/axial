#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { constants as filesystemConstants } from "node:fs";
import { lstat, open, opendir, statfs } from "node:fs/promises";
import path from "node:path";
import { performance } from "node:perf_hooks";
import { fileURLToPath } from "node:url";

import {
  acquireCargoTargetLease,
  CargoTargetError,
  cargoTargetEnvironment,
  cargoTargetQuiescence,
} from "./cargo-target.mjs";
import { isDirectInvocation } from "./direct-invocation.mjs";

const modulePath = fileURLToPath(import.meta.url);
const defaultRepositoryRoot = path.resolve(path.dirname(modulePath), "..");
const schema = "axial.build-storage.v1";
const commitPattern = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/;
const maximumEntries = 2_000_000;
const maximumDepth = 64;
const maximumBytes = 4n * 1024n * 1024n * 1024n * 1024n;
const maximumInputBytes = 8 * 1024 * 1024;
const maximumSourceReadTimeoutMilliseconds = 30_000;
const sourceReadTimeoutMilliseconds = 10_000;
const traversalDeadlineMilliseconds = 30_000;

const nativeLstat = (candidate) => lstat(candidate, { bigint: true });
const nativeOpenDirectory = (directory) =>
  opendir(directory, { encoding: "utf8", bufferSize: 32 });
const nativeStatfs = (candidate) => statfs(candidate, { bigint: true });
const nativeMonotonicNow = () => performance.now();
const nativeOpenSource = (candidate, flags) => open(candidate, flags);
const nativeSourceStream = (handle, options) =>
  handle.createReadStream(options);

export class BuildStorageError extends Error {
  constructor(code) {
    super(`build-storage: ${code}`);
    this.name = "BuildStorageError";
    this.code = code;
  }
}

function fail(code) {
  throw new BuildStorageError(code);
}

function unsigned(value) {
  if (typeof value === "bigint" && value >= 0n) return value;
  if (Number.isSafeInteger(value) && value >= 0) return BigInt(value);
  return null;
}

function integer(value) {
  if (typeof value === "bigint") return value;
  if (Number.isSafeInteger(value)) return BigInt(value);
  return null;
}

function boundedCount(value, fallback, maximum, code) {
  const selected = value ?? fallback;
  if (!Number.isSafeInteger(selected) || selected < 0 || selected > maximum) {
    fail(code);
  }
  return selected;
}

function requireStats(value) {
  if (
    !value ||
    typeof value !== "object" ||
    typeof value.isDirectory !== "function" ||
    typeof value.isFile !== "function" ||
    typeof value.isSymbolicLink !== "function"
  ) {
    fail("invalid_filesystem_stat");
  }
  return value;
}

function inventory(state, totals = {}) {
  const allocated =
    totals.allocationKnown === false ? null : (totals.allocated ?? 0n);
  return {
    state,
    apparent_bytes: (totals.apparent ?? 0n).toString(),
    allocated_bytes: allocated?.toString() ?? null,
    allocated_state: allocated === null ? "unavailable" : "available",
    files: totals.files ?? 0,
    directories: totals.directories ?? 0,
    symlinks: totals.symlinks ?? 0,
    other: totals.other ?? 0,
  };
}

function validName(name) {
  return (
    typeof name === "string" &&
    name.length > 0 &&
    name !== "." &&
    name !== ".." &&
    !name.includes("/") &&
    !name.includes("\\") &&
    !name.includes("\0")
  );
}

export async function collectBuildStorage(options = {}) {
  const {
    targetDirectory,
    lstatImpl = nativeLstat,
    openDirectoryImpl = nativeOpenDirectory,
    monotonicNowImpl = nativeMonotonicNow,
    platform = process.platform,
  } = options;
  if (
    typeof targetDirectory !== "string" ||
    !path.isAbsolute(targetDirectory)
  ) {
    fail("invalid_target_root");
  }
  if (
    typeof lstatImpl !== "function" ||
    typeof openDirectoryImpl !== "function" ||
    typeof monotonicNowImpl !== "function"
  ) {
    fail("invalid_filesystem_adapter");
  }
  let lastMonotonic = monotonicNowImpl();
  if (!Number.isFinite(lastMonotonic) || lastMonotonic < 0) {
    fail("invalid_monotonic_clock");
  }
  const deadline = lastMonotonic + traversalDeadlineMilliseconds;
  const checkDeadline = () => {
    const current = monotonicNowImpl();
    if (!Number.isFinite(current) || current < lastMonotonic) {
      fail("invalid_monotonic_clock");
    }
    lastMonotonic = current;
    if (current > deadline) fail("traversal_deadline_exceeded");
  };
  const entryLimit = boundedCount(
    options.maxEntries,
    maximumEntries,
    maximumEntries,
    "invalid_entry_limit",
  );
  const depthLimit = boundedCount(
    options.maxDepth,
    maximumDepth,
    maximumDepth,
    "invalid_depth_limit",
  );
  const byteLimit = unsigned(options.maxAggregateBytes ?? maximumBytes);
  if (byteLimit === null || byteLimit > maximumBytes) {
    fail("invalid_aggregate_limit");
  }

  let root;
  try {
    checkDeadline();
    root = requireStats(await lstatImpl(targetDirectory));
    checkDeadline();
  } catch (error) {
    if (error?.code === "ENOENT") {
      checkDeadline();
      return inventory("missing");
    }
    if (error instanceof BuildStorageError) throw error;
    fail("target_probe_failed");
  }
  if (root.isSymbolicLink()) fail("target_root_is_symlink");
  if (!root.isDirectory()) fail("target_root_not_directory");

  const totals = {
    entries: 0,
    apparent: 0n,
    allocated: 0n,
    allocationKnown: platform !== "win32",
    files: 0,
    directories: 0,
    symlinks: 0,
    other: 0,
  };
  const hardlinks = new Set();
  const record = (stats) => {
    checkDeadline();
    totals.entries += 1;
    if (totals.entries > entryLimit) fail("entry_limit_exceeded");
    if (stats.isSymbolicLink()) totals.symlinks += 1;
    else if (stats.isFile()) totals.files += 1;
    else if (stats.isDirectory()) totals.directories += 1;
    else totals.other += 1;

    const linkCount = unsigned(stats.nlink);
    if (linkCount !== null && linkCount > 1n) {
      const device = unsigned(stats.dev);
      const inode = unsigned(stats.ino);
      if (device === null || inode === null) fail("invalid_filesystem_stat");
      const identity = `${device}:${inode}`;
      if (hardlinks.has(identity)) return;
      hardlinks.add(identity);
    }

    const size = unsigned(stats.size);
    if (size === null) fail("invalid_filesystem_stat");
    totals.apparent += size;
    if (totals.apparent > byteLimit) fail("aggregate_limit_exceeded");

    if (totals.allocationKnown) {
      const blocks = unsigned(stats.blocks);
      if (blocks === null) {
        totals.allocationKnown = false;
      } else {
        totals.allocated += blocks * 512n;
        if (totals.allocated > byteLimit) fail("aggregate_limit_exceeded");
      }
    }
  };

  const visit = async (directory, depth) => {
    let entries;
    try {
      checkDeadline();
      entries = await openDirectoryImpl(directory);
      checkDeadline();
    } catch (error) {
      if (error instanceof BuildStorageError) throw error;
      fail("target_read_failed");
    }
    if (!entries || typeof entries[Symbol.asyncIterator] !== "function") {
      fail("invalid_directory_adapter");
    }
    try {
      for await (const entry of entries) {
        checkDeadline();
        const name = typeof entry === "string" ? entry : entry?.name;
        if (!validName(name)) fail("invalid_directory_entry");
        if (totals.entries >= entryLimit) fail("entry_limit_exceeded");
        if (depth >= depthLimit) fail("depth_limit_exceeded");

        const candidate = path.join(directory, name);
        let stats;
        try {
          stats = requireStats(await lstatImpl(candidate));
          checkDeadline();
        } catch (error) {
          if (error instanceof BuildStorageError) throw error;
          fail(
            error?.code === "ENOENT" ? "target_changed" : "target_probe_failed",
          );
        }
        record(stats);
        if (!stats.isSymbolicLink() && stats.isDirectory()) {
          await visit(candidate, depth + 1);
        }
        checkDeadline();
      }
    } catch (error) {
      if (error instanceof BuildStorageError) throw error;
      fail("target_read_failed");
    }
  };

  record(root);
  await visit(targetDirectory, 0);
  checkDeadline();
  return inventory("present", totals);
}

function samePath(left, right) {
  const first = path.resolve(left);
  const second = path.resolve(right);
  return process.platform === "win32"
    ? first.toLowerCase() === second.toLowerCase()
    : first === second;
}

export function resolveCanonicalTarget(metadata, root = defaultRepositoryRoot) {
  if (!metadata || typeof metadata !== "object" || Array.isArray(metadata)) {
    fail("invalid_cargo_metadata");
  }
  if (
    typeof metadata.workspace_root !== "string" ||
    !path.isAbsolute(metadata.workspace_root) ||
    !samePath(metadata.workspace_root, root)
  ) {
    fail("cargo_workspace_mismatch");
  }
  const expected = path.resolve(root, "target");
  if (
    typeof metadata.target_directory !== "string" ||
    !path.isAbsolute(metadata.target_directory) ||
    !samePath(metadata.target_directory, expected)
  ) {
    fail("noncanonical_target_directory");
  }
  return expected;
}

function commandOutput(command, args, root, timeout, maxBuffer, code, env) {
  try {
    return execFileSync(command, args, {
      cwd: root,
      encoding: "utf8",
      timeout,
      maxBuffer,
      windowsHide: true,
      stdio: ["ignore", "pipe", "pipe"],
      ...(env ? { env } : {}),
    });
  } catch {
    fail(code);
  }
}

function cargoMetadata(root) {
  const output = commandOutput(
    "cargo",
    ["metadata", "--locked", "--no-deps", "--format-version", "1"],
    root,
    15_000,
    1024 * 1024,
    "cargo_metadata_failed",
    cargoTargetEnvironment(root),
  );
  try {
    return JSON.parse(output);
  } catch {
    fail("invalid_cargo_metadata");
  }
}

function sourceCommit(root) {
  const commit = commandOutput(
    "git",
    ["rev-parse", "--verify", "HEAD^{commit}"],
    root,
    5_000,
    4096,
    "commit_probe_failed",
  ).trim();
  if (!commitPattern.test(commit)) fail("invalid_commit_identity");
  return commit;
}

async function sourceInputStats(candidate, sourceLstatImpl) {
  let source;
  try {
    source = requireStats(await sourceLstatImpl(candidate));
  } catch (error) {
    if (error instanceof BuildStorageError) throw error;
    fail("source_input_probe_failed");
  }
  if (source.isSymbolicLink() || !source.isFile()) {
    fail("source_input_not_regular_file");
  }
  const size = unsigned(source.size);
  if (size === null) fail("source_input_probe_failed");
  if (size > BigInt(maximumInputBytes)) fail("source_input_too_large");
  const device = unsigned(source.dev);
  const inode = unsigned(source.ino);
  const changeTime = integer(source.ctimeNs);
  const modificationTime = integer(source.mtimeNs);
  if (
    device === null ||
    inode === null ||
    changeTime === null ||
    modificationTime === null
  ) {
    fail("source_input_probe_failed");
  }
  return { device, inode, size, changeTime, modificationTime };
}

function sourceOpenFlags() {
  let flags = filesystemConstants.O_RDONLY;
  if (process.platform === "win32") return flags;
  if (
    !Number.isSafeInteger(filesystemConstants.O_NONBLOCK) ||
    !Number.isSafeInteger(filesystemConstants.O_NOFOLLOW)
  ) {
    fail("source_open_policy_unavailable");
  }
  flags |= filesystemConstants.O_NONBLOCK | filesystemConstants.O_NOFOLLOW;
  return flags;
}

function requireSourceHandle(handle) {
  if (
    !handle ||
    typeof handle !== "object" ||
    typeof handle.stat !== "function" ||
    typeof handle.close !== "function"
  ) {
    fail("invalid_source_input_adapter");
  }
  return handle;
}

async function openedSourceIdentity(handle) {
  let source;
  try {
    source = requireStats(await handle.stat({ bigint: true }));
  } catch (error) {
    if (error instanceof BuildStorageError) throw error;
    fail("source_input_probe_failed");
  }
  if (source.isSymbolicLink() || !source.isFile()) {
    fail("source_input_not_regular_file");
  }
  const device = unsigned(source.dev);
  const inode = unsigned(source.ino);
  const size = unsigned(source.size);
  const changeTime = integer(source.ctimeNs);
  const modificationTime = integer(source.mtimeNs);
  if (
    device === null ||
    inode === null ||
    size === null ||
    changeTime === null ||
    modificationTime === null
  ) {
    fail("source_input_probe_failed");
  }
  if (size > BigInt(maximumInputBytes)) fail("source_input_too_large");
  return { device, inode, size, changeTime, modificationTime };
}

function sameSourceIdentity(first, second) {
  return (
    first.device === second.device &&
    first.inode === second.inode &&
    first.size === second.size &&
    first.changeTime === second.changeTime &&
    first.modificationTime === second.modificationTime
  );
}

async function hashInput(candidate, options) {
  const expected = await sourceInputStats(candidate, options.sourceLstatImpl);

  const hash = createHash("sha256");
  let bytes = 0;
  let timedOut = false;
  let handle;
  const controller = new AbortController();
  // O_NONBLOCK closes the POSIX path-to-FIFO race; AbortSignal bounds observable
  // descriptor reads. Uninterruptible kernel calls remain an OS-level boundary.
  const timer = setTimeout(() => {
    timedOut = true;
    controller.abort();
  }, options.readTimeoutMilliseconds);
  try {
    try {
      handle = requireSourceHandle(
        await options.openSourceImpl(candidate, sourceOpenFlags()),
      );
    } catch (error) {
      if (error instanceof BuildStorageError) throw error;
      if (error?.code === "ELOOP") fail("source_input_not_regular_file");
      if (timedOut) fail("source_input_timeout");
      fail("source_input_read_failed");
    }
    const opened = await openedSourceIdentity(handle);
    const openedPath = await sourceInputStats(
      candidate,
      options.sourceLstatImpl,
    );
    if (
      !sameSourceIdentity(expected, opened) ||
      !sameSourceIdentity(expected, openedPath)
    ) {
      fail("source_input_changed");
    }

    for await (const chunk of options.createReadStreamImpl(handle, {
      autoClose: false,
      highWaterMark: 64 * 1024,
      signal: controller.signal,
    })) {
      if (!ArrayBuffer.isView(chunk)) fail("source_input_read_failed");
      bytes += chunk.byteLength;
      if (bytes > maximumInputBytes) fail("source_input_too_large");
      hash.update(chunk);
    }
    if (timedOut) fail("source_input_timeout");
    const completed = await openedSourceIdentity(handle);
    const completedPath = await sourceInputStats(
      candidate,
      options.sourceLstatImpl,
    );
    if (
      !sameSourceIdentity(opened, completed) ||
      !sameSourceIdentity(expected, completedPath) ||
      BigInt(bytes) !== completed.size
    ) {
      fail("source_input_changed");
    }
  } catch (error) {
    if (error instanceof BuildStorageError) throw error;
    if (timedOut) fail("source_input_timeout");
    fail("source_input_read_failed");
  } finally {
    clearTimeout(timer);
    if (handle) {
      try {
        await handle.close();
      } catch {
        fail("source_input_close_failed");
      }
    }
  }
  return hash.digest("hex");
}

async function sourceHashes(root, options) {
  const createReadStreamImpl =
    options.createReadStreamImpl ?? nativeSourceStream;
  const sourceLstatImpl = options.sourceLstatImpl ?? nativeLstat;
  const openSourceImpl = options.openSourceImpl ?? nativeOpenSource;
  if (
    typeof createReadStreamImpl !== "function" ||
    typeof sourceLstatImpl !== "function" ||
    typeof openSourceImpl !== "function"
  ) {
    fail("invalid_source_input_adapter");
  }
  const readTimeoutMilliseconds =
    options.sourceReadTimeoutMilliseconds ?? sourceReadTimeoutMilliseconds;
  if (
    !Number.isSafeInteger(readTimeoutMilliseconds) ||
    readTimeoutMilliseconds <= 0 ||
    readTimeoutMilliseconds > maximumSourceReadTimeoutMilliseconds
  ) {
    fail("invalid_source_read_timeout");
  }

  const hashes = {};
  for (const name of [
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
    "toolchain.json",
  ]) {
    hashes[name] = await hashInput(path.join(root, name), {
      createReadStreamImpl,
      sourceLstatImpl,
      openSourceImpl,
      readTimeoutMilliseconds,
    });
  }
  return hashes;
}

function unavailableFilesystem() {
  return {
    identity: null,
    available_bytes: null,
    availability_state: "unavailable",
  };
}

async function filesystemReport({
  probePath,
  platform,
  lstatImpl,
  statfsImpl,
}) {
  let root;
  let filesystem;
  try {
    root = requireStats(await lstatImpl(probePath));
    if (root.isSymbolicLink() || !root.isDirectory()) {
      fail("filesystem_probe_root_invalid");
    }
    filesystem = await statfsImpl(probePath);
  } catch (error) {
    if (error instanceof BuildStorageError) throw error;
    return unavailableFilesystem();
  }
  const device = unsigned(root.dev);
  const type = unsigned(filesystem?.type);
  const blockSize = unsigned(filesystem?.bsize);
  const available = unsigned(filesystem?.bavail);
  const identity =
    typeof platform === "string" &&
    platform &&
    device !== null &&
    device > 0n &&
    type !== null &&
    type > 0n &&
    blockSize !== null &&
    blockSize > 0n
      ? `sha256:${createHash("sha256")
          .update(
            [
              "axial.build-storage.filesystem.v1",
              platform,
              device,
              type,
              blockSize,
            ].join("\0"),
          )
          .digest("hex")}`
      : null;
  const availableBytes =
    blockSize !== null && blockSize > 0n && available !== null
      ? (available * blockSize).toString()
      : null;
  return {
    identity,
    available_bytes: availableBytes,
    availability_state: availableBytes === null ? "unavailable" : "available",
  };
}

async function createBuildStorageReportOwned(options, root) {
  const metadata =
    options.metadata ?? (await (options.metadataImpl ?? cargoMetadata)(root));
  const targetDirectory = resolveCanonicalTarget(metadata, root);
  const commit =
    options.commit ?? (await (options.commitImpl ?? sourceCommit)(root));
  if (typeof commit !== "string" || !commitPattern.test(commit)) {
    fail("invalid_commit_identity");
  }

  const lstatImpl = options.lstatImpl ?? nativeLstat;
  const target = await collectBuildStorage({
    targetDirectory,
    lstatImpl,
    openDirectoryImpl: options.openDirectoryImpl,
    platform: options.platform,
    maxEntries: options.maxEntries,
    maxDepth: options.maxDepth,
    maxAggregateBytes: options.maxAggregateBytes,
    monotonicNowImpl: options.monotonicNowImpl,
  });
  const filesystem = await filesystemReport({
    probePath: target.state === "present" ? targetDirectory : root,
    platform: options.platform ?? process.platform,
    lstatImpl,
    statfsImpl: options.statfsImpl ?? nativeStatfs,
  });
  return {
    schema,
    quiescence: cargoTargetQuiescence,
    source: {
      commit,
      inputs: await sourceHashes(root, options),
    },
    target: {
      relative_path: "target",
      ...target,
      filesystem,
    },
  };
}

async function acquireReportLease(root, acquireLeaseImpl) {
  try {
    return await acquireLeaseImpl(root);
  } catch (error) {
    if (error instanceof CargoTargetError && error.code === "lease_contended") {
      fail("target_lease_contended");
    }
    fail("target_lease_failed");
  }
}

async function writeReportOutput(destination, output, writeOutputImpl) {
  if (writeOutputImpl !== undefined) {
    if (typeof writeOutputImpl !== "function") fail("invalid_output_adapter");
    await writeOutputImpl(destination, output);
    return;
  }
  if (!destination || typeof destination.write !== "function") {
    fail("invalid_output_adapter");
  }
  if (typeof destination.once !== "function") {
    destination.write(output);
    return;
  }
  await new Promise((resolve, reject) => {
    destination.write(output, (error) => (error ? reject(error) : resolve()));
  });
}

export async function createBuildStorageReport(options = {}) {
  const root = options.repositoryRoot ?? defaultRepositoryRoot;
  if (typeof root !== "string" || !path.isAbsolute(root)) {
    fail("invalid_repository_root");
  }
  const release = await acquireReportLease(
    root,
    options.acquireLeaseImpl ?? acquireCargoTargetLease,
  );
  try {
    return await createBuildStorageReportOwned(options, root);
  } finally {
    await release();
  }
}

export function formatBuildStorage(report) {
  return `${JSON.stringify(report, null, 2)}\n`;
}

export async function main(argv = process.argv.slice(2), options = {}) {
  if (argv.length !== 1 || argv[0] !== "report") fail("invalid_command");
  const root = options.repositoryRoot ?? defaultRepositoryRoot;
  if (typeof root !== "string" || !path.isAbsolute(root)) {
    fail("invalid_repository_root");
  }
  const release = await acquireReportLease(
    root,
    options.acquireLeaseImpl ?? acquireCargoTargetLease,
  );
  try {
    const report = await createBuildStorageReportOwned(options, root);
    const output = options.stdout ?? process.stdout;
    const formatted = formatBuildStorage(report);
    await writeReportOutput(output, formatted, options.writeOutputImpl);
    return report;
  } finally {
    await release();
  }
}

if (isDirectInvocation(import.meta.url)) {
  main().catch((error) => {
    const message =
      error instanceof BuildStorageError
        ? error.message
        : "build-storage: unexpected_error";
    process.stderr.write(`${message}\n`);
    process.exitCode = 1;
  });
}

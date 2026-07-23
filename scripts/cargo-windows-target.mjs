#!/usr/bin/env node

import { spawn } from "node:child_process";
import { lstat } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import {
  acquireCargoTargetLease,
  CargoTargetError,
  parseCargoTargetInvocation,
  runCargoTarget,
} from "./cargo-target.mjs";
import { isDirectInvocation } from "./direct-invocation.mjs";

const modulePath = fileURLToPath(import.meta.url);
const defaultRepositoryRoot = path.resolve(path.dirname(modulePath), "..");

export const windowsTargetTriple = "x86_64-pc-windows-gnu";
export const windowsTargetRelativeDirectory = path.join(
  "target",
  "windows-gnu",
);

export class WindowsCargoTargetError extends Error {
  constructor(code, exitCode = 1) {
    super(`cargo-windows-target: ${code}`);
    this.name = "WindowsCargoTargetError";
    this.code = code;
    this.exitCode = exitCode;
  }
}

function fail(code, exitCode) {
  throw new WindowsCargoTargetError(code, exitCode);
}

function hasTargetOption(cargoArgs) {
  return cargoArgs.some(
    (argument) => argument === "--target" || argument.startsWith("--target="),
  );
}

export function parseWindowsCargoTargetInvocation(argv) {
  const invocation = parseCargoTargetInvocation(argv);
  const { cargoArgs } = invocation;
  if (hasTargetOption(cargoArgs)) fail("caller_target_forbidden");
  if (cargoArgs[0] === "clean") {
    if (cargoArgs.length !== 1) fail("clean_scope_must_be_fixed");
    return Object.freeze([...argv]);
  }
  if (cargoArgs[0] === "build" || cargoArgs[0] === "test") {
    return Object.freeze([
      ...argv.slice(0, 3),
      cargoArgs[0],
      "--target",
      windowsTargetTriple,
      ...cargoArgs.slice(1),
    ]);
  }
  if (cargoArgs[0] === "tauri" && cargoArgs[1] === "dev") {
    return Object.freeze([
      ...argv.slice(0, 3),
      "tauri",
      "dev",
      "--target",
      windowsTargetTriple,
      ...cargoArgs.slice(2),
    ]);
  }
  fail("command_not_allowed");
}

async function validateIsolatedRoot(targetDirectory, lstatImpl) {
  try {
    const metadata = await lstatImpl(targetDirectory);
    if (metadata.isSymbolicLink()) fail("isolated_target_is_symlink");
    if (!metadata.isDirectory()) fail("isolated_target_not_directory");
  } catch (error) {
    if (error instanceof WindowsCargoTargetError) throw error;
    if (error?.code !== "ENOENT") fail("isolated_target_probe_failed");
  }
}

export async function runWindowsCargoTarget(argv, options = {}) {
  const fixedArgv = parseWindowsCargoTargetInvocation(argv);
  const repositoryRoot = options.repositoryRoot ?? defaultRepositoryRoot;
  if (typeof repositoryRoot !== "string" || !path.isAbsolute(repositoryRoot)) {
    fail("invalid_repository_root");
  }
  const targetDirectory = path.join(
    repositoryRoot,
    windowsTargetRelativeDirectory,
  );
  const release = await (options.acquireLeaseImpl ?? acquireCargoTargetLease)(
    repositoryRoot,
  );
  try {
    await validateIsolatedRoot(targetDirectory, options.lstatImpl ?? lstat);
    const spawnImpl = options.spawnImpl ?? spawn;
    return await runCargoTarget(fixedArgv, {
      ...options,
      repositoryRoot,
      acquireLeaseImpl: async () => async () => {},
      spawnImpl(command, args, spawnOptions) {
        return spawnImpl(command, args, {
          ...spawnOptions,
          env: {
            ...spawnOptions.env,
            CARGO_TARGET_DIR: targetDirectory,
          },
        });
      },
    });
  } finally {
    await release();
  }
}

export async function main(argv = process.argv.slice(2), options = {}) {
  const status = await runWindowsCargoTarget(argv, options);
  process.exitCode = status;
  return status;
}

if (isDirectInvocation(import.meta.url)) {
  main().catch((error) => {
    const message =
      error instanceof WindowsCargoTargetError ||
      error instanceof CargoTargetError
        ? error.message
        : "cargo-windows-target: unexpected_error";
    process.stderr.write(`${message}\n`);
    process.exitCode =
      error instanceof WindowsCargoTargetError ||
      error instanceof CargoTargetError
        ? error.exitCode
        : 1;
  });
}

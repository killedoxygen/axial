#!/usr/bin/env node

import { createHash } from "node:crypto";
import { execFile } from "node:child_process";
import { createReadStream, openAsBlob } from "node:fs";
import {
  lstat,
  mkdir,
  open,
  opendir,
  rename,
  rm,
  writeFile,
} from "node:fs/promises";
import path from "node:path";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";

import { isDirectInvocation } from "./direct-invocation.mjs";

const execFileAsync = promisify(execFile);

const VERSION_SOURCE = String.raw`(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-(?:dev|alpha|beta|rc)\.(?:[1-9]\d*))?`;
const VERSION_PATTERN = new RegExp(`^${VERSION_SOURCE}$`);
const TAG_PATTERN = new RegExp(`^v(${VERSION_SOURCE})$`);
const DATE_PATTERN = /^\d{4}-\d{2}-\d{2}$/;
const SHA256_PATTERN = /^[0-9a-f]{64}$/;
const RELEASE_ID_PATTERN = /^[1-9]\d*$/;
const SOURCE_SHA_PATTERN = /^[0-9a-f]{40}$/;
const MAX_SOURCE_BYTES = 4 * 1024 * 1024;
const MAX_METADATA_BYTES = 8 * 1024 * 1024;
const MAX_API_BYTES = 2 * 1024 * 1024;
const MAX_NOTES_BYTES = 120 * 1024;
const MAX_RECEIPT_BYTES = 256 * 1024;
const MAX_ASSET_BYTES = 2 * 1024 * 1024 * 1024;
const MAX_DIRECTORY_ENTRIES = 64;
const MAX_ERROR_DETAIL = 240;
const SOURCE_RECEIPT_SCHEMA = "axial.release-source.v1";
const MODULE_PATH = fileURLToPath(import.meta.url);

export class ReleaseContractError extends Error {
  constructor(code, detail = "") {
    const bounded = String(detail)
      .replaceAll(/[\r\n\t]+/g, " ")
      .slice(0, MAX_ERROR_DETAIL);
    super(bounded ? `${code}: ${bounded}` : code);
    this.name = "ReleaseContractError";
    this.code = code;
  }
}

function fail(code, detail) {
  throw new ReleaseContractError(code, detail);
}

function freezeArray(values) {
  return Object.freeze(values.map((value) => Object.freeze(value)));
}

export function parseReleaseTag(tag) {
  if (typeof tag !== "string" || tag.trim() !== tag)
    fail("invalid_release_tag");
  const match = TAG_PATTERN.exec(tag);
  if (!match) fail("invalid_release_tag", tag);
  const version = match[1];
  return Object.freeze({ tag, version, prerelease: version.includes("-") });
}

export function releasePayloadNames(version) {
  if (typeof version !== "string" || !VERSION_PATTERN.test(version)) {
    fail("invalid_release_version", version);
  }
  return Object.freeze([
    `axial-linux-amd64-${version}`,
    `axial-linux-amd64-${version}.tar.gz`,
    `axial-windows-amd64-${version}.exe`,
    `axial-windows-amd64-${version}.zip`,
    `axial-macos-amd64-${version}.dmg`,
    `axial-macos-amd64-${version}.tar.gz`,
    `axial-macos-arm64-${version}.dmg`,
    `axial-macos-arm64-${version}.tar.gz`,
  ]);
}

export function releaseHandoffLayout(version) {
  const payloads = releasePayloadNames(version);
  return Object.freeze([
    Object.freeze({
      producer: "linux-amd64",
      payloads: Object.freeze(payloads.slice(0, 2)),
    }),
    Object.freeze({
      producer: "windows-amd64",
      payloads: Object.freeze(payloads.slice(2, 4)),
    }),
    Object.freeze({
      producer: "macos-amd64",
      payloads: Object.freeze(payloads.slice(4, 6)),
    }),
    Object.freeze({
      producer: "macos-arm64",
      payloads: Object.freeze(payloads.slice(6, 8)),
    }),
  ]);
}

export function releaseAssetNames(version) {
  return Object.freeze(
    releasePayloadNames(version).flatMap((payload) => [
      payload,
      `${payload}.sha256`,
    ]),
  );
}

function validDate(value) {
  if (!DATE_PATTERN.test(value)) return false;
  const [year, month, day] = value.split("-").map(Number);
  const date = new Date(Date.UTC(year, month - 1, day));
  return (
    date.getUTCFullYear() === year &&
    date.getUTCMonth() === month - 1 &&
    date.getUTCDate() === day
  );
}

function compareVersions(left, right) {
  const parse = (version) => {
    const [core, prerelease] = version.split("-");
    const numbers = core.split(".").map(Number);
    if (!prerelease) return [...numbers, 4, 0];
    const [channel, sequence] = prerelease.split(".");
    const rank = { dev: 0, alpha: 1, beta: 2, rc: 3 }[channel];
    return [...numbers, rank, Number(sequence)];
  };
  const leftParts = parse(left);
  const rightParts = parse(right);
  for (let index = 0; index < leftParts.length; index += 1) {
    if (leftParts[index] !== rightParts[index])
      return leftParts[index] - rightParts[index];
  }
  return 0;
}

export function extractChangelogRelease(source, version) {
  if (typeof source !== "string" || source.includes("\r"))
    fail("invalid_changelog_encoding");
  if (typeof version !== "string" || !VERSION_PATTERN.test(version)) {
    fail("invalid_release_version", version);
  }

  const headings = [];
  for (const match of source.matchAll(/^## .*$/gm)) {
    const heading = match[0];
    const start = match.index;
    if (heading === "## [Unreleased]") {
      headings.push({ kind: "unreleased", start, end: start + heading.length });
      continue;
    }
    const release = /^## \[([^\]]+)] - (\d{4}-\d{2}-\d{2})$/.exec(heading);
    if (!release) fail("invalid_changelog_heading", heading);
    if (!VERSION_PATTERN.test(release[1]))
      fail("invalid_changelog_version", release[1]);
    if (!validDate(release[2])) fail("invalid_changelog_date", release[2]);
    headings.push({
      kind: "release",
      version: release[1],
      date: release[2],
      start,
      end: start + heading.length,
    });
  }

  const unreleased = headings.filter(
    (heading) => heading.kind === "unreleased",
  );
  if (unreleased.length !== 1)
    fail("invalid_unreleased_section_count", unreleased.length);
  if (headings[0]?.kind !== "unreleased") fail("unreleased_section_not_first");

  const seen = new Set();
  const released = headings.filter((candidate) => candidate.kind === "release");
  for (const [index, heading] of released.entries()) {
    if (seen.has(heading.version))
      fail("duplicate_changelog_release", heading.version);
    seen.add(heading.version);
    if (index > 0) {
      const previous = released[index - 1];
      if (heading.date > previous.date)
        fail("changelog_date_order", heading.version);
      if (compareVersions(previous.version, heading.version) <= 0) {
        fail("changelog_version_order", heading.version);
      }
    }
  }

  const target = headings.find(
    (heading) => heading.kind === "release" && heading.version === version,
  );
  if (!target) fail("missing_changelog_release", version);
  const position = headings.indexOf(target);
  if (position !== 1) fail("release_not_latest_changelog_section", version);
  const sectionEnd = headings[position + 1]?.start ?? source.length;
  const notes = source.slice(target.end, sectionEnd).trim();
  if (!notes) fail("empty_changelog_release", version);
  if (Buffer.byteLength(notes) > MAX_NOTES_BYTES)
    fail("changelog_release_notes_too_large");

  return Object.freeze({ version, date: target.date, notes: `${notes}\n` });
}

function workspacePackagesFromMetadata(metadata) {
  if (!metadata || typeof metadata !== "object" || Array.isArray(metadata)) {
    fail("invalid_cargo_metadata");
  }
  if (
    !Array.isArray(metadata.workspace_members) ||
    metadata.workspace_members.length === 0
  ) {
    fail("invalid_cargo_workspace_members");
  }
  if (!Array.isArray(metadata.packages)) fail("invalid_cargo_packages");

  const packages = new Map();
  for (const pkg of metadata.packages) {
    if (!pkg || typeof pkg.id !== "string" || typeof pkg.version !== "string") {
      fail("invalid_cargo_package");
    }
    if (packages.has(pkg.id)) fail("duplicate_cargo_package", pkg.id);
    packages.set(pkg.id, pkg);
  }

  const members = [];
  for (const member of metadata.workspace_members) {
    if (typeof member !== "string") fail("invalid_cargo_workspace_member");
    const pkg = packages.get(member);
    if (!pkg) fail("missing_cargo_workspace_package", member);
    members.push(pkg);
  }
  return members;
}

export function workspaceVersionFromMetadata(metadata) {
  const versions = new Set();
  for (const pkg of workspacePackagesFromMetadata(metadata)) {
    if (!VERSION_PATTERN.test(pkg.version))
      fail("invalid_cargo_release_version", pkg.version);
    versions.add(pkg.version);
  }
  if (versions.size !== 1)
    fail("mixed_cargo_workspace_versions", [...versions].join(","));
  return [...versions][0];
}

async function readBoundedRegularBytes(file, maximumBytes, label) {
  const linkedInfo = await lstat(file).catch((error) => {
    fail(`missing_${label}`, error?.code ?? "unreadable");
  });
  if (linkedInfo.isSymbolicLink() || !linkedInfo.isFile())
    fail(`invalid_${label}_type`);
  if (linkedInfo.size > maximumBytes)
    fail(`${label}_too_large`, linkedInfo.size);
  let handle;
  try {
    handle = await open(file, "r");
    const info = await handle.stat();
    if (!info.isFile()) fail(`invalid_${label}_type`);
    if (info.size > maximumBytes) fail(`${label}_too_large`, info.size);
    const buffer = Buffer.alloc(maximumBytes + 1);
    let length = 0;
    for (;;) {
      const result = await handle.read(
        buffer,
        length,
        buffer.byteLength - length,
        null,
      );
      if (result.bytesRead === 0) break;
      length += result.bytesRead;
      if (length > maximumBytes) fail(`${label}_too_large`, length);
    }
    return buffer.subarray(0, length);
  } catch (error) {
    if (error instanceof ReleaseContractError) throw error;
    fail(`${label}_read_failed`, error?.code ?? "unreadable");
  } finally {
    await handle?.close().catch(() => {});
  }
}

async function readBoundedRegularFile(file, maximumBytes, label) {
  const bytes = await readBoundedRegularBytes(file, maximumBytes, label);
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    fail(`invalid_${label}_encoding`);
  }
}

async function readDirectory(directory, label, maximumEntries) {
  const entries = [];
  try {
    const handle = await opendir(directory);
    for await (const entry of handle) {
      entries.push(entry.name);
      if (entries.length > maximumEntries) {
        fail(`${label}_too_large`, entries.length);
      }
    }
  } catch (error) {
    if (error instanceof ReleaseContractError) throw error;
    fail(`${label}_read_failed`, error?.code ?? "unreadable");
  }
  return entries;
}

export async function runCargoMetadata(repositoryRoot) {
  let stdout;
  try {
    ({ stdout } = await execFileAsync(
      "cargo",
      ["metadata", "--locked", "--no-deps", "--format-version", "1"],
      {
        cwd: repositoryRoot,
        encoding: "utf8",
        maxBuffer: MAX_METADATA_BYTES,
        timeout: 120_000,
        windowsHide: true,
      },
    ));
  } catch (error) {
    fail("cargo_metadata_failed", error?.code ?? error?.message ?? "unknown");
  }
  try {
    return JSON.parse(stdout);
  } catch {
    fail("invalid_cargo_metadata_json");
  }
}

export async function runGitHead(repositoryRoot) {
  let stdout;
  try {
    ({ stdout } = await execFileAsync("git", ["rev-parse", "HEAD"], {
      cwd: repositoryRoot,
      encoding: "utf8",
      maxBuffer: 1024,
      timeout: 10_000,
      windowsHide: true,
    }));
  } catch (error) {
    fail("git_head_failed", error?.code ?? error?.message ?? "unknown");
  }
  const sourceSha = stdout.trim();
  if (!SOURCE_SHA_PATTERN.test(sourceSha)) fail("invalid_git_head");
  return sourceSha;
}

export async function verifyReleaseSource({
  tag,
  repositoryRoot = process.cwd(),
  cargoMetadata = runCargoMetadata,
} = {}) {
  const identity = parseReleaseTag(tag);
  const root = path.resolve(repositoryRoot);
  const metadata = await cargoMetadata(root);
  const cargoVersion = workspaceVersionFromMetadata(metadata);
  if (cargoVersion !== identity.version) {
    fail("release_version_mismatch", `${identity.version} != ${cargoVersion}`);
  }

  if (path.resolve(metadata.workspace_root ?? "") !== root)
    fail("cargo_workspace_root_mismatch");
  for (const pkg of workspacePackagesFromMetadata(metadata)) {
    if (typeof pkg.manifest_path !== "string")
      fail("invalid_cargo_manifest_path", pkg.id);
    const manifestPath = path.resolve(pkg.manifest_path);
    const relative = path.relative(root, manifestPath);
    if (
      !relative ||
      relative.startsWith(`..${path.sep}`) ||
      path.isAbsolute(relative)
    ) {
      fail("cargo_manifest_outside_workspace", pkg.id);
    }
    const manifest = await readBoundedRegularFile(
      manifestPath,
      MAX_SOURCE_BYTES,
      "cargo_manifest",
    );
    const packageSections = [...manifest.matchAll(/^\[package]\s*$/gm)];
    if (packageSections.length !== 1)
      fail("invalid_cargo_package_section", relative);
    const packageStart =
      packageSections[0].index + packageSections[0][0].length;
    const nextSection = manifest.slice(packageStart).search(/^\[/m);
    const packageSource = manifest.slice(
      packageStart,
      nextSection < 0 ? manifest.length : packageStart + nextSection,
    );
    const declarations = packageSource
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => /^version(?:\.workspace)?\s*=/.test(line));
    if (
      declarations.length !== 1 ||
      declarations[0] !== "version.workspace = true"
    ) {
      fail("cargo_version_not_inherited", relative);
    }
  }

  const tauriSource = await readBoundedRegularFile(
    path.join(root, "apps", "desktop", "tauri.conf.json"),
    MAX_SOURCE_BYTES,
    "tauri_config",
  );
  let tauri;
  try {
    tauri = JSON.parse(tauriSource);
  } catch {
    fail("invalid_tauri_config_json");
  }
  if (!tauri || typeof tauri !== "object" || Array.isArray(tauri)) {
    fail("invalid_tauri_config");
  }
  if (Object.hasOwn(tauri, "version")) fail("authored_tauri_version");

  const changelog = await readBoundedRegularFile(
    path.join(root, "CHANGELOG.md"),
    MAX_SOURCE_BYTES,
    "changelog",
  );
  const release = extractChangelogRelease(changelog, identity.version);
  return Object.freeze({
    ...identity,
    date: release.date,
    notes: release.notes,
  });
}

async function sha256File(file, maximumBytes) {
  const digest = createHash("sha256");
  let bytes = 0;
  for await (const chunk of createReadStream(file)) {
    bytes += chunk.byteLength;
    if (bytes > maximumBytes) fail("release_asset_too_large");
    digest.update(chunk);
  }
  return Object.freeze({ bytes, sha256: digest.digest("hex") });
}

async function regularAssetInfo(file, name, { payload }) {
  let info;
  try {
    info = await lstat(file);
  } catch (error) {
    fail("missing_release_asset", `${name} (${error?.code ?? "unreadable"})`);
  }
  if (info.isSymbolicLink()) fail("symlink_release_asset", name);
  if (!info.isFile()) fail("non_regular_release_asset", name);
  if (payload && info.size === 0) fail("empty_release_payload", name);
  if (info.size > MAX_ASSET_BYTES) fail("release_asset_too_large", name);
  return info;
}

export async function verifyReleaseAssets({ tag, assetsDirectory } = {}) {
  const { version } = parseReleaseTag(tag);
  if (typeof assetsDirectory !== "string" || assetsDirectory.length === 0) {
    fail("missing_assets_directory");
  }
  const directory = path.resolve(assetsDirectory);
  const directoryInfo = await lstat(directory).catch((error) => {
    fail("missing_assets_directory", error?.code ?? "unreadable");
  });
  if (directoryInfo.isSymbolicLink() || !directoryInfo.isDirectory()) {
    fail("invalid_assets_directory");
  }

  const entries = await readDirectory(
    directory,
    "release_handoffs",
    MAX_DIRECTORY_ENTRIES,
  );
  const layout = releaseHandoffLayout(version);
  const expectedProducers = new Set(layout.map(({ producer }) => producer));
  const missingProducers = layout
    .map(({ producer }) => producer)
    .filter((producer) => !entries.includes(producer));
  const extraProducers = entries.filter(
    (producer) => !expectedProducers.has(producer),
  );
  if (missingProducers.length > 0) {
    fail("missing_release_handoffs", missingProducers.join(","));
  }
  if (extraProducers.length > 0)
    fail("extra_release_handoffs", extraProducers.join(","));

  const expected = releaseAssetNames(version);
  const records = new Map();
  for (const { producer, payloads } of layout) {
    const producerPath = path.join(directory, producer);
    const producerInfo = await lstat(producerPath).catch((error) => {
      fail(
        "release_handoff_unreadable",
        `${producer}:${error?.code ?? "unknown"}`,
      );
    });
    if (producerInfo.isSymbolicLink() || !producerInfo.isDirectory()) {
      fail("invalid_release_handoff", producer);
    }
    const producerEntries = await readDirectory(
      producerPath,
      "release_handoff",
      8,
    );
    const producerExpected = payloads.flatMap((payload) => [
      payload,
      `${payload}.sha256`,
    ]);
    const producerExpectedSet = new Set(producerExpected);
    const missing = producerExpected.filter(
      (name) => !producerEntries.includes(name),
    );
    const extra = producerEntries.filter(
      (name) => !producerExpectedSet.has(name),
    );
    if (missing.length > 0)
      fail("missing_release_assets", `${producer}:${missing.join(",")}`);
    if (extra.length > 0)
      fail("extra_release_assets", `${producer}:${extra.join(",")}`);

    for (const payload of payloads) {
      const payloadPath = path.join(producerPath, payload);
      const payloadInfo = await regularAssetInfo(payloadPath, payload, {
        payload: true,
      });
      let payloadHash;
      try {
        payloadHash = await sha256File(payloadPath, MAX_ASSET_BYTES);
      } catch (error) {
        if (error instanceof ReleaseContractError) throw error;
        fail(
          "release_asset_read_failed",
          `${payload} (${error?.code ?? "unreadable"})`,
        );
      }
      if (payloadHash.bytes !== payloadInfo.size)
        fail("release_asset_changed", payload);
      const payloadDigest = payloadHash.sha256;
      const sidecar = `${payload}.sha256`;
      const sidecarPath = path.join(producerPath, sidecar);
      const sidecarInfo = await regularAssetInfo(sidecarPath, sidecar, {
        payload: false,
      });
      const sidecarBytes = await readBoundedRegularBytes(
        sidecarPath,
        256,
        "release_sidecar",
      );
      if (sidecarBytes.byteLength !== sidecarInfo.size)
        fail("release_asset_changed", sidecar);
      const expectedSidecar = `${payloadDigest}  ${payload}\n`;
      if (sidecarBytes.toString("utf8") !== expectedSidecar) {
        const text = sidecarBytes.toString("utf8");
        const match = /^([0-9a-f]{64})  ([^\r\n]+)\n$/.exec(text);
        if (!match || !SHA256_PATTERN.test(match[1]))
          fail("malformed_release_sidecar", sidecar);
        if (match[2] !== payload)
          fail("wrong_release_sidecar_basename", sidecar);
        fail("wrong_release_sidecar_digest", sidecar);
      }
      if (records.has(payload) || records.has(sidecar))
        fail("duplicate_release_asset_name");
      records.set(payload, {
        name: payload,
        source: payloadPath,
        producer,
        size: payloadInfo.size,
        sha256: payloadDigest,
      });
      records.set(sidecar, {
        name: sidecar,
        source: sidecarPath,
        producer,
        size: sidecarInfo.size,
        sha256: createHash("sha256").update(sidecarBytes).digest("hex"),
      });
    }
  }

  return Object.freeze({
    version,
    directory,
    assets: freezeArray(expected.map((name) => records.get(name))),
  });
}

function requireSourceSha(sourceSha) {
  if (typeof sourceSha !== "string" || !SOURCE_SHA_PATTERN.test(sourceSha)) {
    fail("invalid_source_sha", sourceSha);
  }
  return sourceSha;
}

function sourceReceipt(source, sourceSha, publisherSha256) {
  return {
    schema: SOURCE_RECEIPT_SCHEMA,
    version: source.version,
    tag: source.tag,
    prerelease: source.prerelease,
    source_sha: requireSourceSha(sourceSha),
    publisher_sha256: publisherSha256,
    notes: source.notes,
  };
}

function canonicalReceipt(document) {
  return `${JSON.stringify(document, null, 2)}\n`;
}

function validateSourceReceipt(document, { tag, sourceSha, publisherSha256 }) {
  if (!document || typeof document !== "object" || Array.isArray(document)) {
    fail("invalid_source_receipt");
  }
  const exactKeys = [
    "schema",
    "version",
    "tag",
    "prerelease",
    "source_sha",
    "publisher_sha256",
    "notes",
  ];
  if (
    Object.keys(document).length !== exactKeys.length ||
    exactKeys.some((key, index) => Object.keys(document)[index] !== key)
  ) {
    fail("invalid_source_receipt_shape");
  }
  const identity = parseReleaseTag(tag);
  if (document.schema !== SOURCE_RECEIPT_SCHEMA)
    fail("invalid_source_receipt_schema");
  if (document.tag !== tag || document.version !== identity.version) {
    fail("source_receipt_tag_mismatch");
  }
  if (document.prerelease !== identity.prerelease)
    fail("source_receipt_channel_mismatch");
  if (document.source_sha !== requireSourceSha(sourceSha))
    fail("source_receipt_sha_mismatch");
  if (document.publisher_sha256 !== publisherSha256)
    fail("source_receipt_publisher_mismatch");
  if (
    typeof document.notes !== "string" ||
    !document.notes.endsWith("\n") ||
    !document.notes.trim() ||
    document.notes.includes("\0") ||
    document.notes.includes("\r") ||
    Buffer.byteLength(document.notes) > MAX_NOTES_BYTES
  ) {
    fail("invalid_source_receipt_notes");
  }
  return Object.freeze({ ...document });
}

export async function stagePublication({
  tag,
  sourceSha,
  outputDirectory,
  repositoryRoot = process.cwd(),
  cargoMetadata = runCargoMetadata,
  gitHead = runGitHead,
  publisherPath = MODULE_PATH,
} = {}) {
  requireSourceSha(sourceSha);
  if (typeof outputDirectory !== "string" || !outputDirectory) {
    fail("missing_publication_output");
  }
  const source = await verifyReleaseSource({
    tag,
    repositoryRoot,
    cargoMetadata,
  });
  const actualSha = await gitHead(path.resolve(repositoryRoot));
  if (actualSha !== sourceSha) fail("source_sha_not_checked_out");
  const publisherBytes = await readBoundedRegularFile(
    path.resolve(publisherPath),
    MAX_SOURCE_BYTES,
    "publisher",
  );
  const publisherSha256 = createHash("sha256")
    .update(publisherBytes)
    .digest("hex");
  const receipt = sourceReceipt(source, sourceSha, publisherSha256);
  const destination = path.resolve(outputDirectory);
  const existing = await lstat(destination).catch((error) => {
    if (error?.code === "ENOENT") return null;
    fail("publication_output_unreadable", error?.code ?? "unknown");
  });
  if (existing) fail("publication_output_exists");
  const parent = path.dirname(destination);
  const parentInfo = await lstat(parent).catch((error) => {
    fail("publication_output_parent_missing", error?.code ?? "unknown");
  });
  if (parentInfo.isSymbolicLink() || !parentInfo.isDirectory()) {
    fail("invalid_publication_output_parent");
  }
  const temporary = path.join(
    parent,
    `.${path.basename(destination)}.tmp-${process.pid}-${Date.now()}`,
  );
  try {
    await mkdir(temporary, { mode: 0o700 });
    await writeFile(
      path.join(temporary, "release-contract.mjs"),
      publisherBytes,
      {
        encoding: "utf8",
        flag: "wx",
        mode: 0o700,
      },
    );
    await writeFile(
      path.join(temporary, "source.json"),
      canonicalReceipt(receipt),
      {
        encoding: "utf8",
        flag: "wx",
        mode: 0o600,
      },
    );
    await rename(temporary, destination);
  } finally {
    await rm(temporary, { recursive: true, force: true }).catch(() => {});
  }
  return Object.freeze({ ...receipt, outputDirectory: destination });
}

export async function readSourceReceipt({
  receiptFile,
  tag,
  sourceSha,
  publisherPath = MODULE_PATH,
} = {}) {
  if (typeof receiptFile !== "string" || !receiptFile)
    fail("missing_source_receipt");
  const receiptSource = await readBoundedRegularFile(
    path.resolve(receiptFile),
    MAX_RECEIPT_BYTES,
    "source_receipt",
  );
  const publisherSource = await readBoundedRegularFile(
    path.resolve(publisherPath),
    MAX_SOURCE_BYTES,
    "publisher",
  );
  const publisherSha256 = createHash("sha256")
    .update(publisherSource)
    .digest("hex");
  let parsed;
  try {
    parsed = JSON.parse(receiptSource);
  } catch {
    fail("invalid_source_receipt_json");
  }
  const receipt = validateSourceReceipt(parsed, {
    tag,
    sourceSha,
    publisherSha256,
  });
  if (receiptSource !== canonicalReceipt(receipt))
    fail("noncanonical_source_receipt");
  return receipt;
}

function githubEnvironment(environment) {
  const token = environment?.GITHUB_TOKEN;
  if (
    typeof token !== "string" ||
    token.length === 0 ||
    token.trim() !== token
  ) {
    fail("missing_github_token");
  }
  const repository = environment?.GITHUB_REPOSITORY;
  const match = /^([A-Za-z0-9_.-]+)\/([A-Za-z0-9_.-]+)$/.exec(repository ?? "");
  if (!match) fail("invalid_github_repository");
  const runId = environment?.GITHUB_RUN_ID;
  const runAttempt = environment?.GITHUB_RUN_ATTEMPT;
  if (!RELEASE_ID_PATTERN.test(runId ?? "") || String(runId).length > 20) {
    fail("invalid_github_run_id");
  }
  if (
    !RELEASE_ID_PATTERN.test(runAttempt ?? "") ||
    String(runAttempt).length > 10
  ) {
    fail("invalid_github_run_attempt");
  }
  const api = environment?.GITHUB_API_URL ?? "https://api.github.com";
  if (api !== "https://api.github.com") fail("invalid_github_api_url");
  return Object.freeze({
    token,
    owner: match[1],
    repository: match[2],
    runId,
    runAttempt,
    api,
  });
}

async function responseText(response) {
  const declared = Number(response.headers?.get?.("content-length"));
  if (Number.isFinite(declared) && declared > MAX_API_BYTES)
    fail("github_response_too_large");
  if (response.body?.getReader) {
    const reader = response.body.getReader();
    const chunks = [];
    let length = 0;
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      length += value.byteLength;
      if (length > MAX_API_BYTES) {
        await reader.cancel();
        fail("github_response_too_large");
      }
      chunks.push(value);
    }
    const bytes = new Uint8Array(length);
    let offset = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, offset);
      offset += chunk.byteLength;
    }
    try {
      return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    } catch {
      fail("invalid_github_response_encoding");
    }
  }
  const text = await response.text();
  if (Buffer.byteLength(text) > MAX_API_BYTES)
    fail("github_response_too_large");
  return text;
}

async function parseGitHubResponse(response) {
  const text = await responseText(response);
  if (!text) return null;
  try {
    return JSON.parse(text);
  } catch {
    fail("invalid_github_response_json");
  }
}

function githubClient({
  environment = process.env,
  fetchImpl = globalThis.fetch,
  signalFactory = AbortSignal.timeout,
} = {}) {
  if (typeof fetchImpl !== "function") fail("missing_fetch_implementation");
  if (typeof signalFactory !== "function") fail("missing_signal_factory");
  const config = githubEnvironment(environment);
  const root = `/repos/${encodeURIComponent(config.owner)}/${encodeURIComponent(config.repository)}`;
  const request = async (
    pathname,
    { method = "GET", body, statuses = [200] } = {},
  ) => {
    let response;
    try {
      response = await fetchImpl(`${config.api}${root}${pathname}`, {
        method,
        redirect: "error",
        signal: signalFactory(30_000),
        headers: {
          accept: "application/vnd.github+json",
          authorization: `Bearer ${config.token}`,
          "user-agent": "axial-release-contract",
          "x-github-api-version": "2022-11-28",
          ...(body === undefined ? {} : { "content-type": "application/json" }),
        },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });
    } catch (error) {
      fail("github_request_failed", error?.code ?? error?.message ?? "network");
    }
    if (!statuses.includes(response.status)) {
      fail("github_request_rejected", `${method} ${response.status}`);
    }
    return Object.freeze({
      status: response.status,
      body: await parseGitHubResponse(response),
    });
  };
  return Object.freeze({ config, request, signalFactory });
}

function requireReleaseIdentity(release, { id, tag, draft }) {
  if (!release || typeof release !== "object" || Array.isArray(release)) {
    fail("invalid_github_release");
  }
  if (!Number.isSafeInteger(release.id) || release.id <= 0)
    fail("invalid_github_release_id");
  if (id !== undefined && release.id !== id) fail("github_release_id_mismatch");
  if (release.tag_name !== tag) fail("github_release_tag_mismatch");
  if (typeof release.draft !== "boolean") fail("invalid_github_release_draft");
  if (draft !== undefined && release.draft !== draft)
    fail("github_release_draft_mismatch");
  return release;
}

function verifyRemoteAssetSubset(release, localAssets, prerelease) {
  if (release.prerelease !== prerelease)
    fail("github_release_channel_mismatch");
  if (!Array.isArray(release.assets)) fail("invalid_github_release_assets");
  const expected = new Map(localAssets.map((asset) => [asset.name, asset]));
  const seen = new Set();
  const ids = new Set();
  for (const asset of release.assets) {
    if (!asset || typeof asset !== "object" || Array.isArray(asset)) {
      fail("invalid_github_release_asset");
    }
    if (!Number.isSafeInteger(asset.id) || asset.id <= 0 || ids.has(asset.id)) {
      fail("invalid_github_release_asset_id", asset.name);
    }
    ids.add(asset.id);
    if (typeof asset.name !== "string" || seen.has(asset.name)) {
      fail("duplicate_github_release_asset", asset.name);
    }
    seen.add(asset.name);
    const local = expected.get(asset.name);
    if (!local) fail("extra_github_release_asset", asset.name);
    if (!Number.isSafeInteger(asset.size) || asset.size !== local.size) {
      fail("github_release_asset_size_mismatch", asset.name);
    }
    if (asset.state !== "uploaded")
      fail("github_release_asset_not_uploaded", asset.name);
    if (asset.digest !== `sha256:${local.sha256}`) {
      fail("github_release_asset_digest_mismatch", asset.name);
    }
  }
  return seen;
}

function verifyRemoteAssets(release, localAssets, prerelease) {
  const seen = verifyRemoteAssetSubset(release, localAssets, prerelease);
  if (seen.size !== localAssets.length)
    fail("github_release_asset_count_mismatch");
}

function stagingName({ tag, runId, runAttempt }) {
  return `Axial ${tag} staging [${runId}.${runAttempt}]`;
}

function stagingHeader({ tag, sourceSha, runId, runAttempt }) {
  return `<!-- axial-release-automation:v1 run=${runId} attempt=${runAttempt} source=${sourceSha} tag=${tag} -->`;
}

function publicationMarker(identity, receipt, config) {
  const marker = Object.freeze({
    tag: identity.tag,
    sourceSha: receipt.source_sha,
    runId: config.runId,
    runAttempt: config.runAttempt,
  });
  return Object.freeze({
    ...marker,
    name: stagingName(marker),
    body: `${stagingHeader(marker)}\n\n${receipt.notes}`,
  });
}

function automationMarker(release, tag) {
  if (typeof release.name !== "string" || typeof release.body !== "string")
    return null;
  const match =
    /^<!-- axial-release-automation:v1 run=([1-9]\d*) attempt=([1-9]\d*) source=([0-9a-f]{40}) tag=(v[^\s]+) -->\n\n/.exec(
      release.body,
    );
  if (!match) return null;
  const marker = {
    tag: match[4],
    sourceSha: match[3],
    runId: match[1],
    runAttempt: match[2],
  };
  if (
    marker.tag !== tag ||
    marker.runId.length > 20 ||
    marker.runAttempt.length > 10 ||
    stagingName(marker) !== release.name ||
    stagingHeader(marker) !== release.body.slice(0, match[0].length - 2)
  ) {
    return null;
  }
  return Object.freeze(marker);
}

function requireOwnedDraft(release, { id, tag, marker, prerelease }) {
  requireReleaseIdentity(release, { id, tag, draft: true });
  if (release.prerelease !== prerelease)
    fail("github_release_channel_mismatch");
  if (release.name !== marker.name || release.body !== marker.body) {
    fail("github_release_ownership_mismatch");
  }
  return release;
}

async function exactTagReleases(request, tag) {
  const matches = [];
  const ids = new Set();
  for (let page = 1; page <= 20; page += 1) {
    const response = await request(`/releases?per_page=100&page=${page}`);
    if (!Array.isArray(response.body)) fail("invalid_github_release_list");
    for (const release of response.body) {
      if (!release || typeof release !== "object" || Array.isArray(release)) {
        fail("invalid_github_release_list_entry");
      }
      if (release.tag_name !== tag) continue;
      requireReleaseIdentity(release, { tag });
      if (ids.has(release.id)) fail("duplicate_github_release_id", release.id);
      ids.add(release.id);
      matches.push(release);
    }
    if (response.body.length < 100) return matches;
  }
  fail("github_release_list_too_large");
}

async function verifyTagRef(request, { tag, sourceSha }) {
  const reference = await request(`/git/ref/tags/${encodeURIComponent(tag)}`);
  if (
    reference.body?.ref !== `refs/tags/${tag}` ||
    !reference.body.object ||
    typeof reference.body.object !== "object"
  ) {
    fail("invalid_github_tag_ref");
  }
  let { type, sha } = reference.body.object;
  const seen = new Set();
  for (let depth = 0; depth <= 8; depth += 1) {
    if (!SOURCE_SHA_PATTERN.test(sha ?? ""))
      fail("invalid_github_tag_object_sha");
    if (type === "commit") {
      if (sha !== sourceSha) fail("github_tag_commit_mismatch");
      return sha;
    }
    if (type !== "tag") fail("invalid_github_tag_object_type", type);
    if (depth === 8) fail("github_tag_depth_exceeded");
    if (seen.has(sha)) fail("github_tag_cycle");
    seen.add(sha);
    const tagObject = await request(`/git/tags/${sha}`);
    if (
      tagObject.body?.sha !== sha ||
      !tagObject.body.object ||
      typeof tagObject.body.object !== "object"
    ) {
      fail("invalid_github_tag_object");
    }
    ({ type, sha } = tagObject.body.object);
  }
  fail("github_tag_depth_exceeded");
}

function uploadBase(uploadUrl, releaseId, config) {
  if (typeof uploadUrl !== "string" || !uploadUrl.endsWith("{?name,label}")) {
    fail("invalid_github_upload_url");
  }
  let parsed;
  try {
    parsed = new URL(uploadUrl.slice(0, -"{?name,label}".length));
  } catch {
    fail("invalid_github_upload_url");
  }
  const expectedPath = `/repos/${encodeURIComponent(config.owner)}/${encodeURIComponent(config.repository)}/releases/${releaseId}/assets`;
  if (
    parsed.protocol !== "https:" ||
    parsed.origin !== "https://uploads.github.com" ||
    parsed.pathname !== expectedPath ||
    parsed.search ||
    parsed.hash ||
    parsed.username ||
    parsed.password
  ) {
    fail("invalid_github_upload_url");
  }
  return parsed.href;
}

function verifyRemoteAsset(remote, local) {
  if (!remote || typeof remote !== "object" || Array.isArray(remote)) {
    fail("invalid_github_release_asset");
  }
  if (remote.name !== local.name)
    fail("github_release_asset_name_mismatch", local.name);
  if (!Number.isSafeInteger(remote.size) || remote.size !== local.size) {
    fail("github_release_asset_size_mismatch", local.name);
  }
  if (remote.state !== "uploaded")
    fail("github_release_asset_not_uploaded", local.name);
  if (remote.digest !== `sha256:${local.sha256}`) {
    fail("github_release_asset_digest_mismatch", local.name);
  }
  return remote;
}

async function uploadAsset({
  fetchImpl,
  config,
  base,
  local,
  blobFactory,
  signalFactory,
}) {
  let blob;
  try {
    blob = await blobFactory(local.source, {
      type: "application/octet-stream",
    });
  } catch (error) {
    fail("release_asset_open_failed", error?.code ?? "unreadable");
  }
  if (!blob || blob.size !== local.size)
    fail("release_asset_changed", local.name);
  let response;
  try {
    response = await fetchImpl(
      `${base}?name=${encodeURIComponent(local.name)}`,
      {
        method: "POST",
        redirect: "error",
        signal: signalFactory(600_000),
        headers: {
          accept: "application/vnd.github+json",
          authorization: `Bearer ${config.token}`,
          "content-type": "application/octet-stream",
          "user-agent": "axial-release-contract",
          "x-github-api-version": "2022-11-28",
        },
        body: blob,
      },
    );
  } catch (error) {
    fail("github_upload_failed", error?.code ?? error?.message ?? "network");
  }
  if (response.status !== 201) fail("github_upload_rejected", response.status);
  return verifyRemoteAsset(await parseGitHubResponse(response), local);
}

function requirePublishedRelease(
  release,
  { id, identity, receipt, localAssets },
) {
  requireReleaseIdentity(release, { id, tag: identity.tag, draft: false });
  if (
    release.prerelease !== identity.prerelease ||
    release.name !== identity.tag ||
    release.body !== receipt.notes
  ) {
    fail("invalid_published_release");
  }
  verifyRemoteAssets(release, localAssets, identity.prerelease);
  return release;
}

async function settlePublicationFailure(
  request,
  { id, identity, receipt, marker, localAssets, publicationAttempted },
) {
  try {
    const current = await request(`/releases/${id}`);
    if (!current.body?.draft) {
      requirePublishedRelease(current.body, {
        id,
        identity,
        receipt,
        localAssets,
      });
      await verifyTagRef(request, {
        tag: identity.tag,
        sourceSha: receipt.source_sha,
      });
      return "published";
    }
    requireOwnedDraft(current.body, {
      id,
      tag: identity.tag,
      marker,
      prerelease: identity.prerelease,
    });
    if (!publicationAttempted) {
      verifyRemoteAssetSubset(current.body, localAssets, identity.prerelease);
      return "resumable";
    }
    verifyRemoteAssets(current.body, localAssets, identity.prerelease);
    await verifyTagRef(request, {
      tag: identity.tag,
      sourceSha: receipt.source_sha,
    });
    try {
      const retried = await request(`/releases/${id}`, {
        method: "PATCH",
        body: {
          tag_name: identity.tag,
          draft: false,
          name: identity.tag,
          body: receipt.notes,
        },
      });
      requirePublishedRelease(retried.body, {
        id,
        identity,
        receipt,
        localAssets,
      });
      await verifyTagRef(request, {
        tag: identity.tag,
        sourceSha: receipt.source_sha,
      });
      return "published";
    } catch {
      try {
        const final = await request(`/releases/${id}`);
        requirePublishedRelease(final.body, {
          id,
          identity,
          receipt,
          localAssets,
        });
        await verifyTagRef(request, {
          tag: identity.tag,
          sourceSha: receipt.source_sha,
        });
        return "published";
      } catch {
        return "indeterminate";
      }
    }
  } catch {
    return "indeterminate";
  }
}

export async function publishRelease({
  receiptFile,
  tag,
  sourceSha,
  assetsDirectory,
  environment = process.env,
  fetchImpl = globalThis.fetch,
  blobFactory = openAsBlob,
  signalFactory = AbortSignal.timeout,
  publisherPath = MODULE_PATH,
} = {}) {
  const identity = parseReleaseTag(tag);
  const receipt = await readSourceReceipt({
    receiptFile,
    tag,
    sourceSha,
    publisherPath,
  });
  const local = await verifyReleaseAssets({ tag, assetsDirectory });
  const { config, request } = githubClient({
    environment,
    fetchImpl,
    signalFactory,
  });

  await verifyTagRef(request, { tag, sourceSha: receipt.source_sha });
  const existing = await exactTagReleases(request, tag);
  if (existing.length > 1) fail("ambiguous_tag_releases", existing.length);
  if (existing.length === 1) {
    if (!existing[0].draft) {
      requirePublishedRelease(existing[0], {
        id: existing[0].id,
        identity,
        receipt,
        localAssets: local.assets,
      });
      await verifyTagRef(request, { tag, sourceSha: receipt.source_sha });
      return Object.freeze({
        releaseId: existing[0].id,
        tag,
        assetCount: local.assets.length,
        status: "already-published",
      });
    }
  }

  let ownedId;
  let marker;
  let publicationAttempted = false;
  try {
    let present;
    let draft;
    if (existing.length === 1) {
      const ownership = automationMarker(existing[0], tag);
      if (!ownership) fail("unowned_draft_exists", tag);
      if (ownership.sourceSha !== receipt.source_sha) {
        fail("stale_draft_source_mismatch");
      }
      marker = publicationMarker(identity, receipt, ownership);
      ownedId = existing[0].id;
      const current = await request(`/releases/${ownedId}`);
      draft = requireOwnedDraft(current.body, {
        id: ownedId,
        tag,
        marker,
        prerelease: identity.prerelease,
      });
      present = verifyRemoteAssetSubset(
        draft,
        local.assets,
        identity.prerelease,
      );
    } else {
      marker = publicationMarker(identity, receipt, config);
      const created = await request("/releases", {
        method: "POST",
        statuses: [201],
        body: {
          tag_name: tag,
          name: marker.name,
          body: marker.body,
          draft: true,
          prerelease: identity.prerelease,
          target_commitish: receipt.source_sha,
        },
      });
      draft = requireReleaseIdentity(created.body, { tag, draft: true });
      ownedId = draft.id;
      requireOwnedDraft(draft, {
        id: ownedId,
        tag,
        marker,
        prerelease: identity.prerelease,
      });
      if (!Array.isArray(draft.assets) || draft.assets.length !== 0) {
        fail("new_github_draft_not_empty");
      }
      present = new Set();
    }
    const base = uploadBase(draft.upload_url, ownedId, config);
    const uploadedIds = new Set();
    for (const asset of local.assets) {
      if (present.has(asset.name)) continue;
      const uploaded = await uploadAsset({
        fetchImpl,
        config,
        base,
        local: asset,
        blobFactory,
        signalFactory,
      });
      if (
        !Number.isSafeInteger(uploaded.id) ||
        uploaded.id <= 0 ||
        uploadedIds.has(uploaded.id)
      ) {
        fail("invalid_github_release_asset_id", asset.name);
      }
      uploadedIds.add(uploaded.id);
    }

    const settled = await request(`/releases/${ownedId}`);
    const settledDraft = requireOwnedDraft(settled.body, {
      id: ownedId,
      tag,
      marker,
      prerelease: identity.prerelease,
    });
    verifyRemoteAssets(settledDraft, local.assets, identity.prerelease);

    const exact = await exactTagReleases(request, tag);
    if (exact.length !== 1 || exact[0].id !== ownedId) {
      fail("release_finalization_race", exact.length);
    }
    requireOwnedDraft(exact[0], {
      id: ownedId,
      tag,
      marker,
      prerelease: identity.prerelease,
    });
    verifyRemoteAssets(exact[0], local.assets, identity.prerelease);
    await verifyTagRef(request, { tag, sourceSha: receipt.source_sha });

    publicationAttempted = true;
    const updated = await request(`/releases/${ownedId}`, {
      method: "PATCH",
      body: { tag_name: tag, draft: false, name: tag, body: receipt.notes },
    });
    requirePublishedRelease(updated.body, {
      id: ownedId,
      identity,
      receipt,
      localAssets: local.assets,
    });
    await verifyTagRef(request, { tag, sourceSha: receipt.source_sha });
    return Object.freeze({
      releaseId: ownedId,
      tag,
      assetCount: local.assets.length,
    });
  } catch (error) {
    if (ownedId !== undefined) {
      const settlement = await settlePublicationFailure(request, {
        id: ownedId,
        identity,
        receipt,
        marker,
        localAssets: local.assets,
        publicationAttempted,
      });
      if (settlement === "published") {
        return Object.freeze({
          releaseId: ownedId,
          tag,
          assetCount: local.assets.length,
          status: "published-after-ambiguous-response",
        });
      }
      if (settlement === "indeterminate") {
        fail(
          "release_settlement_indeterminate",
          error?.code ?? error?.message ?? "unknown",
        );
      }
    }
    throw error;
  }
}

function parseOptions(argv, allowed) {
  const options = Object.create(null);
  for (let index = 0; index < argv.length; index += 1) {
    const option = argv[index];
    if (!option.startsWith("--") || !allowed.has(option))
      fail("unknown_cli_option", option);
    if (Object.hasOwn(options, option)) fail("duplicate_cli_option", option);
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--"))
      fail("missing_cli_option_value", option);
    options[option] = value;
    index += 1;
  }
  for (const option of allowed) {
    if (!Object.hasOwn(options, option)) fail("missing_cli_option", option);
  }
  return options;
}

export async function runCli(argv = process.argv.slice(2)) {
  const [command, ...args] = argv;
  if (command === "verify-source") {
    const options = parseOptions(args, new Set(["--tag"]));
    return verifyReleaseSource({ tag: options["--tag"] });
  }
  if (command === "stage-publication") {
    const options = parseOptions(args, new Set(["--tag", "--sha", "--output"]));
    return stagePublication({
      tag: options["--tag"],
      sourceSha: options["--sha"],
      outputDirectory: options["--output"],
    });
  }
  if (command === "publish") {
    const options = parseOptions(
      args,
      new Set(["--receipt", "--tag", "--sha", "--assets"]),
    );
    return publishRelease({
      receiptFile: options["--receipt"],
      tag: options["--tag"],
      sourceSha: options["--sha"],
      assetsDirectory: options["--assets"],
    });
  }
  fail("unknown_cli_command", command ?? "");
}

if (isDirectInvocation(import.meta.url)) {
  runCli()
    .then((result) => {
      process.stdout.write(`${JSON.stringify(result)}\n`);
    })
    .catch((error) => {
      const message =
        error instanceof ReleaseContractError
          ? error.message
          : "unexpected_release_error";
      process.stderr.write(`release-contract: ${message}\n`);
      process.exitCode = 1;
    });
}

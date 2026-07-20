#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { lstat, readFile, realpath, stat } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { generatedAssetPaths, parseBrandManifest } from "./generate-icons.mjs";

const rootDir = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
);
const manifestPath = path.join(rootDir, "assets", "provenance.json");
const maximumManifestBytes = 128 * 1024;
const maximumAssetBytes = 4 * 1024 * 1024;
const hashPattern = /^[0-9a-f]{64}$/;
const revisionPattern = /^(?:[0-9a-f]{40}|[a-z0-9]+(?:-[a-z0-9]+)*)$/;
const idPattern = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;
const datePattern = /^\d{4}-\d{2}-\d{2}$/;
const generatorId = "tauri-cli-brand-mark-v1";

export class AssetProvenanceError extends Error {
  constructor(code, detail = "") {
    super(detail ? `${code}: ${detail}` : code);
    this.name = "AssetProvenanceError";
    this.code = code;
  }
}

function fail(code, detail) {
  throw new AssetProvenanceError(code, detail);
}

function requireExactKeys(value, expected, label) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    fail("invalid_provenance_manifest", `${label} must be an object`);
  }
  const actual = Object.keys(value).sort();
  const wanted = [...expected].sort();
  if (
    actual.length !== wanted.length ||
    actual.some((key, index) => key !== wanted[index])
  ) {
    fail("invalid_provenance_manifest", `${label} has unexpected fields`);
  }
}

function requireString(value, pattern, label, maximum = 1_024) {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    value.length > maximum ||
    !pattern.test(value)
  ) {
    fail("invalid_provenance_manifest", `${label} is invalid`);
  }
  return value;
}

function requireNullableString(value, pattern, label) {
  if (value === null) return null;
  return requireString(value, pattern, label, 2_048);
}

function requireRelativePath(value, label) {
  const relative = requireString(value, /^[A-Za-z0-9._/-]+$/, label, 512);
  if (
    path.isAbsolute(relative) ||
    relative
      .split("/")
      .some((part) => part === "" || part === "." || part === "..")
  ) {
    fail("invalid_provenance_manifest", `${label} escapes the repository`);
  }
  return relative;
}

function requireLocator(value, label, { nullable = false } = {}) {
  if (nullable && value === null) return null;
  return requireString(
    value,
    /^(?:https:\/\/|repository:\/\/|[A-Za-z0-9_])[A-Za-z0-9._~:/?#[\]@!$&'()*+,;=%-]*$/,
    label,
    2_048,
  );
}

export function parseProvenanceManifest(source) {
  if (
    typeof source !== "string" ||
    Buffer.byteLength(source) > maximumManifestBytes
  ) {
    fail(
      "invalid_provenance_manifest",
      `manifest exceeds ${maximumManifestBytes} bytes`,
    );
  }
  let manifest;
  try {
    manifest = JSON.parse(source);
  } catch {
    fail("invalid_provenance_manifest", "JSON could not be decoded");
  }
  requireExactKeys(manifest, ["assets", "schema_version"], "root");
  if (
    manifest.schema_version !== 1 ||
    !Array.isArray(manifest.assets) ||
    manifest.assets.length === 0 ||
    manifest.assets.length > 64
  ) {
    fail("invalid_provenance_manifest", "unsupported schema or asset count");
  }

  const ids = new Set();
  const paths = new Set();
  const generated = new Set();
  for (const [assetIndex, asset] of manifest.assets.entries()) {
    const label = `assets[${assetIndex}]`;
    requireExactKeys(
      asset,
      ["files", "id", "refresh", "rights", "source"],
      label,
    );
    requireString(asset.id, idPattern, `${label}.id`, 80);
    if (ids.has(asset.id)) fail("duplicate_provenance_id", asset.id);
    ids.add(asset.id);

    requireExactKeys(
      asset.source,
      ["kind", "locator", "revision"],
      `${label}.source`,
    );
    if (!new Set(["repository-owned", "upstream"]).has(asset.source.kind)) {
      fail("invalid_provenance_manifest", `${label}.source.kind`);
    }
    const sourceLocator = requireLocator(
      asset.source.locator,
      `${label}.source.locator`,
    );
    requireString(
      asset.source.revision,
      revisionPattern,
      `${label}.source.revision`,
      80,
    );
    if (
      asset.source.kind === "upstream" &&
      !sourceLocator.startsWith("https://")
    ) {
      fail("invalid_provenance_manifest", `${label}.source must be HTTPS`);
    }
    if (
      asset.source.kind === "repository-owned" &&
      !sourceLocator.startsWith("repository://")
    ) {
      fail(
        "invalid_provenance_manifest",
        `${label}.source must be repository-owned`,
      );
    }

    requireExactKeys(
      asset.rights,
      ["basis", "current_policy", "evidence", "reviewed_at"],
      `${label}.rights`,
    );
    requireString(
      asset.rights.basis,
      /^[a-z0-9]+(?:[.-][a-z0-9]+)*$/,
      `${label}.rights.basis`,
      80,
    );
    requireExactKeys(
      asset.rights.evidence,
      ["locator", "sha256"],
      `${label}.rights.evidence`,
    );
    const evidenceLocator = requireLocator(
      asset.rights.evidence.locator,
      `${label}.rights.evidence.locator`,
    );
    const evidenceHash = requireNullableString(
      asset.rights.evidence.sha256,
      hashPattern,
      `${label}.rights.evidence.sha256`,
    );
    requireNullableString(
      asset.rights.current_policy,
      /^https:\/\//,
      `${label}.rights.current_policy`,
    );
    requireString(
      asset.rights.reviewed_at,
      datePattern,
      `${label}.rights.reviewed_at`,
      10,
    );
    if (evidenceLocator.startsWith("https://") && evidenceHash === null) {
      fail(
        "invalid_provenance_manifest",
        `${label}.rights remote evidence must be hashed`,
      );
    }
    requireString(asset.refresh, /^[^\r\n]+$/, `${label}.refresh`, 512);

    if (
      !Array.isArray(asset.files) ||
      asset.files.length === 0 ||
      asset.files.length > 64
    ) {
      fail("invalid_provenance_manifest", `${label}.files is invalid`);
    }
    for (const [fileIndex, file] of asset.files.entries()) {
      const fileLabel = `${label}.files[${fileIndex}]`;
      requireExactKeys(
        file,
        ["generator", "locator", "mode", "path", "sha256", "upstream_sha256"],
        fileLabel,
      );
      const filePath = requireRelativePath(file.path, `${fileLabel}.path`);
      if (paths.has(filePath)) fail("duplicate_provenance_path", filePath);
      paths.add(filePath);
      if (file.mode !== "100644") fail("invalid_provenance_mode", filePath);
      const hash = requireNullableString(
        file.sha256,
        hashPattern,
        `${fileLabel}.sha256`,
      );
      const generator = requireNullableString(
        file.generator,
        /^[a-z0-9]+(?:-[a-z0-9]+)*$/,
        `${fileLabel}.generator`,
      );
      const locator = requireLocator(file.locator, `${fileLabel}.locator`, {
        nullable: true,
      });
      const upstreamHash = requireNullableString(
        file.upstream_sha256,
        hashPattern,
        `${fileLabel}.upstream_sha256`,
      );
      if (hash === null) fail("invalid_provenance_integrity", filePath);
      if (generator !== null) {
        if (
          generator !== generatorId ||
          locator !== null ||
          upstreamHash !== null
        ) {
          fail("invalid_provenance_generator", filePath);
        }
        generated.add(filePath);
      } else if ((locator === null) !== (upstreamHash === null)) {
        fail("invalid_provenance_upstream", filePath);
      }
    }
  }

  const expectedGenerated = [...generatedAssetPaths].sort();
  const actualGenerated = [...generated].sort();
  if (actualGenerated.join("\0") !== expectedGenerated.join("\0")) {
    fail("invalid_generated_asset_coverage", actualGenerated.join(","));
  }
  if (source !== `${JSON.stringify(manifest, null, 2)}\n`) {
    fail("noncanonical_provenance_manifest");
  }
  return Object.freeze({ manifest, paths: Object.freeze([...paths].sort()) });
}

function gitIndex(root) {
  const result = spawnSync("git", ["ls-files", "--stage", "-z"], {
    cwd: root,
    encoding: "buffer",
    maxBuffer: 4 * 1024 * 1024,
    timeout: 10_000,
  });
  if (result.error || result.status !== 0)
    fail(
      "git_index_unavailable",
      result.error?.message ?? String(result.status),
    );
  const modes = new Map();
  for (const record of result.stdout
    .toString("utf8")
    .split("\0")
    .filter(Boolean)) {
    const match = /^(\d{6}) [0-9a-f]+ ([0-3])\t(.+)$/.exec(record);
    if (!match || match[2] !== "0") fail("invalid_git_index_record", record);
    modes.set(match[3], match[1]);
  }
  return modes;
}

export function untrackedDistributableAssets(root) {
  const result = spawnSync(
    "git",
    [
      "ls-files",
      "--others",
      "--exclude-standard",
      "-z",
      "--",
      "assets",
      "apps/desktop/icons",
      "frontend/static",
    ],
    {
      cwd: root,
      encoding: "buffer",
      maxBuffer: 4 * 1024 * 1024,
      timeout: 10_000,
    },
  );
  if (result.error || result.status !== 0)
    fail(
      "git_index_unavailable",
      result.error?.message ?? String(result.status),
    );
  return distributableAssetInventory(
    result.stdout.toString("utf8").split("\0").filter(Boolean),
  );
}

export function distributableAssetInventory(indexedPaths) {
  return [...indexedPaths]
    .filter(
      (assetPath) =>
        assetPath === "assets/brand-mark.json" ||
        assetPath.startsWith("apps/desktop/icons/") ||
        assetPath.startsWith("frontend/static/fonts/") ||
        assetPath.startsWith("frontend/static/sounds/") ||
        /^frontend\/static\/.+\.(?:avif|gif|icns|ico|jpe?g|mp3|ogg|otf|png|svg|ttf|webp|woff2?)$/i.test(
          assetPath,
        ),
    )
    .sort();
}

export function assertBrandRevision(manifest, brand) {
  const brandOwner = manifest.assets.find(
    (asset) => asset.id === "axial-brand",
  );
  if (brandOwner?.source.revision !== `brand-mark-v${brand.design_revision}`) {
    fail("brand_revision_mismatch", String(brand.design_revision));
  }
}

function withinRoot(root, candidate) {
  return candidate === root || candidate.startsWith(`${root}${path.sep}`);
}

async function readTrackedAsset(root, relativePath) {
  const canonicalRoot = await realpath(root);
  const rootState = await stat(canonicalRoot);
  let current = canonicalRoot;
  for (const segment of relativePath.split("/").slice(0, -1)) {
    current = path.join(current, segment);
    const metadata = await lstat(current);
    if (metadata.isSymbolicLink() || !metadata.isDirectory())
      fail("symlink_asset_parent", relativePath);
    const canonical = await realpath(current);
    const currentState = await stat(canonical);
    if (
      !withinRoot(canonicalRoot, canonical) ||
      currentState.dev !== rootState.dev
    )
      fail("asset_parent_escape", relativePath);
  }
  const filePath = path.join(canonicalRoot, relativePath);
  const metadata = await lstat(filePath);
  if (metadata.isSymbolicLink() || !metadata.isFile())
    fail("invalid_asset_file", relativePath);
  if (metadata.size > maximumAssetBytes) fail("asset_too_large", relativePath);
  const bytes = await readFile(filePath);
  if (bytes.length !== metadata.size)
    fail("asset_changed_during_read", relativePath);
  return Object.freeze({
    bytes,
    sha256: createHash("sha256").update(bytes).digest("hex"),
  });
}

export async function verifyAssetProvenance({
  root = rootDir,
  indexedModes = null,
} = {}) {
  const canonicalRoot = await realpath(root);
  if (canonicalRoot !== rootDir) fail("invalid_repository_root", canonicalRoot);
  const modes = indexedModes ?? gitIndex(root);
  const untracked = untrackedDistributableAssets(root);
  if (untracked.length > 0)
    fail("untracked_distributable_asset", untracked.join(","));
  if (modes.get("assets/provenance.json") !== "100644")
    fail("invalid_provenance_mode", "assets/provenance.json");
  const manifestBytes = await readTrackedAsset(root, "assets/provenance.json");
  const parsed = parseProvenanceManifest(manifestBytes.bytes.toString("utf8"));
  const inventory = distributableAssetInventory(modes.keys());
  if (inventory.join("\0") !== parsed.paths.join("\0")) {
    const missing = inventory.filter(
      (assetPath) => !parsed.paths.includes(assetPath),
    );
    const extra = parsed.paths.filter(
      (assetPath) => !inventory.includes(assetPath),
    );
    fail(
      "provenance_coverage_mismatch",
      `missing=${missing.join(",")} extra=${extra.join(",")}`,
    );
  }

  const files = new Map(
    parsed.manifest.assets.flatMap((asset) =>
      asset.files.map((file) => [file.path, file]),
    ),
  );
  for (const assetPath of inventory) {
    if (modes.get(assetPath) !== "100644")
      fail("invalid_provenance_mode", assetPath);
    const actual = await readTrackedAsset(root, assetPath);
    const expected = files.get(assetPath);
    if (expected.sha256 !== actual.sha256)
      fail("asset_hash_mismatch", assetPath);
  }

  for (const asset of parsed.manifest.assets) {
    const evidence = asset.rights.evidence;
    if (!evidence.locator.startsWith("https://")) {
      if (!modes.has(evidence.locator))
        fail("untracked_rights_evidence", evidence.locator);
      if (evidence.sha256 !== null) {
        const actual = await readTrackedAsset(root, evidence.locator);
        if (actual.sha256 !== evidence.sha256)
          fail("rights_evidence_hash_mismatch", evidence.locator);
      }
    }
  }

  const brand = parseBrandManifest(
    (await readTrackedAsset(root, "assets/brand-mark.json")).bytes.toString(
      "utf8",
    ),
  );
  assertBrandRevision(parsed.manifest, brand);
  return Object.freeze({
    assets: parsed.manifest.assets.length,
    files: inventory.length,
  });
}

async function main() {
  if (process.argv.length !== 2)
    fail("invalid_argument", "usage: node scripts/verify-assets.mjs");
  const result = await verifyAssetProvenance();
  process.stdout.write(`asset_provenance_verified:${result.files}\n`);
}

const isMain =
  process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  main().catch((error) => {
    const message =
      error instanceof AssetProvenanceError
        ? error.message
        : `unexpected_error: ${error?.message ?? error}`;
    process.stderr.write(`asset_provenance_failed:${message}\n`);
    process.exitCode = 1;
  });
}

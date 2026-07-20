#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { constants as fsConstants } from "node:fs";
import {
  chmod,
  copyFile,
  lstat,
  mkdir,
  mkdtemp,
  open,
  readFile,
  realpath,
  rename,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { crc32, inflateSync } from "node:zlib";

import { readToolchainIdentity } from "./toolchain.mjs";

const rootDir = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
);
const brandPath = path.join(rootDir, "assets", "brand-mark.json");
const provenancePath = path.join(rootDir, "assets", "provenance.json");
const expectedTauriVersion = readToolchainIdentity({
  repositoryRoot: rootDir,
}).tauri_cli;
const maximumBrandBytes = 16 * 1024;
const maximumProvenanceBytes = 128 * 1024;
const maximumGeneratedAssetBytes = 2 * 1024 * 1024;
const maximumCommandOutputBytes = 2 * 1024 * 1024;

export const generatedAssetPaths = Object.freeze([
  "apps/desktop/icons/icon.png",
  "apps/desktop/icons/icon.ico",
  "apps/desktop/icons/dev/icon.png",
  "apps/desktop/icons/dev/icon.ico",
  "frontend/static/favicon.png",
]);

const exactBrandKeys = Object.freeze({
  root: [
    "background",
    "colors",
    "design_revision",
    "paths",
    "schema_version",
    "view_box",
  ],
  background: ["fill", "height", "radius", "width", "x", "y"],
  colors: ["development", "interface", "release"],
  paths: ["bottom_left", "ribbon", "top_right"],
});

export class AssetGenerationError extends Error {
  constructor(code, detail = "") {
    super(detail ? `${code}: ${detail}` : code);
    this.name = "AssetGenerationError";
    this.code = code;
  }
}

function fail(code, detail) {
  throw new AssetGenerationError(code, detail);
}

function exactKeys(value, expected, label) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    fail("invalid_brand_manifest", `${label} must be an object`);
  }
  const actual = Object.keys(value).sort();
  if (
    actual.length !== expected.length ||
    actual.some((key, index) => key !== expected[index])
  ) {
    fail("invalid_brand_manifest", `${label} has unexpected fields`);
  }
}

function finiteNumber(value, label) {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    fail("invalid_brand_manifest", `${label} must be finite`);
  }
  return value;
}

function boundedString(value, pattern, label) {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    value.length > 2_048 ||
    !pattern.test(value)
  ) {
    fail("invalid_brand_manifest", `${label} is invalid`);
  }
  return value;
}

export function parseBrandManifest(source) {
  if (
    typeof source !== "string" ||
    Buffer.byteLength(source) > maximumBrandBytes
  ) {
    fail(
      "invalid_brand_manifest",
      `manifest exceeds ${maximumBrandBytes} bytes`,
    );
  }
  let brand;
  try {
    brand = JSON.parse(source);
  } catch {
    fail("invalid_brand_manifest", "JSON could not be decoded");
  }

  exactKeys(brand, exactBrandKeys.root, "root");
  exactKeys(brand.background, exactBrandKeys.background, "background");
  exactKeys(brand.colors, exactBrandKeys.colors, "colors");
  exactKeys(brand.paths, exactBrandKeys.paths, "paths");
  if (brand.schema_version !== 1)
    fail("invalid_brand_manifest", "unsupported schema version");
  if (
    !Number.isSafeInteger(brand.design_revision) ||
    brand.design_revision < 1
  ) {
    fail(
      "invalid_brand_manifest",
      "design_revision must be a positive integer",
    );
  }
  if (
    !Array.isArray(brand.view_box) ||
    brand.view_box.length !== 4 ||
    brand.view_box.some(
      (value) => typeof value !== "number" || !Number.isFinite(value),
    ) ||
    brand.view_box[2] <= 0 ||
    brand.view_box[3] <= 0
  ) {
    fail(
      "invalid_brand_manifest",
      "view_box must contain four finite dimensions",
    );
  }

  for (const key of exactBrandKeys.background.filter((key) => key !== "fill")) {
    finiteNumber(brand.background[key], `background.${key}`);
  }
  if (
    brand.background.width <= 0 ||
    brand.background.height <= 0 ||
    brand.background.radius < 0
  ) {
    fail("invalid_brand_manifest", "background dimensions are invalid");
  }
  for (const [key, value] of Object.entries(brand.colors)) {
    boundedString(value, /^#[0-9A-F]{6}$/, `colors.${key}`);
  }
  boundedString(brand.background.fill, /^#[0-9A-F]{6}$/, "background.fill");
  for (const [key, value] of Object.entries(brand.paths)) {
    boundedString(value, /^[A-Za-z0-9 .,+-]+$/, `paths.${key}`);
  }
  return brand;
}

function escapeAttribute(value) {
  return String(value).replaceAll("&", "&amp;").replaceAll('"', "&quot;");
}

export function renderBrandSvg(brand, color, includeBackground) {
  const [x, y, width, height] = brand.view_box;
  const background = brand.background;
  const nodes = includeBackground
    ? [
        `<rect x="${background.x}" y="${background.y}" width="${background.width}" height="${background.height}" rx="${background.radius}" fill="${background.fill}"/>`,
      ]
    : [];
  nodes.push(
    `<path fill="${color}" fill-rule="evenodd" d="${escapeAttribute(brand.paths.ribbon)}"/>`,
    `<path fill="${color}" d="${escapeAttribute(brand.paths.top_right)}"/>`,
    `<path fill="${color}" d="${escapeAttribute(brand.paths.bottom_left)}"/>`,
  );
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="${x} ${y} ${width} ${height}" width="${width}" height="${height}">${nodes.join("")}</svg>\n`;
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

async function readRegularSnapshot(
  filePath,
  maximumBytes,
  { allowMissing = false } = {},
) {
  let metadata;
  try {
    metadata = await lstat(filePath);
  } catch (error) {
    if (allowMissing && error?.code === "ENOENT") return null;
    fail("invalid_asset_path", `${filePath}: ${error?.code ?? "unreadable"}`);
  }
  if (metadata.isSymbolicLink()) fail("symlink_asset_path", filePath);
  if (!metadata.isFile())
    fail("invalid_asset_path", `${filePath} is not a regular file`);
  if (metadata.size > maximumBytes) fail("asset_too_large", filePath);
  const bytes = await readFile(filePath);
  if (bytes.length !== metadata.size)
    fail("asset_changed_during_read", filePath);
  return Object.freeze({
    bytes,
    state: Object.freeze({
      hash: sha256(bytes),
      mode: metadata.mode & 0o777,
      size: bytes.length,
    }),
  });
}

async function readBoundedBytes(filePath, maximumBytes) {
  return (await readRegularSnapshot(filePath, maximumBytes)).bytes;
}

async function regularFileState(
  filePath,
  { allowMissing = false, maximumBytes = maximumGeneratedAssetBytes } = {},
) {
  return (
    (await readRegularSnapshot(filePath, maximumBytes, { allowMissing }))
      ?.state ?? null
  );
}

function withinRoot(root, candidate) {
  return candidate === root || candidate.startsWith(`${root}${path.sep}`);
}

async function assertDestinationParent(root, relativePath) {
  if (
    typeof relativePath !== "string" ||
    relativePath.length === 0 ||
    path.isAbsolute(relativePath) ||
    relativePath.includes("\\") ||
    path.posix.normalize(relativePath) !== relativePath ||
    relativePath.split(/[\\/]/).includes("..")
  ) {
    fail("invalid_asset_path", relativePath);
  }
  const canonicalRoot = await realpath(root);
  const rootState = await stat(canonicalRoot);
  let current = canonicalRoot;
  for (const segment of path
    .dirname(relativePath)
    .split(/[\\/]/)
    .filter((value) => value && value !== ".")) {
    current = path.join(current, segment);
    const metadata = await lstat(current);
    if (metadata.isSymbolicLink() || !metadata.isDirectory())
      fail("symlink_asset_parent", current);
    const canonicalCurrent = await realpath(current);
    const currentState = await stat(canonicalCurrent);
    if (
      !withinRoot(canonicalRoot, canonicalCurrent) ||
      currentState.dev !== rootState.dev
    ) {
      fail("asset_parent_escape", current);
    }
  }
  return Object.freeze({ canonicalRoot, device: rootState.dev });
}

function statesEqual(left, right) {
  if (left === null || right === null) return left === right;
  return (
    left.hash === right.hash &&
    left.mode === right.mode &&
    left.size === right.size
  );
}

async function requireUnchanged(filePath, baseline) {
  const current = await regularFileState(filePath, { allowMissing: true });
  if (!statesEqual(current, baseline))
    fail("asset_changed_during_generation", filePath);
}

function runTauriIcon(inputPath, outputDir, pngSize = null) {
  const args = ["tauri", "icon", inputPath, "--output", outputDir];
  if (pngSize !== null) args.push("--png", String(pngSize));
  const result = spawnSync("cargo", args, {
    cwd: rootDir,
    encoding: "utf8",
    maxBuffer: maximumCommandOutputBytes,
    stdio: ["ignore", "pipe", "pipe"],
    timeout: 60_000,
  });
  if (result.error) fail("tauri_icon_unavailable", result.error.message);
  if (result.status !== 0) {
    fail(
      "tauri_icon_failed",
      (result.stderr || result.stdout || `status ${result.status}`).trim(),
    );
  }
}

function requireTauriVersion() {
  const result = spawnSync("cargo", ["tauri", "--version"], {
    cwd: rootDir,
    encoding: "utf8",
    maxBuffer: maximumCommandOutputBytes,
    stdio: ["ignore", "pipe", "pipe"],
    timeout: 5_000,
  });
  if (result.error) fail("tauri_icon_unavailable", result.error.message);
  const output = `${result.stdout ?? ""}\n${result.stderr ?? ""}`.trim();
  if (result.status !== 0 || output !== `tauri-cli ${expectedTauriVersion}`) {
    fail(
      "tauri_icon_version_mismatch",
      output || `expected ${expectedTauriVersion}`,
    );
  }
}

export function validatePngBytes(
  bytes,
  width,
  height,
  label,
  {
    maximumEncodedBytes = maximumGeneratedAssetBytes,
    maximumDecodedBytes = maximumGeneratedAssetBytes,
  } = {},
) {
  if (
    bytes.length < 57 ||
    bytes.length > maximumEncodedBytes ||
    bytes.subarray(0, 8).toString("hex") !== "89504e470d0a1a0a"
  ) {
    fail("invalid_generated_png", label);
  }

  let offset = 8;
  let sawHeader = false;
  let sawData = false;
  let sawEnd = false;
  const compressed = [];
  while (offset < bytes.length) {
    if (offset + 12 > bytes.length) fail("invalid_generated_png", label);
    const length = bytes.readUInt32BE(offset);
    const type = bytes.subarray(offset + 4, offset + 8).toString("ascii");
    const dataStart = offset + 8;
    const dataEnd = dataStart + length;
    const chunkEnd = dataEnd + 4;
    if (dataEnd < dataStart || chunkEnd > bytes.length)
      fail("invalid_generated_png", label);
    const expectedCrc = bytes.readUInt32BE(dataEnd);
    const actualCrc = crc32(bytes.subarray(offset + 4, dataEnd)) >>> 0;
    if (actualCrc !== expectedCrc) fail("invalid_generated_png", label);

    if (type === "IHDR") {
      if (sawHeader || sawData || length !== 13)
        fail("invalid_generated_png", label);
      if (
        bytes.readUInt32BE(dataStart) !== width ||
        bytes.readUInt32BE(dataStart + 4) !== height ||
        bytes[dataStart + 8] !== 8 ||
        bytes[dataStart + 9] !== 6 ||
        bytes[dataStart + 10] !== 0 ||
        bytes[dataStart + 11] !== 0 ||
        bytes[dataStart + 12] !== 0
      ) {
        fail("invalid_generated_png", label);
      }
      sawHeader = true;
    } else if (type === "IDAT") {
      if (!sawHeader || sawEnd || length === 0)
        fail("invalid_generated_png", label);
      sawData = true;
      compressed.push(bytes.subarray(dataStart, dataEnd));
    } else if (type === "IEND") {
      if (
        !sawHeader ||
        !sawData ||
        sawEnd ||
        length !== 0 ||
        chunkEnd !== bytes.length
      ) {
        fail("invalid_generated_png", label);
      }
      sawEnd = true;
    } else {
      fail("invalid_generated_png", `${label}: unexpected ${type}`);
    }
    offset = chunkEnd;
  }
  if (!sawEnd || offset !== bytes.length) fail("invalid_generated_png", label);

  const rowBytes = width * 4 + 1;
  const expectedInflatedBytes = rowBytes * height;
  if (
    !Number.isSafeInteger(expectedInflatedBytes) ||
    expectedInflatedBytes > maximumDecodedBytes
  ) {
    fail("invalid_generated_png", label);
  }
  let inflated;
  try {
    const compressedBytes = Buffer.concat(compressed);
    const decoded = inflateSync(compressedBytes, {
      info: true,
      maxOutputLength: expectedInflatedBytes,
    });
    if (decoded.engine.bytesWritten !== compressedBytes.length)
      fail("invalid_generated_png", label);
    inflated = decoded.buffer;
  } catch {
    fail("invalid_generated_png", label);
  }
  if (
    inflated.length !== expectedInflatedBytes ||
    Array.from({ length: height }, (_, row) => inflated[row * rowBytes]).some(
      (filter) => filter > 4,
    )
  ) {
    fail("invalid_generated_png", label);
  }

  const pixels = Buffer.allocUnsafe(width * height * 4);
  let firstPixel = null;
  let visible = false;
  let nonuniform = false;
  const paeth = (left, up, upperLeft) => {
    const prediction = left + up - upperLeft;
    const leftDistance = Math.abs(prediction - left);
    const upDistance = Math.abs(prediction - up);
    const upperLeftDistance = Math.abs(prediction - upperLeft);
    return leftDistance <= upDistance && leftDistance <= upperLeftDistance
      ? left
      : upDistance <= upperLeftDistance
        ? up
        : upperLeft;
  };
  for (let row = 0; row < height; row += 1) {
    const filter = inflated[row * rowBytes];
    for (let column = 0; column < width * 4; column += 1) {
      const raw = inflated[row * rowBytes + column + 1];
      const offset = row * width * 4 + column;
      const left = column >= 4 ? pixels[offset - 4] : 0;
      const up = row > 0 ? pixels[offset - width * 4] : 0;
      const upperLeft =
        row > 0 && column >= 4 ? pixels[offset - width * 4 - 4] : 0;
      const prediction =
        filter === 1
          ? left
          : filter === 2
            ? up
            : filter === 3
              ? Math.floor((left + up) / 2)
              : filter === 4
                ? paeth(left, up, upperLeft)
                : 0;
      pixels[offset] = (raw + prediction) & 0xff;
    }
    for (let column = 0; column < width; column += 1) {
      const offset = (row * width + column) * 4;
      const pixel = pixels.subarray(offset, offset + 4);
      visible ||= pixel[3] !== 0;
      firstPixel ??= Buffer.from(pixel);
      nonuniform ||= !pixel.equals(firstPixel);
    }
  }
  if (!visible || !nonuniform) fail("blank_generated_png", label);
}

async function assertPng(filePath, width, height) {
  const bytes = await readBoundedBytes(filePath, maximumGeneratedAssetBytes);
  validatePngBytes(bytes, width, height, filePath);
}

export function validateIcoBytes(bytes, label) {
  const expectedDimensions = [16, 24, 32, 48, 64, 256];
  if (
    bytes.length < 6 + expectedDimensions.length * 16 ||
    bytes.length > maximumGeneratedAssetBytes ||
    bytes.readUInt16LE(0) !== 0 ||
    bytes.readUInt16LE(2) !== 1 ||
    bytes.readUInt16LE(4) !== expectedDimensions.length
  ) {
    fail("invalid_generated_ico", label);
  }
  const dimensions = [];
  const payloads = [];
  for (let index = 0; index < expectedDimensions.length; index += 1) {
    const entry = 6 + index * 16;
    const width = bytes[entry] || 256;
    const height = bytes[entry + 1] || 256;
    const size = bytes.readUInt32LE(entry + 8);
    const offset = bytes.readUInt32LE(entry + 12);
    const end = offset + size;
    if (
      width !== height ||
      bytes[entry + 2] !== 0 ||
      bytes[entry + 3] !== 0 ||
      bytes.readUInt16LE(entry + 4) !== 0 ||
      bytes.readUInt16LE(entry + 6) !== 32 ||
      size < 45 ||
      offset < 6 + expectedDimensions.length * 16 ||
      end > bytes.length ||
      end < offset ||
      bytes.subarray(offset, offset + 8).toString("hex") !== "89504e470d0a1a0a"
    ) {
      fail("invalid_generated_ico", label);
    }
    validatePngBytes(
      bytes.subarray(offset, end),
      width,
      height,
      `${label}:${width}`,
    );
    dimensions.push(width);
    payloads.push([offset, end]);
  }
  dimensions.sort((left, right) => left - right);
  payloads.sort((left, right) => left[0] - right[0]);
  if (
    dimensions.some((value, index) => value !== expectedDimensions[index]) ||
    payloads.some(
      ([offset], index) => index > 0 && offset < payloads[index - 1][1],
    )
  ) {
    fail("invalid_generated_ico", label);
  }
}

async function assertIco(filePath) {
  validateIcoBytes(
    await readBoundedBytes(filePath, maximumGeneratedAssetBytes),
    filePath,
  );
}

export async function generateSelectedAssets(
  stageDir,
  brand,
  runIcon = runTauriIcon,
) {
  const sourcesDir = path.join(stageDir, "sources");
  const releaseDir = path.join(stageDir, "tauri-release");
  const developmentDir = path.join(stageDir, "tauri-development");
  const faviconDir = path.join(stageDir, "tauri-favicon");
  const selectedDir = path.join(stageDir, "selected");
  await Promise.all(
    [sourcesDir, releaseDir, developmentDir, faviconDir, selectedDir].map(
      (dir) => mkdir(dir, { recursive: true }),
    ),
  );

  const releaseSvg = path.join(sourcesDir, "release.svg");
  const developmentSvg = path.join(sourcesDir, "development.svg");
  const glyphSvg = path.join(sourcesDir, "glyph.svg");
  await writeFile(
    releaseSvg,
    renderBrandSvg(brand, brand.colors.release, true),
    { mode: 0o644 },
  );
  await writeFile(
    developmentSvg,
    renderBrandSvg(brand, brand.colors.development, true),
    { mode: 0o644 },
  );
  await writeFile(
    glyphSvg,
    renderBrandSvg(brand, brand.colors.interface, false),
    { mode: 0o644 },
  );

  await runIcon(releaseSvg, releaseDir);
  await runIcon(developmentSvg, developmentDir);
  await runIcon(glyphSvg, faviconDir, 32);

  const selections = [
    [path.join(releaseDir, "icon.png"), generatedAssetPaths[0], "png", 512],
    [path.join(releaseDir, "icon.ico"), generatedAssetPaths[1], "ico", null],
    [path.join(developmentDir, "icon.png"), generatedAssetPaths[2], "png", 512],
    [
      path.join(developmentDir, "icon.ico"),
      generatedAssetPaths[3],
      "ico",
      null,
    ],
    [path.join(faviconDir, "32x32.png"), generatedAssetPaths[4], "png", 32],
  ];

  const outputs = [];
  for (const [source, destination, kind, dimension] of selections) {
    if (kind === "png") await assertPng(source, dimension, dimension);
    else await assertIco(source);
    const staged = path.join(selectedDir, destination.replaceAll("/", "__"));
    await rename(source, staged);
    await chmod(staged, 0o644);
    outputs.push(Object.freeze({ destination, staged }));
  }
  const releasePng = await regularFileState(outputs[0].staged);
  const developmentPng = await regularFileState(outputs[2].staged);
  if (statesEqual(releasePng, developmentPng))
    fail("indistinct_brand_variants");
  return Object.freeze(outputs);
}

export async function captureAssetBaselines(root, paths) {
  const entries = await Promise.all(
    paths.map(async (relativePath) => {
      await assertDestinationParent(root, relativePath);
      return [
        relativePath,
        await regularFileState(path.join(root, relativePath), {
          allowMissing: true,
        }),
      ];
    }),
  );
  return new Map(entries);
}

async function compareOutputSets(first, second) {
  if (first.length !== second.length) fail("nondeterministic_asset_inventory");
  for (let index = 0; index < first.length; index += 1) {
    if (first[index].destination !== second[index].destination)
      fail("nondeterministic_asset_inventory");
    const [left, right] = await Promise.all([
      regularFileState(first[index].staged),
      regularFileState(second[index].staged),
    ]);
    if (!statesEqual(left, right))
      fail("nondeterministic_asset_output", first[index].destination);
  }
}

export async function stageUpdatedProvenance(
  stageDir,
  source,
  brand,
  brandBaseline,
  outputs,
) {
  const { parseProvenanceManifest } = await import("./verify-assets.mjs");
  const parsed = parseProvenanceManifest(source).manifest;
  const brandAsset = parsed.assets.find((asset) => asset.id === "axial-brand");
  if (
    !brandAsset ||
    parsed.assets.filter((asset) => asset.id === "axial-brand").length !== 1
  ) {
    fail("invalid_provenance_manifest", "axial-brand owner is missing");
  }
  const outputStates = new Map();
  for (const output of outputs)
    outputStates.set(output.destination, await regularFileState(output.staged));
  brandAsset.source.revision = `brand-mark-v${brand.design_revision}`;
  for (const file of brandAsset.files) {
    if (file.path === "assets/brand-mark.json")
      file.sha256 = brandBaseline.hash;
    else if (outputStates.has(file.path))
      file.sha256 = outputStates.get(file.path).hash;
    else
      fail(
        "invalid_provenance_manifest",
        `unexpected axial-brand path ${file.path}`,
      );
  }
  const expectedPaths = [
    ...generatedAssetPaths,
    "assets/brand-mark.json",
  ].sort();
  const actualPaths = brandAsset.files.map((file) => file.path).sort();
  if (expectedPaths.join("\0") !== actualPaths.join("\0")) {
    fail("invalid_provenance_manifest", "axial-brand coverage mismatch");
  }
  const canonical = `${JSON.stringify(parsed, null, 2)}\n`;
  parseProvenanceManifest(canonical);
  const staged = path.join(stageDir, "selected", "assets__provenance.json");
  await writeFile(staged, canonical, { mode: 0o644 });
  await chmod(staged, 0o644);
  return Object.freeze({ destination: "assets/provenance.json", staged });
}

async function assertGeneratedMatches(root, outputs, baselines) {
  for (const output of outputs) {
    const destination = path.join(root, output.destination);
    await requireUnchanged(destination, baselines.get(output.destination));
    const [expected, current] = await Promise.all([
      regularFileState(output.staged),
      regularFileState(destination, { allowMissing: true }),
    ]);
    if (!statesEqual(expected, current))
      fail("generated_asset_drift", output.destination);
  }
}

function invokeFault(fault, phase, index) {
  if (fault !== null) fault(Object.freeze({ index, phase }));
}

async function rollbackPromotions(completed, backupDir, fault, root) {
  const rollbackErrors = [];
  for (const record of [...completed].reverse()) {
    if (record.phase === "pending") continue;
    try {
      invokeFault(fault, "before_restore", record.index);
      await assertDestinationParent(root, record.relativePath);
      const destinationState = await regularFileState(record.destination, {
        allowMissing: true,
      });
      if (record.phase === "backed_up") {
        if (!statesEqual(destinationState, record.baseline)) {
          fail("rollback_destination_changed", record.relativePath);
        }
        record.phase = "restored";
        invokeFault(fault, "after_restore", record.index);
        continue;
      }
      if (!statesEqual(destinationState, record.expected)) {
        fail("rollback_destination_changed", record.relativePath);
      }
      if (record.hadOriginal) {
        const backupState = await regularFileState(record.backup);
        if (!statesEqual(backupState, record.baseline))
          fail("rollback_backup_changed", record.relativePath);
        await assertDestinationParent(root, record.relativePath);
        await rename(record.backup, record.destination);
      } else {
        const displaced = path.join(
          backupDir,
          `rollback-${record.index}-${randomBytes(4).toString("hex")}`,
        );
        await rename(record.destination, displaced);
      }
      record.phase = "restored";
      invokeFault(fault, "after_restore", record.index);
    } catch (error) {
      rollbackErrors.push(error);
    }
  }
  if (rollbackErrors.length > 0) {
    fail(
      "asset_rollback_failed",
      rollbackErrors.map((error) => error.message).join("; "),
    );
  }
}

export async function promoteGeneratedAssets({
  root,
  outputs,
  baselines,
  backupDir,
  commitPath = null,
  failAtIndex = null,
  fault = null,
  beforeCommit = null,
}) {
  if (commitPath !== null && outputs.at(-1)?.destination !== commitPath) {
    fail("invalid_commit_marker", commitPath);
  }
  await mkdir(backupDir, { recursive: true });
  for (const output of outputs) {
    await assertDestinationParent(root, output.destination);
    await requireUnchanged(
      path.join(root, output.destination),
      baselines.get(output.destination),
    );
  }

  const completed = [];
  try {
    for (let index = 0; index < outputs.length; index += 1) {
      const output = outputs[index];
      const isCommitMarker = output.destination === commitPath;
      if (failAtIndex === index)
        fail("injected_promotion_failure", String(index));
      invokeFault(fault, "before_backup", index);
      const destination = path.join(root, output.destination);
      await assertDestinationParent(root, output.destination);
      const baseline = baselines.get(output.destination);
      await requireUnchanged(destination, baseline);
      await mkdir(path.dirname(destination), { recursive: true });
      const backup = path.join(backupDir, `${index}.original`);
      const record = {
        backup,
        baseline,
        destination,
        expected: await regularFileState(output.staged),
        hadOriginal: baseline !== null,
        index,
        phase: "pending",
        relativePath: output.destination,
      };
      completed.push(record);
      if (baseline !== null) {
        await assertDestinationParent(root, output.destination);
        await copyFile(destination, backup, fsConstants.COPYFILE_EXCL);
        await chmod(backup, baseline.mode);
        const backupHandle = await open(backup, "r");
        try {
          await backupHandle.sync();
        } finally {
          await backupHandle.close();
        }
        const backupState = await regularFileState(backup);
        if (!statesEqual(backupState, baseline))
          fail("asset_backup_mismatch", output.destination);
      }
      record.phase = "backed_up";
      invokeFault(fault, "after_backup", index);

      if (isCommitMarker) {
        invokeFault(fault, "before_commit", index);
        if (beforeCommit !== null) await beforeCommit();
        for (const published of completed.slice(0, -1)) {
          const current = await regularFileState(published.destination);
          if (!statesEqual(current, published.expected)) {
            fail("published_asset_mismatch", published.relativePath);
          }
        }
        await assertDestinationParent(root, output.destination);
        await requireUnchanged(destination, baseline);

        // This atomic rename is the commit point. Nothing fallible may run after it.
        await rename(output.staged, destination);
        return;
      }

      await assertDestinationParent(root, output.destination);
      await rename(output.staged, destination);
      record.phase = "published";
      invokeFault(fault, "after_publish", index);
      const published = await regularFileState(destination);
      if (!statesEqual(published, record.expected))
        fail("published_asset_mismatch", output.destination);
      record.phase = "verified";
      invokeFault(fault, "after_verify", index);
    }
  } catch (error) {
    await rollbackPromotions(completed, backupDir, fault, root);
    throw error;
  }
}

async function makeStage(suffix) {
  const tmpDir = path.join(rootDir, "tmp");
  await mkdir(tmpDir, { recursive: true, mode: 0o755 });
  const metadata = await lstat(tmpDir);
  if (metadata.isSymbolicLink() || !metadata.isDirectory())
    fail("invalid_staging_root", tmpDir);
  const [canonicalRoot, canonicalTmp] = await Promise.all([
    realpath(rootDir),
    realpath(tmpDir),
  ]);
  const [rootState, tmpState] = await Promise.all([
    stat(canonicalRoot),
    stat(canonicalTmp),
  ]);
  if (
    !withinRoot(canonicalRoot, canonicalTmp) ||
    rootState.dev !== tmpState.dev
  ) {
    fail("invalid_staging_root", tmpDir);
  }
  return mkdtemp(path.join(tmpDir, `assets-${process.pid}-${suffix}-`));
}

async function removeDisposableStage(stage) {
  try {
    await rm(stage, {
      recursive: true,
      force: true,
      maxRetries: 2,
      retryDelay: 20,
    });
  } catch (error) {
    throw new AssetGenerationError(
      "stage_cleanup_failed",
      `${stage}: ${error?.message ?? "unknown"}`,
    );
  }
}

export async function runAssetGeneration({
  check = false,
  root = rootDir,
} = {}) {
  if (path.resolve(root) !== rootDir) fail("invalid_repository_root");
  requireTauriVersion();
  await assertDestinationParent(root, "assets/brand-mark.json");
  const brandSnapshot = await readRegularSnapshot(brandPath, maximumBrandBytes);
  const brandBaseline = brandSnapshot.state;
  const brand = parseBrandManifest(brandSnapshot.bytes.toString("utf8"));
  await assertDestinationParent(root, "assets/provenance.json");
  const provenanceSnapshot = await readRegularSnapshot(
    provenancePath,
    maximumProvenanceBytes,
  );
  const baselines = await captureAssetBaselines(root, [
    ...generatedAssetPaths,
    "assets/provenance.json",
  ]);
  const firstStage = await makeStage(check ? "check-a" : "generate");
  let secondStage = null;
  let preserveStage = false;
  let result;
  let primaryError = null;
  let committed = false;
  try {
    const first = await generateSelectedAssets(firstStage, brand);
    await requireUnchanged(brandPath, brandBaseline);
    if (check) {
      secondStage = await makeStage("check-b");
      const second = await generateSelectedAssets(secondStage, brand);
      await requireUnchanged(brandPath, brandBaseline);
      await compareOutputSets(first, second);
      await assertGeneratedMatches(root, first, baselines);
      result = Object.freeze({
        checked: first.map((output) => output.destination),
      });
    } else {
      await requireUnchanged(provenancePath, provenanceSnapshot.state);
      const provenance = await stageUpdatedProvenance(
        firstStage,
        provenanceSnapshot.bytes.toString("utf8"),
        brand,
        brandBaseline,
        first,
      );
      await promoteGeneratedAssets({
        root,
        outputs: [...first, provenance],
        baselines,
        backupDir: path.join(firstStage, "backup"),
        commitPath: "assets/provenance.json",
        beforeCommit: async () => {
          await requireUnchanged(brandPath, brandBaseline);
          await requireUnchanged(provenancePath, provenanceSnapshot.state);
        },
      });
      committed = true;
      result = Object.freeze({
        generated: first.map((output) => output.destination),
      });
    }
  } catch (error) {
    preserveStage =
      error instanceof AssetGenerationError &&
      error.code === "asset_rollback_failed";
    primaryError = error;
  }

  const disposable = [!preserveStage && firstStage, secondStage].filter(
    Boolean,
  );
  const cleanup = await Promise.allSettled(
    disposable.map((stage) => removeDisposableStage(stage)),
  );
  const cleanupErrors = cleanup
    .filter((entry) => entry.status === "rejected")
    .map((entry) => entry.reason);
  if (cleanupErrors.length > 0) {
    const residue = cleanupErrors.map((error) => error.message).join("; ");
    if (primaryError !== null) primaryError.cleanup_residue = residue;
    else if (committed)
      result = Object.freeze({ ...result, cleanup_residue: residue });
    else throw new AssetGenerationError("stage_cleanup_failed", residue);
  }
  if (primaryError !== null) throw primaryError;
  return result;
}

function parseArguments(argv) {
  if (argv.length === 0) return Object.freeze({ check: false });
  if (argv.length === 1 && argv[0] === "--check")
    return Object.freeze({ check: true });
  fail("invalid_argument", "usage: node scripts/generate-icons.mjs [--check]");
}

async function main() {
  const options = parseArguments(process.argv.slice(2));
  const result = await runAssetGeneration(options);
  const action = options.check ? "checked" : "generated";
  process.stdout.write(`assets_${action}:${result[action].length}\n`);
  if (result.cleanup_residue)
    process.stderr.write(`asset_cleanup_warning:${result.cleanup_residue}\n`);
}

const isMain =
  process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  main().catch((error) => {
    const message =
      error instanceof AssetGenerationError
        ? error.message
        : `unexpected_error: ${error?.message ?? error}`;
    process.stderr.write(`asset_generation_failed:${message}\n`);
    if (error?.cleanup_residue)
      process.stderr.write(`asset_cleanup_warning:${error.cleanup_residue}\n`);
    process.exitCode = 1;
  });
}

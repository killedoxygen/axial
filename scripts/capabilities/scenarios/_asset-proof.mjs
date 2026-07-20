import { createHash } from "node:crypto";
import { lstat, readFile, realpath } from "node:fs/promises";
import path from "node:path";

import { validateIcoBytes, validatePngBytes } from "../../generate-icons.mjs";
import {
  parseProvenanceManifest,
  verifyAssetProvenance,
} from "../../verify-assets.mjs";

const maximumAssetBytes = 4 * 1024 * 1024;

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

async function readAsset(root, relativePath) {
  const canonicalRoot = await realpath(root);
  const destination = path.join(canonicalRoot, ...relativePath.split("/"));
  const canonicalDestination = await realpath(destination);
  const metadata = await lstat(destination);
  if (
    canonicalDestination !== destination ||
    !canonicalDestination.startsWith(`${canonicalRoot}${path.sep}`) ||
    metadata.isSymbolicLink() ||
    !metadata.isFile() ||
    metadata.size > maximumAssetBytes
  ) {
    throw new Error(`invalid asset ${relativePath}`);
  }
  const bytes = await readFile(destination);
  if (bytes.length !== metadata.size)
    throw new Error(`changed asset ${relativePath}`);
  return Object.freeze({ bytes, sha256: sha256(bytes) });
}

async function provenance(root) {
  await verifyAssetProvenance({ root });
  const source = await readAsset(root, "assets/provenance.json");
  return Object.freeze({
    source,
    manifest: parseProvenanceManifest(source.bytes.toString("utf8")).manifest,
  });
}

function assetFiles(manifest, id) {
  const owner = manifest.assets.find((asset) => asset.id === id);
  if (!owner) throw new Error(`missing asset owner ${id}`);
  return owner.files;
}

async function receiptForFiles(root, files) {
  const receipt = {};
  for (const file of files) {
    const current = await readAsset(root, file.path);
    if (current.sha256 !== file.sha256)
      throw new Error(`asset hash mismatch ${file.path}`);
    receipt[file.path.replaceAll("/", ".")] = current.sha256;
  }
  return receipt;
}

export async function proveIcons(root) {
  const { manifest } = await provenance(root);
  const files = [
    ...assetFiles(manifest, "axial-brand"),
    ...assetFiles(manifest, "approved-macos-icons"),
  ];
  const dimensions = new Map([
    ["apps/desktop/icons/icon.png", 512],
    ["apps/desktop/icons/dev/icon.png", 512],
    ["frontend/static/favicon.png", 32],
  ]);
  for (const file of files) {
    if (dimensions.has(file.path)) {
      const { bytes } = await readAsset(root, file.path);
      validatePngBytes(
        bytes,
        dimensions.get(file.path),
        dimensions.get(file.path),
        file.path,
      );
    } else if (file.path.endsWith(".ico")) {
      validateIcoBytes((await readAsset(root, file.path)).bytes, file.path);
    } else if (file.path.endsWith(".icns")) {
      const { bytes } = await readAsset(root, file.path);
      if (
        bytes.length < 16 ||
        bytes.subarray(0, 4).toString("ascii") !== "icns" ||
        bytes.readUInt32BE(4) !== bytes.length
      ) {
        throw new Error(`invalid ICNS ${file.path}`);
      }
    }
  }
  return receiptForFiles(root, files);
}

export async function proveFonts(root) {
  const { manifest } = await provenance(root);
  const files = [
    ...assetFiles(manifest, "geist-mono"),
    ...assetFiles(manifest, "manrope"),
  ];
  for (const file of files) {
    const { bytes } = await readAsset(root, file.path);
    if (
      file.path.endsWith(".woff2") &&
      bytes.subarray(0, 4).toString("ascii") !== "wOF2"
    ) {
      throw new Error(`invalid WOFF2 ${file.path}`);
    }
    if (
      file.path.endsWith("OFL.txt") &&
      !bytes.toString("utf8").includes("SIL OPEN FONT LICENSE Version 1.1")
    ) {
      throw new Error(`invalid OFL ${file.path}`);
    }
  }
  return receiptForFiles(root, files);
}

export async function proveLoaderMarks(root) {
  const { manifest } = await provenance(root);
  const files = assetFiles(manifest, "loader-marks");
  const bodies = [];
  for (const file of files) {
    const body = (await readAsset(root, file.path)).bytes.toString("utf8");
    if (
      !body.startsWith("<svg ") ||
      !body.endsWith("</svg>\n") ||
      /fabric|forge|neoforge|quilt/i.test(body)
    ) {
      throw new Error(`invalid neutral loader mark ${file.path}`);
    }
    bodies.push(body);
  }
  if (new Set(bodies).size !== 5)
    throw new Error("loader marks are not distinct");
  const expectedMapping = {
    vanilla: "loader-base.svg",
    fabric: "loader-grid.svg",
    forge: "loader-cross.svg",
    neoforge: "loader-orbit.svg",
    quilt: "loader-diamonds.svg",
  };
  const mappingSource = await readAsset(
    root,
    "frontend/src/views/create/loader-logos.tsx",
  );
  for (const [loader, asset] of Object.entries(expectedMapping)) {
    if (
      !mappingSource.bytes.toString("utf8").includes(`${loader}: '${asset}'`)
    ) {
      throw new Error(`missing loader mapping ${loader}`);
    }
  }
  return {
    ...(await receiptForFiles(root, files)),
    loader_key_mapping: sha256(mappingSource.bytes),
  };
}

export async function proveProvenance(root) {
  const { source, manifest } = await provenance(root);
  const fileCount = manifest.assets.reduce(
    (total, asset) => total + asset.files.length,
    0,
  );
  return {
    file_count: fileCount,
    manifest_sha256: source.sha256,
    owner_count: manifest.assets.length,
  };
}

export function scenarioResult(id, receipt) {
  return {
    ok: true,
    observations: [{ id, outcome: "pass", receipt }],
    artifacts: [],
  };
}

export async function currentReceipt(context, id, prove) {
  if (
    !Array.isArray(context.observations) ||
    context.observations.length !== 1 ||
    context.observations[0] !== id
  ) {
    throw new Error("unexpected receipt inventory");
  }
  return {
    observations: [{ id, receipt: await prove(context.repository_root) }],
  };
}

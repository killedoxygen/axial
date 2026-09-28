import assert from "node:assert/strict";
import { execFile as execFileCallback } from "node:child_process";
import { createHash } from "node:crypto";
import { writeFileSync } from "node:fs";
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { promisify } from "node:util";
import { crc32, deflateSync } from "node:zlib";

import {
  AssetGenerationError,
  captureAssetBaselines,
  generatedAssetPaths,
  parseBrandManifest,
  promoteGeneratedAssets,
  renderBrandSvg,
  stageUpdatedProvenance,
  validateIcoBytes,
  validatePngBytes,
} from "../../generate-icons.mjs";
import {
  AssetProvenanceError,
  assertBrandRevision,
  distributableAssetInventory,
  parseProvenanceManifest,
  untrackedDistributableAssets,
  verifyAssetProvenance,
} from "../../verify-assets.mjs";

const root = path.resolve(import.meta.dirname, "../../..");
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
const execFile = promisify(execFileCallback);

async function rejectsCode(callback, code) {
  await assert.rejects(callback, (error) => {
    assert.equal(error?.code, code);
    return true;
  });
}

function pngChunks(bytes) {
  const chunks = [];
  for (let offset = 8; offset < bytes.length; ) {
    const length = bytes.readUInt32BE(offset);
    const type = bytes.subarray(offset + 4, offset + 8).toString("ascii");
    chunks.push({
      type,
      data: bytes.subarray(offset + 8, offset + 8 + length),
    });
    offset += length + 12;
  }
  return chunks;
}

function encodePng(chunks) {
  const signature = Buffer.from("89504e470d0a1a0a", "hex");
  return Buffer.concat([
    signature,
    ...chunks.map(({ type, data }) => {
      const typeBytes = Buffer.from(type, "ascii");
      const header = Buffer.alloc(8);
      header.writeUInt32BE(data.length, 0);
      typeBytes.copy(header, 4);
      const checksum = Buffer.alloc(4);
      checksum.writeUInt32BE(crc32(Buffer.concat([typeBytes, data])) >>> 0);
      return Buffer.concat([header, data, checksum]);
    }),
  ]);
}

function validateIcns(bytes, label) {
  assert.equal(bytes.subarray(0, 4).toString("ascii"), "icns", label);
  assert.equal(bytes.readUInt32BE(4), bytes.length, label);
  const expected = [
    "ic12",
    "ic07",
    "ic13",
    "ic08",
    "ic04",
    "ic14",
    "ic09",
    "ic05",
    "ic10",
    "ic11",
    "info",
  ];
  const pngDimensions = new Map([
    ["ic12", 64],
    ["ic07", 128],
    ["ic13", 256],
    ["ic08", 256],
    ["ic14", 512],
    ["ic09", 512],
    ["ic10", 1024],
    ["ic11", 32],
  ]);
  const types = [];
  let offset = 8;
  while (offset < bytes.length) {
    assert.ok(offset + 8 <= bytes.length, label);
    const type = bytes.subarray(offset, offset + 4).toString("ascii");
    const size = bytes.readUInt32BE(offset + 4);
    assert.ok(size >= 8 && offset + size <= bytes.length, `${label}:${type}`);
    const payload = bytes.subarray(offset + 8, offset + size);
    if (pngDimensions.has(type)) {
      const dimension = pngDimensions.get(type);
      assert.equal(
        payload.subarray(0, 8).toString("hex"),
        "89504e470d0a1a0a",
        `${label}:${type}`,
      );
      const chunks = pngChunks(payload);
      let cursor = 8;
      for (const chunk of chunks) {
        const length = payload.readUInt32BE(cursor);
        const dataEnd = cursor + 8 + length;
        assert.ok(
          dataEnd + 4 <= payload.length,
          `${label}:${type}:${chunk.type}`,
        );
        assert.equal(
          payload.readUInt32BE(dataEnd),
          crc32(payload.subarray(cursor + 4, dataEnd)) >>> 0,
        );
        cursor = dataEnd + 4;
      }
      assert.equal(cursor, payload.length, `${label}:${type}`);
      assert.equal(chunks[0].type, "IHDR");
      assert.equal(chunks[0].data.readUInt32BE(0), dimension);
      assert.equal(chunks[0].data.readUInt32BE(4), dimension);
      assert.equal(chunks.at(-1).type, "IEND");
      const essential = chunks.filter(({ type: chunkType }) =>
        ["IHDR", "IDAT", "IEND"].includes(chunkType),
      );
      validatePngBytes(
        encodePng(essential),
        dimension,
        dimension,
        `${label}:${type}`,
        {
          maximumDecodedBytes: 8 * 1024 * 1024,
        },
      );
    } else if (type === "info") {
      assert.equal(payload.subarray(0, 6).toString("ascii"), "bplist");
    } else {
      assert.ok(payload.length > 0, `${label}:${type}`);
    }
    types.push(type);
    offset += size;
  }
  assert.equal(offset, bytes.length, label);
  assert.deepEqual(types, expected, label);
}

test("brand geometry has a bounded closed schema and deterministic SVG projection", async () => {
  const source = await readFile(
    path.join(root, "assets/brand-mark.json"),
    "utf8",
  );
  const brand = parseBrandManifest(source);
  const svg = renderBrandSvg(brand, brand.colors.interface, false);
  assert.equal((svg.match(/<path /g) ?? []).length, 3);
  assert.equal(svg.includes("<rect "), false);
  for (const geometry of Object.values(brand.paths))
    assert.ok(svg.includes(geometry));

  assert.throws(
    () => parseBrandManifest(`${source.trimEnd().slice(0, -1)},"extra":true}`),
    AssetGenerationError,
  );
  assert.throws(
    () => parseBrandManifest("x".repeat(16 * 1024 + 1)),
    (error) => error.code === "invalid_brand_manifest",
  );
  assert.throws(
    () => parseBrandManifest(JSON.stringify({ ...brand, schema_version: 2 })),
    AssetGenerationError,
  );
});

test("selected PNG, ICO, and retained ICNS bytes pass strict structural decoding", async () => {
  for (const [relativePath, dimension] of [
    ["apps/desktop/icons/icon.png", 512],
    ["apps/desktop/icons/dev/icon.png", 512],
    ["frontend/static/favicon.png", 32],
  ]) {
    const bytes = await readFile(path.join(root, relativePath));
    validatePngBytes(bytes, dimension, dimension, relativePath);
  }
  for (const relativePath of [
    "apps/desktop/icons/icon.ico",
    "apps/desktop/icons/dev/icon.ico",
  ]) {
    validateIcoBytes(
      await readFile(path.join(root, relativePath)),
      relativePath,
    );
  }
  for (const relativePath of [
    "apps/desktop/icons/macos/icon.icns",
    "apps/desktop/icons/dev/macos/icon.icns",
  ]) {
    validateIcns(await readFile(path.join(root, relativePath)), relativePath);
  }
});

test("PNG and ICO validators reject missing data, corrupt CRC, trailing zlib bytes, and overlapping entries", async () => {
  const png = await readFile(path.join(root, "frontend/static/favicon.png"));
  assert.throws(() =>
    validatePngBytes(
      encodePng(pngChunks(png).filter(({ type }) => type !== "IDAT")),
      32,
      32,
      "no-idat",
    ),
  );

  const corruptCrc = Buffer.from(png);
  corruptCrc[corruptCrc.length - 5] ^= 1;
  assert.throws(() => validatePngBytes(corruptCrc, 32, 32, "crc"));

  const trailingZlib = pngChunks(png).map((chunk) =>
    chunk.type === "IDAT"
      ? { ...chunk, data: Buffer.concat([chunk.data, Buffer.from([0])]) }
      : chunk,
  );
  assert.throws(() =>
    validatePngBytes(encodePng(trailingZlib), 32, 32, "zlib-trailing"),
  );

  const blank = pngChunks(png).map((chunk) =>
    chunk.type === "IDAT"
      ? { ...chunk, data: deflateSync(Buffer.alloc((32 * 4 + 1) * 32)) }
      : chunk,
  );
  assert.throws(
    () => validatePngBytes(encodePng(blank), 32, 32, "blank"),
    (error) => error.code === "blank_generated_png",
  );

  const solidPixels = Buffer.alloc((32 * 4 + 1) * 32);
  for (let row = 0; row < 32; row += 1) {
    solidPixels[row * (32 * 4 + 1)] = 0;
    for (let column = 0; column < 32; column += 1) {
      solidPixels.set([255, 0, 0, 255], row * (32 * 4 + 1) + 1 + column * 4);
    }
  }
  const solid = pngChunks(png).map((chunk) =>
    chunk.type === "IDAT"
      ? { ...chunk, data: deflateSync(solidPixels) }
      : chunk,
  );
  assert.throws(
    () => validatePngBytes(encodePng(solid), 32, 32, "solid"),
    (error) => error.code === "blank_generated_png",
  );

  const ico = await readFile(path.join(root, "apps/desktop/icons/icon.ico"));
  const overlap = Buffer.from(ico);
  overlap.writeUInt32LE(overlap.readUInt32LE(6 + 12), 6 + 16 + 12);
  assert.throws(() => validateIcoBytes(overlap, "overlap"));
  const wrongSize = Buffer.from(ico);
  wrongSize[6] = 15;
  assert.throws(() => validateIcoBytes(wrongSize, "wrong-size"));
});

test("strict provenance is canonical, complete, hash-bound, and mode-bound", async () => {
  const source = await readFile(
    path.join(root, "assets/provenance.json"),
    "utf8",
  );
  const parsed = parseProvenanceManifest(source);
  await verifyAssetProvenance({ root });

  const malformed = JSON.parse(source);
  malformed.assets[0].files[0].mode = "100755";
  assert.throws(
    () => parseProvenanceManifest(`${JSON.stringify(malformed, null, 2)}\n`),
    AssetProvenanceError,
  );
  malformed.assets[0].files[0].mode = "100644";
  malformed.assets[0].files[0].sha256 = "0".repeat(64);
  const indexed = new Map(
    parsed.paths.map((assetPath) => [assetPath, "100644"]),
  );
  indexed.set("frontend/static/unowned.svg", "100644");
  assert.ok(
    distributableAssetInventory(indexed.keys()).includes(
      "frontend/static/unowned.svg",
    ),
  );
  assert.throws(
    () => assertBrandRevision(parsed.manifest, { design_revision: 2 }),
    (error) => error.code === "brand_revision_mismatch",
  );
});

test("untracked delivery media is discovered recursively while ignored files remain outside ownership", async (t) => {
  const directory = await mkdtemp(
    path.join(os.tmpdir(), "axial-assets-untracked-"),
  );
  t.after(() => rm(directory, { recursive: true, force: true }));
  await execFile("git", ["init", "--quiet", directory]);
  const mediaDir = path.join(directory, "frontend/static/nested");
  await mkdir(mediaDir, { recursive: true });
  await writeFile(path.join(mediaDir, "probe.svg"), "<svg/>\n");
  assert.deepEqual(untrackedDistributableAssets(directory), [
    "frontend/static/nested/probe.svg",
  ]);
  await writeFile(
    path.join(directory, ".gitignore"),
    "frontend/static/nested/\n",
  );
  assert.deepEqual(untrackedDistributableAssets(directory), []);
});

async function transactionFixture(t) {
  const directory = await mkdtemp(
    path.join(os.tmpdir(), "axial-assets-transaction-"),
  );
  t.after(() => rm(directory, { recursive: true, force: true }));
  const destinations = ["icons/a.bin", "icons/b.bin", "assets/provenance.json"];
  await Promise.all([
    mkdir(path.join(directory, "icons")),
    mkdir(path.join(directory, "assets")),
    mkdir(path.join(directory, "stage")),
  ]);
  for (const [index, destination] of destinations.entries()) {
    await writeFile(path.join(directory, destination), `old-${index}\n`, {
      mode: 0o644,
    });
    await chmod(path.join(directory, destination), 0o644);
    await writeFile(
      path.join(directory, "stage", `${index}.new`),
      `new-${index}\n`,
      { mode: 0o644 },
    );
    await chmod(path.join(directory, "stage", `${index}.new`), 0o644);
  }
  const outputs = destinations.map((destination, index) => ({
    destination,
    staged: path.join(directory, "stage", `${index}.new`),
  }));
  return {
    directory,
    destinations,
    outputs,
    baselines: await captureAssetBaselines(directory, destinations),
    backupDir: path.join(directory, "backup"),
  };
}

async function assertOldState(fixture) {
  for (const [index, destination] of fixture.destinations.entries()) {
    assert.equal(
      await readFile(path.join(fixture.directory, destination), "utf8"),
      `old-${index}\n`,
    );
  }
}

test("every pre-marker promotion failure rolls all destinations back in reverse", async (t) => {
  const injections = [
    ...[0, 1, 2].map((index) => ({ kind: "index", index })),
    ...[0, 1].flatMap((index) =>
      ["before_backup", "after_backup", "after_publish", "after_verify"].map(
        (phase) => ({ kind: "phase", phase, index }),
      ),
    ),
    ...["before_backup", "after_backup", "before_commit"].map((phase) => ({
      kind: "phase",
      phase,
      index: 2,
    })),
  ];
  for (const injection of injections) {
    await t.test(
      `${injection.kind}-${injection.phase ?? injection.index}-${injection.index}`,
      async (subtest) => {
        const fixture = await transactionFixture(subtest);
        await assert.rejects(
          promoteGeneratedAssets({
            root: fixture.directory,
            outputs: fixture.outputs,
            baselines: fixture.baselines,
            backupDir: fixture.backupDir,
            commitPath: "assets/provenance.json",
            failAtIndex: injection.kind === "index" ? injection.index : null,
            fault:
              injection.kind === "phase"
                ? ({ phase, index }) => {
                    if (phase === injection.phase && index === injection.index)
                      throw new Error("injected");
                  }
                : null,
          }),
        );
        await assertOldState(fixture);
      },
    );
  }
});

test("destination, source, and marker drift fail before the marker and preserve reviewable rollback evidence", async (t) => {
  await t.test("published destination drift", async (subtest) => {
    const fixture = await transactionFixture(subtest);
    await rejectsCode(
      () =>
        promoteGeneratedAssets({
          root: fixture.directory,
          outputs: fixture.outputs,
          baselines: fixture.baselines,
          backupDir: fixture.backupDir,
          commitPath: "assets/provenance.json",
          fault: ({ phase }) => {
            if (phase === "before_commit")
              writeFileSync(
                path.join(fixture.directory, "icons/a.bin"),
                "drift\n",
              );
          },
        }),
      "asset_rollback_failed",
    );
    assert.equal(
      await readFile(
        path.join(fixture.directory, "assets/provenance.json"),
        "utf8",
      ),
      "old-2\n",
    );
    await lstat(path.join(fixture.backupDir, "0.original"));
  });

  for (const label of ["brand", "provenance"]) {
    await t.test(`${label} baseline drift`, async (subtest) => {
      const fixture = await transactionFixture(subtest);
      await assert.rejects(
        promoteGeneratedAssets({
          root: fixture.directory,
          outputs: fixture.outputs,
          baselines: fixture.baselines,
          backupDir: fixture.backupDir,
          commitPath: "assets/provenance.json",
          beforeCommit: async () => {
            throw new Error(`${label} changed`);
          },
        }),
      );
      await assertOldState(fixture);
    });
  }
});

test("rollback failure is explicit and retains its backup directory", async (t) => {
  const fixture = await transactionFixture(t);
  await rejectsCode(
    () =>
      promoteGeneratedAssets({
        root: fixture.directory,
        outputs: fixture.outputs,
        baselines: fixture.baselines,
        backupDir: fixture.backupDir,
        commitPath: "assets/provenance.json",
        fault: ({ phase, index }) => {
          if (
            (phase === "after_publish" || phase === "before_restore") &&
            index === 0
          )
            throw new Error("injected");
        },
      }),
    "asset_rollback_failed",
  );
  await lstat(path.join(fixture.backupDir, "0.original"));
});

async function hardExitFixture(t) {
  const directory = await mkdtemp(
    path.join(os.tmpdir(), "axial-assets-hard-exit-"),
  );
  t.after(() => rm(directory, { recursive: true, force: true }));
  const destinations = ["icons/a.bin", "icons/b.bin", "icons/c.bin"];
  await Promise.all([
    mkdir(path.join(directory, "icons")),
    mkdir(path.join(directory, "assets")),
  ]);
  const oldHashes = {};
  const newHashes = {};
  for (const [index, destination] of destinations.entries()) {
    const oldBytes = Buffer.from(`old-${index}\n`);
    const newBytes = Buffer.from(`new-${index}\n`);
    await writeFile(path.join(directory, destination), oldBytes, {
      mode: 0o644,
    });
    oldHashes[destination] = sha256(oldBytes);
    newHashes[destination] = sha256(newBytes);
  }
  await writeFile(
    path.join(directory, "assets/provenance.json"),
    `${JSON.stringify({ hashes: oldHashes }, null, 2)}\n`,
    { mode: 0o644 },
  );

  async function stagedOutputs(name) {
    const stage = path.join(directory, name);
    await mkdir(stage);
    const outputs = [];
    for (const [index, destination] of destinations.entries()) {
      const staged = path.join(stage, `${index}.new`);
      await writeFile(staged, `new-${index}\n`, { mode: 0o644 });
      outputs.push({ destination, staged });
    }
    const marker = path.join(stage, "provenance.new");
    await writeFile(
      marker,
      `${JSON.stringify({ hashes: newHashes }, null, 2)}\n`,
      { mode: 0o644 },
    );
    outputs.push({ destination: "assets/provenance.json", staged: marker });
    return { stage, outputs };
  }

  return { directory, destinations, newHashes, stagedOutputs };
}

async function markerMatches(directory) {
  const marker = JSON.parse(
    await readFile(path.join(directory, "assets/provenance.json"), "utf8"),
  );
  for (const [destination, expected] of Object.entries(marker.hashes)) {
    if (sha256(await readFile(path.join(directory, destination))) !== expected)
      return false;
  }
  return true;
}

test("a hard exit leaves complete mixed files fail-closed until a fresh marker-last rerun", async (t) => {
  const fixture = await hardExitFixture(t);
  const first = await fixture.stagedOutputs("first-stage");
  const baselines = await captureAssetBaselines(fixture.directory, [
    ...fixture.destinations,
    "assets/provenance.json",
  ]);
  const configPath = path.join(fixture.directory, "child-config.json");
  await writeFile(
    configPath,
    JSON.stringify({
      root: fixture.directory,
      outputs: first.outputs,
      baselines: [...baselines],
      backupDir: path.join(first.stage, "backup"),
    }),
  );
  const moduleUrl = new URL("../../generate-icons.mjs", import.meta.url).href;
  const child = `
    import { readFile } from "node:fs/promises";
    import { promoteGeneratedAssets } from ${JSON.stringify(moduleUrl)};
    const config = JSON.parse(await readFile(process.env.AXIAL_HARD_EXIT_CONFIG, "utf8"));
    await promoteGeneratedAssets({
      ...config,
      baselines: new Map(config.baselines),
      commitPath: "assets/provenance.json",
      fault: ({ phase, index }) => {
        if (phase === "after_publish" && index === 1) process.exit(91);
      },
    });
  `;
  await assert.rejects(
    execFile(process.execPath, ["--input-type=module", "--eval", child], {
      env: { ...process.env, AXIAL_HARD_EXIT_CONFIG: configPath },
      timeout: 5_000,
    }),
    (error) => error.code === 91,
  );

  assert.equal(
    await readFile(path.join(fixture.directory, "icons/a.bin"), "utf8"),
    "new-0\n",
  );
  assert.equal(
    await readFile(path.join(fixture.directory, "icons/b.bin"), "utf8"),
    "new-1\n",
  );
  assert.equal(
    await readFile(path.join(fixture.directory, "icons/c.bin"), "utf8"),
    "old-2\n",
  );
  assert.equal(await markerMatches(fixture.directory), false);

  const rerun = await fixture.stagedOutputs("rerun-stage");
  await promoteGeneratedAssets({
    root: fixture.directory,
    outputs: rerun.outputs,
    baselines: await captureAssetBaselines(fixture.directory, [
      ...fixture.destinations,
      "assets/provenance.json",
    ]),
    backupDir: path.join(rerun.stage, "backup"),
    commitPath: "assets/provenance.json",
  });
  assert.equal(await markerMatches(fixture.directory), true);
  for (const [destination, expected] of Object.entries(fixture.newHashes)) {
    assert.equal(
      sha256(await readFile(path.join(fixture.directory, destination))),
      expected,
    );
  }
});

test("unsafe destination spellings and linked parents fail closed", async (t) => {
  const fixture = await transactionFixture(t);
  await rejectsCode(
    () =>
      promoteGeneratedAssets({
        root: fixture.directory,
        outputs: [
          {
            destination: "icons\\escape.bin",
            staged: fixture.outputs[0].staged,
          },
        ],
        baselines: new Map([["icons\\escape.bin", null]]),
        backupDir: fixture.backupDir,
      }),
    "invalid_asset_path",
  );

  const outside = await mkdtemp(
    path.join(os.tmpdir(), "axial-assets-outside-"),
  );
  t.after(() => rm(outside, { recursive: true, force: true }));
  await symlink(
    outside,
    path.join(fixture.directory, "linked"),
    process.platform === "win32" ? "junction" : "dir",
  );
  await rejectsCode(
    () =>
      promoteGeneratedAssets({
        root: fixture.directory,
        outputs: [
          {
            destination: "linked/escape.bin",
            staged: fixture.outputs[0].staged,
          },
        ],
        baselines: new Map([["linked/escape.bin", null]]),
        backupDir: fixture.backupDir,
      }),
    "symlink_asset_parent",
  );
});

test("generation stages one canonical manifest changing only the six brand-bound hashes", async (t) => {
  const directory = await mkdtemp(
    path.join(os.tmpdir(), "axial-assets-manifest-"),
  );
  t.after(() => rm(directory, { recursive: true, force: true }));
  await mkdir(path.join(directory, "selected"), { recursive: true });
  const outputs = [];
  for (const [index, destination] of generatedAssetPaths.entries()) {
    const staged = path.join(directory, `output-${index}`);
    await writeFile(staged, `next-${index}\n`, { mode: 0o644 });
    await chmod(staged, 0o644);
    outputs.push({ destination, staged });
  }
  const source = await readFile(
    path.join(root, "assets/provenance.json"),
    "utf8",
  );
  const brand = parseBrandManifest(
    await readFile(path.join(root, "assets/brand-mark.json"), "utf8"),
  );
  const replacementBrandHash = "f".repeat(64);
  const staged = await stageUpdatedProvenance(
    directory,
    source,
    brand,
    { hash: replacementBrandHash, mode: 0o644, size: 1 },
    outputs,
  );
  const nextSource = await readFile(staged.staged, "utf8");
  const previous = parseProvenanceManifest(source).manifest;
  const next = parseProvenanceManifest(nextSource).manifest;
  const previousBrand = previous.assets.find(
    (asset) => asset.id === "axial-brand",
  );
  const nextBrand = next.assets.find((asset) => asset.id === "axial-brand");
  assert.deepEqual(next.assets.slice(1), previous.assets.slice(1));
  assert.deepEqual(
    { ...nextBrand, files: undefined },
    { ...previousBrand, files: undefined },
  );
  assert.equal(
    nextBrand.files.find((file) => file.path === "assets/brand-mark.json")
      .sha256,
    replacementBrandHash,
  );
  for (const output of outputs) {
    assert.equal(
      nextBrand.files.find((file) => file.path === output.destination).sha256,
      sha256(await readFile(output.staged)),
    );
  }
  const stagedMetadata = await lstat(staged.staged);
  assert.equal(stagedMetadata.isFile(), true);
  assert.notEqual(stagedMetadata.mode & 0o200, 0);
  assert.equal(stagedMetadata.mode & 0o111, 0);
});

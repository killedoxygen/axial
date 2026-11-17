import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { access, mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { executableArchitecture, packageArtifacts } from "./package.mjs";
import { hostPlatform } from "./matrix.mjs";

async function temporary(t) {
  const root = await mkdtemp(path.join(tmpdir(), "axial-package-contract-test-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  return root;
}

// Minimal header probes only; these bytes cannot launch or prove a native build.
function header(platform) {
  const bytes = Buffer.alloc(128);
  if (platform === "linux-amd64") {
    bytes.set([127, 69, 76, 70, 2, 1]);
    bytes.writeUInt16LE(62, 18);
  } else if (platform === "windows-amd64") {
    bytes.set([0x4d, 0x5a]);
    bytes.writeUInt32LE(64, 60);
    bytes.writeUInt32LE(0x4550, 64);
    bytes.writeUInt16LE(0x8664, 68);
  } else {
    bytes.writeUInt32LE(0xfeedfacf, 0);
    bytes.writeUInt32LE(platform === "macos-amd64" ? 0x01000007 : 0x0100000c, 4);
  }
  return bytes;
}

test("architecture probes identify only the four supported header types", async (t) => {
  const root = await temporary(t);
  for (const platform of ["linux-amd64", "windows-amd64", "macos-amd64", "macos-arm64"]) {
    const file = path.join(root, platform);
    await writeFile(file, header(platform));
    assert.equal(await executableArchitecture(file), platform);
  }
});

test("architecture probes reject truncated, unsupported and malformed headers", async (t) => {
  const cases = [
    ["short", Buffer.alloc(20), "invalid_executable_header"],
    ["unknown", Buffer.alloc(128), "unsupported_executable_format"],
    ["ELF ARM", header("linux-amd64"), "unsupported_elf_architecture", (bytes) => bytes.writeUInt16LE(183, 18)],
    ["Mach-O unsupported CPU", header("macos-arm64"), "unsupported_macho_architecture", (bytes) => bytes.writeUInt32LE(12, 4)],
    ["PE offset out of bounds", header("windows-amd64"), "invalid_pe_offset", (bytes) => bytes.writeUInt32LE(2 ** 20 + 1, 60)],
    ["PE truncated section", header("windows-amd64"), "unsupported_pe_architecture", (bytes) => bytes.writeUInt32LE(4096, 60)],
    ["PE ARM", header("windows-amd64"), "unsupported_pe_architecture", (bytes) => bytes.writeUInt16LE(0xaa64, 68)],
  ];
  for (const [name, bytes, message, mutate] of cases) {
    await t.test(name, async (t) => {
      const root = await temporary(t);
      mutate?.(bytes);
      const file = path.join(root, "synthetic-header");
      await writeFile(file, bytes);
      await assert.rejects(executableArchitecture(file), { message });
    });
  }
});

test("packaging refuses foreign hosts and mismatched binaries before creating output", async (t) => {
  const root = await temporary(t);
  const native = hostPlatform();
  const foreign = native === "linux-amd64" ? "macos-arm64" : "linux-amd64";
  const output = path.join(root, "candidate");
  const binary = path.join(root, "synthetic-header");
  const source = { schema: "axial.source.v1", app_id: "com.killedoxygen.axial", tag: "v9.8.7", source_sha: "a".repeat(40) };
  await assert.rejects(packageArtifacts({ platform: foreign, source, binary, bundleRoot: root, output }), { message: "packaging_requires_supported_host" });
  await assert.rejects(access(output), { code: "ENOENT" });
  if (native) {
    await writeFile(binary, header(foreign));
    await assert.rejects(packageArtifacts({ platform: native, source, binary, bundleRoot: root, output }), { message: "binary_architecture_mismatch" });
    await assert.rejects(access(output), { code: "ENOENT" });
  }
});

test("packaging never merges a retry into an existing candidate directory", async (t) => {
  const platform = hostPlatform();
  if (!platform) return t.skip("this host has no supported native artifact target");
  const root = await temporary(t);
  const output = path.join(root, "existing-candidate");
  await mkdir(output);
  const marker = path.join(output, "preserve.txt");
  await writeFile(marker, "previous candidate must remain unchanged");
  const binary = path.join(root, "synthetic-header");
  await writeFile(binary, header(platform));
  await assert.rejects(packageArtifacts({ platform, binary, bundleRoot: root, output,
    source: { schema: "axial.source.v1", app_id: "com.killedoxygen.axial", tag: "v9.8.7", source_sha: "a".repeat(40) },
  }), { code: "EEXIST" });
  assert.equal(await readFile(marker, "utf8"), "previous candidate must remain unchanged");
});


test("Windows cross-packaging preserves the executable and installer", async (t) => {
  if (!hostPlatform() || process.platform === "win32") return t.skip("requires a supported Unix host");
  const root = await temporary(t);
  const binary = path.join(root, "synthetic-header");
  await writeFile(binary, header("windows-amd64"));
  await mkdir(path.join(root, "nsis"));
  const installer = Buffer.from("installer fixture only");
  await writeFile(path.join(root, "nsis", "setup.exe"), installer);
  const output = path.join(root, "candidate");
  const manifest = await packageArtifacts({ platform: "windows-amd64", binary, bundleRoot: root, output,
    source: { schema: "axial.source.v1", app_id: "com.killedoxygen.axial", tag: "v9.8.7", source_sha: "a".repeat(40) },
  });
  const portable = manifest.files.find((item) => item.role === "portable");
  const updater = manifest.files.find((item) => item.role === "updater");
  assert.deepEqual(await readFile(path.join(output, portable.name)), header("windows-amd64"));
  assert.deepEqual(await readFile(path.join(output, updater.name)), installer);
  const archive = manifest.files.find((item) => item.role === "portable-archive");
  const unpacked = await promisify(execFile)("unzip", ["-p", path.join(output, archive.name), "axial.exe"], { encoding: "buffer" });
  assert.deepEqual(unpacked.stdout, header("windows-amd64"));
});


test("macOS packaging accepts either supported binary architecture", async (t) => {
  if (process.platform !== "darwin") return t.skip("requires macOS packaging tools");
  const root = await temporary(t);
  const platform = hostPlatform() === "macos-arm64" ? "macos-amd64" : "macos-arm64";
  const binary = path.join(root, "synthetic-header");
  await writeFile(binary, header(platform));
  await mkdir(path.join(root, "dmg"));
  await writeFile(path.join(root, "dmg", "Axial.dmg"), "disk image fixture only");
  await mkdir(path.join(root, "macos", "Axial.app"), { recursive: true });
  await writeFile(path.join(root, "macos", "Axial.app", "fixture"), header(platform));
  const output = path.join(root, "candidate");
  const manifest = await packageArtifacts({ platform, binary, bundleRoot: root, output,
    source: { schema: "axial.source.v1", app_id: "com.killedoxygen.axial", tag: "v9.8.7", source_sha: "a".repeat(40) },
  });
  const updater = manifest.files.find((item) => item.role === "updater");
  const unpacked = await promisify(execFile)("tar", ["-xOzf", path.join(output, updater.name), "Axial.app/fixture"], { encoding: "buffer" });
  assert.deepEqual(unpacked.stdout, header(platform));
});

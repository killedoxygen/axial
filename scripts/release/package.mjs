#!/usr/bin/env node
import { execFile } from "node:child_process";
import { chmod, copyFile, lstat, mkdir, mkdtemp, open, readdir, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { promisify } from "node:util";
import { APPLICATION_ID, hostPlatform, payloads } from "./matrix.mjs";
import { check, digest, jsonFile, main, options, regularFile, writeJson } from "./contract.mjs";

const exec = promisify(execFile);

export async function executableArchitecture(file) {
  await regularFile(file);
  const handle = await open(file, "r");
  try {
    const head = Buffer.alloc(4096);
    const { bytesRead } = await handle.read(head, 0, head.length, 0);
    check(bytesRead >= 64, "invalid_executable_header");
    if (head.subarray(0, 4).equals(Buffer.from([127, 69, 76, 70]))) {
      check(head[4] === 2 && head[5] === 1 && head.readUInt16LE(18) === 62, "unsupported_elf_architecture");
      return "linux-amd64";
    }
    if (head.readUInt32LE(0) === 0xfeedfacf) {
      const cpu = head.readUInt32LE(4);
      check([0x01000007, 0x0100000c].includes(cpu), "unsupported_macho_architecture");
      return cpu === 0x01000007 ? "macos-amd64" : "macos-arm64";
    }
    if (head[0] === 0x4d && head[1] === 0x5a) {
      const offset = head.readUInt32LE(60);
      check(offset >= 64 && offset <= 1024 ** 2, "invalid_pe_offset");
      const pe = Buffer.alloc(6);
      const read = await handle.read(pe, 0, pe.length, offset);
      check(read.bytesRead === 6 && pe.readUInt32LE(0) === 0x4550 && pe.readUInt16LE(4) === 0x8664, "unsupported_pe_architecture");
      return "windows-amd64";
    }
    throw new Error("unsupported_executable_format");
  } finally {
    await handle.close();
  }
}

async function oneFile(directory, extension) {
  const entries = (await readdir(directory)).filter((name) => name.endsWith(extension));
  check(entries.length === 1, "bundle_missing_or_ambiguous");
  const file = path.join(directory, entries[0]);
  await regularFile(file);
  return file;
}

export async function packageArtifacts({ platform, source, binary, bundleRoot, output }) {
  check(hostPlatform() === platform, "packaging_requires_matching_native_host");
  check(source.schema === "axial.rewrite.source.v1" && source.app_id === APPLICATION_ID, "invalid_source_receipt");
  check(await executableArchitecture(binary) === platform, "binary_architecture_mismatch");
  const artifacts = payloads(platform, source.tag);
  output = path.resolve(output);
  bundleRoot = path.resolve(bundleRoot);
  // Refuse an existing directory: a failed/retried candidate never mixes builds.
  await mkdir(output, { recursive: false });
  const work = await mkdtemp(path.join(output, ".package-"));
  try {
    const executableName = platform.startsWith("windows") ? "axial.exe" : "axial";
    const portable = path.join(work, executableName);
    await copyFile(binary, portable);
    await chmod(portable, 0o755);
    let updaterSource;
    for (const item of artifacts) {
      const destination = path.join(output, item.name);
      if (item.role === "portable") {
        await copyFile(portable, destination);
        await chmod(destination, 0o755);
      } else if (item.role === "portable-archive") {
        if (platform.startsWith("windows")) {
          await exec("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", "Compress-Archive -LiteralPath $env:AXIAL_PACKAGE_INPUT -DestinationPath $env:AXIAL_PACKAGE_OUTPUT -ErrorAction Stop"], {
            env: { ...process.env, AXIAL_PACKAGE_INPUT: portable, AXIAL_PACKAGE_OUTPUT: destination }, timeout: 60000, maxBuffer: 8192,
          });
        } else {
          await exec("tar", ["-czf", destination, "-C", work, executableName], { timeout: 60000, maxBuffer: 8192 });
        }
      } else if (item.role === "installer") {
        await copyFile(await oneFile(path.join(bundleRoot, "dmg"), ".dmg"), destination);
      } else if (platform.startsWith("linux")) {
        updaterSource = await oneFile(path.join(bundleRoot, "appimage"), ".AppImage");
        await copyFile(updaterSource, destination);
        await chmod(destination, 0o755);
      } else if (platform.startsWith("windows")) {
        updaterSource = await oneFile(path.join(bundleRoot, "nsis"), ".exe");
        await copyFile(updaterSource, destination);
      } else {
        const macos = path.join(bundleRoot, "macos");
        const archives = (await readdir(macos)).filter((name) => name.endsWith(".app.tar.gz"));
        check(archives.length <= 1, "updater_bundle_ambiguous");
        if (archives.length === 1) {
          updaterSource = path.join(macos, archives[0]);
          await regularFile(updaterSource);
          await copyFile(updaterSource, destination);
        } else {
          const apps = (await readdir(macos)).filter((name) => name.endsWith(".app"));
          check(apps.length === 1 && (await lstat(path.join(macos, apps[0]))).isDirectory(), "app_bundle_missing_or_ambiguous");
          // Unsigned candidate archive only. Signed updates use Tauri's output.
          await exec("tar", ["-czf", destination, "-C", macos, apps[0]], { timeout: 60000, maxBuffer: 8192 });
        }
      }
    }
    if (updaterSource) {
      const signature = `${updaterSource}.sig`;
      const info = await lstat(signature).catch((error) => { if (error.code === "ENOENT") return null; throw error; });
      if (info) {
        await regularFile(signature, 8192);
        const name = `${artifacts.find((item) => item.role === "updater").name}.sig`;
        await copyFile(signature, path.join(output, name));
        artifacts.push({ name, role: "updater-signature" });
      }
    }
    const files = [];
    for (const item of artifacts) {
      const file = path.join(output, item.name);
      const info = await regularFile(file);
      const sha256 = await digest(file);
      await writeFile(`${file}.sha256`, `${sha256}  ${item.name}\n`, { flag: "wx" });
      files.push({ ...item, bytes: info.size, sha256 });
    }
    const manifest = { schema: "axial.rewrite.artifacts.v1", app_id: APPLICATION_ID, platform,
      tag: source.tag, source_sha: source.source_sha, files,
      authenticity: "not-verified", native_acceptance: "not-run" };
    await writeJson(path.join(output, "artifacts.json"), manifest);
    return manifest;
  } finally {
    await rm(work, { recursive: true, force: true });
  }
}

main(import.meta.url, async () => {
  const o = options(process.argv.slice(2), ["platform", "source", "binary", "bundle-root", "output"]);
  return packageArtifacts({ platform: o.platform, source: await jsonFile(o.source), binary: o.binary, bundleRoot: o["bundle-root"], output: o.output });
});

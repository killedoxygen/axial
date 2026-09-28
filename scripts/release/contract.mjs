#!/usr/bin/env node
import { createHash } from "node:crypto";
import { execFile } from "node:child_process";
import { createReadStream } from "node:fs";
import { lstat, readFile, writeFile, readdir, realpath, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { promisify } from "node:util";
import { pathToFileURL } from "node:url";
import { APPLICATION_ID, PLATFORMS, payloads, releaseIdentity } from "./matrix.mjs";

const exec = promisify(execFile);
export const SHA = /^[a-f0-9]{64}$/;
export const COMMIT = /^[a-f0-9]{40}$/;

export function check(condition, message) {
  if (!condition) throw new Error(message);
}

export async function regularFile(file, maximum = 2 * 1024 ** 3) {
  const info = await lstat(file);
  check(info.isFile() && !info.isSymbolicLink() && info.size > 0 && info.size <= maximum, "invalid_regular_file");
  return info;
}

export async function jsonFile(file) {
  await regularFile(file, 8 * 1024 ** 2);
  return JSON.parse(await readFile(file, "utf8"));
}

export async function digest(file) {
  await regularFile(file);
  const hash = createHash("sha256");
  for await (const bytes of createReadStream(file)) hash.update(bytes);
  return hash.digest("hex");
}

export async function writeJson(file, value) {
  await writeFile(file, `${JSON.stringify(value, null, 2)}\n`, { flag: "wx" });
}

export function changelogRelease(text, version) {
  check(typeof text === "string" && !text.includes("\r"), "invalid_changelog_encoding");
  const headings = [...text.matchAll(/^## (.*)$/gm)];
  check(headings[0]?.[1] === "[Unreleased]", "missing_first_unreleased_section");
  const seen = new Set();
  for (const heading of headings.slice(1)) {
    const parsed = /^\[(.+)\] - (\d{4}-\d{2}-\d{2})$/.exec(heading[1]);
    check(parsed, "invalid_changelog_heading");
    releaseIdentity(`v${parsed[1]}`);
    check(!seen.has(parsed[1]), "duplicate_changelog_release");
    seen.add(parsed[1]);
    const date = new Date(`${parsed[2]}T00:00:00Z`);
    check(!Number.isNaN(date.valueOf()) && date.toISOString().slice(0, 10) === parsed[2], "invalid_changelog_date");
  }
  const current = headings[1];
  check(current?.[1].startsWith(`[${version}] - `), "release_not_latest_changelog_section");
  const notes = text.slice(current.index + current[0].length, headings[2]?.index ?? text.length).trim();
  check(notes.length > 0 && Buffer.byteLength(notes) <= 120 * 1024, "invalid_release_notes");
  return { date: current[1].slice(-10), notes };
}

export async function verifySource({ root = process.cwd(), tag, sourceSha, metadata }) {
  const identity = releaseIdentity(tag);
  check(COMMIT.test(sourceSha), "invalid_source_sha");
  root = await realpath(root);
  check(await realpath(metadata.workspace_root) === root, "metadata_workspace_mismatch");
  check(Array.isArray(metadata.workspace_members) && metadata.workspace_members.length >= 3, "incomplete_workspace");
  const members = metadata.workspace_members.map((id) => metadata.packages.find((pkg) => pkg.id === id));
  check(members.every((pkg) => pkg?.version === identity.version), "workspace_version_mismatch");
  check(new Set(members.map((pkg) => pkg.id)).size === members.length, "duplicate_workspace_member");
  for (const pkg of members) {
    const relative = path.relative(root, await realpath(pkg.manifest_path));
    check(relative && !relative.startsWith("..") && !path.isAbsolute(relative) && !relative.startsWith(`legacy${path.sep}`), "manifest_outside_rewrite");
    const manifest = await readFile(pkg.manifest_path, "utf8");
    const section = /^\[package\]\s*\n([\s\S]*?)(?=^\[|$(?![\s\S]))/m.exec(manifest)?.[1];
    check(section && /^version\.workspace\s*=\s*true\s*$/m.test(section), "package_version_not_inherited");
  }
  const desktop = await jsonFile(path.join(root, "apps/desktop/tauri.conf.json"));
  check(desktop.identifier === APPLICATION_ID, "application_identity_mismatch");
  check(!desktop.version || desktop.version === identity.version, "desktop_version_mismatch");
  const frontend = await jsonFile(path.join(root, "frontend/package.json"));
  check(!frontend.version || frontend.version === identity.version, "frontend_version_mismatch");
  const git = async (...args) => (await exec("git", args, { cwd: root, timeout: 10000, maxBuffer: 1024 ** 2 })).stdout.trim();
  check(await git("rev-parse", "HEAD") === sourceSha, "source_head_mismatch");
  check(await git("rev-parse", `refs/tags/${tag}^{commit}`) === sourceSha, "tag_commit_mismatch");
  check(await git("status", "--porcelain", "--untracked-files=normal") === "", "source_tree_not_clean");
  const changelog = path.join(root, "CHANGELOG.md");
  await regularFile(changelog, 4 * 1024 ** 2);
  return { schema: "axial.rewrite.source.v1", app_id: APPLICATION_ID, ...identity, source_sha: sourceSha,
    ...changelogRelease(await readFile(changelog, "utf8"), identity.version), changelog_sha256: await digest(changelog) };
}

export async function verifyArtifacts(directory, source, platform) {
  check(source.schema === "axial.rewrite.source.v1" && source.app_id === APPLICATION_ID && COMMIT.test(source.source_sha), "invalid_source_receipt");
  check(releaseIdentity(source.tag).version === source.version, "source_version_mismatch");
  const expected = payloads(platform, source.tag);
  const manifest = await jsonFile(path.join(directory, "artifacts.json"));
  check(manifest.schema === "axial.rewrite.artifacts.v1" && manifest.app_id === APPLICATION_ID, "invalid_artifact_receipt");
  check(manifest.platform === platform && manifest.source_sha === source.source_sha && manifest.tag === source.tag, "artifact_source_mismatch");
  check(Array.isArray(manifest.files), "invalid_artifact_files");
  const updater = expected.find((entry) => entry.role === "updater");
  if (manifest.files.some((entry) => entry.role === "updater-signature")) expected.push({ name: `${updater.name}.sig`, role: "updater-signature" });
  check(manifest.files.length === expected.length, "artifact_count_mismatch");
  const entries = await readdir(directory);
  const required = ["artifacts.json", ...expected.flatMap(({ name }) => [name, `${name}.sha256`])];
  if (entries.includes("artifacts.json.sig")) {
    await regularFile(path.join(directory, "artifacts.json.sig"), 8192);
    required.push("artifacts.json.sig");
  }
  required.sort();
  check(JSON.stringify(entries.sort()) === JSON.stringify(required), "artifact_directory_incomplete_or_unexpected");
  for (const item of expected) {
    const recorded = manifest.files.filter((entry) => entry.name === item.name);
    check(recorded.length === 1 && recorded[0].role === item.role && SHA.test(recorded[0].sha256), "invalid_artifact_record");
    const file = path.join(directory, item.name);
    const info = await regularFile(file);
    const sha256 = await digest(file);
    check(recorded[0].bytes === info.size && recorded[0].sha256 === sha256, "artifact_digest_mismatch");
    const checksum = path.join(directory, `${item.name}.sha256`);
    await regularFile(checksum, 1024);
    check(await readFile(checksum, "utf8") === `${sha256}  ${item.name}\n`, "artifact_checksum_mismatch");
  }
  return manifest;
}

function minisignText(text, kind) {
  const decoded = text.trim().startsWith("untrusted comment:") ? text.trim() : Buffer.from(text.trim(), "base64").toString("utf8").trim();
  const lines = decoded.split(/\r?\n/);
  check(lines[0]?.startsWith("untrusted comment:") && lines.length === (kind === "key" ? 2 : 4), "invalid_minisign_encoding");
  check(Buffer.from(lines[1], "base64").length === (kind === "key" ? 42 : 74), "invalid_minisign_packet");
  return `${decoded}\n`;
}

// The caller supplies the trusted key out of band. A checksum or .sig filename
// is never treated as publisher authentication. Minisign performs verification.
export async function verifyUpdaterSignature(directory, manifest, publicKeyFile) {
  check(publicKeyFile, "publisher_key_unavailable");
  await regularFile(publicKeyFile, 8192);
  const updater = manifest.files.find((item) => item.role === "updater");
  const signature = manifest.files.find((item) => item.role === "updater-signature");
  check(updater && signature, "updater_signature_unavailable");
  const work = await mkdtemp(path.join(tmpdir(), "axial-rewrite-signature-"));
  try {
    const key = minisignText(await readFile(publicKeyFile, "utf8"), "key");
    await regularFile(path.join(directory, signature.name), 8192);
    await writeFile(path.join(work, "publisher.pub"), key, { flag: "wx" });
    for (const [artifact, sig] of [[updater.name, signature.name], ["artifacts.json", "artifacts.json.sig"]]) {
      await regularFile(path.join(directory, sig), 8192);
      const decoded = path.join(work, `${sig}.decoded`);
      await writeFile(decoded, minisignText(await readFile(path.join(directory, sig), "utf8"), "signature"), { flag: "wx" });
      await exec("minisign", ["-Vm", path.resolve(directory, artifact), "-p", path.join(work, "publisher.pub"), "-x", decoded], { timeout: 60000, maxBuffer: 8192 });
    }
    return { publisher_key_sha256: createHash("sha256").update(key).digest("hex"), artifact_sha256: updater.sha256 };
  } finally {
    await rm(work, { recursive: true, force: true });
  }
}

export function options(argv, allowed, required = allowed) {
  const result = {};
  for (let index = 0; index < argv.length; index += 2) {
    const key = argv[index]?.replace(/^--/, "");
    check(argv[index]?.startsWith("--") && allowed.includes(key) && !Object.hasOwn(result, key) && argv[index + 1] && !argv[index + 1].startsWith("--"), "invalid_cli_arguments");
    result[key] = argv[index + 1];
  }
  check(required.every((key) => result[key]), "missing_cli_argument");
  return result;
}

export function main(moduleUrl, run) {
  if (process.argv[1] && pathToFileURL(path.resolve(process.argv[1])).href === moduleUrl) {
    run().then((result) => console.log(JSON.stringify(result))).catch((error) => {
      console.error(`delivery: ${String(error.message).split("\n")[0].slice(0, 240)}`);
      process.exitCode = 1;
    });
  }
}

main(import.meta.url, async () => {
  const [command, ...args] = process.argv.slice(2);
  if (command === "verify-source") {
    const o = options(args, ["tag", "sha", "metadata", "output"]);
    const source = await verifySource({ tag: o.tag, sourceSha: o.sha, metadata: await jsonFile(o.metadata) });
    await writeJson(o.output, source);
    return source;
  }
  if (command === "verify-artifacts") {
    const o = options(args, ["source", "assets", "platform"]);
    return verifyArtifacts(o.assets, await jsonFile(o.source), o.platform);
  }
  if (command === "matrix") return PLATFORMS;
  throw new Error("unknown_release_command");
});

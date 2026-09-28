import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { promisify } from "node:util";
import { changelogRelease, verifyArtifacts, verifySource, verifyUpdaterSignature } from "./contract.mjs";
import { payloads, releaseIdentity } from "./matrix.mjs";

const exec = promisify(execFile);
const APP_ID = "com.mateoltd.axial.rewrite";
const VERSION = "9.8.7-rc.1";
const TAG = `v${VERSION}`;
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

async function temporary(t) {
  const root = await mkdtemp(path.join(tmpdir(), "axial-delivery-contract-test-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  return root;
}

async function put(root, relative, value) {
  const file = path.join(root, relative);
  await mkdir(path.dirname(file), { recursive: true });
  await writeFile(file, typeof value === "string" ? value : JSON.stringify(value));
  return file;
}

async function sourceFixture(t) {
  const root = await temporary(t);
  const git = async (...args) => (await exec("git", [
    "-c", "user.name=Axial synthetic fixture", "-c", "user.email=fixture@example.invalid",
    "-c", "commit.gpgsign=false", "-c", `core.hooksPath=${path.join(root, "disabled-hooks")}`,
    ...args,
  ], { cwd: root })).stdout.trim();
  await git("init", "--quiet", "--template=");
  const packages = [];
  for (const relative of ["core/app", "apps/api", "apps/desktop"]) {
    const name = relative.replaceAll("/", "-");
    const manifest = await put(root, `${relative}/Cargo.toml`, `[package]\nname = "${name}"\nversion.workspace = true\n`);
    packages.push({ id: name, version: VERSION, manifest_path: manifest });
  }
  await put(root, "Cargo.toml", `[workspace]\nmembers = ["core/app", "apps/api", "apps/desktop"]\n[workspace.package]\nversion = "${VERSION}"\n`);
  await put(root, "apps/desktop/tauri.conf.json", { identifier: APP_ID, version: VERSION });
  await put(root, "frontend/package.json", { name: "synthetic-fixture", version: VERSION });
  await put(root, "CHANGELOG.md", `# Synthetic fixture\n\n## [Unreleased]\n\n## [${VERSION}] - 2026-09-20\n\nFixture release notes.\n`);
  await git("add", ".");
  await git("commit", "--quiet", "-m", "test: create isolated source fixture");
  await git("tag", TAG);
  return { root, tag: TAG, sourceSha: await git("rev-parse", "HEAD"), git,
    metadata: { workspace_root: root, workspace_members: packages.map(({ id }) => id), packages } };
}

// Independent expected filenames make these fixtures sensitive to matrix drift.
const ARTIFACTS = {
  "linux-amd64": [["", "portable"], [".tar.gz", "portable-archive"], [".AppImage", "updater"]],
  "windows-amd64": [[".exe", "portable"], [".zip", "portable-archive"], ["-setup.exe", "updater"]],
  "macos-amd64": [[".tar.gz", "portable-archive"], [".dmg", "installer"], [".app.tar.gz", "updater"]],
  "macos-arm64": [[".tar.gz", "portable-archive"], [".dmg", "installer"], [".app.tar.gz", "updater"]],
};

async function artifactFixture(t, platform = "linux-amd64") {
  const root = await temporary(t);
  const source = { schema: "axial.rewrite.source.v1", app_id: APP_ID, tag: TAG, version: VERSION, source_sha: "a".repeat(40) };
  const files = [];
  for (const [suffix, role] of ARTIFACTS[platform]) {
    const name = `axial-rewrite-${platform}-${VERSION}${suffix}`;
    const bytes = `Synthetic ${role} bytes; not an installable artifact.\n`;
    const hash = sha256(bytes);
    await put(root, name, bytes);
    await put(root, `${name}.sha256`, `${hash}  ${name}\n`);
    files.push({ name, role, bytes: Buffer.byteLength(bytes), sha256: hash });
  }
  const manifest = { schema: "axial.rewrite.artifacts.v1", app_id: APP_ID, platform,
    tag: TAG, source_sha: source.source_sha, files, authenticity: "not-verified", native_acceptance: "not-run" };
  await put(root, "artifacts.json", manifest);
  return { root, source, platform, manifest };
}

test("release identities and the four artifact layouts stay explicit", () => {
  assert.deepEqual(releaseIdentity(TAG), { tag: TAG, version: VERSION });
  for (const invalid of ["9.8.7", "v01.2.3", "v1.2", "v1.2.3-rc.0", "v1.2.3+build", "v1.2.3/../../other", null]) {
    assert.throws(() => releaseIdentity(invalid), { message: "invalid_release_tag" });
  }
  for (const [platform, expected] of Object.entries(ARTIFACTS)) {
    assert.deepEqual(payloads(platform, TAG), expected.map(([suffix, role]) => ({ name: `axial-rewrite-${platform}-${VERSION}${suffix}`, role })));
  }
  assert.throws(() => payloads("linux-arm64", TAG), { message: "unsupported_artifact_platform" });
});

test("source verification binds a clean isolated Git tag to inherited versions and changelog", async (t) => {
  const fixture = await sourceFixture(t);
  const receipt = await verifySource(fixture);
  assert.equal(receipt.source_sha, fixture.sourceSha);
  assert.equal(receipt.version, VERSION);
  assert.equal(receipt.notes, "Fixture release notes.");
  assert.equal(receipt.changelog_sha256, sha256(await readFile(path.join(fixture.root, "CHANGELOG.md"))));
});

test("source verification rejects mismatched metadata and dirty or retagged source", async (t) => {
  const cases = [
    ["version mismatch", "workspace_version_mismatch", async (f) => { f.metadata.packages[0].version = "0.0.1"; }],
    ["duplicate members", "duplicate_workspace_member", async (f) => { f.metadata.workspace_members[2] = f.metadata.workspace_members[0]; }],
    ["missing workspace member", "incomplete_workspace", async (f) => { f.metadata.workspace_members.pop(); }],
    ["wrong workspace root", "metadata_workspace_mismatch", async (f) => { f.metadata.workspace_root = path.join(f.root, "core"); }],
    ["noninherited package version", "package_version_not_inherited", async (f) => { await writeFile(f.metadata.packages[0].manifest_path, `[package]\nname="fixture"\nversion="${VERSION}"\n`); }],
    ["legacy manifest", "manifest_outside_rewrite", async (f) => { f.metadata.packages[0].manifest_path = await put(f.root, "legacy/fixture/Cargo.toml", "[package]\nversion.workspace = true\n"); }],
    ["wrong app identity", "application_identity_mismatch", async (f) => { await put(f.root, "apps/desktop/tauri.conf.json", { identifier: "com.mateoltd.axial", version: VERSION }); }],
    ["wrong frontend version", "frontend_version_mismatch", async (f) => { await put(f.root, "frontend/package.json", { version: "0.0.1" }); }],
    ["untracked changes", "source_tree_not_clean", async (f) => { await put(f.root, "untracked.txt", "uncommitted fixture\n"); }],
    ["incorrect source commit", "source_head_mismatch", async (f) => { f.sourceSha = "f".repeat(40); }],
    ["tag points to another commit", "tag_commit_mismatch", async (f) => {
      await put(f.root, "another.txt", "second fixture commit\n");
      await f.git("add", ".");
      await f.git("commit", "--quiet", "-m", "test: diverge from fixture tag");
      f.sourceSha = await f.git("rev-parse", "HEAD");
    }],
  ];
  for (const [name, message, mutate] of cases) {
    await t.test(name, async (t) => {
      const fixture = await sourceFixture(t);
      await mutate(fixture);
      await assert.rejects(verifySource(fixture), { message });
    });
  }
});

test("changelog rejects duplicate, impossible-date, empty and nonlatest releases", () => {
  const heading = `## [Unreleased]\n\n## [${VERSION}] - 2026-09-20\n\nNotes.\n`;
  assert.throws(() => changelogRelease(`${heading}\n## [${VERSION}] - 2026-09-19\nOld.`, VERSION), { message: "duplicate_changelog_release" });
  assert.throws(() => changelogRelease(heading.replace("2026-09-20", "2026-02-30"), VERSION), { message: "invalid_changelog_date" });
  assert.throws(() => changelogRelease(heading.replace("Notes.", ""), VERSION), { message: "invalid_release_notes" });
  assert.throws(() => changelogRelease(heading, "1.0.0"), { message: "release_not_latest_changelog_section" });
});

test("artifact integrity accepts complete candidate bytes without claiming authenticity", async (t) => {
  for (const platform of Object.keys(ARTIFACTS)) {
    await t.test(platform, async (t) => {
      const fixture = await artifactFixture(t, platform);
      const manifest = await verifyArtifacts(fixture.root, fixture.source, platform);
      assert.equal(manifest.authenticity, "not-verified");
      assert.equal(manifest.native_acceptance, "not-run");
    });
  }
});

test("artifact verification rejects corruption, incomplete sets and substituted identities", async (t) => {
  const cases = [
    ["changed payload", "artifact_digest_mismatch", async (f) => { await put(f.root, f.manifest.files[0].name, "tampered bytes"); }],
    ["incorrect recorded length", "artifact_digest_mismatch", async (f) => { f.manifest.files[0].bytes += 1; }],
    ["incorrect checksum text", "artifact_checksum_mismatch", async (f) => { await put(f.root, `${f.manifest.files[0].name}.sha256`, "0".repeat(64)); }],
    ["missing checksum", "artifact_directory_incomplete_or_unexpected", async (f) => { await rm(path.join(f.root, `${f.manifest.files[0].name}.sha256`)); }],
    ["unexpected payload", "artifact_directory_incomplete_or_unexpected", async (f) => { await put(f.root, "other.exe", "not admitted"); }],
    ["duplicate record", "invalid_artifact_record", async (f) => { f.manifest.files[1] = structuredClone(f.manifest.files[0]); }],
    ["incorrect role", "invalid_artifact_record", async (f) => { f.manifest.files[0].role = "updater"; }],
    ["other source commit", "artifact_source_mismatch", async (f) => { f.manifest.source_sha = "b".repeat(40); }],
    ["other platform", "artifact_source_mismatch", async (f) => { f.manifest.platform = "windows-amd64"; }],
    ["wrong source version", "source_version_mismatch", async (f) => { f.source.version = "0.0.1"; }],
    ["symlink payload", "invalid_regular_file", async (f) => {
      const file = path.join(f.root, f.manifest.files[0].name);
      await rm(file);
      await symlink(path.join(f.root, f.manifest.files[1].name), file);
    }],
  ];
  for (const [name, message, mutate] of cases) {
    await t.test(name, async (t) => {
      const fixture = await artifactFixture(t);
      await mutate(fixture);
      await put(fixture.root, "artifacts.json", fixture.manifest);
      await assert.rejects(verifyArtifacts(fixture.root, fixture.source, fixture.platform), { message });
    });
  }
});

test("checksums and a signature filename cannot substitute for publisher trust", async (t) => {
  const fixture = await artifactFixture(t);
  await assert.rejects(verifyUpdaterSignature(fixture.root, fixture.manifest), { message: "publisher_key_unavailable" });
  const key = await put(fixture.root, "untrusted-key.txt", "this is not a trusted publisher key");
  await assert.rejects(verifyUpdaterSignature(fixture.root, fixture.manifest, key), { message: "updater_signature_unavailable" });
  const signature = `${fixture.manifest.files.find(({ role }) => role === "updater").name}.sig`;
  await put(fixture.root, signature, "a filename is not a signature");
  fixture.manifest.files.push({ name: signature, role: "updater-signature" });
  await assert.rejects(verifyUpdaterSignature(fixture.root, fixture.manifest, key), { message: "invalid_minisign_encoding" });
});

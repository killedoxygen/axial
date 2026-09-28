import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { releaseReadiness, verifyNativeReceipt } from "./verify.mjs";
import { NATIVE_CASES } from "./matrix.mjs";
import { payloads } from "../../scripts/release/matrix.mjs";

const exec = promisify(execFile);
const APP_ID = "com.mateoltd.axial.rewrite";
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

async function temporary(t) {
  const root = await mkdtemp(path.join(tmpdir(), "axial-native-contract-test-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  return root;
}

async function writeJson(file, value) {
  await writeFile(file, JSON.stringify(value));
}

async function receiptFixture(t) {
  const root = await temporary(t);
  const directory = path.join(root, "receipt");
  await mkdir(directory);
  const bytes = "Synthetic verifier input. This is not installed application evidence.\n";
  await writeFile(path.join(directory, "fixture.txt"), bytes);
  const manifest = { platform: "linux-amd64", source_sha: "a".repeat(40), tag: "v9.8.7", files: [
    { name: "synthetic.AppImage", role: "updater", sha256: "b".repeat(64) },
    { name: "synthetic-installer", role: "installer", sha256: "c".repeat(64) },
  ] };
  // A structurally complete fixture tests validation only. It stays in the OS
  // temporary directory and is never accepted as actual release evidence.
  const receipt = { schema: "axial.rewrite.native.v1", app_id: APP_ID, platform: manifest.platform,
    source_sha: manifest.source_sha, tag: manifest.tag,
    host: { os: "linux", arch: "x64", os_release: "synthetic host", webview_version: "synthetic webview" },
    environment: "clean-machine", installation: "installed-package",
    profile: { isolated: true, app_id: APP_ID, baseline_untouched: true },
    observer: "synthetic test fixture", observed_at: "2026-09-20T12:00:00Z",
    artifacts: [{ name: manifest.files[0].name, sha256: manifest.files[0].sha256 }],
    cases: NATIVE_CASES.map((id) => ({ id, status: "passed", mode: "installed",
      observed: "Synthetic contract input only; no real native observation.",
      evidence: [{ path: "fixture.txt", sha256: sha256(bytes) }] })),
    update: { from_version: "9.8.6", to_version: "9.8.7", previous_artifact_sha256: "d".repeat(64),
      profile_before_sha256: "e".repeat(64), profile_after_sha256: "e".repeat(64) },
  };
  const file = path.join(directory, "receipt.json");
  await writeJson(file, receipt);
  return { root, directory, file, receipt, manifest };
}

test("native case matrix retains install, transport, process and update obligations", () => {
  assert.deepEqual(NATIVE_CASES, [
    "clean-machine-install", "platform-publisher-trust", "installed-start", "authenticated-json",
    "scoped-media", "sse-reconnect", "navigation-csp", "native-dialogs", "window-chrome",
    "offline-vanilla-journey", "close-busy-refusal", "shutdown-process-tree", "profile-restart-persistence",
    "update-check-download-stage", "update-reject-corrupt", "update-reject-untrusted", "update-busy-refusal",
    "installed-update-restart", "update-interruption-recovery", "update-profile-preservation",
  ]);
});

test("native receipt parser accepts a complete synthetic schema fixture", async (t) => {
  const fixture = await receiptFixture(t);
  assert.deepEqual(await verifyNativeReceipt(fixture.file, fixture.manifest), fixture.receipt);
});

test("native receipt rejects incomplete observations and incorrect artifact or host bindings", async (t) => {
  const cases = [
    ["wrong source", "native_source_mismatch", (r) => { r.source_sha = "f".repeat(40); }],
    ["wrong architecture", "native_host_mismatch", (r) => { r.host.arch = "arm64"; }],
    ["missing webview version", "native_host_details_missing", (r) => { delete r.host.webview_version; }],
    ["development execution", "native_evidence_not_installed", (r) => { r.installation = "development-binary"; }],
    ["nonisolated profile", "native_profile_not_isolated", (r) => { r.profile.isolated = false; }],
    ["baseline was changed", "native_profile_not_isolated", (r) => { r.profile.baseline_untouched = false; }],
    ["missing observer", "native_observer_missing", (r) => { r.observer = " "; }],
    ["invalid observation time", "native_observation_time_missing", (r) => { r.observed_at = "not a date"; }],
    ["unbound artifact", "native_artifact_binding_mismatch", (r) => { r.artifacts[0].sha256 = "f".repeat(64); }],
    ["updater never tested", "native_updater_artifact_not_tested", (r, f) => { r.artifacts = [{ name: f.manifest.files[1].name, sha256: f.manifest.files[1].sha256 }]; }],
    ["omitted case", "native_cases_incomplete", (r) => { r.cases.pop(); }],
    ["duplicated case", "native_case_missing_or_duplicate", (r) => { r.cases[1] = structuredClone(r.cases[0]); }],
    ["failed case", "native_case_not_observed:clean-machine-install", (r) => { r.cases[0].status = "failed"; }],
    ["fixture case mode", "native_case_not_observed:clean-machine-install", (r) => { r.cases[0].mode = "fixture"; }],
    ["missing observation", "native_case_not_observed:clean-machine-install", (r) => { r.cases[0].observed = "passed"; }],
    ["missing evidence", "native_case_evidence_missing:clean-machine-install", (r) => { r.cases[0].evidence = []; }],
    ["same update version", "installed_update_versions_missing", (r) => { r.update.from_version = r.update.to_version; }],
    ["changed update profile", "installed_update_preservation_missing", (r) => { r.update.profile_after_sha256 = "f".repeat(64); }],
  ];
  for (const [name, message, mutate] of cases) {
    await t.test(name, async (t) => {
      const fixture = await receiptFixture(t);
      mutate(fixture.receipt, fixture);
      await writeJson(fixture.file, fixture.receipt);
      await assert.rejects(verifyNativeReceipt(fixture.file, fixture.manifest), { message });
    });
  }
});

test("native evidence must remain inside the receipt directory and match its digest", async (t) => {
  const cases = [
    ["parent traversal", "invalid_evidence_reference", async (f, item) => { item.path = "../outside.txt"; }],
    ["Windows parent traversal", "invalid_evidence_reference", async (f, item) => { item.path = "..\\outside.txt"; }],
    ["absolute path", "invalid_evidence_reference", async (f, item) => { item.path = path.join(f.directory, "fixture.txt"); }],
    ["changed evidence bytes", "evidence_digest_mismatch", async (f) => { await writeFile(path.join(f.directory, "fixture.txt"), "changed bytes"); }],
    ["escaping symlink", "evidence_outside_receipt_directory", async (f, item) => {
      await writeFile(path.join(f.root, "outside.txt"), "unrelated file");
      await symlink(path.join(f.root, "outside.txt"), path.join(f.directory, "escape.txt"));
      item.path = "escape.txt";
    }],
  ];
  for (const [name, message, mutate] of cases) {
    await t.test(name, async (t) => {
      const fixture = await receiptFixture(t);
      await mutate(fixture, fixture.receipt.cases[0].evidence[0]);
      await writeJson(fixture.file, fixture.receipt);
      await assert.rejects(verifyNativeReceipt(fixture.file, fixture.manifest), { message });
    });
  }
  await t.test("missing evidence file", async (t) => {
    const fixture = await receiptFixture(t);
    await rm(path.join(fixture.directory, "fixture.txt"));
    await assert.rejects(verifyNativeReceipt(fixture.file, fixture.manifest), { code: "ENOENT" });
  });
});

test("all four intact unsigned candidates remain unready without installed evidence", async (t) => {
  const root = await temporary(t);
  const assets = path.join(root, "assets");
  const evidence = path.join(root, "evidence");
  await mkdir(evidence);
  const source = { schema: "axial.rewrite.source.v1", app_id: APP_ID, tag: "v9.8.7", version: "9.8.7", source_sha: "a".repeat(40) };
  for (const platform of ["linux-amd64", "windows-amd64", "macos-amd64", "macos-arm64"]) {
    const directory = path.join(assets, platform);
    await mkdir(directory, { recursive: true });
    const files = [];
    for (const item of payloads(platform, source.tag)) {
      const bytes = `Synthetic candidate: ${item.name}\n`;
      const hash = sha256(bytes);
      await writeFile(path.join(directory, item.name), bytes);
      await writeFile(path.join(directory, `${item.name}.sha256`), `${hash}  ${item.name}\n`);
      files.push({ ...item, sha256: hash, bytes: Buffer.byteLength(bytes) });
    }
    await writeJson(path.join(directory, "artifacts.json"), { schema: "axial.rewrite.artifacts.v1", app_id: APP_ID,
      platform, source_sha: source.source_sha, tag: source.tag, files });
  }
  const readiness = await releaseReadiness({ source, assets, evidence });
  assert.equal(readiness.release_ready, false);
  assert.equal(readiness.platforms.length, 4);
  for (const platform of readiness.platforms) {
    assert.equal(platform.integrity, "passed");
    assert.equal(platform.authenticity, "not-verified");
    assert.equal(platform.installed, "not-verified");
    assert.deepEqual(platform.gaps, ["authenticity: publisher_key_unavailable", "installed: ENOENT"]);
  }
  const sourceFile = path.join(root, "source.json");
  await writeJson(sourceFile, source);
  const verifier = fileURLToPath(new URL("./verify.mjs", import.meta.url));
  await assert.rejects(exec(process.execPath, [verifier, "--source", sourceFile, "--assets", assets, "--evidence", evidence]), (error) => {
    assert.equal(error.code, 1);
    assert.equal(JSON.parse(error.stdout).release_ready, false);
    return true;
  });
});

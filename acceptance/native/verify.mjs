#!/usr/bin/env node
import path from "node:path";
import { realpath } from "node:fs/promises";
import { APPLICATION_ID, PLATFORMS } from "../../scripts/release/matrix.mjs";
import { check, digest, jsonFile, main, options, SHA, verifyArtifacts, verifyUpdaterSignature } from "../../scripts/release/contract.mjs";
import { NATIVE_CASES } from "./matrix.mjs";

async function evidenceFile(directory, relative, sha256) {
  check(typeof relative === "string" && relative.length > 0 && !path.isAbsolute(relative) && !relative.split(/[\\/]/).includes("..") && SHA.test(sha256), "invalid_evidence_reference");
  const root = await realpath(directory);
  const file = await realpath(path.join(root, relative));
  const inside = path.relative(root, file);
  check(inside && !inside.startsWith("..") && !path.isAbsolute(inside), "evidence_outside_receipt_directory");
  check(await digest(file) === sha256, "evidence_digest_mismatch");
}

export async function verifyNativeReceipt(file, manifest) {
  const receipt = await jsonFile(file);
  check(receipt.schema === "axial.rewrite.native.v1" && receipt.app_id === APPLICATION_ID, "invalid_native_receipt");
  check(receipt.platform === manifest.platform && receipt.source_sha === manifest.source_sha && receipt.tag === manifest.tag, "native_source_mismatch");
  const host = PLATFORMS[manifest.platform];
  check(receipt.host?.os === host.os && receipt.host?.arch === host.arch, "native_host_mismatch");
  check(typeof receipt.host.os_release === "string" && receipt.host.os_release.length > 0 && typeof receipt.host.webview_version === "string" && receipt.host.webview_version.length > 0, "native_host_details_missing");
  check(receipt.environment === "clean-machine" && receipt.installation === "installed-package", "native_evidence_not_installed");
  check(receipt.profile?.isolated === true && receipt.profile.app_id === APPLICATION_ID && receipt.profile.baseline_untouched === true, "native_profile_not_isolated");
  check(typeof receipt.observer === "string" && receipt.observer.trim().length >= 3 && receipt.observer.length <= 240, "native_observer_missing");
  check(typeof receipt.observed_at === "string" && !Number.isNaN(Date.parse(receipt.observed_at)), "native_observation_time_missing");
  check(Array.isArray(receipt.artifacts) && receipt.artifacts.length > 0, "native_artifact_binding_missing");
  const installable = manifest.files.filter((item) => ["installer", "updater"].includes(item.role));
  check(receipt.artifacts.every((item) => installable.some((artifact) => artifact.name === item.name && artifact.sha256 === item.sha256)), "native_artifact_binding_mismatch");
  check(receipt.artifacts.some((item) => manifest.files.some((artifact) => artifact.role === "updater" && artifact.name === item.name)), "native_updater_artifact_not_tested");
  check(Array.isArray(receipt.cases) && receipt.cases.length === NATIVE_CASES.length, "native_cases_incomplete");
  for (const id of NATIVE_CASES) {
    const matches = receipt.cases.filter((item) => item.id === id);
    check(matches.length === 1, "native_case_missing_or_duplicate");
    const result = matches[0];
    check(result.status === "passed" && result.mode === "installed" && typeof result.observed === "string" && result.observed.trim().length >= 12, `native_case_not_observed:${id}`);
    check(Array.isArray(result.evidence) && result.evidence.length > 0, `native_case_evidence_missing:${id}`);
    for (const item of result.evidence) await evidenceFile(path.dirname(file), item.path, item.sha256);
  }
  check(receipt.update?.from_version && receipt.update.to_version === manifest.tag.slice(1) && receipt.update.from_version !== receipt.update.to_version, "installed_update_versions_missing");
  check(SHA.test(receipt.update.previous_artifact_sha256) && SHA.test(receipt.update.profile_before_sha256) && receipt.update.profile_before_sha256 === receipt.update.profile_after_sha256, "installed_update_preservation_missing");
  return receipt;
}

export async function releaseReadiness({ source, assets, evidence, publicKey }) {
  const result = { schema: "axial.rewrite.readiness.v1", source_sha: source.source_sha, tag: source.tag, release_ready: false, platforms: [] };
  for (const platform of Object.keys(PLATFORMS)) {
    const row = { platform, integrity: "not-verified", authenticity: "not-verified", installed: "not-verified", gaps: [] };
    let manifest;
    try {
      manifest = await verifyArtifacts(path.join(assets, platform), source, platform);
      row.integrity = "passed";
    } catch (error) { row.gaps.push(`artifacts: ${error.code ?? error.message}`); }
    if (manifest) {
      try {
        row.publisher = await verifyUpdaterSignature(path.join(assets, platform), manifest, publicKey);
        row.authenticity = "passed";
      } catch (error) { row.gaps.push(`authenticity: ${error.code ?? error.message}`); }
      try {
        await verifyNativeReceipt(path.join(evidence, platform, "receipt.json"), manifest);
        row.installed = "passed";
      } catch (error) { row.gaps.push(`installed: ${error.code ?? error.message}`); }
    }
    result.platforms.push(row);
  }
  result.release_ready = result.platforms.every((row) => row.gaps.length === 0);
  return result;
}

main(import.meta.url, async () => {
  const o = options(process.argv.slice(2), ["source", "assets", "evidence", "public-key"], ["source", "assets", "evidence"]);
  const result = await releaseReadiness({ source: await jsonFile(o.source), assets: o.assets, evidence: o.evidence, publicKey: o["public-key"] });
  if (!result.release_ready) process.exitCode = 1;
  return result;
});

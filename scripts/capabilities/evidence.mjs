import { createHash } from "node:crypto";
import { lstat, mkdir, open, realpath, rename, rm } from "node:fs/promises";
import path from "node:path";

const HEX_40 = /^[0-9a-f]{40}$/;
const HEX_64 = /^[0-9a-f]{64}$/;
const CLOSED_ID = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;
const OBSERVATION_LIMIT = 64;
const ARTIFACT_LIMIT = 64;
const RECEIPT_BYTE_LIMIT = 1024 * 1024;

export class EvidenceError extends Error {
  constructor(code) {
    super(code);
    this.name = "EvidenceError";
    this.code = code;
  }
}

function fail(code) {
  throw new EvidenceError(code);
}

function isPlainObject(value) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return false;
  }
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

function requireExactKeys(value, keys, code) {
  if (!isPlainObject(value)) {
    fail(code);
  }
  const actual = Object.keys(value).sort();
  const expected = [...keys].sort();
  if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) {
    fail(code);
  }
}

function requireClosedId(value, code) {
  if (typeof value !== "string" || value.length === 0 || value.length > 96 || !CLOSED_ID.test(value)) {
    fail(code);
  }
}

function requireHash(value, pattern, code) {
  if (typeof value !== "string" || !pattern.test(value)) {
    fail(code);
  }
}

function requireRfc3339Utc(value, code) {
  if (typeof value !== "string") fail(code);
  try {
    if (new Date(value).toISOString() !== value) fail(code);
  } catch {
    fail(code);
  }
}

function requireJsonValue(value, depth = 0) {
  if (depth > 16) {
    fail("invalid_json_depth");
  }
  if (value === null || typeof value === "string" || typeof value === "boolean") {
    return;
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) fail("invalid_json_number");
    return;
  }
  if (Array.isArray(value)) {
    if (value.length > 256) fail("invalid_json_array");
    for (const item of value) requireJsonValue(item, depth + 1);
    return;
  }
  if (!isPlainObject(value) || Object.keys(value).length > 256) {
    fail("invalid_json_object");
  }
  for (const [key, item] of Object.entries(value)) {
    if (!/^[A-Za-z0-9_.-]{1,96}$/.test(key)) fail("invalid_json_key");
    requireJsonValue(item, depth + 1);
  }
}

export function canonicalize(value) {
  if (Array.isArray(value)) {
    return value.map(canonicalize);
  }
  if (!isPlainObject(value)) {
    return value;
  }
  return Object.fromEntries(
    Object.keys(value)
      .sort()
      .map((key) => [key, canonicalize(value[key])]),
  );
}

export function canonicalJson(value) {
  requireJsonValue(value);
  return `${JSON.stringify(canonicalize(value), null, 2)}\n`;
}

export function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

function normalizeRepoPath(value) {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    value.length > 512 ||
    value.includes("\\") ||
    !/^[A-Za-z0-9._/-]+$/.test(value)
  ) {
    fail("invalid_artifact_path");
  }
  if (path.posix.isAbsolute(value) || path.posix.normalize(value) !== value) {
    fail("invalid_artifact_path");
  }
  const segments = value.split("/");
  if (segments.some((segment) => segment === "" || segment === "." || segment === "..")) {
    fail("invalid_artifact_path");
  }
  return value;
}

async function assertRegularPathInside(root, relativePath) {
  const rootReal = await realpath(root).catch(() => fail("invalid_repository_root"));
  const candidate = path.resolve(rootReal, ...relativePath.split("/"));
  const candidateReal = await realpath(candidate).catch(() => fail("artifact_absent"));
  if (candidate !== candidateReal || !candidateReal.startsWith(`${rootReal}${path.sep}`)) {
    fail("invalid_artifact_path");
  }
  const metadata = await lstat(candidateReal);
  if (!metadata.isFile() || metadata.isSymbolicLink()) {
    fail("invalid_artifact_path");
  }
  return { path: candidateReal, bytes: metadata.size };
}

async function hashArtifact(source) {
  const handle = await open(source.path, "r");
  try {
    const before = await handle.stat();
    if (!before.isFile() || before.size !== source.bytes) fail("artifact_changed");
    const digest = createHash("sha256");
    const buffer = Buffer.allocUnsafe(64 * 1024);
    let bytes = 0;
    for (;;) {
      const { bytesRead } = await handle.read(buffer, 0, buffer.length, null);
      if (bytesRead === 0) break;
      digest.update(buffer.subarray(0, bytesRead));
      bytes += bytesRead;
    }
    const after = await handle.stat();
    if (bytes !== before.size || after.size !== before.size || after.mtimeMs !== before.mtimeMs) {
      fail("artifact_changed");
    }
    return { sha256: digest.digest("hex"), bytes };
  } finally {
    await handle.close();
  }
}

export async function sealScenarioResult(result, repoRoot) {
  if (isPlainObject(result) && Object.keys(result).length === 1 && result.ok === false) {
    fail("scenario_failed");
  }
  requireExactKeys(result, ["ok", "observations", "artifacts"], "malformed_scenario_result");
  if (result.ok !== true) fail("scenario_failed");
  if (!Array.isArray(result.observations) || result.observations.length === 0 || result.observations.length > OBSERVATION_LIMIT) {
    fail("invalid_observations");
  }
  if (!Array.isArray(result.artifacts) || result.artifacts.length > ARTIFACT_LIMIT) {
    fail("invalid_artifacts");
  }

  const observationIds = new Set();
  const observations = result.observations.map((observation) => {
    requireExactKeys(observation, ["id", "outcome", "receipt"], "invalid_observation");
    requireClosedId(observation.id, "invalid_observation_id");
    if (observationIds.has(observation.id)) fail("duplicate_observation_id");
    observationIds.add(observation.id);
    if (observation.outcome !== "pass") fail("scenario_failed");
    requireJsonValue(observation.receipt);
    const receipt = canonicalJson(observation.receipt);
    if (Buffer.byteLength(receipt) > RECEIPT_BYTE_LIMIT) fail("invalid_receipt");
    return {
      id: observation.id,
      outcome: "pass",
      receipt_sha256: sha256(receipt),
    };
  });

  const artifactIds = new Set();
  const artifactPaths = new Set();
  const artifacts = [];
  for (const artifact of result.artifacts) {
    requireExactKeys(artifact, ["id", "repo_relative_path"], "invalid_artifact");
    requireClosedId(artifact.id, "invalid_artifact_id");
    const relativePath = normalizeRepoPath(artifact.repo_relative_path);
    if (artifactIds.has(artifact.id) || artifactPaths.has(relativePath)) fail("duplicate_artifact");
    artifactIds.add(artifact.id);
    artifactPaths.add(relativePath);
    const source = await assertRegularPathInside(repoRoot, relativePath);
    const sealed = await hashArtifact(source);
    artifacts.push({
      id: artifact.id,
      repo_relative_path: relativePath,
      sha256: sealed.sha256,
      bytes: sealed.bytes,
    });
  }

  const byId = (left, right) => (left.id < right.id ? -1 : left.id > right.id ? 1 : 0);
  observations.sort(byId);
  artifacts.sort(byId);
  return { observations, artifacts };
}

function validatePlatform(platform) {
  requireExactKeys(platform, ["os", "arch", "runner_image_os", "runner_image_version"], "invalid_evidence_platform");
  if (!["linux", "windows", "macos", "browser"].includes(platform.os)) fail("invalid_evidence_platform");
  if (!["x64", "arm64"].includes(platform.arch)) fail("invalid_evidence_platform");
  for (const key of ["runner_image_os", "runner_image_version"]) {
    if (platform[key] !== null && (typeof platform[key] !== "string" || !/^[A-Za-z0-9_.-]{1,128}$/.test(platform[key]))) {
      fail("invalid_evidence_platform");
    }
  }
}

export function validateEvidenceDocument(document) {
  requireExactKeys(
    document,
    ["schema_version", "result", "proof_id", "scenario_id", "capability_id", "owner_phase", "source", "platform", "toolchain", "timing", "observations", "artifacts"],
    "invalid_evidence",
  );
  if (document.schema_version !== 1 || document.result !== "verified") fail("invalid_evidence");
  if (!/^(?:CAP|PM)-[A-Z0-9]+(?:-[A-Z0-9]+)*$/.test(document.proof_id)) fail("invalid_evidence_proof");
  if (!/^(?:CP|PM)-[A-Z0-9]+(?:-[A-Z0-9]+)*$/.test(document.scenario_id)) fail("invalid_evidence_scenario");
  requireClosedId(document.capability_id, "invalid_evidence_capability");
  if (!/^P(?:0[0-9]|1[0-4])$/.test(document.owner_phase)) fail("invalid_evidence_phase");

  requireExactKeys(document.source, ["commit", "tree"], "invalid_evidence_source");
  requireHash(document.source.commit, HEX_40, "invalid_evidence_source");
  requireHash(document.source.tree, HEX_40, "invalid_evidence_source");
  validatePlatform(document.platform);

  requireExactKeys(document.toolchain, ["manifest_sha256", "identity"], "invalid_evidence_toolchain");
  requireHash(document.toolchain.manifest_sha256, HEX_64, "invalid_evidence_toolchain");
  requireJsonValue(document.toolchain.identity);
  if (!isPlainObject(document.toolchain.identity)) fail("invalid_evidence_toolchain");

  requireExactKeys(document.timing, ["started_at", "completed_at", "duration_ms"], "invalid_evidence_timing");
  requireRfc3339Utc(document.timing.started_at, "invalid_evidence_timing");
  requireRfc3339Utc(document.timing.completed_at, "invalid_evidence_timing");
  if (!Number.isSafeInteger(document.timing.duration_ms) || document.timing.duration_ms < 0) fail("invalid_evidence_timing");
  const elapsed = Date.parse(document.timing.completed_at) - Date.parse(document.timing.started_at);
  if (elapsed !== document.timing.duration_ms) fail("invalid_evidence_timing");

  if (!Array.isArray(document.observations) || document.observations.length === 0 || document.observations.length > OBSERVATION_LIMIT) fail("invalid_evidence_observations");
  if (!Array.isArray(document.artifacts) || document.artifacts.length > ARTIFACT_LIMIT) fail("invalid_evidence_artifacts");
  const observations = new Set();
  let previousObservation = "";
  for (const observation of document.observations) {
    requireExactKeys(observation, ["id", "outcome", "receipt_sha256"], "invalid_evidence_observation");
    requireClosedId(observation.id, "invalid_evidence_observation");
    if (observations.has(observation.id) || observation.id <= previousObservation || observation.outcome !== "pass") {
      fail("invalid_evidence_observation");
    }
    observations.add(observation.id);
    previousObservation = observation.id;
    requireHash(observation.receipt_sha256, HEX_64, "invalid_evidence_observation");
  }
  const artifacts = new Set();
  const artifactPaths = new Set();
  let previousArtifact = "";
  for (const artifact of document.artifacts) {
    requireExactKeys(artifact, ["id", "repo_relative_path", "sha256", "bytes"], "invalid_evidence_artifact");
    requireClosedId(artifact.id, "invalid_evidence_artifact");
    const relativePath = normalizeRepoPath(artifact.repo_relative_path);
    if (artifacts.has(artifact.id) || artifact.id <= previousArtifact || artifactPaths.has(relativePath)) {
      fail("invalid_evidence_artifact");
    }
    artifacts.add(artifact.id);
    previousArtifact = artifact.id;
    artifactPaths.add(relativePath);
    requireHash(artifact.sha256, HEX_64, "invalid_evidence_artifact");
    if (!Number.isSafeInteger(artifact.bytes) || artifact.bytes < 0) fail("invalid_evidence_artifact");
  }
  return document;
}

async function ensureDirectoryWithoutSymlinks(root) {
  const absolute = path.resolve(root);
  const parsed = path.parse(absolute);
  let current = parsed.root;
  for (const segment of absolute.slice(parsed.root.length).split(path.sep).filter(Boolean)) {
    current = path.join(current, segment);
    await mkdir(current).catch((error) => {
      if (error.code !== "EEXIST") throw error;
    });
    const metadata = await lstat(current);
    if (!metadata.isDirectory() || metadata.isSymbolicLink()) fail("unsafe_evidence_root");
  }
}

export async function writeCanonicalAtomic(destination, document) {
  const directory = path.dirname(destination);
  await ensureDirectoryWithoutSymlinks(directory);
  const temporary = path.join(directory, `.${path.basename(destination)}.${process.pid}.${Date.now()}.tmp`);
  let handle;
  try {
    handle = await open(temporary, "wx", 0o600);
    await handle.writeFile(canonicalJson(document), "utf8");
    await handle.sync();
    await handle.close();
    handle = undefined;
    await rename(temporary, destination);
  } finally {
    await handle?.close().catch(() => {});
    await rm(temporary, { force: true }).catch(() => {});
  }
}

export async function writeEvidenceAtomic(destination, document) {
  validateEvidenceDocument(document);
  await writeCanonicalAtomic(destination, document);
}

export async function aggregateCapabilityEvidence(documents, expected) {
  if (!Array.isArray(documents) || documents.length === 0) fail("empty_evidence_aggregation");
  requireExactKeys(
    expected,
    ["record", "required_platforms", "repository_root", "source", "manifest_sha256", "manifest_identity", "tool_identity_validator", "receipt_provider"],
    "invalid_evidence_aggregation",
  );
  if (!Array.isArray(expected.required_platforms) || expected.required_platforms.length === 0) fail("invalid_evidence_aggregation");
  const required = [...new Set(expected.required_platforms)].sort();
  if (required.length !== expected.required_platforms.length || required.some((item) => !["linux", "windows", "macos", "browser"].includes(item))) {
    fail("invalid_evidence_aggregation");
  }
  if (
    required.length !== expected.record.allowed_platforms.length ||
    required.some((platform, index) => platform !== [...expected.record.allowed_platforms].sort()[index])
  ) {
    fail("registry_platform_mismatch");
  }
  if (typeof expected.receipt_provider !== "function") fail("current_receipts_unavailable");
  if (typeof expected.tool_identity_validator !== "function") fail("invalid_evidence_aggregation");

  const validated = documents.map(validateEvidenceDocument);
  const first = validated[0];
  const sourceIdentity = canonicalJson(expected.source);
  const manifestIdentity = expected.manifest_sha256;
  const platforms = new Set();
  for (const document of validated) {
    if (
      document.scenario_id !== expected.record.scenario_id ||
      document.proof_id !== expected.record.proof_id ||
      document.capability_id !== expected.record.capability_id ||
      document.owner_phase !== expected.record.owner_phase
    ) {
      fail("mixed_evidence_identity");
    }
    if (!expected.record.allowed_platforms.includes(document.platform.os)) fail("registry_platform_mismatch");
    if (canonicalJson(document.source) !== sourceIdentity) fail("mixed_source_identity");
    if (document.toolchain.manifest_sha256 !== manifestIdentity) fail("mixed_toolchain_identity");
    await expected.tool_identity_validator(document.toolchain, document.platform.os);
    if (platforms.has(document.platform.os)) fail("duplicate_evidence_platform");
    platforms.add(document.platform.os);

    for (const artifact of document.artifacts) {
      const source = await assertRegularPathInside(expected.repository_root, artifact.repo_relative_path);
      const sealed = await hashArtifact(source);
      if (sealed.bytes !== artifact.bytes || sealed.sha256 !== artifact.sha256) fail("artifact_evidence_mismatch");
    }
    for (const observation of document.observations) {
      const receipt = await expected.receipt_provider(document, observation.id);
      if (receipt === undefined || sha256(canonicalJson(receipt)) !== observation.receipt_sha256) {
        fail("receipt_evidence_mismatch");
      }
    }
  }
  if (required.some((item) => !platforms.has(item)) || platforms.size !== required.length) fail("incomplete_evidence_matrix");

  return Object.freeze({
    schema_version: 1,
    result: "verified",
    scenario_id: expected.record.scenario_id,
    proof_id: expected.record.proof_id,
    capability_id: expected.record.capability_id,
    owner_phase: expected.record.owner_phase,
    source: Object.freeze({ ...first.source }),
    toolchain: { manifest_sha256: manifestIdentity, identity: expected.manifest_identity },
    platforms: Object.freeze(required),
    evidence_sha256: Object.freeze(
      Object.fromEntries(validated.map((document) => [document.platform.os, sha256(canonicalJson(document))])),
    ),
  });
}

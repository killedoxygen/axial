import { createRequire } from "node:module";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptPath = fileURLToPath(import.meta.url);
const defaultRepositoryRoot = resolve(dirname(scriptPath), "..");
const maximumPolicyBytes = 128 * 1024;
const maximumCommandBytes = 64 * 1024 * 1024;
const exactVersionPattern =
  /^(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;
const packagePattern = /^(?:@[a-z0-9._-]+\/)?[a-z0-9._-]+$/i;
const integrityPattern = /^sha512-[A-Za-z0-9+/]+={0,2}$/;
const exceptionKeys = Object.freeze([
  "ecosystem",
  "package",
  "version",
  "finding",
  "reviewed_at",
  "expires_at",
  "reason",
]);

export class DependencyPolicyError extends Error {
  constructor(message) {
    super(`dependency-policy: ${message}`);
    this.name = "DependencyPolicyError";
  }
}

function fail(message) {
  throw new DependencyPolicyError(message);
}

function requireRecord(value, location) {
  if (value === null || Array.isArray(value) || typeof value !== "object")
    fail(`${location} must be an object`);
  return value;
}

function requireKeys(value, expected, location) {
  const actual = Object.keys(requireRecord(value, location)).sort();
  const wanted = [...expected].sort();
  if (actual.join("\0") !== wanted.join("\0"))
    fail(`${location} keys must be exactly: ${wanted.join(", ")}`);
}

function requireString(value, location, pattern, maximumBytes = 256) {
  if (
    typeof value !== "string" ||
    Buffer.byteLength(value) > maximumBytes ||
    !pattern.test(value)
  ) {
    fail(`${location} is invalid`);
  }
  return value;
}

function parseDate(value, location) {
  requireString(value, location, /^\d{4}-\d{2}-\d{2}$/, 10);
  const instant = Date.parse(`${value}T00:00:00.000Z`);
  if (
    !Number.isFinite(instant) ||
    new Date(instant).toISOString().slice(0, 10) !== value
  )
    fail(`${location} is not a valid UTC date`);
  return instant;
}

function dateOnly(now) {
  const instant = now instanceof Date ? now : new Date(now);
  if (!Number.isFinite(instant.valueOf())) fail("current time is invalid");
  return Date.parse(`${instant.toISOString().slice(0, 10)}T00:00:00.000Z`);
}

function normalizeException(value, index, today) {
  const location = `advisory_exceptions[${index}]`;
  requireKeys(value, exceptionKeys, location);
  const ecosystem = requireString(
    value.ecosystem,
    `${location}.ecosystem`,
    /^(?:cargo|pnpm)$/,
    5,
  );
  const findingPattern =
    ecosystem === "cargo"
      ? /^RUSTSEC-\d{4}-\d{4}$/
      : /^GHSA-[23456789cfghjmpqrvwx]{4}-[23456789cfghjmpqrvwx]{4}-[23456789cfghjmpqrvwx]{4}$/;
  const reviewedAt = parseDate(value.reviewed_at, `${location}.reviewed_at`);
  const expiresAt = parseDate(value.expires_at, `${location}.expires_at`);
  const lifetimeDays = (expiresAt - reviewedAt) / 86_400_000;
  if (reviewedAt > today) fail(`${location}.reviewed_at is in the future`);
  if (expiresAt <= today) fail(`${location} is expired`);
  if (lifetimeDays <= 0 || lifetimeDays > 90)
    fail(`${location} must expire within 90 days of review`);

  return {
    ecosystem,
    package: requireString(
      value.package,
      `${location}.package`,
      packagePattern,
    ),
    version: requireString(
      value.version,
      `${location}.version`,
      exactVersionPattern,
    ),
    finding: requireString(
      value.finding,
      `${location}.finding`,
      findingPattern,
    ),
    reviewed_at: value.reviewed_at,
    expires_at: value.expires_at,
    reason: requireString(
      value.reason,
      `${location}.reason`,
      /^(?=.{20,500}$)[^\r\n]+$/,
      500,
    ),
  };
}

export function parseDependencyPolicy(source, options = {}) {
  if (
    typeof source !== "string" ||
    Buffer.byteLength(source) > maximumPolicyBytes
  ) {
    fail(
      `policy must be UTF-8 JSON no larger than ${maximumPolicyBytes} bytes`,
    );
  }

  let parsed;
  try {
    parsed = JSON.parse(source);
  } catch (error) {
    fail(`policy is not valid JSON: ${error.message}`);
  }
  requireKeys(
    parsed,
    ["schema_version", "pnpm_licenses", "advisory_exceptions"],
    "policy",
  );
  if (parsed.schema_version !== 1) fail("schema_version must be 1");
  if (!Array.isArray(parsed.pnpm_licenses) || parsed.pnpm_licenses.length === 0)
    fail("pnpm_licenses must be a nonempty array");
  if (!Array.isArray(parsed.advisory_exceptions))
    fail("advisory_exceptions must be an array");

  const pnpmLicenses = parsed.pnpm_licenses.map((license, index) =>
    requireString(
      license,
      `pnpm_licenses[${index}]`,
      /^[A-Za-z0-9][A-Za-z0-9.+-]*(?: WITH [A-Za-z0-9.-]+)?$/,
      128,
    ),
  );
  if ([...pnpmLicenses].sort().join("\0") !== pnpmLicenses.join("\0"))
    fail("pnpm_licenses must be sorted");
  if (new Set(pnpmLicenses).size !== pnpmLicenses.length)
    fail("pnpm_licenses contains a duplicate");

  const today = dateOnly(options.now ?? new Date());
  const exceptions = parsed.advisory_exceptions.map((exception, index) =>
    normalizeException(exception, index, today),
  );
  const identities = new Set();
  for (const exception of exceptions) {
    const identity = advisoryIdentity(exception);
    if (identities.has(identity)) fail(`duplicate exception ${identity}`);
    identities.add(identity);
  }

  const normalized = {
    schema_version: 1,
    pnpm_licenses: pnpmLicenses,
    advisory_exceptions: exceptions,
  };
  if (source !== `${JSON.stringify(normalized, null, 2)}\n`)
    fail("policy must use canonical JSON without duplicate keys");
  return normalized;
}

function advisoryIdentity(value) {
  return [value.ecosystem, value.package, value.version, value.finding].join(
    ":",
  );
}

function normalizeFinding(value, location) {
  requireKeys(value, ["ecosystem", "package", "version", "finding"], location);
  const ecosystem = requireString(
    value.ecosystem,
    `${location}.ecosystem`,
    /^(?:cargo|pnpm)$/,
    5,
  );
  return {
    ecosystem,
    package: requireString(
      value.package,
      `${location}.package`,
      packagePattern,
    ),
    version: requireString(
      value.version,
      `${location}.version`,
      exactVersionPattern,
    ),
    finding: requireString(
      value.finding,
      `${location}.finding`,
      ecosystem === "cargo"
        ? /^RUSTSEC-\d{4}-\d{4}$/
        : /^GHSA-[23456789cfghjmpqrvwx]{4}-[23456789cfghjmpqrvwx]{4}-[23456789cfghjmpqrvwx]{4}$/,
    ),
  };
}

export function enforceAdvisoryPolicy(findings, exceptions) {
  if (!Array.isArray(findings) || !Array.isArray(exceptions))
    fail("advisory findings and exceptions must be arrays");
  const uniqueFindings = new Map();
  findings.forEach((finding, index) => {
    const normalized = normalizeFinding(finding, `findings[${index}]`);
    uniqueFindings.set(advisoryIdentity(normalized), normalized);
  });

  const exceptionByIdentity = new Map(
    exceptions.map((exception) => [advisoryIdentity(exception), exception]),
  );
  const failures = [];
  for (const [identity, finding] of uniqueFindings) {
    if (!exceptionByIdentity.has(identity))
      failures.push(`prohibited advisory ${identity}`);
  }
  for (const exception of exceptions) {
    const identity = advisoryIdentity(exception);
    if (uniqueFindings.has(identity)) continue;
    const findingChanged = [...uniqueFindings.values()].some(
      (finding) =>
        finding.ecosystem === exception.ecosystem &&
        finding.finding === exception.finding,
    );
    failures.push(
      `${findingChanged ? "mismatched" : "unused"} exception ${identity}`,
    );
  }
  if (failures.length) fail(failures.join("; "));
  return {
    findings: uniqueFindings.size,
    exceptions: exceptions.length,
  };
}

function parseJson(source, location) {
  try {
    return JSON.parse(source);
  } catch (error) {
    fail(`${location} is not valid JSON: ${error.message}`);
  }
}

export function parseCargoDenyAdvisories(source) {
  if (
    typeof source !== "string" ||
    Buffer.byteLength(source) > maximumCommandBytes
  )
    fail("cargo-deny advisory output is missing or too large");
  const findings = [];
  let summary;
  let advisoryErrors = 0;
  for (const [index, line] of source.split(/\r?\n/).entries()) {
    if (!line.trim()) continue;
    const record = parseJson(line, `cargo-deny line ${index + 1}`);
    if (record.type === "log" && record.fields?.level === "ERROR")
      fail(
        `cargo-deny reported an internal error: ${record.fields.message ?? "unknown"}`,
      );
    if (record.type === "summary") summary = record.fields?.advisories;
    if (record.type !== "diagnostic" || record.fields?.severity !== "error")
      continue;
    const advisory = record.fields.advisory;
    if (!advisory)
      fail("cargo-deny emitted a non-advisory error that cannot be excepted");
    advisoryErrors += 1;
    if (
      !Array.isArray(record.fields.graphs) ||
      record.fields.graphs.length === 0
    )
      fail(
        `cargo-deny advisory ${advisory.id ?? "unknown"} has no package graph`,
      );
    for (const graph of record.fields.graphs) {
      const crate = graph?.Krate;
      findings.push({
        ecosystem: "cargo",
        package: crate?.name,
        version: crate?.version,
        finding: advisory.id,
      });
    }
  }
  if (!summary || !Number.isInteger(summary.errors))
    fail("cargo-deny advisory output has no valid summary");
  if (summary.errors !== advisoryErrors)
    fail("cargo-deny advisory diagnostics do not match its summary");
  return findings.map((finding, index) =>
    normalizeFinding(finding, `cargo findings[${index}]`),
  );
}

export function parsePnpmAudit(source) {
  if (
    typeof source !== "string" ||
    Buffer.byteLength(source) > maximumCommandBytes
  )
    fail("pnpm audit output is missing or too large");
  const report = parseJson(source, "pnpm audit output");
  requireKeys(report, ["advisories", "metadata"], "pnpm audit output");
  const advisories = requireRecord(report.advisories, "pnpm audit advisories");
  const metadata = requireRecord(report.metadata, "pnpm audit metadata");
  const vulnerabilities = requireRecord(
    metadata.vulnerabilities,
    "pnpm audit metadata.vulnerabilities",
  );
  requireKeys(
    vulnerabilities,
    ["info", "low", "moderate", "high", "critical"],
    "pnpm audit metadata.vulnerabilities",
  );
  let vulnerabilityCount = 0;
  for (const [severity, count] of Object.entries(vulnerabilities)) {
    if (!Number.isInteger(count) || count < 0)
      fail(`pnpm audit metadata.vulnerabilities.${severity} is invalid`);
    vulnerabilityCount += count;
  }
  if (vulnerabilityCount !== Object.keys(advisories).length)
    fail("pnpm audit advisories do not match its vulnerability summary");
  const findings = [];
  for (const [key, advisory] of Object.entries(advisories)) {
    requireRecord(advisory, `pnpm advisory ${key}`);
    const finding = requireString(
      advisory.github_advisory_id,
      `pnpm advisory ${key}.github_advisory_id`,
      /^GHSA-[23456789cfghjmpqrvwx]{4}-[23456789cfghjmpqrvwx]{4}-[23456789cfghjmpqrvwx]{4}$/,
    );
    const packageName = requireString(
      advisory.module_name,
      `pnpm advisory ${key}.module_name`,
      packagePattern,
    );
    if (!Array.isArray(advisory.findings) || advisory.findings.length === 0)
      fail(`pnpm advisory ${key} has no findings`);
    for (const [index, occurrence] of advisory.findings.entries()) {
      requireRecord(occurrence, `pnpm advisory ${key}.findings[${index}]`);
      findings.push({
        ecosystem: "pnpm",
        package: packageName,
        version: occurrence.version,
        finding,
      });
    }
  }
  return findings.map((finding, index) =>
    normalizeFinding(finding, `pnpm findings[${index}]`),
  );
}

export function verifyPnpmRegistry(source) {
  if (source !== "https://registry.npmjs.org/\n")
    fail("pnpm registry must be exactly https://registry.npmjs.org/");
  return "https://registry.npmjs.org/";
}

export function verifyCargoPolicyOutput(source) {
  if (
    typeof source !== "string" ||
    Buffer.byteLength(source) > maximumCommandBytes
  )
    fail("cargo-deny license/source output is missing or too large");
  let summary;
  for (const [index, line] of source.split(/\r?\n/).entries()) {
    if (!line.trim()) continue;
    const record = parseJson(line, `cargo-deny policy line ${index + 1}`);
    if (record.type === "log" && record.fields?.level === "ERROR")
      fail(
        `cargo-deny reported an internal error: ${record.fields.message ?? "unknown"}`,
      );
    if (record.type === "diagnostic" && record.fields?.severity === "error")
      fail("cargo-deny license/source policy reported an error");
    if (record.type === "summary") summary = record.fields;
  }
  for (const check of ["licenses", "sources"]) {
    if (!summary || !Number.isInteger(summary[check]?.errors))
      fail(`cargo-deny policy output has no valid ${check} summary`);
    if (summary[check].errors !== 0)
      fail(`cargo-deny ${check} summary contains errors`);
  }
  return { licenses: 0, sources: 0 };
}

export function verifyPnpmLicenses(source, allowedLicenses) {
  const report = parseJson(source, "pnpm license output");
  const allowed = new Set(allowedLicenses);
  const packages = new Set();
  for (const [license, entries] of Object.entries(
    requireRecord(report, "pnpm license output"),
  )) {
    if (!allowed.has(license)) fail(`pnpm license ${license} is not allowed`);
    if (!Array.isArray(entries) || entries.length === 0)
      fail(`pnpm license ${license} has no packages`);
    for (const [index, entry] of entries.entries()) {
      requireRecord(entry, `pnpm license ${license}[${index}]`);
      const name = requireString(
        entry.name,
        `pnpm license ${license}[${index}].name`,
        packagePattern,
      );
      if (!Array.isArray(entry.versions) || entry.versions.length === 0)
        fail(`pnpm license ${license}[${index}] has no exact versions`);
      for (const version of entry.versions) {
        requireString(
          version,
          `pnpm license ${license}[${index}].version`,
          exactVersionPattern,
        );
        packages.add(`${name}@${version}`);
      }
    }
  }
  if (packages.size === 0) fail("pnpm license output contains no packages");
  return {
    packages: packages.size,
    licenses: Object.keys(report).length,
    package_ids: [...packages].sort(),
  };
}

function parseLockPackageId(packageId, location) {
  if (typeof packageId !== "string") fail(`${location} is invalid`);
  const separator = packageId.lastIndexOf("@");
  if (separator <= 0)
    fail(`${location} does not contain an exact package version`);
  return {
    name: requireString(
      packageId.slice(0, separator),
      `${location}.name`,
      packagePattern,
    ),
    version: requireString(
      packageId.slice(separator + 1),
      `${location}.version`,
      exactVersionPattern,
    ),
  };
}

function isRegistryImporterSpecifier(value) {
  if (typeof value !== "string") return false;
  if (exactVersionPattern.test(value)) return true;
  return (
    (value.startsWith("^") || value.startsWith("~")) &&
    exactVersionPattern.test(value.slice(1))
  );
}

function parseRegistryImporterResolution(value) {
  if (typeof value !== "string") return undefined;
  const suffixStart = value.indexOf("(");
  const version = suffixStart === -1 ? value : value.slice(0, suffixStart);
  if (!exactVersionPattern.test(version)) return undefined;

  let suffixes = suffixStart === -1 ? "" : value.slice(suffixStart);
  while (suffixes) {
    if (!suffixes.startsWith("(")) return undefined;
    const close = suffixes.indexOf(")");
    if (close <= 1) return undefined;
    const peer = suffixes.slice(1, close);
    const separator = peer.lastIndexOf("@");
    if (
      separator <= 0 ||
      !packagePattern.test(peer.slice(0, separator)) ||
      !exactVersionPattern.test(peer.slice(separator + 1))
    )
      return undefined;
    suffixes = suffixes.slice(close + 1);
  }
  return version;
}

export function verifyPnpmLock(lock) {
  requireRecord(lock, "pnpm lockfile");
  if (lock.lockfileVersion !== "9.0")
    fail("pnpm lockfileVersion must be exactly 9.0");
  const packages = requireRecord(lock.packages, "pnpm lockfile packages");
  const importers = requireRecord(lock.importers, "pnpm lockfile importers");
  if (Object.keys(packages).length === 0)
    fail("pnpm lockfile packages is empty");
  if (Object.keys(importers).length === 0)
    fail("pnpm lockfile importers is empty");

  for (const [packageId, value] of Object.entries(packages)) {
    parseLockPackageId(packageId, `pnpm package ${packageId}`);
    const packageRecord = requireRecord(value, `pnpm package ${packageId}`);
    requireKeys(
      packageRecord.resolution,
      ["integrity"],
      `pnpm package ${packageId}.resolution`,
    );
    requireString(
      packageRecord.resolution.integrity,
      `pnpm package ${packageId}.resolution.integrity`,
      integrityPattern,
    );
  }

  for (const [importerName, importer] of Object.entries(importers)) {
    const importerRecord = requireRecord(
      importer,
      `pnpm importer ${importerName}`,
    );
    for (const section of [
      "dependencies",
      "devDependencies",
      "optionalDependencies",
    ]) {
      if (importerRecord[section] === undefined) continue;
      for (const [name, value] of Object.entries(
        requireRecord(
          importerRecord[section],
          `pnpm importer ${importerName}.${section}`,
        ),
      )) {
        const dependencyName = requireString(
          name,
          `pnpm importer ${importerName}.${section}.${name}.name`,
          packagePattern,
        );
        const dependency = requireRecord(
          value,
          `pnpm importer ${importerName}.${section}.${name}`,
        );
        requireKeys(
          dependency,
          ["specifier", "version"],
          `pnpm importer ${importerName}.${section}.${name}`,
        );
        if (!isRegistryImporterSpecifier(dependency.specifier))
          fail(
            `pnpm importer ${importerName}.${section}.${name}.specifier uses a non-registry source`,
          );
        const version = parseRegistryImporterResolution(dependency.version);
        if (version === undefined)
          fail(
            `pnpm importer ${importerName}.${section}.${name}.version uses a non-registry source`,
          );
        if (!Object.hasOwn(packages, `${dependencyName}@${version}`))
          fail(
            `pnpm importer ${importerName}.${section}.${name}.version has no integrity-backed package`,
          );
      }
    }
  }
  return {
    packages: Object.keys(packages).length,
    package_ids: Object.keys(packages).sort(),
  };
}

function isIncompatibleEsbuildTarget(
  packageId,
  packageRecord,
  lock,
  platform,
  architecture,
) {
  const { name, version } = parseLockPackageId(
    packageId,
    `pnpm package ${packageId}`,
  );
  if (!name.startsWith("@esbuild/")) return false;
  if (
    !Array.isArray(packageRecord.os) ||
    packageRecord.os.length !== 1 ||
    !Array.isArray(packageRecord.cpu) ||
    packageRecord.cpu.length !== 1
  )
    return false;
  if (
    packageRecord.os.includes(platform) &&
    packageRecord.cpu.includes(architecture)
  )
    return false;
  const optionalDependencies =
    lock.snapshots?.[`esbuild@${version}`]?.optionalDependencies;
  return optionalDependencies?.[name] === version;
}

export function reconcilePnpmLicenseCoverage(
  lock,
  lockReport,
  licenseReport,
  options = {},
) {
  const locked = new Set(lockReport.package_ids);
  const licensed = new Set(licenseReport.package_ids);
  const failures = [];
  for (const packageId of licensed) {
    if (!locked.has(packageId))
      failures.push(
        `pnpm license output contains unlocked package ${packageId}`,
      );
  }

  let platformOmissions = 0;
  const platform = options.platform ?? process.platform;
  const architecture = options.architecture ?? process.arch;
  for (const packageId of locked) {
    if (licensed.has(packageId)) continue;
    const packageRecord = lock.packages[packageId];
    const { version } = parseLockPackageId(
      packageId,
      `pnpm package ${packageId}`,
    );
    const parentId = `esbuild@${version}`;
    if (
      isIncompatibleEsbuildTarget(
        packageId,
        packageRecord,
        lock,
        platform,
        architecture,
      ) &&
      licensed.has(parentId)
    ) {
      platformOmissions += 1;
      continue;
    }
    failures.push(`pnpm license output omitted locked package ${packageId}`);
  }
  if (failures.length) fail(failures.join("; "));
  if (licenseReport.packages + platformOmissions !== lockReport.packages)
    fail("pnpm license coverage does not reconcile with the lock graph");
  return {
    licensed: licenseReport.packages,
    platform_omissions: platformOmissions,
    locked: lockReport.packages,
  };
}

function parsePnpmLock(source, repositoryRoot) {
  let yaml;
  let yamlVersion;
  try {
    const frontendRequire = createRequire(
      resolve(repositoryRoot, "frontend/package.json"),
    );
    const modulePath = frontendRequire.resolve("yaml");
    yaml = frontendRequire("yaml");
    yamlVersion = parseJson(
      readFileSync(resolve(dirname(modulePath), "../package.json"), "utf8"),
      "yaml package manifest",
    ).version;
  } catch (error) {
    fail(`could not load frontend yaml parser: ${error.message}`);
  }
  if (yamlVersion !== "2.9.0")
    fail(
      `frontend yaml parser is ${JSON.stringify(yamlVersion)}; expected "2.9.0"`,
    );
  const document = yaml.parseDocument(source, {
    maxAliasCount: 0,
    merge: false,
    uniqueKeys: true,
  });
  if (document.errors.length || document.warnings.length)
    fail(
      `pnpm lockfile YAML is invalid: ${[
        ...document.errors,
        ...document.warnings,
      ]
        .map((error) => error.message)
        .join("; ")}`,
    );
  try {
    return document.toJS({ maxAliasCount: 0 });
  } catch (error) {
    fail(`pnpm lockfile YAML could not be decoded: ${error.message}`);
  }
}

function runCommand(command, args, repositoryRoot) {
  const result = spawnSync(command, args, {
    cwd: repositoryRoot,
    encoding: "utf8",
    env: { ...process.env, NO_COLOR: "1" },
    maxBuffer: maximumCommandBytes,
    timeout: 10 * 60 * 1000,
    windowsHide: true,
  });
  if (result.error)
    fail(`could not execute ${command}: ${result.error.message}`);
  if (result.signal) fail(`${command} was terminated by ${result.signal}`);
  return {
    status: result.status,
    stdout: result.stdout,
    stderr: result.stderr,
    combined: `${result.stdout}${result.stdout && result.stderr ? "\n" : ""}${result.stderr}`,
  };
}

function requireSuccess(result, name) {
  if (result.status !== 0)
    fail(
      `${name} exited with status ${result.status}: ${result.combined.trim().slice(0, 4000)}`,
    );
}

export function checkDependencyPolicy(options = {}) {
  const repositoryRoot = options.repositoryRoot ?? defaultRepositoryRoot;
  const runner = options.runCommand ?? runCommand;
  const policySource = readFileSync(
    resolve(repositoryRoot, "dependency-policy.json"),
    "utf8",
  );
  const policy = parseDependencyPolicy(policySource, { now: options.now });
  const lockSource = readFileSync(
    resolve(repositoryRoot, "frontend/pnpm-lock.yaml"),
    "utf8",
  );
  const lock = (options.parseLock ?? parsePnpmLock)(lockSource, repositoryRoot);
  const lockReport = verifyPnpmLock(lock);

  const cargoAdvisories = runner(
    "cargo",
    ["deny", "--format", "json", "check", "advisories"],
    repositoryRoot,
  );
  if (![0, 1].includes(cargoAdvisories.status))
    fail(`cargo-deny advisories exited with status ${cargoAdvisories.status}`);
  const cargoFindings = parseCargoDenyAdvisories(cargoAdvisories.combined);

  const cargoPolicy = runner(
    "cargo",
    ["deny", "--format", "json", "check", "licenses", "sources"],
    repositoryRoot,
  );
  requireSuccess(cargoPolicy, "cargo-deny licenses/sources");
  verifyCargoPolicyOutput(cargoPolicy.combined);

  const pnpmRegistry = runner(
    "pnpm",
    ["--dir", "frontend", "config", "get", "registry"],
    repositoryRoot,
  );
  requireSuccess(pnpmRegistry, "pnpm registry query");
  verifyPnpmRegistry(pnpmRegistry.stdout);

  const pnpmAudit = runner(
    "pnpm",
    ["--dir", "frontend", "audit", "--json"],
    repositoryRoot,
  );
  if (![0, 1].includes(pnpmAudit.status))
    fail(`pnpm audit exited with status ${pnpmAudit.status}`);
  const pnpmFindings = parsePnpmAudit(pnpmAudit.stdout);

  const pnpmLicenses = runner(
    "pnpm",
    ["--dir", "frontend", "licenses", "list", "--json"],
    repositoryRoot,
  );
  requireSuccess(pnpmLicenses, "pnpm licenses");
  const licenseReport = verifyPnpmLicenses(
    pnpmLicenses.stdout,
    policy.pnpm_licenses,
  );
  const licenseCoverage = reconcilePnpmLicenseCoverage(
    lock,
    lockReport,
    licenseReport,
  );

  const advisoryReport = enforceAdvisoryPolicy(
    [...cargoFindings, ...pnpmFindings],
    policy.advisory_exceptions,
  );
  return {
    cargo_advisories: cargoFindings.length,
    pnpm_advisories: pnpmFindings.length,
    exceptions: advisoryReport.exceptions,
    pnpm_packages: lockReport.packages,
    pnpm_licensed_packages: licenseReport.packages,
    pnpm_platform_omissions: licenseCoverage.platform_omissions,
  };
}

function main() {
  const [command, ...rest] = process.argv.slice(2);
  if (command !== "check" || rest.length)
    fail("usage: dependency-policy.mjs check");
  const report = checkDependencyPolicy();
  process.stdout.write(
    `dependency policy verified (${JSON.stringify(report)})\n`,
  );
}

if (process.argv[1] && resolve(process.argv[1]) === scriptPath) {
  try {
    main();
  } catch (error) {
    process.stderr.write(`${error.message}\n`);
    process.exitCode = 1;
  }
}

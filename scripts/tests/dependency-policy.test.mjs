import assert from "node:assert/strict";
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { afterEach, test } from "node:test";

import {
  DependencyPolicyError,
  checkDependencyPolicy,
  enforceAdvisoryPolicy,
  parseCargoDenyAdvisories,
  parseDependencyPolicy,
  parsePnpmAudit,
  reconcilePnpmLicenseCoverage,
  verifyCargoPolicyOutput,
  verifyPnpmLicenses,
  verifyPnpmLock,
  verifyPnpmRegistry,
} from "../dependency-policy.mjs";

const repositoryRoot = resolve(import.meta.dirname, "../..");
const policySource = readFileSync(
  resolve(repositoryRoot, "dependency-policy.json"),
  "utf8",
);
const temporaryRoots = [];
const today = new Date("2026-07-20T12:00:00.000Z");

afterEach(() => {
  for (const root of temporaryRoots.splice(0))
    rmSync(root, { recursive: true, force: true });
});

function exception(overrides = {}) {
  return {
    ecosystem: "cargo",
    package: "example-crate",
    version: "1.2.3",
    finding: "RUSTSEC-2026-0001",
    reviewed_at: "2026-07-20",
    expires_at: "2026-10-17",
    reason: "The affected parser path is not reachable in Axial.",
    ...overrides,
  };
}

function policy(overrides = {}) {
  return `${JSON.stringify(
    {
      schema_version: 1,
      pnpm_licenses: ["ISC", "MIT"],
      advisory_exceptions: [],
      ...overrides,
    },
    null,
    2,
  )}\n`;
}

function cargoOutput(findings = []) {
  return [
    ...findings.map(({ finding, package: packageName, version }) =>
      JSON.stringify({
        fields: {
          advisory: { id: finding, package: packageName },
          graphs: [{ Krate: { name: packageName, version } }],
          severity: "error",
        },
        type: "diagnostic",
      }),
    ),
    JSON.stringify({
      fields: {
        advisories: {
          errors: findings.length,
          helps: 0,
          notes: 0,
          warnings: 0,
        },
      },
      type: "summary",
    }),
  ].join("\n");
}

function pnpmAudit(findings = []) {
  const advisories = Object.fromEntries(
    findings.map(({ finding, package: packageName, version }, index) => [
      String(index + 1),
      {
        github_advisory_id: finding,
        module_name: packageName,
        findings: [{ version }],
      },
    ]),
  );
  return JSON.stringify({
    advisories,
    metadata: {
      vulnerabilities: {
        info: 0,
        low: 0,
        moderate: findings.length,
        high: 0,
        critical: 0,
      },
    },
  });
}

function cargoPolicyOutput() {
  return JSON.stringify({
    fields: {
      licenses: { errors: 0, helps: 1, notes: 0, warnings: 0 },
      sources: { errors: 0, helps: 0, notes: 0, warnings: 0 },
    },
    type: "summary",
  });
}

function exactLock() {
  return {
    lockfileVersion: "9.0",
    importers: {
      ".": {
        dependencies: {
          package: { specifier: "^1.2.0", version: "1.2.3" },
        },
      },
    },
    packages: {
      "package@1.2.3": {
        resolution: { integrity: "sha512-YQ==" },
      },
    },
    snapshots: { "package@1.2.3": {} },
  };
}

test("tracked policy is canonical, strict, and starts without exceptions", () => {
  const parsed = parseDependencyPolicy(policySource, { now: today });
  assert.equal(parsed.schema_version, 1);
  assert.deepEqual(parsed.advisory_exceptions, []);
  assert.ok(parsed.pnpm_licenses.includes("MIT"));
});

test("policy rejects unknown, missing, duplicate, and noncanonical fields", () => {
  assert.throws(
    () => parseDependencyPolicy(policy({ extra: true }), { now: today }),
    /keys must be exactly/,
  );
  assert.throws(
    () =>
      parseDependencyPolicy(
        `${JSON.stringify({ schema_version: 1, advisory_exceptions: [] })}\n`,
        { now: today },
      ),
    /keys must be exactly/,
  );
  assert.throws(
    () =>
      parseDependencyPolicy(policy({ pnpm_licenses: ["MIT", "MIT"] }), {
        now: today,
      }),
    /contains a duplicate/,
  );
  assert.throws(
    () =>
      parseDependencyPolicy(
        policy().replace(
          '  "schema_version": 1,',
          '  "schema_version": 1,\n  "schema_version": 1,',
        ),
        { now: today },
      ),
    /canonical JSON/,
  );
});

test("exceptions require exact coordinates and a review window no longer than 90 days", () => {
  assert.doesNotThrow(() =>
    parseDependencyPolicy(policy({ advisory_exceptions: [exception()] }), {
      now: today,
    }),
  );
  for (const [mutation, expected] of [
    [{ ecosystem: "npm" }, /ecosystem is invalid/],
    [{ version: "^1.2.3" }, /version is invalid/],
    [{ finding: "RUSTSEC-*" }, /finding is invalid/],
    [{ reviewed_at: "2026-07-21" }, /reviewed_at is in the future/],
    [{ expires_at: "2026-07-20" }, /is expired/],
    [{ expires_at: "2026-10-19" }, /within 90 days/],
  ]) {
    assert.throws(
      () =>
        parseDependencyPolicy(
          policy({ advisory_exceptions: [exception(mutation)] }),
          { now: today },
        ),
      expected,
    );
  }
});

test("exceptions reject duplicates, stale entries, and coordinate mismatches", () => {
  const parsed = parseDependencyPolicy(
    policy({ advisory_exceptions: [exception()] }),
    { now: today },
  );
  assert.deepEqual(
    enforceAdvisoryPolicy(
      [
        {
          ecosystem: "cargo",
          package: "example-crate",
          version: "1.2.3",
          finding: "RUSTSEC-2026-0001",
        },
      ],
      parsed.advisory_exceptions,
    ),
    { findings: 1, exceptions: 1 },
  );
  assert.throws(
    () => enforceAdvisoryPolicy([], parsed.advisory_exceptions),
    /unused exception/,
  );
  assert.throws(
    () =>
      enforceAdvisoryPolicy(
        [
          {
            ecosystem: "cargo",
            package: "example-crate",
            version: "1.2.4",
            finding: "RUSTSEC-2026-0001",
          },
        ],
        parsed.advisory_exceptions,
      ),
    /mismatched exception/,
  );
  assert.throws(
    () =>
      parseDependencyPolicy(
        policy({ advisory_exceptions: [exception(), exception()] }),
        { now: today },
      ),
    /duplicate exception/,
  );
});

test("unexcepted advisories fail for both ecosystems", () => {
  for (const finding of [
    {
      ecosystem: "cargo",
      package: "crate-name",
      version: "1.0.0",
      finding: "RUSTSEC-2026-0002",
    },
    {
      ecosystem: "pnpm",
      package: "package-name",
      version: "2.0.0",
      finding: "GHSA-r5fr-rjxr-66jc",
    },
  ]) {
    assert.throws(
      () => enforceAdvisoryPolicy([finding], []),
      new RegExp(`prohibited advisory ${finding.ecosystem}`),
    );
  }
});

test("cargo-deny JSON parser reads exact advisory coordinates and requires a summary", () => {
  const finding = {
    package: "quick-xml",
    version: "0.38.4",
    finding: "RUSTSEC-2026-0194",
  };
  assert.deepEqual(parseCargoDenyAdvisories(cargoOutput([finding])), [
    { ecosystem: "cargo", ...finding },
  ]);
  assert.throws(
    () => parseCargoDenyAdvisories(cargoOutput([finding]).split("\n")[0]),
    /no valid summary/,
  );
  assert.throws(() => parseCargoDenyAdvisories("not-json"), /not valid JSON/);
  assert.throws(
    () =>
      parseCargoDenyAdvisories(
        `${JSON.stringify({ fields: { severity: "error" }, type: "diagnostic" })}\n${cargoOutput()}`,
      ),
    /non-advisory error/,
  );
});

test("pnpm audit parser expands every exact vulnerable version", () => {
  const findings = [
    {
      package: "lodash",
      version: "4.17.21",
      finding: "GHSA-r5fr-rjxr-66jc",
    },
  ];
  assert.deepEqual(parsePnpmAudit(pnpmAudit(findings)), [
    { ecosystem: "pnpm", ...findings[0] },
  ]);
  assert.throws(
    () => parsePnpmAudit(JSON.stringify({ error: "offline" })),
    /keys must be exactly/,
  );
  const inconsistent = JSON.parse(pnpmAudit(findings));
  inconsistent.metadata.vulnerabilities.moderate = 0;
  assert.throws(
    () => parsePnpmAudit(JSON.stringify(inconsistent)),
    /do not match its vulnerability summary/,
  );
});

test("cargo policy summaries and the ambient pnpm registry fail closed", () => {
  assert.deepEqual(verifyCargoPolicyOutput(cargoPolicyOutput()), {
    licenses: 0,
    sources: 0,
  });
  assert.throws(
    () =>
      verifyCargoPolicyOutput(
        JSON.stringify({
          fields: { licenses: { errors: 0 }, sources: { errors: 1 } },
          type: "summary",
        }),
      ),
    /sources summary contains errors/,
  );
  assert.equal(
    verifyPnpmRegistry("https://registry.npmjs.org/\n"),
    "https://registry.npmjs.org/",
  );
  assert.throws(
    () => verifyPnpmRegistry("https://packages.invalid/\n"),
    /registry must be exactly/,
  );
});

test("pnpm license policy rejects unknown licenses and malformed versions", () => {
  assert.deepEqual(
    verifyPnpmLicenses(
      JSON.stringify({
        MIT: [{ name: "package", versions: ["1.2.3"] }],
      }),
      ["MIT"],
    ),
    { packages: 1, licenses: 1, package_ids: ["package@1.2.3"] },
  );
  assert.throws(
    () =>
      verifyPnpmLicenses(
        JSON.stringify({ GPL: [{ name: "package", versions: ["1.2.3"] }] }),
        ["MIT"],
      ),
    /license GPL is not allowed/,
  );
  assert.throws(
    () =>
      verifyPnpmLicenses(
        JSON.stringify({ MIT: [{ name: "package", versions: ["latest"] }] }),
        ["MIT"],
      ),
    /version is invalid/,
  );
});

test("structured pnpm lock verification rejects source and integrity drift", () => {
  assert.deepEqual(verifyPnpmLock(exactLock()), {
    packages: 1,
    package_ids: ["package@1.2.3"],
  });

  const tarball = exactLock();
  tarball.packages["package@1.2.3"].resolution.tarball =
    "https://packages.invalid/package.tgz";
  assert.throws(() => verifyPnpmLock(tarball), /keys must be exactly/);

  const missingIntegrity = exactLock();
  missingIntegrity.packages["package@1.2.3"].resolution = {};
  assert.throws(() => verifyPnpmLock(missingIntegrity), /keys must be exactly/);

  const gitSpecifier = exactLock();
  gitSpecifier.importers["."].dependencies.package.specifier =
    "git+https://example.invalid/package";
  assert.throws(() => verifyPnpmLock(gitSpecifier), /non-registry source/);

  for (const reference of [
    "ftp://example.invalid/package.tgz",
    "custom+transport:package",
    "npm:other-package@1.2.3",
    "catalog:default",
    "patch:package@npm%3A1.2.3#patch.diff",
    "portal:../package",
    "owner/repository",
    "git@github.com:owner/repository",
    ".",
    "..",
    "./package",
    "../package",
    "/tmp/package",
    "\\\\server\\package",
  ]) {
    for (const field of ["specifier", "version"]) {
      const nonRegistry = exactLock();
      nonRegistry.importers["."].dependencies.package[field] = reference;
      assert.throws(
        () => verifyPnpmLock(nonRegistry),
        /non-registry source/,
        `${field} accepted ${reference}`,
      );
    }
  }

  const peerResolution = exactLock();
  peerResolution.importers["."].dependencies.package.version =
    "1.2.3(@types/react@19.2.14)";
  assert.doesNotThrow(() => verifyPnpmLock(peerResolution));

  const missingPackage = exactLock();
  missingPackage.importers["."].dependencies.package.version = "1.2.4";
  assert.throws(
    () => verifyPnpmLock(missingPackage),
    /no integrity-backed package/,
  );

  const oldSchema = exactLock();
  oldSchema.lockfileVersion = "8.0";
  assert.throws(() => verifyPnpmLock(oldSchema), /exactly 9\.0/);
});

test("license identities cover the lock except closed incompatible esbuild targets", () => {
  const lock = exactLock();
  const lockReport = verifyPnpmLock(lock);
  const licenseReport = verifyPnpmLicenses(
    JSON.stringify({
      MIT: [{ name: "package", versions: ["1.2.3"] }],
    }),
    ["MIT"],
  );
  assert.deepEqual(
    reconcilePnpmLicenseCoverage(lock, lockReport, licenseReport, {
      platform: "linux",
      architecture: "x64",
    }),
    { licensed: 1, platform_omissions: 0, locked: 1 },
  );

  const esbuildLock = {
    lockfileVersion: "9.0",
    importers: { ".": {} },
    packages: {
      "esbuild@0.25.12": { resolution: { integrity: "sha512-YQ==" } },
      "@esbuild/win32-x64@0.25.12": {
        resolution: { integrity: "sha512-Yg==" },
        os: ["win32"],
        cpu: ["x64"],
      },
    },
    snapshots: {
      "esbuild@0.25.12": {
        optionalDependencies: {
          "@esbuild/win32-x64": "0.25.12",
        },
      },
      "@esbuild/win32-x64@0.25.12": {},
    },
  };
  const esbuildReport = verifyPnpmLock(esbuildLock);
  const esbuildLicenses = verifyPnpmLicenses(
    JSON.stringify({
      MIT: [{ name: "esbuild", versions: ["0.25.12"] }],
    }),
    ["MIT"],
  );
  assert.deepEqual(
    reconcilePnpmLicenseCoverage(esbuildLock, esbuildReport, esbuildLicenses, {
      platform: "linux",
      architecture: "x64",
    }),
    { licensed: 1, platform_omissions: 1, locked: 2 },
  );
  assert.throws(
    () =>
      reconcilePnpmLicenseCoverage(
        esbuildLock,
        esbuildReport,
        esbuildLicenses,
        { platform: "win32", architecture: "x64" },
      ),
    /omitted locked package/,
  );

  esbuildLock.snapshots["esbuild@0.25.12"].optionalDependencies[
    "@esbuild/win32-x64"
  ] = "0.25.11";
  assert.throws(
    () =>
      reconcilePnpmLicenseCoverage(
        esbuildLock,
        esbuildReport,
        esbuildLicenses,
        { platform: "linux", architecture: "x64" },
      ),
    /omitted locked package/,
  );

  const unlockedLicense = structuredClone(licenseReport);
  unlockedLicense.packages += 1;
  unlockedLicense.package_ids.push("unlocked@1.0.0");
  assert.throws(
    () =>
      reconcilePnpmLicenseCoverage(lock, lockReport, unlockedLicense, {
        platform: "linux",
        architecture: "x64",
      }),
    /contains unlocked package/,
  );
});

test("one orchestrator runs each scanner once and keeps license/source failures non-overridable", () => {
  const root = mkdtempSync(resolve(tmpdir(), "axial-dependency-policy-"));
  temporaryRoots.push(root);
  mkdirSync(resolve(root, "frontend"));
  writeFileSync(
    resolve(root, "dependency-policy.json"),
    policy({ pnpm_licenses: ["MIT"] }),
  );
  writeFileSync(resolve(root, "frontend/pnpm-lock.yaml"), "fixture");

  const calls = [];
  const outputs = new Map([
    [
      "cargo deny --format json check advisories",
      { status: 0, stdout: "", stderr: cargoOutput(), combined: cargoOutput() },
    ],
    [
      "cargo deny --format json check licenses sources",
      {
        status: 0,
        stdout: "",
        stderr: cargoPolicyOutput(),
        combined: cargoPolicyOutput(),
      },
    ],
    [
      "pnpm --dir frontend config get registry",
      {
        status: 0,
        stdout: "https://registry.npmjs.org/\n",
        stderr: "",
        combined: "https://registry.npmjs.org/\n",
      },
    ],
    [
      "pnpm --dir frontend audit --json",
      { status: 0, stdout: pnpmAudit(), stderr: "", combined: pnpmAudit() },
    ],
    [
      "pnpm --dir frontend licenses list --json",
      {
        status: 0,
        stdout: JSON.stringify({
          MIT: [{ name: "package", versions: ["1.2.3"] }],
        }),
        stderr: "",
        combined: "",
      },
    ],
  ]);
  const runner = (command, args) => {
    const key = `${command} ${args.join(" ")}`;
    calls.push(key);
    return structuredClone(outputs.get(key));
  };

  assert.deepEqual(
    checkDependencyPolicy({
      repositoryRoot: root,
      now: today,
      parseLock: () => exactLock(),
      runCommand: runner,
    }),
    {
      cargo_advisories: 0,
      pnpm_advisories: 0,
      exceptions: 0,
      pnpm_packages: 1,
      pnpm_licensed_packages: 1,
      pnpm_platform_omissions: 0,
    },
  );
  assert.deepEqual(calls, [...outputs.keys()]);

  outputs.get("cargo deny --format json check licenses sources").status = 1;
  assert.throws(
    () =>
      checkDependencyPolicy({
        repositoryRoot: root,
        now: today,
        parseLock: () => exactLock(),
        runCommand: runner,
      }),
    /cargo-deny licenses\/sources exited with status 1/,
  );
});

test("public failures retain the dependency-policy error boundary", () => {
  assert.throws(
    () => parseDependencyPolicy("{}", { now: today }),
    DependencyPolicyError,
  );
});

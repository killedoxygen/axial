import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import {
  enforceAdvisoryPolicy,
  parseDependencyPolicy,
  reconcilePnpmLicenseCoverage,
  verifyCargoPolicyOutput,
  verifyPnpmLicenses,
  verifyPnpmLock,
  verifyPnpmRegistry,
} from '../../../scripts/dependency-policy.mjs';
import { parseToolchainManifest, readToolchainIdentity } from '../../../scripts/toolchain.mjs';

const root = new URL('../../../', import.meta.url);

function policy(overrides = {}) {
  return `${JSON.stringify(
    {
      schema_version: 1,
      pnpm_licenses: ['MIT'],
      advisory_exceptions: [],
      ...overrides,
    },
    null,
    2,
  )}\n`;
}

function exception(overrides = {}) {
  return {
    ecosystem: 'cargo',
    package: 'example-crate',
    version: '1.2.3',
    finding: 'RUSTSEC-2026-0001',
    reviewed_at: '2026-07-20',
    expires_at: '2026-10-17',
    reason: 'The affected parser path is not reachable in Axial.',
    ...overrides,
  };
}

test('one canonical policy owns exact expiring advisory exceptions', async () => {
  const source = await readFile(new URL('dependency-policy.json', root), 'utf8');
  const parsed = parseDependencyPolicy(source, {
    now: new Date('2026-07-20T12:00:00.000Z'),
  });
  assert.deepEqual(parsed.advisory_exceptions, []);

  for (const [mutation, expected] of [
    [{ version: '^1.2.3' }, /version is invalid/],
    [{ finding: 'RUSTSEC-*' }, /finding is invalid/],
    [{ expires_at: '2026-10-19' }, /within 90 days/],
    [{ expires_at: '2026-07-20' }, /expired/],
  ]) {
    assert.throws(
      () =>
        parseDependencyPolicy(policy({ advisory_exceptions: [exception(mutation)] }), {
          now: new Date('2026-07-20T12:00:00.000Z'),
        }),
      expected,
    );
  }
});

test('unknown, duplicate, unused, mismatched, and prohibited advisories fail closed', () => {
  const finding = {
    ecosystem: 'cargo',
    package: 'example-crate',
    version: '1.2.3',
    finding: 'RUSTSEC-2026-0001',
  };
  assert.throws(() => enforceAdvisoryPolicy([finding], []), /prohibited advisory/);
  assert.throws(
    () =>
      parseDependencyPolicy(policy({ extra: true }), {
        now: new Date('2026-07-20T12:00:00.000Z'),
      }),
    /keys must be exactly/,
  );
  assert.throws(
    () =>
      parseDependencyPolicy(policy({ advisory_exceptions: [exception(), exception()] }), {
        now: new Date('2026-07-20T12:00:00.000Z'),
      }),
    /duplicate exception/,
  );

  const reviewed = parseDependencyPolicy(policy({ advisory_exceptions: [exception()] }), {
    now: new Date('2026-07-20T12:00:00.000Z'),
  });
  assert.throws(() => enforceAdvisoryPolicy([], reviewed.advisory_exceptions), /unused exception/);
  assert.throws(
    () => enforceAdvisoryPolicy([{ ...finding, version: '1.2.4' }], reviewed.advisory_exceptions),
    /mismatched exception/,
  );
});

test('pnpm sources, integrity, and licenses have no advisory bypass', () => {
  const validLock = {
    lockfileVersion: '9.0',
    importers: {
      '.': {
        dependencies: {
          package: { specifier: '^1.2.0', version: '1.2.3' },
        },
      },
    },
    packages: {
      'package@1.2.3': { resolution: { integrity: 'sha512-YQ==' } },
    },
  };
  const lockReport = verifyPnpmLock(validLock);
  assert.deepEqual(lockReport, {
    packages: 1,
    package_ids: ['package@1.2.3'],
  });
  const remoteTarball = structuredClone(validLock);
  remoteTarball.packages['package@1.2.3'].resolution.tarball = 'https://packages.invalid/package.tgz';
  assert.throws(() => verifyPnpmLock(remoteTarball), /keys must be exactly/);
  for (const reference of [
    'ftp://packages.invalid/package.tgz',
    'unknown:package',
    'owner/repository',
    'git@github.com:owner/repository',
    '../package',
  ]) {
    const nonRegistry = structuredClone(validLock);
    nonRegistry.importers['.'].dependencies.package.specifier = reference;
    assert.throws(() => verifyPnpmLock(nonRegistry), /non-registry source/);
  }
  assert.throws(
    () =>
      verifyPnpmLicenses(
        JSON.stringify({
          GPL: [{ name: 'package', versions: ['1.2.3'] }],
        }),
        ['MIT'],
      ),
    /license GPL is not allowed/,
  );
  assert.throws(() => verifyPnpmRegistry('https://packages.invalid/\n'), /registry must be exactly/);
  assert.throws(
    () =>
      verifyCargoPolicyOutput(
        JSON.stringify({
          fields: { licenses: { errors: 0 }, sources: { errors: 1 } },
          type: 'summary',
        }),
      ),
    /sources summary contains errors/,
  );
  assert.throws(
    () =>
      reconcilePnpmLicenseCoverage(
        validLock,
        lockReport,
        {
          packages: 1,
          licenses: 1,
          package_ids: ['different-package@1.2.3'],
        },
        { platform: 'linux', architecture: 'x64' },
      ),
    /contains unlocked package/,
  );
});

test('cargo-deny archive identity is exact in the toolchain manifest', async () => {
  const source = await readFile(new URL('toolchain.json', root), 'utf8');
  const parsed = parseToolchainManifest(source);
  assert.deepEqual(parsed.cargo_deny, {
    release: '0.20.2',
    linux_archive: {
      target: 'x86_64-unknown-linux-musl',
      sha256: '9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f',
    },
  });
  assert.equal(readToolchainIdentity().cargo_deny.release, '0.20.2');
});

export type AdvisoryFinding = {
  ecosystem: string;
  package: string;
  version: string;
  finding: string;
};

export type AdvisoryException = AdvisoryFinding & {
  reviewed_at: string;
  expires_at: string;
  reason: string;
};

export type DependencyPolicy = {
  schema_version: 1;
  pnpm_licenses: string[];
  advisory_exceptions: AdvisoryException[];
};

export type PnpmLockReport = {
  packages: number;
  package_ids: string[];
};

export class DependencyPolicyError extends Error {}

export function parseDependencyPolicy(
  source: string,
  options?: { now?: Date },
): DependencyPolicy;
export function enforceAdvisoryPolicy(
  findings: AdvisoryFinding[],
  exceptions: AdvisoryException[],
): void;
export function parseCargoDenyAdvisories(source: string): AdvisoryFinding[];
export function parsePnpmAudit(source: string): AdvisoryFinding[];
export function verifyPnpmRegistry(source: string): void;
export function verifyCargoPolicyOutput(source: string): void;
export function verifyPnpmLicenses(
  source: string,
  allowedLicenses: string[],
): { packages: number; licenses: number; package_ids: string[] };
export function verifyPnpmLock(lock: unknown): PnpmLockReport;
export function reconcilePnpmLicenseCoverage(
  lock: unknown,
  lockReport: PnpmLockReport,
  licenseReport: { packages: number; licenses: number; package_ids: string[] },
  platform?: { platform?: string; architecture?: string; libc?: string },
): unknown;
export function checkDependencyPolicy(options?: {
  repositoryRoot?: string;
  now?: Date;
  parseLock?: (source: string) => unknown;
  runCommand?: (
    command: string,
    args: string[],
    repositoryRoot: string,
  ) => unknown;
}): Promise<unknown>;

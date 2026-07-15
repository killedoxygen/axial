#!/usr/bin/env node
// Verify that every expected desktop release asset was published for a tag and
// is actually downloadable. Run after the platform jobs upload their artifacts;
// it catches a half-published release (a missing binary or checksum) before
// anyone relies on it. Zero dependencies.
//
// Usage: node scripts/verify-release-assets.mjs [--version <v>]
// Falls back to GITHUB_REF_NAME when --version is omitted.

import { setTimeout as sleep } from 'node:timers/promises';

const ATTEMPTS = 5;
const DELAY_MS = 3000;

// Raw executables for manual downloads. Kept in sync with the package steps in
// .github/workflows/release.yml.
const EXECUTABLE_TEMPLATES = [
  (v) => `axial-linux-amd64-${v}`,
  (v) => `axial-windows-amd64-${v}.exe`,
  (v) => `axial-macos-amd64-${v}`,
  (v) => `axial-macos-arm64-${v}`,
];

// Archives consumed by the in-app updater.
const UPDATE_PACKAGE_TEMPLATES = [
  (v) => `axial-linux-amd64-${v}.tar.gz`,
  (v) => `axial-windows-amd64-${v}.zip`,
  (v) => `axial-macos-amd64-${v}.tar.gz`,
  (v) => `axial-macos-arm64-${v}.tar.gz`,
];

function fail(message) {
  console.error(`verify-release-assets: ${message}`);
  process.exit(1);
}

function parseVersionArg(argv) {
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--version' || arg === '--tag') return argv[i + 1] ?? null;
    if (arg.startsWith('--version=')) return arg.slice('--version='.length);
    if (arg.startsWith('--tag=')) return arg.slice('--tag='.length);
  }
  return null;
}

function normalizeVersion(raw) {
  return (raw || '').trim().replace(/^v/, '');
}

function releasesDownloadBase() {
  const server = process.env.GITHUB_SERVER_URL || 'https://github.com';
  const repo = process.env.GITHUB_REPOSITORY || 'mateoltd/axial';
  return `${server}/${repo}/releases/download`;
}

function expectedFiles(version) {
  // Every manual executable and updater package has a checksum sidecar.
  return [...EXECUTABLE_TEMPLATES, ...UPDATE_PACKAGE_TEMPLATES].flatMap((template) => {
    const file = template(version);
    return [file, `${file}.sha256`];
  });
}

async function reachable(url) {
  const headers = { 'user-agent': 'axial-release-verify', range: 'bytes=0-0' };
  if (process.env.GITHUB_TOKEN) headers.authorization = `Bearer ${process.env.GITHUB_TOKEN}`;
  for (let attempt = 1; attempt <= ATTEMPTS; attempt += 1) {
    try {
      const response = await fetch(url, { method: 'GET', redirect: 'follow', headers });
      if (response.status === 200 || response.status === 206) return true;
    } catch {
      // transient network error; fall through to retry
    }
    if (attempt < ATTEMPTS) await sleep(DELAY_MS);
  }
  return false;
}

async function main() {
  const version = normalizeVersion(parseVersionArg(process.argv.slice(2)) ?? process.env.GITHUB_REF_NAME);
  if (!version) fail('no version given (pass --version <v> or set GITHUB_REF_NAME)');

  const tag = `v${version}`;
  const base = releasesDownloadBase();
  const urls = expectedFiles(version).map((file) => `${base}/${tag}/${file}`);

  console.log(`Verifying ${urls.length} release assets for ${tag}`);
  const missing = [];
  for (const url of urls) {
    const ok = await reachable(url);
    console.log(`  ${ok ? 'ok     ' : 'MISSING'} ${url}`);
    if (!ok) missing.push(url);
  }
  if (missing.length > 0) {
    fail(`release assets are not reachable for ${tag}:\n  ${missing.join('\n  ')}`);
  }
  console.log(`All ${urls.length} assets reachable.`);
}

main().catch((err) => fail(err?.stack || String(err)));

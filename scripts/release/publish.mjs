#!/usr/bin/env node
// Stages a verified candidate for publication: release files, notes, and the update feed.
import { copyFile, mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { PLATFORMS } from "./matrix.mjs";
import { changelogRelease, check, digest, jsonFile, main, options, verifyArtifacts, verifyUpdaterSignature, writeJson } from "./contract.mjs";

const REPOSITORY = "https://github.com/killedoxygen/axial";
const PUBLISHED_ROLES = new Set(["installer", "portable-archive", "updater", "updater-signature"]);
const NOTICE = `Start with a fresh profile: earlier profiles are not imported.

On macOS the first open is blocked because Axial is not notarized. Click Done, then open System Settings → Privacy & Security, scroll down and choose Open Anyway. Windows SmartScreen may warn before the installer runs: choose More info → Run anyway. After that, Axial updates itself.`;

export async function stagePublication({ source, assets, publicKey, changelog, output, now = new Date() }) {
  const release = path.join(output, "release");
  const site = path.join(output, "site");
  await mkdir(release, { recursive: true });
  await mkdir(site, { recursive: true });
  const platforms = {};
  const sums = [];
  for (const [platform, info] of Object.entries(PLATFORMS)) {
    const directory = path.join(assets, platform);
    const manifest = await verifyArtifacts(directory, source, platform);
    await verifyUpdaterSignature(directory, manifest, publicKey);
    for (const file of manifest.files.filter((item) => PUBLISHED_ROLES.has(item.role))) {
      await copyFile(path.join(directory, file.name), path.join(release, file.name));
      if (file.role !== "updater-signature") sums.push(`${file.sha256}  ${file.name}`);
    }
    const updater = manifest.files.find((item) => item.role === "updater");
    platforms[`axial-rewrite-${info.updater}`] = {
      signature: (await readFile(path.join(directory, `${updater.name}.sig`), "utf8")).trim(),
      url: `${REPOSITORY}/releases/download/${source.tag}/${updater.name}`,
    };
  }
  await writeFile(path.join(release, "SHA256SUMS"), `${sums.sort((a, b) => a.slice(66).localeCompare(b.slice(66))).join("\n")}\n`, { flag: "wx" });
  const { notes } = changelogRelease(changelog, source.version);
  await writeFile(path.join(output, "notes.md"), `${NOTICE}\n\n${notes}\n`, { flag: "wx" });
  const feed = { version: source.version, notes: `${REPOSITORY}/releases/tag/${source.tag}`, pub_date: now.toISOString(), platforms };
  await writeJson(path.join(site, "latest.json"), feed);
  await writeFile(path.join(site, "index.html"), `<!doctype html><meta http-equiv="refresh" content="0; url=${REPOSITORY}/releases"><title>Axial updates</title>\n`, { flag: "wx" });
  check(Object.keys(platforms).length === Object.keys(PLATFORMS).length, "feed_platforms_incomplete");
  return { tag: source.tag, feed_sha256: await digest(path.join(site, "latest.json")), files: sums.length };
}

main(import.meta.url, async () => {
  const o = options(process.argv.slice(2), ["source", "assets", "public-key", "changelog", "output"]);
  return stagePublication({ source: await jsonFile(o.source), assets: o.assets, publicKey: o["public-key"], changelog: await readFile(o.changelog, "utf8"), output: o.output });
});

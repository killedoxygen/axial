#!/usr/bin/env node
// Stages a verified candidate for publication: release files, notes, and the update feed.
import { copyFile, mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { PLATFORMS } from "./matrix.mjs";
import { changelogRelease, check, digest, jsonFile, main, options, verifyArtifacts, verifyUpdaterSignature, writeJson } from "./contract.mjs";

const REPOSITORY = "https://github.com/killedoxygen/axial";
const PUBLISHED_ROLES = new Set(["installer", "portable-archive", "updater", "updater-signature"]);
const SITE = "https://updates.axial.run";
const NOTICE = `**New to Axial?** Go to **${SITE}**. It picks the right download for your computer and shows exactly what to click when your system asks whether to open it.

Axial isn't signed with a paid certificate, so macOS and Windows warn once. The guide walks you through it in about a minute; after that Axial updates itself.

This is a pre-release. Start with a fresh profile; earlier Axial profiles aren't imported.`;
// The file each platform's download button offers.
const DOWNLOAD_ROLE = { linux: "updater", win32: "updater", darwin: "installer" };
const RELEASE_SLOT = '<script id="release" type="application/json">null</script>';

export async function stagePublication({ source, assets, publicKey, changelog, guide, output, now = new Date() }) {
  const release = path.join(output, "release");
  const site = path.join(output, "site");
  await mkdir(release, { recursive: true });
  await mkdir(site, { recursive: true });
  const platforms = {};
  const downloads = {};
  const sums = [];
  for (const [platform, info] of Object.entries(PLATFORMS)) {
    const directory = path.join(assets, platform);
    const manifest = await verifyArtifacts(directory, source, platform);
    await verifyUpdaterSignature(directory, manifest, publicKey);
    for (const file of manifest.files.filter((item) => PUBLISHED_ROLES.has(item.role))) {
      await copyFile(path.join(directory, file.name), path.join(release, file.name));
      if (file.role !== "updater-signature") sums.push(`${file.sha256}  ${file.name}`);
    }
    const offered = manifest.files.find((item) => item.role === DOWNLOAD_ROLE[info.os]);
    downloads[platform] = { name: offered.name, bytes: offered.bytes, url: `${REPOSITORY}/releases/download/${source.tag}/${offered.name}` };
    const updater = manifest.files.find((item) => item.role === "updater");
    platforms[`axial-rewrite-${info.updater}`] = {
      signature: (await readFile(path.join(directory, `${updater.name}.sig`), "utf8")).trim(),
      url: `${REPOSITORY}/releases/download/${source.tag}/${updater.name}`,
    };
  }
  await writeFile(path.join(release, "SHA256SUMS"), `${sums.sort((a, b) => a.slice(66).localeCompare(b.slice(66))).join("\n")}\n`, { flag: "wx" });
  const { notes } = changelogRelease(changelog, source.version);
  await writeFile(path.join(output, "notes.md"), `${NOTICE}\n\n${notes}\n`, { flag: "wx" });
  const page = `${REPOSITORY}/releases/tag/${source.tag}`;
  const feed = { version: source.version, notes: page, pub_date: now.toISOString(), platforms };
  await writeJson(path.join(site, "latest.json"), feed);
  check(guide.split(RELEASE_SLOT).length === 2, "guide_release_slot_missing");
  const data = JSON.stringify({ version: source.version, page, downloads }).replaceAll("<", "\\u003c");
  await writeFile(path.join(site, "index.html"), guide.replace(RELEASE_SLOT, () => RELEASE_SLOT.replace("null", () => data)), { flag: "wx" });
  check(Object.keys(platforms).length === Object.keys(PLATFORMS).length, "feed_platforms_incomplete");
  return { tag: source.tag, feed_sha256: await digest(path.join(site, "latest.json")), files: sums.length };
}

main(import.meta.url, async () => {
  const o = options(process.argv.slice(2), ["source", "assets", "public-key", "changelog", "guide", "output"]);
  return stagePublication({ source: await jsonFile(o.source), assets: o.assets, publicKey: o["public-key"], changelog: await readFile(o.changelog, "utf8"), guide: await readFile(o.guide, "utf8"), output: o.output });
});

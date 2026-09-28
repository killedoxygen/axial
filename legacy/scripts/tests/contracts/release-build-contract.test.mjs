import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test, { after } from "node:test";

import {
  extractChangelogRelease,
  parseReleaseTag,
  publishRelease,
  readSourceReceipt,
  releaseAssetNames,
  releaseHandoffLayout,
  releasePayloadNames,
  stagePublication,
  verifyReleaseAssets,
  verifyReleaseSource,
  workspaceVersionFromMetadata,
} from "../../release-contract.mjs";

const VERSION = "1.2.3-rc.4";
const TAG = `v${VERSION}`;
const SOURCE_SHA = "a".repeat(40);
const OTHER_SHA = "b".repeat(40);
const TAG_OBJECT_SHA = "c".repeat(40);
const PUBLISHER = path.resolve("scripts/release-contract.mjs");
const ENVIRONMENT = Object.freeze({
  GITHUB_TOKEN: "test-token",
  GITHUB_REPOSITORY: "acme/axial",
  GITHUB_API_URL: "https://api.github.com",
  GITHUB_RUN_ID: "701",
  GITHUB_RUN_ATTEMPT: "2",
});
const temporaryRoots = [];

after(async () => {
  await Promise.all(
    temporaryRoots.map((root) => rm(root, { recursive: true, force: true })),
  );
});

async function temporaryRoot(label) {
  const root = await mkdtemp(path.join(os.tmpdir(), `axial-${label}-`));
  temporaryRoots.push(root);
  return root;
}

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

async function expectCode(promise, code) {
  await assert.rejects(promise, (error) => {
    assert.equal(error?.code, code, error?.stack ?? String(error));
    return true;
  });
}

function changelog(overrides = {}) {
  const unreleased = overrides.unreleased ?? "## [Unreleased]\n\n- Pending\n";
  const current =
    overrides.current ??
    `## [${VERSION}] - 2026-07-21\n\n### Release\n- Atomic publication\n`;
  const previous =
    overrides.previous ??
    "## [1.2.3-rc.3] - 2026-07-20\n\n### Release\n- Previous candidate\n";
  return `# Changelog\n\n${unreleased}\n${current}\n${previous}\n## [1.2.2] - 2026-07-19\n\n- Stable\n`;
}

async function sourceFixture() {
  const root = await temporaryRoot("release-source");
  await mkdir(path.join(root, "apps", "desktop"), { recursive: true });
  await mkdir(path.join(root, "crates", "one"), { recursive: true });
  await mkdir(path.join(root, "crates", "two"), { recursive: true });
  await writeFile(
    path.join(root, "apps", "desktop", "tauri.conf.json"),
    "{}\n",
  );
  await writeFile(path.join(root, "CHANGELOG.md"), changelog());
  const members = ["one", "two"].map((name) => ({
    id: `path+file://${root}/crates/${name}#axial-${name}@${VERSION}`,
    name,
    version: VERSION,
    manifest_path: path.join(root, "crates", name, "Cargo.toml"),
  }));
  for (const member of members) {
    await writeFile(
      member.manifest_path,
      `[package]\nname = "axial-${member.name}"\nversion.workspace = true\n\n[dependencies]\n`,
    );
  }
  const metadata = {
    workspace_root: root,
    workspace_members: members.map(({ id }) => id),
    packages: members,
  };
  return { root, metadata, members };
}

async function assetFixture() {
  const root = await temporaryRoot("release-assets");
  for (const { producer, payloads } of releaseHandoffLayout(VERSION)) {
    const directory = path.join(root, producer);
    await mkdir(directory);
    for (const payload of payloads) {
      const bytes = Buffer.from(`payload:${producer}:${payload}\n`);
      await writeFile(path.join(directory, payload), bytes);
      await writeFile(
        path.join(directory, `${payload}.sha256`),
        `${sha256(bytes)}  ${payload}\n`,
      );
    }
  }
  const verified = await verifyReleaseAssets({
    tag: TAG,
    assetsDirectory: root,
  });
  return { root, verified };
}

async function writeReceipt(
  root,
  overrides = {},
  { publisherPath = PUBLISHER } = {},
) {
  const publisher = await readFile(publisherPath);
  const document = {
    schema: "axial.release-source.v1",
    version: VERSION,
    tag: TAG,
    prerelease: true,
    source_sha: SOURCE_SHA,
    publisher_sha256: sha256(publisher),
    notes: "### Release\n- Atomic publication\n",
    ...overrides,
  };
  const receipt = path.join(
    root,
    `source-${Math.random().toString(16).slice(2)}.json`,
  );
  await writeFile(receipt, `${JSON.stringify(document, null, 2)}\n`);
  return { receipt, document };
}

function response(status, body) {
  return new Response(status === 204 ? null : JSON.stringify(body), {
    status,
    headers: status === 204 ? {} : { "content-type": "application/json" },
  });
}

function remoteAssets(localAssets) {
  return localAssets.map((asset, index) => ({
    id: 1000 + index,
    name: asset.name,
    size: asset.size,
    state: "uploaded",
    digest: `sha256:${asset.sha256}`,
  }));
}

function stagingRelease({
  id = 41,
  sourceSha = SOURCE_SHA,
  runId = "600",
  runAttempt = "1",
  bodyNotes = "old notes\n",
  assets = [],
} = {}) {
  const name = `Axial ${TAG} staging [${runId}.${runAttempt}]`;
  const header = `<!-- axial-release-automation:v1 run=${runId} attempt=${runAttempt} source=${sourceSha} tag=${TAG} -->`;
  return {
    id,
    tag_name: TAG,
    draft: true,
    prerelease: true,
    name,
    body: `${header}\n\n${bodyNotes}`,
    assets,
    upload_url: `https://uploads.github.com/repos/acme/axial/releases/${id}/assets{?name,label}`,
  };
}

function apiFixture(localAssets, behavior = {}) {
  const calls = [];
  let releases = structuredClone(behavior.existing ?? []);
  let nextReleaseId = 77;
  let nextAssetId = 2000;
  let uploadCount = 0;
  let listCount = 0;
  let refCount = 0;
  let patchLost = false;
  let patchLossConsumed = false;
  let createLost = false;
  let uploadLost = false;

  const fetchImpl = async (rawUrl, init = {}) => {
    const url = new URL(rawUrl);
    const method = init.method ?? "GET";
    calls.push({ method, url: url.href, body: init.body });
    const apiPrefix = "/repos/acme/axial";

    if (url.origin === "https://uploads.github.com") {
      const releaseId = Number(
        url.pathname.match(/\/releases\/(\d+)\/assets$/)?.[1],
      );
      const release = releases.find(({ id }) => id === releaseId);
      assert.ok(release, "upload targets a known release ID");
      const name = url.searchParams.get("name");
      const local = localAssets.find((asset) => asset.name === name);
      assert.ok(local, `upload basename is in the local contract: ${name}`);
      uploadCount += 1;
      if (behavior.failUploadAt === uploadCount)
        return response(500, { message: "failed" });
      const uploaded = {
        id: nextAssetId++,
        name,
        size: local.size,
        state: "uploaded",
        digest: `sha256:${local.sha256}`,
      };
      if (behavior.corruptUploadAt === uploadCount)
        uploaded.digest = `sha256:${"f".repeat(64)}`;
      release.assets.push(uploaded);
      if (behavior.uploadLossAt === uploadCount && !uploadLost) {
        uploadLost = true;
        throw new Error("upload response lost after apply");
      }
      return response(201, uploaded);
    }

    assert.equal(url.origin, "https://api.github.com");
    assert.ok(url.pathname.startsWith(apiPrefix));
    const endpoint = url.pathname.slice(apiPrefix.length);

    if (method === "GET" && endpoint === `/git/ref/tags/${TAG}`) {
      refCount += 1;
      const selected = behavior.refs?.[
        Math.min(refCount - 1, behavior.refs.length - 1)
      ] ?? {
        type: "commit",
        sha: SOURCE_SHA,
      };
      return response(200, { ref: `refs/tags/${TAG}`, object: selected });
    }
    if (method === "GET" && endpoint.startsWith("/git/tags/")) {
      const objectSha = endpoint.slice("/git/tags/".length);
      const tagObject = behavior.tagObjects?.[objectSha];
      return response(
        200,
        tagObject ?? {
          sha: objectSha,
          object: { type: "commit", sha: SOURCE_SHA },
        },
      );
    }
    if (method === "GET" && endpoint === "/releases") {
      listCount += 1;
      if (behavior.addCompetitorAtFinalList && listCount === 2) {
        releases.push({
          ...stagingRelease({ id: 99 }),
          name: "manual competitor",
          body: "manual",
        });
      }
      return response(200, structuredClone(releases));
    }
    if (method === "POST" && endpoint === "/releases") {
      const body = JSON.parse(init.body);
      assert.equal(body.target_commitish, SOURCE_SHA);
      const release = {
        id: nextReleaseId++,
        tag_name: body.tag_name,
        draft: body.draft,
        prerelease: body.prerelease,
        name: body.name,
        body: body.body,
        assets: [],
        upload_url: `https://uploads.github.com/repos/acme/axial/releases/${nextReleaseId - 1}/assets{?name,label}`,
      };
      releases.push(release);
      if (behavior.createLossOnce && !createLost) {
        createLost = true;
        throw new Error("create response lost after apply");
      }
      return response(201, structuredClone(release));
    }
    const releaseId = Number(endpoint.match(/^\/releases\/(\d+)$/)?.[1]);
    if (releaseId) {
      const index = releases.findIndex(({ id }) => id === releaseId);
      if (method === "GET") {
        if (behavior.failSettlementGet && patchLost)
          throw new Error("settlement unavailable");
        return index < 0
          ? response(404, { message: "missing" })
          : response(200, structuredClone(releases[index]));
      }
      if (method === "PATCH") {
        assert.ok(index >= 0, "patch targets an existing exact ID");
        if (
          behavior.patchMode === "always-loss-draft" ||
          (behavior.patchMode === "loss-draft" && !patchLossConsumed)
        ) {
          patchLossConsumed = true;
          patchLost = true;
          throw new Error("response lost before apply");
        }
        const body = JSON.parse(init.body);
        Object.assign(releases[index], body);
        if (behavior.patchMode === "loss-public" && !patchLossConsumed) {
          patchLossConsumed = true;
          patchLost = true;
          throw new Error("response lost after apply");
        }
        return response(200, structuredClone(releases[index]));
      }
    }
    throw new Error(`unexpected API call: ${method} ${endpoint}${url.search}`);
  };

  return {
    calls,
    fetchImpl,
    get releases() {
      return structuredClone(releases);
    },
  };
}

const noTimeout = () => undefined;
const blobFactory = async (file, options) =>
  new Blob([await readFile(file)], options);

test("published version and tag grammar is deliberately closed", () => {
  for (const value of [
    "v0.0.0",
    "v1.2.3",
    "v1.2.3-dev.1",
    "v1.2.3-alpha.2",
    "v1.2.3-beta.3",
    TAG,
  ]) {
    assert.equal(parseReleaseTag(value).tag, value);
  }
  for (const value of [
    "1.2.3",
    "V1.2.3",
    "v01.2.3",
    "v1.02.3",
    "v1.2.03",
    "v1.2.3-dev.0",
    "v1.2.3-dev.01",
    "v1.2.3-preview.1",
    "v1.2.3+build",
    " v1.2.3",
  ]) {
    assert.throws(() => parseReleaseTag(value), {
      code: "invalid_release_tag",
    });
  }
});

test("asset authority owns eight payloads and four producer-isolated handoffs", () => {
  assert.equal(releasePayloadNames(VERSION).length, 8);
  assert.equal(releaseAssetNames(VERSION).length, 16);
  assert.deepEqual(
    releaseHandoffLayout(VERSION).map(({ producer, payloads }) => [
      producer,
      payloads.length,
    ]),
    [
      ["linux-amd64", 2],
      ["windows-amd64", 2],
      ["macos-amd64", 2],
      ["macos-arm64", 2],
    ],
  );
});

test("changelog extraction requires newest-first unique canonical sections", () => {
  const release = extractChangelogRelease(changelog(), VERSION);
  assert.equal(release.date, "2026-07-21");
  assert.equal(release.notes, "### Release\n- Atomic publication\n");
  assert.throws(
    () =>
      extractChangelogRelease(
        `# Changelog\n\n## [${VERSION}] - 2026-07-21\n\n- Current\n\n## [Unreleased]\n\n- Pending\n`,
        VERSION,
      ),
    { code: "unreleased_section_not_first" },
  );
  assert.throws(
    () =>
      extractChangelogRelease(
        changelog({
          current: "## [1.2.3-rc.3] - 2026-07-21\n\n- Old\n",
          previous: "## [1.2.3-rc.2] - 2026-07-20\n\n- Older\n",
        }),
        VERSION,
      ),
    { code: "missing_changelog_release" },
  );
  assert.throws(
    () =>
      extractChangelogRelease(
        `${changelog()}\n## [${VERSION}] - 2026-01-01\n\n- Duplicate\n`,
        VERSION,
      ),
    { code: "duplicate_changelog_release" },
  );
  assert.throws(
    () =>
      extractChangelogRelease(
        changelog({ current: `## [${VERSION}] - 2026-02-30\n\n- Bad\n` }),
        VERSION,
      ),
    { code: "invalid_changelog_date" },
  );
  assert.throws(
    () =>
      extractChangelogRelease(
        changelog({ current: `## [${VERSION}] - 2026-07-21\n\n` }),
        VERSION,
      ),
    { code: "empty_changelog_release" },
  );
});

test("Cargo metadata has one strict workspace release version", () => {
  const metadata = {
    workspace_members: ["one", "two"],
    packages: [
      { id: "one", version: VERSION },
      { id: "two", version: VERSION },
    ],
  };
  assert.equal(workspaceVersionFromMetadata(metadata), VERSION);
  metadata.packages[1].version = "1.2.3-rc.3";
  assert.throws(() => workspaceVersionFromMetadata(metadata), {
    code: "mixed_cargo_workspace_versions",
  });
});

test("source verification binds metadata, inherited versions, Tauri inheritance, and latest notes", async () => {
  const fixture = await sourceFixture();
  const verified = await verifyReleaseSource({
    tag: TAG,
    repositoryRoot: fixture.root,
    cargoMetadata: async (root) => {
      assert.equal(root, fixture.root);
      return fixture.metadata;
    },
  });
  assert.equal(verified.version, VERSION);
  assert.equal(verified.prerelease, true);

  await writeFile(
    fixture.members[0].manifest_path,
    `[package]\nname = "one"\nversion = "${VERSION}"\n`,
  );
  await expectCode(
    verifyReleaseSource({
      tag: TAG,
      repositoryRoot: fixture.root,
      cargoMetadata: async () => fixture.metadata,
    }),
    "cargo_version_not_inherited",
  );
  await writeFile(
    fixture.members[0].manifest_path,
    `[package]\nname = "one"\nversion.workspace = true\n`,
  );
  await writeFile(
    path.join(fixture.root, "apps", "desktop", "tauri.conf.json"),
    `{"version":"${VERSION}"}\n`,
  );
  await expectCode(
    verifyReleaseSource({
      tag: TAG,
      repositoryRoot: fixture.root,
      cargoMetadata: async () => fixture.metadata,
    }),
    "authored_tauri_version",
  );
});

test("stage-publication atomically writes only the self-bound publisher and canonical receipt", async () => {
  const fixture = await sourceFixture();
  const output = path.join(fixture.root, "publication");
  await stagePublication({
    tag: TAG,
    sourceSha: SOURCE_SHA,
    outputDirectory: output,
    repositoryRoot: fixture.root,
    cargoMetadata: async () => fixture.metadata,
    gitHead: async () => SOURCE_SHA,
  });
  assert.deepEqual((await readdir(output)).sort(), [
    "release-contract.mjs",
    "source.json",
  ]);
  const receipt = await readSourceReceipt({
    receiptFile: path.join(output, "source.json"),
    tag: TAG,
    sourceSha: SOURCE_SHA,
    publisherPath: path.join(output, "release-contract.mjs"),
  });
  assert.equal(receipt.source_sha, SOURCE_SHA);
  assert.equal(
    receipt.publisher_sha256,
    sha256(await readFile(path.join(output, "release-contract.mjs"))),
  );
  await expectCode(
    stagePublication({
      tag: TAG,
      sourceSha: SOURCE_SHA,
      outputDirectory: path.join(fixture.root, "wrong-sha"),
      repositoryRoot: fixture.root,
      cargoMetadata: async () => fixture.metadata,
      gitHead: async () => OTHER_SHA,
    }),
    "source_sha_not_checked_out",
  );
});

test("receipt rejects tampering, extra fields, mismatched bindings, symlinks, and noncanonical bytes", async () => {
  const root = await temporaryRoot("receipt-reject");
  for (const [overrides, code] of [
    [{ extra: true }, "invalid_source_receipt_shape"],
    [{ tag: "v1.2.3-rc.3" }, "source_receipt_tag_mismatch"],
    [{ source_sha: OTHER_SHA }, "source_receipt_sha_mismatch"],
    [{ publisher_sha256: "0".repeat(64) }, "source_receipt_publisher_mismatch"],
    [{ notes: "" }, "invalid_source_receipt_notes"],
    [{ notes: "bad\r\nnotes\n" }, "invalid_source_receipt_notes"],
    [{ notes: `${"x".repeat(120 * 1024)}\n` }, "invalid_source_receipt_notes"],
  ]) {
    const { receipt } = await writeReceipt(root, overrides);
    await expectCode(
      readSourceReceipt({
        receiptFile: receipt,
        tag: TAG,
        sourceSha: SOURCE_SHA,
      }),
      code,
    );
  }
  const { receipt } = await writeReceipt(root);
  const noncanonical = path.join(root, "noncanonical.json");
  await writeFile(
    noncanonical,
    JSON.stringify(JSON.parse(await readFile(receipt, "utf8"))),
  );
  await expectCode(
    readSourceReceipt({
      receiptFile: noncanonical,
      tag: TAG,
      sourceSha: SOURCE_SHA,
    }),
    "noncanonical_source_receipt",
  );
  const linked = path.join(root, "linked.json");
  await symlink(receipt, linked);
  await expectCode(
    readSourceReceipt({ receiptFile: linked, tag: TAG, sourceSha: SOURCE_SHA }),
    "invalid_source_receipt_type",
  );
});

test("handoff validation rejects missing, extra, symlinked, empty, and noncanonical artifacts", async () => {
  {
    const fixture = await assetFixture();
    await rm(path.join(fixture.root, "windows-amd64"), { recursive: true });
    await expectCode(
      verifyReleaseAssets({ tag: TAG, assetsDirectory: fixture.root }),
      "missing_release_handoffs",
    );
  }
  {
    const fixture = await assetFixture();
    await mkdir(path.join(fixture.root, "foreign"));
    await expectCode(
      verifyReleaseAssets({ tag: TAG, assetsDirectory: fixture.root }),
      "extra_release_handoffs",
    );
  }
  {
    const fixture = await assetFixture();
    await Promise.all(
      Array.from({ length: 61 }, (_, index) =>
        writeFile(path.join(fixture.root, `foreign-${index}`), ""),
      ),
    );
    await expectCode(
      verifyReleaseAssets({ tag: TAG, assetsDirectory: fixture.root }),
      "release_handoffs_too_large",
    );
  }
  {
    const fixture = await assetFixture();
    const producer = path.join(fixture.root, "linux-amd64");
    await Promise.all(
      Array.from({ length: 5 }, (_, index) =>
        writeFile(path.join(producer, `foreign-${index}`), ""),
      ),
    );
    await expectCode(
      verifyReleaseAssets({ tag: TAG, assetsDirectory: fixture.root }),
      "release_handoff_too_large",
    );
  }
  {
    const fixture = await assetFixture();
    const target = path.join(fixture.root, "linux-amd64");
    const moved = `${fixture.root}-linux-real`;
    temporaryRoots.push(moved);
    await rm(moved, { force: true });
    await import("node:fs/promises").then(({ rename: move }) =>
      move(target, moved),
    );
    await symlink(moved, target);
    await expectCode(
      verifyReleaseAssets({ tag: TAG, assetsDirectory: fixture.root }),
      "invalid_release_handoff",
    );
  }
  {
    const fixture = await assetFixture();
    const payload = releaseHandoffLayout(VERSION)[0].payloads[0];
    await writeFile(path.join(fixture.root, "linux-amd64", payload), "");
    await expectCode(
      verifyReleaseAssets({ tag: TAG, assetsDirectory: fixture.root }),
      "empty_release_payload",
    );
  }
  for (const [contents, code] of [
    ["not-a-checksum\n", "malformed_release_sidecar"],
    [`${"0".repeat(64)}  wrong-name\n`, "wrong_release_sidecar_basename"],
    [
      `${"0".repeat(64)}  axial-linux-amd64-${VERSION}\n`,
      "wrong_release_sidecar_digest",
    ],
    [
      `${"0".repeat(64)}  axial-linux-amd64-${VERSION}`,
      "malformed_release_sidecar",
    ],
  ]) {
    const fixture = await assetFixture();
    const payload = releaseHandoffLayout(VERSION)[0].payloads[0];
    await writeFile(
      path.join(fixture.root, "linux-amd64", `${payload}.sha256`),
      contents,
    );
    await expectCode(
      verifyReleaseAssets({ tag: TAG, assetsDirectory: fixture.root }),
      code,
    );
  }
  {
    const fixture = await assetFixture();
    const payload = releaseHandoffLayout(VERSION)[0].payloads[0];
    await writeFile(
      path.join(fixture.root, "linux-amd64", `${payload}.sha256`),
      Buffer.alloc(257, "a"),
    );
    await expectCode(
      verifyReleaseAssets({ tag: TAG, assetsDirectory: fixture.root }),
      "release_sidecar_too_large",
    );
  }
});

async function publicationFixture() {
  const assets = await assetFixture();
  const receiptRoot = await temporaryRoot("publication-receipt");
  const { receipt, document } = await writeReceipt(receiptRoot);
  return { assets, receipt, document };
}

async function publishWithApi(fixture, behavior = {}, options = {}) {
  const api = apiFixture(fixture.assets.verified.assets, behavior);
  const result = publishRelease({
    receiptFile: fixture.receipt,
    tag: TAG,
    sourceSha: SOURCE_SHA,
    assetsDirectory: fixture.assets.root,
    environment: ENVIRONMENT,
    fetchImpl: api.fetchImpl,
    blobFactory,
    signalFactory: noTimeout,
    ...options,
  });
  return { api, result };
}

test("local receipt and handoff failures perform zero network calls", async () => {
  const fixture = await publicationFixture();
  const calls = [];
  await writeFile(fixture.receipt, "{}\n");
  await expectCode(
    publishRelease({
      receiptFile: fixture.receipt,
      tag: TAG,
      sourceSha: SOURCE_SHA,
      assetsDirectory: fixture.assets.root,
      environment: ENVIRONMENT,
      fetchImpl: async (...args) => calls.push(args),
      signalFactory: noTimeout,
    }),
    "invalid_source_receipt_shape",
  );
  assert.equal(calls.length, 0);

  const valid = await publicationFixture();
  await rm(path.join(valid.assets.root, "macos-arm64"), { recursive: true });
  await expectCode(
    publishRelease({
      receiptFile: valid.receipt,
      tag: TAG,
      sourceSha: SOURCE_SHA,
      assetsDirectory: valid.assets.root,
      environment: ENVIRONMENT,
      fetchImpl: async (...args) => calls.push(args),
      signalFactory: noTimeout,
    }),
    "missing_release_handoffs",
  );
  assert.equal(calls.length, 0);
});

test("publisher proves a lightweight tag, uploads all assets by exact ID, then exposes once", async () => {
  const fixture = await publicationFixture();
  const { api, result } = await publishWithApi(fixture);
  const published = await result;
  assert.equal(published.assetCount, 16);
  assert.equal(api.releases.length, 1);
  assert.equal(api.releases[0].draft, false);
  assert.equal(api.releases[0].name, TAG);
  assert.equal(api.releases[0].body, fixture.document.notes);
  const methods = api.calls.map(({ method }) => method);
  assert.equal(methods.filter((method) => method === "PATCH").length, 1);
  assert.equal(
    api.calls.filter(({ url }) => url.startsWith("https://uploads.github.com/"))
      .length,
    16,
  );
  const patchIndex = methods.indexOf("PATCH");
  assert.ok(
    api.calls
      .slice(0, patchIndex)
      .filter(({ url }) => url.startsWith("https://uploads.github.com/"))
      .length === 16,
  );
  assert.ok(
    api.calls.every(
      ({ url, method }) => method === "GET" || !url.includes("/tags/"),
    ),
  );
});

test("lost create and upload responses converge without duplicate asset uploads", async () => {
  for (const behavior of [{ createLossOnce: true }, { uploadLossAt: 4 }]) {
    const fixture = await publicationFixture();
    const api = apiFixture(fixture.assets.verified.assets, behavior);
    const publish = () =>
      publishRelease({
        receiptFile: fixture.receipt,
        tag: TAG,
        sourceSha: SOURCE_SHA,
        assetsDirectory: fixture.assets.root,
        environment: ENVIRONMENT,
        fetchImpl: api.fetchImpl,
        blobFactory,
        signalFactory: noTimeout,
      });
    await assert.rejects(publish());
    assert.equal(api.releases.length, 1);
    assert.equal(api.releases[0].draft, true);
    assert.equal((await publish()).assetCount, 16);
    assert.equal(api.releases[0].draft, false);
    assert.equal(
      api.calls.filter(({ url }) =>
        url.startsWith("https://uploads.github.com/"),
      ).length,
      16,
    );
    assert.equal(
      api.calls.some(({ method }) => method === "DELETE"),
      false,
    );
  }
});

test("annotated tags are peeled with cycle, type, depth, and moved-ref failures closed", async () => {
  {
    const fixture = await publicationFixture();
    const { result } = await publishWithApi(fixture, {
      refs: [{ type: "tag", sha: TAG_OBJECT_SHA }],
      tagObjects: {
        [TAG_OBJECT_SHA]: {
          sha: TAG_OBJECT_SHA,
          object: { type: "commit", sha: SOURCE_SHA },
        },
      },
    });
    assert.equal((await result).assetCount, 16);
  }
  for (const [tagObjects, code] of [
    [
      {
        [TAG_OBJECT_SHA]: {
          sha: TAG_OBJECT_SHA,
          object: { type: "tag", sha: TAG_OBJECT_SHA },
        },
      },
      "github_tag_cycle",
    ],
    [
      {
        [TAG_OBJECT_SHA]: {
          sha: TAG_OBJECT_SHA,
          object: { type: "tree", sha: SOURCE_SHA },
        },
      },
      "invalid_github_tag_object_type",
    ],
  ]) {
    const fixture = await publicationFixture();
    const { api, result } = await publishWithApi(fixture, {
      refs: [{ type: "tag", sha: TAG_OBJECT_SHA }],
      tagObjects,
    });
    await expectCode(result, code);
    assert.equal(
      api.calls.some(({ method }) => method === "POST"),
      false,
    );
  }
  {
    const chain = {};
    let current = TAG_OBJECT_SHA;
    for (let index = 0; index < 9; index += 1) {
      const next = createHash("sha1").update(`tag-${index}`).digest("hex");
      chain[current] = { sha: current, object: { type: "tag", sha: next } };
      current = next;
    }
    const fixture = await publicationFixture();
    const { result } = await publishWithApi(fixture, {
      refs: [{ type: "tag", sha: TAG_OBJECT_SHA }],
      tagObjects: chain,
    });
    await expectCode(result, "github_tag_depth_exceeded");
  }
  {
    const fixture = await publicationFixture();
    const { api, result } = await publishWithApi(fixture, {
      refs: [
        { type: "commit", sha: SOURCE_SHA },
        { type: "commit", sha: OTHER_SHA },
      ],
    });
    await expectCode(result, "github_tag_commit_mismatch");
    assert.equal(
      api.calls.some(({ method }) => method === "PATCH"),
      false,
    );
    assert.equal(api.releases.length, 1);
    assert.equal(api.releases[0].draft, true);
  }
});

test("same-source drafts resume while manual, moved, and ambiguous drafts fail closed", async () => {
  {
    const fixture = await publicationFixture();
    const stale = stagingRelease({ bodyNotes: fixture.document.notes });
    const { api, result } = await publishWithApi(fixture, {
      existing: [stale],
    });
    assert.equal((await result).assetCount, 16);
    assert.equal(
      api.calls.some(
        ({ method, url }) =>
          method === "POST" &&
          url === "https://api.github.com/repos/acme/axial/releases",
      ),
      false,
    );
    assert.equal(
      api.calls.some(({ method }) => method === "DELETE"),
      false,
    );
    assert.equal(api.releases[0].id, 41);
    assert.equal(api.releases[0].draft, false);
  }
  {
    const fixture = await publicationFixture();
    const partial = remoteAssets(fixture.assets.verified.assets).slice(0, 5);
    const { api, result } = await publishWithApi(fixture, {
      existing: [
        stagingRelease({ assets: partial, bodyNotes: fixture.document.notes }),
      ],
    });
    assert.equal((await result).assetCount, 16);
    assert.equal(
      api.calls.filter(({ url }) =>
        url.startsWith("https://uploads.github.com/"),
      ).length,
      11,
    );
  }
  for (const [existing, code] of [
    [
      { ...stagingRelease(), name: "manual", body: "manual" },
      "unowned_draft_exists",
    ],
    [stagingRelease({ sourceSha: OTHER_SHA }), "stale_draft_source_mismatch"],
  ]) {
    const fixture = await publicationFixture();
    const { api, result } = await publishWithApi(fixture, {
      existing: [existing],
    });
    await expectCode(result, code);
    assert.equal(
      api.calls.some(({ method }) => method === "DELETE"),
      false,
    );
    assert.equal(
      api.calls.some(({ method }) => method === "POST"),
      false,
    );
  }
  {
    const fixture = await publicationFixture();
    const foreign = {
      id: 900,
      name: "foreign.bin",
      size: 1,
      state: "uploaded",
      digest: `sha256:${"0".repeat(64)}`,
    };
    const corrupt = {
      ...remoteAssets(fixture.assets.verified.assets)[0],
      digest: `sha256:${"0".repeat(64)}`,
    };
    for (const asset of [foreign, corrupt]) {
      const { api, result } = await publishWithApi(fixture, {
        existing: [
          stagingRelease({
            assets: [asset],
            bodyNotes: fixture.document.notes,
          }),
        ],
      });
      await assert.rejects(result);
      assert.equal(
        api.calls.some(({ url }) =>
          url.startsWith("https://uploads.github.com/"),
        ),
        false,
      );
      assert.equal(
        api.calls.some(({ method }) => method === "PATCH"),
        false,
      );
    }
  }
  {
    const fixture = await publicationFixture();
    const { api, result } = await publishWithApi(fixture, {
      existing: [stagingRelease({ id: 40 }), stagingRelease({ id: 41 })],
    });
    await expectCode(result, "ambiguous_tag_releases");
    assert.equal(
      api.calls.some(({ method }) => method === "DELETE"),
      false,
    );
  }
});

test("matching public releases are idempotent while mismatched public releases are immutable refusals", async () => {
  const fixture = await publicationFixture();
  const complete = {
    ...stagingRelease({
      id: 55,
      assets: remoteAssets(fixture.assets.verified.assets),
    }),
    draft: false,
    name: TAG,
    body: fixture.document.notes,
  };
  {
    const { api, result } = await publishWithApi(fixture, {
      existing: [complete],
    });
    assert.equal((await result).status, "already-published");
    assert.equal(
      api.calls.some(({ method }) => method !== "GET"),
      false,
    );
  }
  {
    const { api, result } = await publishWithApi(fixture, {
      existing: [{ ...complete, body: "manual notes" }],
    });
    await expectCode(result, "invalid_published_release");
    assert.equal(
      api.calls.some(({ method }) => method !== "GET"),
      false,
    );
  }
  {
    const { api, result } = await publishWithApi(fixture, {
      existing: [complete],
      refs: [
        { type: "commit", sha: SOURCE_SHA },
        { type: "commit", sha: OTHER_SHA },
      ],
    });
    await expectCode(result, "github_tag_commit_mismatch");
    assert.equal(
      api.calls.some(({ method }) => method !== "GET"),
      false,
    );
  }
});

test("upload and final race failures never PATCH or delete any draft", async () => {
  for (const behavior of [
    { failUploadAt: 3 },
    { corruptUploadAt: 4 },
    { addCompetitorAtFinalList: true },
  ]) {
    const fixture = await publicationFixture();
    const { api, result } = await publishWithApi(fixture, behavior);
    await assert.rejects(result);
    assert.equal(
      api.calls.some(({ method }) => method === "PATCH"),
      false,
    );
    assert.equal(
      api.calls.some(({ method }) => method === "DELETE"),
      false,
    );
    if (behavior.addCompetitorAtFinalList) {
      assert.deepEqual(
        api.releases.map(({ id }) => id),
        [77, 99],
      );
    }
  }
});

test("ambiguous PATCH outcomes settle public success or preserve an indeterminate draft", async () => {
  {
    const fixture = await publicationFixture();
    const { api, result } = await publishWithApi(fixture, {
      patchMode: "loss-public",
    });
    assert.equal((await result).status, "published-after-ambiguous-response");
    assert.equal(api.releases[0].draft, false);
    assert.equal(
      api.calls.some(({ method }) => method === "DELETE"),
      false,
    );
  }
  {
    const fixture = await publicationFixture();
    const { api, result } = await publishWithApi(fixture, {
      refs: [
        { type: "commit", sha: SOURCE_SHA },
        { type: "commit", sha: SOURCE_SHA },
        { type: "commit", sha: OTHER_SHA },
      ],
    });
    await expectCode(result, "release_settlement_indeterminate");
    assert.equal(api.releases[0].draft, false);
    assert.equal(
      api.calls.filter(({ method }) => method === "PATCH").length,
      1,
    );
    assert.equal(
      api.calls.some(({ method }) => method === "DELETE"),
      false,
    );
  }
  {
    const fixture = await publicationFixture();
    const { api, result } = await publishWithApi(fixture, {
      patchMode: "loss-draft",
    });
    assert.equal((await result).status, "published-after-ambiguous-response");
    assert.equal(api.releases.length, 1);
    assert.equal(api.releases[0].draft, false);
    assert.equal(
      api.calls.filter(({ method }) => method === "PATCH").length,
      2,
    );
  }
  {
    const fixture = await publicationFixture();
    const { api, result } = await publishWithApi(fixture, {
      patchMode: "always-loss-draft",
    });
    await expectCode(result, "release_settlement_indeterminate");
    assert.equal(api.releases.length, 1);
    assert.equal(api.releases[0].draft, true);
    assert.equal(
      api.calls.some(({ method }) => method === "DELETE"),
      false,
    );
  }
  {
    const fixture = await publicationFixture();
    const { api, result } = await publishWithApi(fixture, {
      patchMode: "loss-public",
      failSettlementGet: true,
    });
    await expectCode(result, "release_settlement_indeterminate");
    assert.equal(api.releases[0].draft, false);
    assert.equal(
      api.calls.some(({ method }) => method === "DELETE"),
      false,
    );
  }
});

test("later invocations converge public and draft states left by ambiguous PATCH responses", async () => {
  {
    const behavior = { patchMode: "loss-public", failSettlementGet: true };
    const fixture = await publicationFixture();
    const api = apiFixture(fixture.assets.verified.assets, behavior);
    const publish = () =>
      publishRelease({
        receiptFile: fixture.receipt,
        tag: TAG,
        sourceSha: SOURCE_SHA,
        assetsDirectory: fixture.assets.root,
        environment: ENVIRONMENT,
        fetchImpl: api.fetchImpl,
        blobFactory,
        signalFactory: noTimeout,
      });
    await expectCode(publish(), "release_settlement_indeterminate");
    const recovered = await publish();
    assert.equal(recovered.status, "already-published");
    assert.equal(recovered.assetCount, 16);
    assert.equal(api.releases.length, 1);
    assert.equal(api.releases[0].draft, false);
    assert.equal(
      api.calls.filter(({ url }) =>
        url.startsWith("https://uploads.github.com/"),
      ).length,
      16,
    );
    assert.equal(
      api.calls.filter(({ method }) => method === "PATCH").length,
      1,
    );
    assert.equal(
      api.calls.some(({ method }) => method === "DELETE"),
      false,
    );
  }
  {
    const behavior = { patchMode: "always-loss-draft" };
    const fixture = await publicationFixture();
    const api = apiFixture(fixture.assets.verified.assets, behavior);
    const publish = () =>
      publishRelease({
        receiptFile: fixture.receipt,
        tag: TAG,
        sourceSha: SOURCE_SHA,
        assetsDirectory: fixture.assets.root,
        environment: ENVIRONMENT,
        fetchImpl: api.fetchImpl,
        blobFactory,
        signalFactory: noTimeout,
      });
    await expectCode(publish(), "release_settlement_indeterminate");
    assert.equal(api.releases[0].draft, true);
    behavior.patchMode = undefined;
    assert.equal((await publish()).assetCount, 16);
    assert.equal(api.releases[0].draft, false);
    assert.equal(
      api.calls.some(({ method }) => method === "DELETE"),
      false,
    );
  }
});

test("release publication has no destructive GitHub request path", async () => {
  assert.doesNotMatch(
    await readFile(PUBLISHER, "utf8"),
    /method:\s*["']DELETE["']/,
  );
});

test("GitHub environment is bounded to the supported API and all external requests receive timeouts", async () => {
  const fixture = await publicationFixture();
  const calls = [];
  await expectCode(
    publishRelease({
      receiptFile: fixture.receipt,
      tag: TAG,
      sourceSha: SOURCE_SHA,
      assetsDirectory: fixture.assets.root,
      environment: {
        ...ENVIRONMENT,
        GITHUB_API_URL: "https://github.example/api/v3",
      },
      fetchImpl: async (...args) => calls.push(args),
      signalFactory: noTimeout,
    }),
    "invalid_github_api_url",
  );
  assert.equal(calls.length, 0);

  const durations = [];
  const { result } = await publishWithApi(
    fixture,
    {},
    {
      signalFactory: (duration) => {
        durations.push(duration);
        return undefined;
      },
    },
  );
  await result;
  assert.ok(durations.includes(30_000));
  assert.equal(durations.filter((duration) => duration === 600_000).length, 16);
});

test("receipt and handoff roots themselves must be regular, non-symlinked filesystem objects", async () => {
  const fixture = await publicationFixture();
  const linkedRoot = `${fixture.assets.root}-link`;
  await symlink(fixture.assets.root, linkedRoot);
  await expectCode(
    verifyReleaseAssets({ tag: TAG, assetsDirectory: linkedRoot }),
    "invalid_assets_directory",
  );
  const info = await lstat(fixture.receipt);
  assert.ok(info.isFile());
});

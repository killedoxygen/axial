import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { access, readFile } from "node:fs/promises";
import test from "node:test";

import { capabilityRegistry } from "../../capabilities/registry.mjs";
import {
  parseProvenanceManifest,
  verifyAssetProvenance,
} from "../../verify-assets.mjs";

const hash = (bytes) => createHash("sha256").update(bytes).digest("hex");

test("release and development Tauri consumers select only the approved icon triples", async () => {
  const release = JSON.parse(
    await readFile("apps/desktop/tauri.conf.json", "utf8"),
  );
  const development = JSON.parse(
    await readFile("apps/desktop/tauri.dev.conf.json", "utf8"),
  );
  assert.deepEqual(release.bundle.icon, [
    "icons/macos/icon.icns",
    "icons/icon.png",
    "icons/icon.ico",
  ]);
  assert.deepEqual(development.bundle.icon, [
    "icons/dev/macos/icon.icns",
    "icons/dev/icon.png",
    "icons/dev/icon.ico",
  ]);

  const build = await readFile("apps/desktop/build.rs", "utf8");
  for (const [constant, icon] of [
    ["DEV_ICON_ICNS", "icons/dev/macos/icon.icns"],
    ["DEV_ICON_PNG", "icons/dev/icon.png"],
    ["DEV_ICON_ICO", "icons/dev/icon.ico"],
  ]) {
    assert.match(
      build,
      new RegExp(`const ${constant}: &str = "${icon.replaceAll(".", "\\.")}";`),
    );
    assert.match(build, new RegExp(`rerun-if-changed=\\{${constant}\\}`));
  }
  assert.match(build, /merge_json\(&mut config, dev_config\)/);
  assert.doesNotMatch(
    build,
    /icons\/source|icon\.icon|assets\/icon\.ico|winres/,
  );
});

test("one brand manifest owns frontend SVG geometry and all generated destinations", async () => {
  const logo = await readFile("frontend/src/ui/Logo.tsx", "utf8");
  assert.match(
    logo,
    /import brandMark from '\.\.\/\.\.\/\.\.\/assets\/brand-mark\.json'/,
  );
  for (const key of ["ribbon", "top_right", "bottom_left"]) {
    assert.match(logo, new RegExp(`brandMark\\.paths\\.${key}`));
  }
  assert.doesNotMatch(logo, /d=["'][Mm]/);

  const generator = await readFile("scripts/generate-icons.mjs", "utf8");
  assert.match(generator, /readToolchainIdentity/);
  assert.match(generator, /commitPath: "assets\/provenance\.json"/);
  assert.match(generator, /Nothing fallible may run after it/);
  assert.doesNotMatch(generator, /resvg|sharp|canvas|generate-lock|journal/i);
});

test("all LoaderKey values map totally to distinct neutral marks and reach every instance surface", async () => {
  const mapping = await readFile(
    "frontend/src/views/create/loader-logos.tsx",
    "utf8",
  );
  const expected = {
    vanilla: "loader-base.svg",
    fabric: "loader-grid.svg",
    forge: "loader-cross.svg",
    neoforge: "loader-orbit.svg",
    quilt: "loader-diamonds.svg",
  };
  for (const [loader, asset] of Object.entries(expected)) {
    assert.match(
      mapping,
      new RegExp(`${loader}: '${asset.replace(".", "\\.")}'`),
    );
  }
  assert.match(mapping, /Record<LoaderKey, string>/);
  assert.doesNotMatch(mapping, /return null|\?\?/);

  const visual = await readFile("frontend/src/ui/InstanceVisual.tsx", "utf8");
  assert.match(visual, /loaderLogoSrc\(loader\)/);
  assert.match(visual, /version_display\?\.loader_key/);
  assert.match(visual, /loaderKeyFromVersion/);
  assert.match(visual, /loaderKeyFromComponentId/);
  assert.doesNotMatch(visual, /<svg|data-loader/);

  const consumers = await Promise.all(
    [
      "frontend/src/shell/Sidebar.tsx",
      "frontend/src/ui/InstanceCard.tsx",
      "frontend/src/views/create/CreateView.tsx",
      "frontend/src/views/discover/TargetBar.tsx",
      "frontend/src/views/home/HomeView.tsx",
      "frontend/src/views/instance/InstanceDetailView.tsx",
      "frontend/src/views/instances/InstancesView.tsx",
    ].map((path) => readFile(path, "utf8")),
  );
  assert.ok(
    consumers.every((source) =>
      /InstanceTile|InstanceGlyph|LoaderLogo/.test(source),
    ),
  );
});

test("font, favicon, Microsoft authentication, and sound consumers resolve retained assets", async () => {
  const index = await readFile("frontend/static/index.html", "utf8");
  assert.match(index, /href="favicon\.png"/);

  const css = await readFile("frontend/src/base.css", "utf8");
  for (const file of [
    "Manrope-cyrillic-ext.woff2",
    "Manrope-cyrillic.woff2",
    "Manrope-greek.woff2",
    "Manrope-vietnamese.woff2",
    "Manrope-latin-ext.woff2",
    "Manrope-latin.woff2",
    "GeistMono-Variable.woff2",
  ]) {
    assert.match(css, new RegExp(`fonts/${file.replaceAll(".", "\\.")}`));
  }

  const microsoft = await readFile("frontend/src/ui/MicrosoftMark.tsx", "utf8");
  assert.match(microsoft, /<img/);
  assert.match(microsoft, /src="microsoft-auth-symbol\.svg"/);
  const authenticationConsumers = [
    "frontend/src/views/onboarding/Onboarding.tsx",
    "frontend/src/views/accounts/AccountSwitcher.tsx",
  ];
  for (const path of authenticationConsumers)
    assert.match(await readFile(path, "utf8"), /<MicrosoftMark/);
  const microsoftReferences = [
    ...(
      await readFile("frontend/src/views/onboarding/Onboarding.tsx", "utf8")
    ).matchAll(/MicrosoftMark/g),
    ...(
      await readFile("frontend/src/views/accounts/AccountSwitcher.tsx", "utf8")
    ).matchAll(/MicrosoftMark/g),
  ];
  assert.ok(microsoftReferences.length >= 4);
  assert.equal(
    hash(await readFile("frontend/static/microsoft-auth-symbol.svg")),
    "929f48f88c8ca7f3f5d294be47ec4caf51acc28ac25340c19a903125d7ecd84a",
  );

  const sound = await readFile("frontend/src/sound.ts", "utf8");
  const sprite = JSON.parse(
    await readFile("frontend/static/sounds/snd01/audioSprite.json", "utf8"),
  );
  assert.ok(
    sprite.spritemap.celebration.end > sprite.spritemap.celebration.start,
  );
  assert.match(sound, /case 'launchSuccess':[\s\S]*?playSprite\('celebration'/);
  assert.match(sound, /case 'launchSuccess':[\s\S]*?this\.sequence\(/);
  assert.doesNotMatch(
    sound,
    /launch\.ogg|customBuffer|decodeAudioData\([^)]*launch/i,
  );
});

test("the production capability registry and Task gates own the four portable asset proofs", async () => {
  assert.deepEqual(
    capabilityRegistry.map(({ scenario_id }) => scenario_id).sort(),
    ["CP-OA-FONTS", "CP-OA-ICONS", "CP-OA-LOADER-MARKS", "CP-OA-PROVENANCE"],
  );
  for (const record of capabilityRegistry) {
    assert.deepEqual(record.allowed_platforms, ["linux", "windows", "macos"]);
    assert.equal(record.owner_phase, "P00");
    assert.match(record.proof_id, /^CAP-OA-/);
  }

  const taskfile = await readFile("Taskfile.yml", "utf8");
  assert.match(taskfile, /^  assets:generate:/m);
  assert.match(taskfile, /^  assets:check:/m);
  assert.doesNotMatch(taskfile, /^  icons:/m);
  assert.match(
    taskfile,
    /verify:linux:[\s\S]*?- task: assets:check[\s\S]*?- task: verify:delivery/,
  );
  for (const task of ["verify:native:windows", "verify:native:macos"]) {
    assert.match(
      taskfile,
      new RegExp(
        `${task.replaceAll(":", "\\:")}:[\\s\\S]*?p00-b03-contract\\.test\\.mjs scripts/tests/contracts/p00-b03-contract-cross-owner\\.test\\.mjs[\\s\\S]*?node scripts/verify-assets\\.mjs`,
      ),
    );
  }
});

test("each portable asset scenario reruns to the same current receipt", async () => {
  for (const record of capabilityRegistry) {
    const implementation = await import(record.module_url.href);
    const context = {
      scenario_id: record.scenario_id,
      proof_id: record.proof_id,
      capability_id: record.capability_id,
      owner_phase: record.owner_phase,
      platform:
        process.platform === "win32"
          ? "windows"
          : process.platform === "darwin"
            ? "macos"
            : "linux",
      repository_root: process.cwd(),
    };
    const first = await implementation.runScenario(Object.freeze(context));
    assert.equal(first.ok, true);
    assert.equal(first.observations.length, 1);
    const current = await implementation.readCurrentReceipts(
      Object.freeze({
        ...context,
        observations: first.observations.map(({ id }) => id),
      }),
    );
    assert.deepEqual(
      current.observations,
      first.observations.map(({ id, receipt }) => ({ id, receipt })),
    );
  }
});

test("provenance owns exactly the shipped inventory and retired duplicates stay absent", async () => {
  await verifyAssetProvenance();
  const parsed = parseProvenanceManifest(
    await readFile("assets/provenance.json", "utf8"),
  );
  assert.equal(parsed.paths.length, 26);

  const retired = [
    "assets/icon.ico",
    "fabric_logo_0.svg",
    "winres/icon.ico",
    "winres/winres.json",
    "apps/desktop/icons/dev/icon.svg",
    "apps/desktop/icons/source/axial-dev-flat.svg",
    "apps/desktop/icons/source/axial-dev-glyph.svg",
    "apps/desktop/icons/source/axial-flat.svg",
    "apps/desktop/icons/source/axial-glyph.svg",
    "frontend/static/fabric_icon.svg",
    "frontend/static/forge_icon.svg",
    "frontend/static/neoforge_icon.svg",
    "frontend/static/quilt_icon.svg",
    "frontend/static/vanilla_icon.svg",
    "frontend/static/logo.png",
    "frontend/static/sounds/launch.ogg",
  ];
  for (const path of retired)
    await assert.rejects(access(path), { code: "ENOENT" });
});

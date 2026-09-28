import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import { configResponse } from '../../src/dto-core';

const repositoryRoot = basename(process.cwd()) === 'frontend' ? resolve(process.cwd(), '..') : process.cwd();
/** @param {string} path */
const read = (path) => readFile(resolve(repositoryRoot, path), 'utf8');
const internalFields = ['telemetry_install_id', 'feature_overrides', 'library_dir', 'library_mode'];

test('config route uses a strict revisioned public projection without Guardian or private authority', async () => {
  const [model, route, frontend, generated] = await Promise.all([
    read('core/app/src/settings/model.rs'),
    read('apps/api/src/routes/config.rs'),
    read('frontend/src/types-settings.ts'),
    read('frontend/src/generated/ConfigView.ts'),
  ]);
  const view = model.match(/pub struct ConfigView \{([\s\S]*?)\n\}/)?.[1] ?? '';
  const patch = model.match(/pub struct ConfigPatch \{([\s\S]*?)\n\}/)?.[1] ?? '';
  const generatedView = generated.match(/export type ConfigView = \{([\s\S]*?)\};/)?.[1] ?? '';
  assert.match(view, /pub revision: u64/);
  assert.match(view, /pub account_selection_revision: u64/);
  assert.match(model, /#\[serde\(deny_unknown_fields\)\]\s*pub struct ConfigPatch/);
  assert.match(patch, /pub expected_revision: u64/);
  assert.match(patch, /expected_account_selection_revision: Option<u64>/);
  assert.match(patch, /theme: Option<ConfigTheme>/);
  assert.match(patch, /#\[serde\(default, deserialize_with = "present"\)\]\s*pub username: Option<String>/);
  assert.match(model, /Option::<T>::deserialize\(deserializer\)\.map\(Self::Set\)/);
  assert.match(route, /Result<Json<ConfigPatch>, JsonRejection>/);
  assert.match(route, /Result<Json<ConfigView>, ApiError>/);
  assert.match(frontend, /export type \{ ConfigView as Config \} from ['"]\.\/generated\/ConfigView['"]/);
  assert.match(generatedView, /\brevision: number/);
  assert.match(generatedView, /\baccount_selection_revision: number/);
  assert.deepEqual(
    [...generatedView.matchAll(/\b(\w+):/g)].map((match) => match[1]).sort(),
    [...view.matchAll(/\bpub (\w+):/g)].map((match) => match[1]).sort(),
    'generated ConfigView must retain every required Rust projection field',
  );
  for (const field of [...internalFields, 'guardian_mode', 'guardian_idle_integrity_enabled']) {
    for (const source of [view, patch, frontend, generatedView])
      assert.doesNotMatch(source, new RegExp(`\\b${field}:`));
  }
});

test('config and flags retain shared revisioned persistence and committed-consent publication', async () => {
  const [config, flags, store, status, setup, main] = await Promise.all([
    read('apps/api/src/routes/config.rs'),
    read('apps/api/src/routes/flags.rs'),
    read('core/app/src/settings/mod.rs'),
    read('apps/api/src/routes/mod.rs'),
    read('core/app/src/instances/setup.rs'),
    read('frontend/src/main.tsx'),
  ]);
  assert.match(config, /\.try_spawn\([\s\S]*?consent_change_owned\(\)[\s\S]*?spawn_blocking/);
  assert.match(config, /commit_with_transaction\([\s\S]*?Ok\(commit\) => \{\s*consent\.publish/);
  assert.match(config, /suppress_failure = patch\.telemetry_enabled == Some\(false\)/);
  assert.match(config, /if !suppress_failure \{\s*report_save_failure/);
  assert.match(flags, /\.try_spawn\([\s\S]*?state\.settings\.update_flag\(&key, patch\)/);
  assert.match(flags, /report_save_failure\(&state\.telemetry, error\)/);
  assert.match(store, /document\.config\.revision != expected_revision/);
  assert.match(store, /WHERE singleton = 1 AND revision = \?3/);
  assert.match(store, /self\.mutate\(patch\.expected_revision/);
  assert.doesNotMatch(store, /use [^;]*telemetry::|TelemetryEvent|consent_change_owned/);
  const statusView = status.match(/async fn status\([\s\S]*?\n\}/)?.[0] ?? '';
  const setupView = setup.match(/pub struct SetupStatusResponse \{([\s\S]*?)\n\}/)?.[1] ?? '';
  assert.ok(statusView && setupView);
  for (const source of [statusView, setupView, main]) assert.doesNotMatch(source, /library_(?:dir|mode)/);
});

test('config decoder validates revisions and retained fields and projects away internal data', () => {
  const fixture = {
    revision: 4,
    account_selection_revision: 7,
    username: 'A',
    launch_auth_mode: 'online',
    max_memory_mb: 4096,
    min_memory_mb: 512,
    java_path_override: '',
    window_width: 0,
    window_height: 0,
    jvm_preset: '',
    performance_mode: 'managed',
    theme: '',
    custom_hue: null,
    custom_vibrancy: null,
    lightness: null,
    onboarding_done: false,
    telemetry_enabled: false,
    discord_rpc_enabled: true,
    discord_rpc_onboarding_seen: false,
    music_enabled: null,
    music_volume: null,
    music_track: 0,
  };
  assert.deepEqual(configResponse(fixture), fixture);
  for (const field of ['revision', 'account_selection_revision']) {
    for (const invalid of [undefined, null, '4', -1, 0.5, Number.MAX_SAFE_INTEGER + 1, Infinity]) {
      assert.throws(() => configResponse({ ...fixture, [field]: invalid }), /Config/);
    }
  }
  for (const invalid of [
    { launch_auth_mode: 'guest' },
    { theme: 'guardian' },
    { performance_mode: 'unknown' },
    { jvm_preset: 'unsupported' },
    { telemetry_enabled: 'true' },
    { music_enabled: 'true' },
    { music_track: undefined },
    { max_memory_mb: NaN },
    { custom_hue: '12' },
  ])
    assert.throws(() => configResponse({ ...fixture, ...invalid }), /Config/);
  const projected = configResponse({
    ...fixture,
    telemetry_install_id: 'private',
    library_dir: '/private',
    guardian_mode: 'managed',
  });
  assert.deepEqual(projected, fixture);
  assert.equal(configResponse({ ...fixture, custom_hue: 12 }).custom_hue, 12);
  assert.equal(configResponse({ ...fixture, music_enabled: false }).music_enabled, false);
});

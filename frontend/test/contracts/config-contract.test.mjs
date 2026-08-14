import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { basename, resolve } from 'node:path';
import test from 'node:test';

const repositoryRoot = basename(process.cwd()) === 'frontend' ? resolve(process.cwd(), '..') : process.cwd();
/** @param {string} path */
const read = (path) => readFile(resolve(repositoryRoot, path), 'utf8');

const internalFields = ['telemetry_install_id', 'feature_overrides', 'library_dir', 'library_mode'];

test('config route uses a strict revisioned public projection', async () => {
  const [application, route, frontend] = await Promise.all([
    read('apps/api/src/application/config.rs'),
    read('apps/api/src/routes/config.rs'),
    read('frontend/src/types-settings.ts'),
  ]);
  const view = application.match(/pub struct ConfigView \{([\s\S]*?)\n\}/)?.[1] ?? '';
  const patch = application.match(/pub struct ConfigPatch \{([\s\S]*?)\n\}/)?.[1] ?? '';

  assert.match(view, /pub revision: u64/);
  assert.match(application, /#\[serde\(deny_unknown_fields\)\][\s\S]*?pub struct ConfigPatch/);
  assert.match(patch, /guardian_mode: Option<ConfigGuardianMode>/);
  assert.match(patch, /theme: Option<ConfigTheme>/);
  assert.match(route, /Json<application::ConfigView>/);
  assert.doesNotMatch(route, /Json<AppConfig>/);
  assert.match(frontend, /revision: number/);
  for (const field of internalFields) {
    assert.doesNotMatch(view, new RegExp(`pub ${field}:`));
    assert.doesNotMatch(frontend, new RegExp(`\\b${field}:`));
  }
});

test('config and flags share consent-aware persistence projection', async () => {
  const [config, flags, status, setup, main] = await Promise.all([
    read('apps/api/src/application/config.rs'),
    read('apps/api/src/application/flags.rs'),
    read('apps/api/src/application/status.rs'),
    read('apps/api/src/application/setup.rs'),
    read('frontend/src/main.tsx'),
  ]);

  assert.match(config, /pub\(super\) async fn persist_config_mutation/);
  assert.match(config, /ConfigFailureTelemetry::SuppressForConsentDisable/);
  assert.match(config, /!state\.config\(\)\.current\(\)\.telemetry_enabled/);
  assert.match(flags, /persist_config_mutation\([\s\S]*?RespectCommittedConsent/);
  const statusView = status.match(/pub struct StatusResponse \{([\s\S]*?)\n\}/)?.[1] ?? '';
  const setupView = setup.match(/pub struct SetupLibraryResponse \{([\s\S]*?)\n\}/)?.[1] ?? '';
  for (const source of [statusView, setupView, main]) {
    assert.doesNotMatch(source, /library_(?:dir|mode)/);
  }
});

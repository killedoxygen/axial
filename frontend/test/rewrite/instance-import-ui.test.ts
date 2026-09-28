import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import type { ImportPreview } from '../../src/generated/ImportPreview';
import type { EnrichedInstance } from '../../src/types-instance';
import type { Config } from '../../src/types-settings';
import type { MetadataImportReceipt } from '../../src/generated/MetadataImportReceipt';
import type { SkinImportResponse } from '../../src/generated/SkinImportResponse';
import type { RulesImportReceipt } from '../../src/generated/RulesImportReceipt';
import type { WardrobeData } from '../../src/machines/skin-wardrobe';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const ts: typeof import('typescript') = createRequire(resolve(frontend, 'package.json'))('typescript');

function source<T>(path: string, imports: Record<string, unknown> = {}, globals: Record<string, unknown> = {}): T {
  const filename = resolve(frontend, 'src', path);
  const { outputText } = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: {
      module: ts.ModuleKind.CommonJS,
      target: ts.ScriptTarget.ES2020,
      jsx: ts.JsxEmit.ReactJSX,
      jsxImportSource: 'preact',
    },
  });
  const exports = {};
  vm.runInNewContext(
    outputText,
    {
      exports,
      Error,
      TextEncoder,
      URLSearchParams,
      require(id: string): unknown {
        if (id === '@preact/signals')
          return { signal: <V>(value: V) => ({ value }), batch: (run: () => void) => run() };
        if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
        throw new Error(`Unreviewed import test dependency: ${id}`);
      },
      ...globals,
    },
    { filename },
  );
  return exports as T;
}

function deferred<T>() {
  let done!: (value: T) => void;
  const promise = new Promise<T>((resolve) => {
    done = resolve;
  });
  return { promise, resolve: done };
}
const settle = (): Promise<void> => new Promise((resolve) => setImmediate(resolve));
const legacyId = '0000000000000001';
const instanceId = '94d4ec3a-4a90-4d70-9e97-a5774f1c0a8a';
const metadataId = 'c'.repeat(64);
const skinImportId = 'd'.repeat(64);
const rulesImportId = '9'.repeat(64);
const rulesReceipt: RulesImportReceipt = {
  rules_import_id: rulesImportId,
  fingerprint: 'a'.repeat(64),
  cache_sha256: '7'.repeat(64),
  refresh_history: [
    { operation_id: `op-${instanceId}`, sequence: '2', outcome: { state: 'succeeded', cache_changed: true } },
    {
      operation_id: 'op-94d4ec3a-4a90-4d70-8e97-a5774f1c0a8a',
      sequence: '7',
      outcome: { state: 'failed', failure_point: 'refresh_remote_rules' },
    },
  ],
};
const skinReceipt: SkinImportResponse['receipt'] = {
  skin_import_id: skinImportId,
  fingerprint: 'a'.repeat(64),
  texture_keys: ['e'.repeat(64)],
};
const offlineId = 'offline-92d74dda76fe332cb669d0daddfb7952';
const secondOfflineId = 'offline-761ba00914be3766a1c04a87a961857c';
const microsoftSource = 'microsoft-msa-1111111111111111-2222222222222222';
const microsoftId = '01234567-89ab-cdef-0123-456789abcdef';
const receipt: MetadataImportReceipt = {
  metadata_import_id: metadataId,
  imported_offline_account_count: 1,
  imported_microsoft_account_count: 0,
  account_id_mapping: { [offlineId]: offlineId },
  settings_revision: 8,
  account_selection_revision: 5,
};
const settings: Config = {
  revision: 7,
  account_selection_revision: 4,
  username: 'Current_Player',
  launch_auth_mode: 'offline',
  max_memory_mb: 4096,
  min_memory_mb: 512,
  java_path_override: '',
  window_width: 1280,
  window_height: 720,
  jvm_preset: '',
  performance_mode: 'managed',
  theme: '',
  custom_hue: null,
  custom_vibrancy: null,
  lightness: null,
  onboarding_done: true,
  telemetry_enabled: false,
  discord_rpc_enabled: false,
  discord_rpc_onboarding_seen: true,
  music_enabled: null,
  music_volume: null,
  music_track: 0,
};
function preview(overrides: Partial<ImportPreview> = {}): ImportPreview {
  return {
    fingerprint: 'a'.repeat(64),
    cutover_available: false,
    metadata_import_id: metadataId,
    metadata_import_available: true,
    skin_import_id: skinImportId,
    skin_import_available: true,
    rules_import_id: rulesImportId,
    rules_import_available: false,
    instances: [
      {
        legacy_id: legacyId,
        name: 'Vanilla Fixture',
        loader_key: 'vanilla',
        ordinary_import_available: true,
        blockers: [],
      },
    ],
    file_count: 5,
    byte_count: 200,
    offline_account_count: 1,
    microsoft_reauthentication_count: 0,
    saved_skin_count: 0,
    retained_obligation_count: 0,
    retained_records: [],
    blockers: ['cutover_not_implemented'],
    ...overrides,
  };
}
const instance: EnrichedInstance = {
  id: instanceId,
  name: 'Vanilla Fixture',
  version_id: '1.21.1',
  created_at: '2026-09-08T12:00:00Z',
  version_display: {
    loader_key: 'vanilla',
    loader_label: 'Vanilla',
    minecraft_label: '1.21.1',
    loader_version_label: '',
    loader_detail_label: '',
    summary_label: 'Minecraft 1.21.1',
    supports_mods: false,
  },
  launchable: false,
  launch_action: {
    state_id: 'install_required',
    label: 'Install',
    tone: 'warn',
    launchable: false,
    primary_action: 'install',
  },
  saves_count: 1,
  mods_count: 0,
  resource_count: 0,
  shader_count: 0,
};
const contract = source<typeof import('../../src/dto-contract')>('dto-contract.ts');
const apiBoundary = source<typeof import('../../src/api')>(
  'api.ts',
  {
    './native': {},
    './dto-contract': contract,
  },
  { __AXIAL_WEB_API_BASE__: '', __AXIAL_TEST_API_CAPABILITY__: '' },
);
const installDto = source<typeof import('../../src/dto-install')>('dto-install.ts', { './dto-contract': contract });
const coreDto = source<typeof import('../../src/dto-core')>('dto-core.ts', {
  './dto-contract': contract,
  './dto-install': installDto,
});
const preferences = source<typeof import('../../src/preferences/local')>('preferences/local.ts');
const transfer = source<typeof import('../../src/profile-transfer')>('profile-transfer.ts', {
  './preferences/local': preferences,
  './default-skins': source('default-skins.ts'),
});
type ViewNode = { type: unknown; props: Record<string, unknown> };
const jsx = {
  jsx: (type: unknown, props: Record<string, unknown>) => ({ type, props }),
  jsxs: (type: unknown, props: Record<string, unknown>) => ({ type, props }),
  Fragment: 'Fragment',
};
function nodes(value: unknown): ViewNode[] {
  if (Array.isArray(value)) return value.flatMap(nodes);
  if (!value || typeof value !== 'object' || !('props' in value)) return [];
  const node = value as ViewNode;
  return [node, ...nodes(node.props.children), ...nodes(node.props.control)];
}

function harness(desktop = true) {
  const calls: Array<[string, string, unknown?]> = [];
  const nativeCalls: Array<[string, Record<string, unknown> | undefined]> = [];
  const notices: string[] = [];
  const navigation: unknown[] = [];
  const store = { instances: { value: [] as EnrichedInstance[] }, config: { value: { ...settings, revision: 1 } } };
  const accountSnapshot = {
    value: {
      state: 'ready',
      selection_revision: 4,
      accounts: [{ account_id: offlineId }, { account_id: microsoftId }],
    },
  };
  const stored = new Map<string, string>([
    ['axial_rewrite_ui', '{"theme":"birch"}'],
    ['axial-rewrite:route', '{"name":"settings"}'],
  ]);
  let writeStorage: (() => void) | null = null;
  const storage = {
    getItem: (key: string) => stored.get(key) ?? null,
    setItem: (key: string, value: string) => {
      writeStorage?.();
      stored.set(key, value);
    },
    removeItem: (key: string) => {
      writeStorage?.();
      stored.delete(key);
    },
  };
  // These retained import cases exercise browser storage/rollback, while the
  // native preference owner has its own profile-backed behavioral suite.
  const browserPreferences = {
    './preferences/local': preferences,
    './native': { hasNativeDesktopRuntime: () => false },
    './preferences/persistence': { canEditPreferences: () => true },
  };
  const localState = source<typeof import('../../src/state')>('state.ts', browserPreferences, {
    localStorage: storage,
  });
  const uiState = source<typeof import('../../src/ui-state')>('ui-state.ts', browserPreferences, {
    localStorage: storage,
  });
  let reload: (() => void) | null = null;
  let reloadAdmission: (() => boolean | Promise<boolean>) | null = null;
  const reloads: true[] = [];
  const accountRefreshes: unknown[] = [];
  const flagRefreshes: unknown[] = [];
  const readinessRefreshes: true[] = [];
  const musicConfigs: Array<[Config, boolean]> = [];
  const themeConfigs: Config[] = [];
  const wardrobe = { value: { state: 'ready', skins: [], pendingApply: null, error: null } as WardrobeData };
  const wardrobeRefreshes: true[] = [];
  let currentSettings = { ...settings };
  let currentPreview: unknown = preview();
  let picker: (() => Promise<unknown>) | null = null;
  let read: (() => Promise<unknown>) | null = null;
  let post: (() => Promise<unknown>) | null = null;
  let registry: (() => Promise<unknown>) | null = null;
  let metadataPost: (() => Promise<unknown>) | null = null;
  let metadataRead: (() => Promise<unknown>) | null = null;
  let skinPost: (() => Promise<unknown>) | null = null;
  let skinRead: (() => Promise<unknown>) | null = null;
  let rulesPost: (() => Promise<unknown>) | null = null;
  let rulesRead: (() => Promise<unknown>) | null = null;
  let mappingRead: (() => Promise<unknown>) | null = null;
  let wardrobeRead: (() => Promise<void>) | null = null;
  let configRead: (() => Promise<unknown>) | null = null;
  let accountsRead: (() => Promise<void>) | null = null;
  let flagsRead: (() => Promise<void>) | null = null;
  let applyMusic: ((value: Config, sync: boolean) => void) | null = null;
  let applyTheme: ((value: Config, preferenceVersion: number) => Promise<void>) | null = null;
  const actions = source<typeof import('../../src/actions')>('actions.ts', {
    './store': store,
    './launch-response-adapters': {},
  });
  const native = source<typeof import('../../src/native')>(
    'native.ts',
    { './dto-contract': contract },
    {
      window: desktop
        ? {
            __TAURI__: {
              core: {
                async invoke(command: string, args?: Record<string, unknown>) {
                  nativeCalls.push([command, args]);
                  if (command === 'forget_import_profile') return;
                  if (command === 'pick_import_profile' || command === 'pick_import_instance_source')
                    return picker ? picker() : currentPreview;
                  throw new Error(`Unexpected native command: ${command}`);
                },
              },
            },
          }
        : {},
    },
  );
  const helpers = source<typeof import('../../src/views/settings/instance-import')>(
    'views/settings/instance-import.ts',
    {
      '../../api': {
        isApiError: apiBoundary.isApiError,
        async api(method: string, path: string, body?: unknown) {
          calls.push([method, path, body]);
          if (method === 'GET' && path === '/import/preview') return read ? read() : currentPreview;
          if (method === 'GET' && path.startsWith('/import/instances/'))
            return mappingRead
              ? mappingRead()
              : {
                  fingerprint: 'a'.repeat(64),
                  metadata_import_id: metadataId,
                  instance_id_mapping: { [legacyId]: instanceId },
                  cutover_available: false,
                };
          if (method === 'POST' && path === '/import/instances')
            return post ? post() : { legacy_id: legacyId, cutover_available: false, instance };
          if (method === 'GET' && path === '/instances')
            return registry ? registry() : { instances: [instance], last_instance_id: null };
          if (method === 'GET' && path === '/config') return configRead ? configRead() : currentSettings;
          if (method === 'POST' && path === '/import/metadata') {
            if (metadataPost) return metadataPost();
            currentSettings = { ...settings, revision: 8, account_selection_revision: 5, username: 'Imported_Player' };
            return { receipt, already_imported: false, cutover_available: false };
          }
          if (method === 'GET' && path.startsWith('/import/metadata/'))
            return metadataRead ? metadataRead() : { receipt: null, cutover_available: false };
          if (method === 'POST' && path === '/import/skins')
            return skinPost ? skinPost() : { receipt: skinReceipt, already_imported: false, cutover_available: false };
          if (method === 'GET' && path.startsWith('/import/skins/'))
            return skinRead ? skinRead() : { receipt: null, cutover_available: false };
          if (method === 'POST' && path === '/import/rules')
            return rulesPost
              ? rulesPost()
              : {
                  receipt: rulesReceipt,
                  already_imported: false,
                  stored_cache_matches_import: true,
                  cutover_available: false,
                };
          if (method === 'GET' && path.startsWith('/import/rules/'))
            return rulesRead
              ? rulesRead()
              : { receipt: null, stored_cache_matches_import: false, cutover_available: false };
          throw new Error(`Unexpected import request: ${method} ${path}`);
        },
      },
      '../../actions': {
        setConfig: actions.setConfig,
        addInstance: (next: EnrichedInstance) => store.instances.value.push(next),
        updateInstanceInList: (next: EnrichedInstance) => {
          store.instances.value = store.instances.value.map((row) => (row.id === next.id ? next : row));
        },
      },
      '../../dto-core': coreDto,
      '../../dto-contract': contract,
      '../../native': { ...native, hasNativeDesktopRuntime: () => false },
      '../../preferences/local': preferences,
      '../../preferences/persistence': {
        reloadApplication() {
          if (reloadAdmission) return reloadAdmission();
          reloads.push(true);
          reload?.();
          return true;
        },
      },
      '../../profile-transfer': transfer,
      '../../state': localState,
      '../../music': {
        Music: {
          applyConfig: (value: Config, sync: boolean) => {
            musicConfigs.push([value, sync]);
            applyMusic?.(value, sync);
          },
        },
      },
      '../../theme': {
        applyImportedConfigTheme: async (value: Config, preferenceVersion: number) => {
          themeConfigs.push(value);
          await applyTheme?.(value, preferenceVersion);
        },
      },
      '../../flags': {
        refreshFlags: async (options: unknown) => {
          flagRefreshes.push(options);
          await flagsRead?.();
        },
      },
      '../../instance-readiness': {
        refreshInstanceReadiness: async () => {
          readinessRefreshes.push(true);
        },
      },
      '../../machines/accounts': {
        accountsSnapshot: accountSnapshot,
        refreshAccountsData: async (options: unknown) => {
          accountRefreshes.push(options);
          if (accountsRead) await accountsRead();
          else
            accountSnapshot.value = {
              ...accountSnapshot.value,
              state: 'ready',
              selection_revision: currentSettings.account_selection_revision,
            };
        },
      },
      '../../machines/skin-wardrobe': {
        wardrobeData: wardrobe,
        refreshWardrobe: async () => {
          wardrobeRefreshes.push(true);
          if (wardrobeRead) await wardrobeRead();
          else wardrobe.value = { ...wardrobe.value, state: 'ready', error: null };
        },
      },
      '../../store': store,
      '../../toast': { toast: (message: string) => notices.push(message) },
      '../../ui-state': { ...uiState, navigate: (route: unknown) => navigation.push(route) },
      '../../utils': { errMessage: (error: unknown) => (error instanceof Error ? error.message : String(error)) },
    },
    {
      localStorage: storage,
      location: {
        reload() {
          reloads.push(true);
          reload?.();
        },
      },
    },
  );
  const workflow = helpers.createInstanceImportWorkflow();
  const { InstanceImportRow } = source<typeof import('../../src/views/settings/InstanceImportRow')>(
    'views/settings/InstanceImportRow.tsx',
    {
      './instance-import': { ...helpers, createInstanceImportWorkflow: () => workflow },
      '../create/defaults': source('views/create/defaults.ts'),
      '../../native': native,
      '../../ui/Atoms': { Button: 'Button' },
      '../../ui/Select': { SelectField: 'SelectField' },
      '../../ui/SettingsSheet': { SettingRow: 'SettingRow' },
      '../../ui/Modal': {
        Modal: 'Modal',
        ModalContent: 'ModalContent',
        ModalHeader: 'ModalHeader',
        ModalTitle: 'ModalTitle',
      },
      'preact/hooks': {
        useMemo: <V>(calculate: () => V) => calculate(),
        useEffect() {},
        useRef: () => ({ current: null }),
      },
      'preact/jsx-runtime': jsx,
    },
  );
  return {
    workflow,
    helpers,
    calls,
    nativeCalls,
    notices,
    navigation,
    store,
    accountSnapshot,
    accountRefreshes,
    flagRefreshes,
    readinessRefreshes,
    musicConfigs,
    themeConfigs,
    wardrobe,
    wardrobeRefreshes,
    stored,
    storage,
    localState,
    uiState,
    reloads,
    setStorageWriter(value: (() => void) | null) {
      writeStorage = value;
    },
    setReload(value: (() => void) | null) {
      reload = value;
    },
    setReloadAdmission(value: (() => boolean | Promise<boolean>) | null) {
      reloadAdmission = value;
    },
    setMappingRead(value: (() => Promise<unknown>) | null) {
      mappingRead = value;
    },
    setPreview(value: unknown) {
      currentPreview = value;
    },
    setPicker(value: (() => Promise<unknown>) | null) {
      picker = value;
    },
    setRead(value: (() => Promise<unknown>) | null) {
      read = value;
    },
    setPost(value: (() => Promise<unknown>) | null) {
      post = value;
    },
    setRegistry(value: (() => Promise<unknown>) | null) {
      registry = value;
    },
    setMetadataPost(value: (() => Promise<unknown>) | null) {
      metadataPost = value;
    },
    setMetadataRead(value: (() => Promise<unknown>) | null) {
      metadataRead = value;
    },
    setSkinPost(value: (() => Promise<unknown>) | null) {
      skinPost = value;
    },
    setSkinRead(value: (() => Promise<unknown>) | null) {
      skinRead = value;
    },
    setRulesPost(value: (() => Promise<unknown>) | null) {
      rulesPost = value;
    },
    setRulesRead(value: (() => Promise<unknown>) | null) {
      rulesRead = value;
    },
    setWardrobeRead(value: (() => Promise<void>) | null) {
      wardrobeRead = value;
    },
    setConfigRead(value: (() => Promise<unknown>) | null) {
      configRead = value;
    },
    setAccountsRead(value: (() => Promise<void>) | null) {
      accountsRead = value;
    },
    setFlagsRead(value: (() => Promise<void>) | null) {
      flagsRead = value;
    },
    setSettings(value: Config) {
      currentSettings = value;
    },
    setMusicApply(value: (config: Config, sync: boolean) => void) {
      applyMusic = value;
    },
    setThemeApply(value: (config: Config, preferenceVersion: number) => Promise<void>) {
      applyTheme = value;
    },
    view() {
      return nodes(InstanceImportRow());
    },
  };
}

function button(h: ReturnType<typeof harness>, label: string): ViewNode {
  const node = h.view().find((node) => node.type === 'Button' && node.props.children === label);
  assert.ok(node, `Expected ${label} button`);
  return node;
}

function textOf(value: unknown): string {
  if (Array.isArray(value)) return value.map(textOf).join('');
  if (typeof value === 'string' || typeof value === 'number') return String(value);
  if (value && typeof value === 'object' && 'props' in value) return textOf((value as ViewNode).props.children);
  return '';
}

test('instance choices label only supported blank predecessor loaders as Vanilla', async () => {
  for (const [loader_key, blockers, available, label] of [
    ['', [], true, 'Vanilla'],
    ['', ['missing_instance_source'], false, 'Vanilla'],
    ['', ['unsupported_loader'], false, 'Unknown loader'],
    ['future-loader', ['unsupported_loader'], false, 'future-loader'],
  ] as const) {
    const h = harness();
    h.setPreview(
      preview({
        instances: [
          {
            ...preview().instances[0],
            loader_key,
            blockers: [...blockers],
            ordinary_import_available: available,
          },
        ],
      }),
    );
    await h.workflow.chooseProfile();
    const select = h
      .view()
      .find((node) => node.type === 'SelectField' && node.props.ariaLabel === 'Instance to import');
    assert.ok(select);
    assert.equal((select.props.options as Array<{ label: string }>)[0].label, `Vanilla Fixture (${label})`);
  }
});

function preferenceExport(references = false) {
  return {
    format: 'axial-browser-preferences',
    version: 1,
    preferences: {
      ...preferences.defaultLocalPreferences(),
      theme: 'custom',
      customHue: 217,
      selectedSkin: references ? `saved:${skinReceipt.texture_keys[0]}` : 'default:alex',
      selectedSkinsByAccount: references ? { [`account:${microsoftSource}`]: 'default:steve' } : {},
    },
    route: references ? { name: 'content', id: 'modrinth:unchanged', target: legacyId } : { name: 'settings' },
  };
}

function preferenceFile(references = false): Pick<File, 'size' | 'text'> {
  const text = JSON.stringify(preferenceExport(references));
  return { size: new TextEncoder().encode(text).length, text: async () => text };
}

function withPreferenceReferences(h: ReturnType<typeof harness>): void {
  h.setMetadataRead(async () => ({
    receipt: {
      ...receipt,
      imported_offline_account_count: 0,
      imported_microsoft_account_count: 1,
      account_id_mapping: { [microsoftSource]: microsoftId },
    },
    cutover_available: false,
  }));
  h.setSkinRead(async () => ({ receipt: skinReceipt, cutover_available: false }));
  h.wardrobe.value = {
    ...h.wardrobe.value,
    skins: [
      {
        texture_key: skinReceipt.texture_keys[0],
        name: 'Imported skin',
        variant: 'classic',
        source: 'local_upload',
        cape_id: null,
        created_at: '2026-09-08T12:00:00Z',
        updated_at: '2026-09-08T12:00:00Z',
        applied_at: null,
        byte_size: 180,
      },
    ],
  };
}

test('file selection resolves exact imported references before explicit association and ordinary reload', async () => {
  const h = harness();
  withPreferenceReferences(h);
  await h.workflow.chooseProfile();
  const input = h.view().find((node) => node.type === 'input' && node.props.type === 'file');
  assert.ok(input);
  const target = { files: [preferenceFile(true)], value: 'selected.json' };
  (input.props.onChange as (event: unknown) => void)({ currentTarget: target });
  await settle();
  assert.equal(target.value, '');
  assert.equal(h.workflow.state.value.phase, 'confirming-preferences');
  const copy = h.view().map(textOf).join(' ');
  assert.match(copy, /belongs to the selected older profile/);
  assert.match(copy, /source is not recorded/);
  assert.match(copy, /Unsaved drafts will be discarded/);
  assert.match(copy, /Close other Axial tabs/);
  h.setReload(() => {
    h.localState.local.theme = 'end';
    h.localState.saveLocalState();
    h.uiState.navigate({ name: 'home' });
  });
  (button(h, 'Apply preferences and reload').props.onClick as () => void)();
  await settle();
  assert.equal(h.workflow.state.value.phase, 'reloading');
  assert.equal(h.reloads.length, 1);
  assert.equal(
    h.calls.some(([method]) => method !== 'GET'),
    false,
  );
  assert.equal(
    h.nativeCalls.some(([command]) => command === 'app_restart'),
    false,
  );
  const imported = JSON.parse(h.storage.getItem('axial_rewrite_ui')!);
  assert.equal(imported.theme, 'custom');
  assert.equal(imported.customHue, 217);
  assert.deepEqual(imported.selectedSkinsByAccount, { [`account:${microsoftId}`]: 'default:steve' });
  assert.deepEqual(JSON.parse(h.storage.getItem('axial-rewrite:route')!), {
    name: 'content',
    id: 'modrinth:unchanged',
    target: instanceId,
  });
  h.workflow.close();
  assert.equal(h.workflow.state.value.phase, 'reloading');
  assert.equal(h.calls.filter(([, path]) => path.startsWith('/import/instances/')).length, 2);
});

test('ID-free exports read no account, skin or instance owners; cancel and invalid files never write', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  const before = [...h.stored];
  for (const file of [
    {
      size: 1_048_577,
      text: async () => {
        throw new Error('Oversized file should not be read');
      },
    },
    { size: 1, text: async () => '{' },
  ]) {
    await h.workflow.choosePreferences(file);
    assert.equal(h.workflow.state.value.phase, 'preview');
    assert.ok(h.workflow.state.value.error);
  }
  const input = h.view().find((node) => node.type === 'input');
  (input!.props.onChange as (event: unknown) => void)({ currentTarget: { files: [], value: '' } });
  assert.equal(h.calls.length, 0);
  await h.workflow.choosePreferences(preferenceFile());
  assert.equal(h.workflow.state.value.phase, 'confirming-preferences');
  assert.deepEqual(
    h.calls.map(([, path]) => path),
    ['/import/preview'],
  );
  assert.equal(h.accountRefreshes.length + h.wardrobeRefreshes.length, 0);
  (button(h, 'Cancel').props.onClick as () => void)();
  assert.equal(h.workflow.state.value.phase, 'preview');
  assert.equal(h.workflow.state.value.preferences, null);
  assert.deepEqual([...h.stored], before);
  assert.equal(h.reloads.length, 0);
});

test('missing receipts, removed references and same-fingerprint wrong-source mappings block confirmation', async () => {
  const failures = [
    (h: ReturnType<typeof harness>) => h.setMetadataRead(async () => ({ receipt: null, cutover_available: false })),
    (h: ReturnType<typeof harness>) =>
      h.setMetadataRead(async () => ({ receipt: { ...receipt, account_id_mapping: null }, cutover_available: false })),
    (h: ReturnType<typeof harness>) => {
      h.accountSnapshot.value.accounts = [];
    },
    (h: ReturnType<typeof harness>) => h.setSkinRead(async () => ({ receipt: null, cutover_available: false })),
    (h: ReturnType<typeof harness>) => {
      h.wardrobe.value = { ...h.wardrobe.value, skins: [] };
    },
    (h: ReturnType<typeof harness>) =>
      h.setMappingRead(async () => ({
        fingerprint: 'a'.repeat(64),
        metadata_import_id: metadataId,
        instance_id_mapping: {},
        cutover_available: false,
      })),
    (h: ReturnType<typeof harness>) =>
      h.setMappingRead(async () => ({
        fingerprint: 'a'.repeat(64),
        metadata_import_id: 'f'.repeat(64),
        instance_id_mapping: { [legacyId]: instanceId },
        cutover_available: false,
      })),
  ];
  for (const fail of failures) {
    const h = harness();
    withPreferenceReferences(h);
    fail(h);
    const before = [...h.stored];
    await h.workflow.chooseProfile();
    await h.workflow.choosePreferences(preferenceFile(true));
    assert.equal(h.workflow.state.value.phase, 'preview');
    assert.ok(h.workflow.state.value.error);
    assert.equal(h.workflow.state.value.preferences, null);
    assert.deepEqual([...h.stored], before);
    assert.equal(h.reloads.length, 0);
  }
});

test('Apply revalidates bindings and rejects a removed target or changed native source', async () => {
  for (const changed of ['instance', 'skin', 'account', 'source']) {
    const h = harness();
    withPreferenceReferences(h);
    await h.workflow.chooseProfile();
    await h.workflow.choosePreferences(preferenceFile(true));
    const before = [...h.stored];
    if (changed === 'instance')
      h.setMappingRead(async () => ({
        fingerprint: 'a'.repeat(64),
        metadata_import_id: metadataId,
        instance_id_mapping: {},
        cutover_available: false,
      }));
    if (changed === 'skin') h.wardrobe.value = { ...h.wardrobe.value, skins: [] };
    if (changed === 'account') h.accountSnapshot.value.accounts = [];
    if (changed === 'source') h.setPreview(preview({ metadata_import_id: 'f'.repeat(64) }));
    await h.workflow.applyPreferences();
    assert.equal(h.workflow.state.value.phase, 'confirming-preferences');
    assert.ok(h.workflow.state.value.error);
    assert.deepEqual([...h.stored], before);
    assert.equal(h.reloads.length, 0);
    assert.equal(
      h.calls.some(([method]) => method !== 'GET'),
      false,
    );
  }
});

test('Apply fences all four live/raw witnesses and observed instance publications during the last mapping read', async () => {
  const changes = [
    (h: ReturnType<typeof harness>) => {
      h.localState.local.sounds = false;
    },
    (h: ReturnType<typeof harness>) => {
      h.uiState.route.value = { name: 'accounts' };
    },
    (h: ReturnType<typeof harness>) => {
      h.stored.set('axial_rewrite_ui', '{"theme":"end"}');
    },
    (h: ReturnType<typeof harness>) => {
      h.stored.set('axial-rewrite:route', '{"name":"accounts"}');
    },
    (h: ReturnType<typeof harness>) => {
      h.store.instances.value = [];
    },
    (h: ReturnType<typeof harness>) => {
      h.accountSnapshot.value = { ...h.accountSnapshot.value };
    },
    (h: ReturnType<typeof harness>) => {
      h.wardrobe.value = { ...h.wardrobe.value };
    },
  ];
  for (const change of changes) {
    const h = harness();
    withPreferenceReferences(h);
    await h.workflow.chooseProfile();
    await h.workflow.choosePreferences(preferenceFile(true));
    const mapping = deferred<unknown>();
    h.setMappingRead(() => mapping.promise);
    const applying = h.workflow.applyPreferences();
    await settle();
    assert.equal(h.calls[h.calls.length - 1]?.[1], `/import/instances/${'a'.repeat(64)}`);
    change(h);
    const before = [...h.stored];
    mapping.resolve({
      fingerprint: 'a'.repeat(64),
      metadata_import_id: metadataId,
      instance_id_mapping: { [legacyId]: instanceId },
      cutover_available: false,
    });
    await applying;
    assert.equal(h.workflow.state.value.phase, 'confirming-preferences');
    assert.match(h.workflow.state.value.error ?? '', /changed/);
    assert.deepEqual([...h.stored], before);
    assert.equal(h.reloads.length, 0);
  }
});

test('closing while the file or final recheck is pending discards completion without writes or reload', async () => {
  for (const stage of ['file', 'apply']) {
    const h = harness();
    await h.workflow.chooseProfile();
    const pending = deferred<string>();
    let operation: Promise<void>;
    if (stage === 'file') operation = h.workflow.choosePreferences({ size: 100, text: () => pending.promise });
    else {
      await h.workflow.choosePreferences(preferenceFile());
      h.setRead(async () => {
        await pending.promise;
        return preview();
      });
      operation = h.workflow.applyPreferences();
    }
    const before = [...h.stored];
    h.workflow.close();
    pending.resolve(JSON.stringify(preferenceExport()));
    await operation;
    assert.equal(h.workflow.state.value.phase, 'closed');
    assert.deepEqual([...h.stored], before);
    assert.equal(h.reloads.length, 0);
  }
});

test('restored storage or reload failures resume writers; incomplete rollback remains reload-only and fenced', async () => {
  for (const mode of ['write', 'reload', 'rollback']) {
    const h = harness();
    await h.workflow.chooseProfile();
    await h.workflow.choosePreferences(preferenceFile());
    const before = [...h.stored];
    let writes = 0;
    h.setStorageWriter(() => {
      writes += 1;
      if (mode !== 'reload' && (writes === 2 || (mode === 'rollback' && writes > 2)))
        throw new Error('Storage unavailable');
    });
    if (mode === 'reload')
      h.setReload(() => {
        throw new Error('Reload refused');
      });
    await h.workflow.applyPreferences();
    const incomplete = mode === 'rollback';
    assert.equal(h.workflow.state.value.phase, incomplete ? 'preference-recovery' : 'confirming-preferences');
    assert.match(h.workflow.state.value.error ?? '', incomplete ? /could not be fully restored/ : /were restored/);
    if (!incomplete) assert.deepEqual([...h.stored], before);
    const retained = [...h.stored];
    h.setStorageWriter(null);
    h.localState.local.theme = 'end';
    h.localState.saveLocalState();
    h.uiState.navigate({ name: 'accounts' });
    if (incomplete) {
      assert.deepEqual([...h.stored], retained);
      h.workflow.close();
      assert.equal(h.workflow.state.value.phase, 'preference-recovery');
      assert.equal(
        h.view().some((node) => node.type === 'Button' && node.props.children === 'Apply preferences and reload'),
        false,
      );
      h.setReloadAdmission(async () => false);
      await h.workflow.reloadPreferences();
      await h.workflow.reloadPreferences();
      assert.equal(h.workflow.state.value.phase, 'preference-recovery');
      assert.ok(button(h, 'Reload interface'));
      h.setReloadAdmission(null);
      (button(h, 'Reload interface').props.onClick as () => void)();
      await settle();
      assert.equal(h.workflow.state.value.phase, 'reloading');
      assert.deepEqual([...h.stored], retained);
      assert.doesNotMatch(h.view().map(textOf).join(' '), /Preferences saved/);
    } else {
      assert.equal(JSON.parse(h.storage.getItem('axial_rewrite_ui')!).theme, 'end');
      assert.equal(JSON.parse(h.storage.getItem('axial-rewrite:route')!).name, 'accounts');
    }
  }
});

test('a fresh account refresh cannot reuse or publish an older in-flight selection read', async () => {
  const oldDirectory = deferred<unknown>();
  const oldStatus = deferred<unknown>();
  const snapshot = (revision: number) => ({
    revision,
    selection_revision: revision,
    launch_auth_mode: 'offline',
    accounts: [],
  });
  let directories = 0;
  let statuses = 0;
  const accountState = source<typeof import('../../src/machines/accounts-state')>('machines/accounts-state.ts');
  const owner = source<typeof import('../../src/machines/accounts')>('machines/accounts.ts', {
    './accounts-state': accountState,
    '../api': {
      api: async (_method: string, path: string) =>
        path === '/accounts'
          ? ++directories === 1
            ? oldDirectory.promise
            : snapshot(6)
          : ++statuses === 1
            ? oldStatus.promise
            : snapshot(6),
    },
    '../views/accounts/api': {
      launcherAccountsResponse: (value: unknown) => value,
      authStatusResponse: (value: unknown) => value,
      isRecord: contract.isDtoRecord,
    },
    '../actions': {},
    '../native': {},
    '../player-name': {},
    '../player-skin': { refreshAccountSkin() {} },
    '../toast': {},
    '../ui/Dialog': {},
    '../views/accounts/auth': {},
    '../dto-core': coreDto,
    '../instance-readiness': {},
  });
  const oldRead = owner.refreshAccountsData();
  await owner.refreshAccountsData({ fresh: true });
  assert.equal(owner.accountsSnapshot.value.selection_revision, 6);
  oldDirectory.resolve(snapshot(4));
  oldStatus.resolve(snapshot(4));
  await oldRead;
  assert.equal(owner.accountsSnapshot.value.selection_revision, 6);
  assert.equal(directories, 2);
  assert.equal(statuses, 2);
});

test('a fresh flags refresh queues a new read and older completion cannot clear its ownership', async () => {
  const oldRead = deferred<unknown>();
  const newRead = deferred<unknown>();
  let reads = 0;
  const store = { featureFlags: { value: null }, featureFlagsLoadState: { value: { status: 'idle', error: null } } };
  const owner = source<typeof import('../../src/flags')>('flags.ts', {
    './api': { api: async () => (++reads === 1 ? oldRead.promise : newRead.promise) },
    './store': store,
    './toast': {},
    './utils': { errMessage: String },
    './dto-contract': contract,
  });
  const before = owner.refreshFlags();
  const after = owner.refreshFlags({ fresh: true });
  assert.notEqual(before, after);
  oldRead.resolve({ revision: 7, flags: [] });
  await before;
  assert.equal(owner.refreshFlags(), after);
  newRead.resolve({ revision: 8, flags: [] });
  await after;
  assert.equal(reads, 2);
  assert.equal(store.featureFlagsLoadState.value.status, 'ready');
});

test('shared config publication rejects older settings and older selections independently', () => {
  const store = { config: { value: { ...settings, revision: 8, account_selection_revision: 6, username: 'Latest' } } };
  const owner = source<typeof import('../../src/actions')>('actions.ts', {
    './store': store,
    './launch-response-adapters': {},
  });
  assert.equal(owner.setConfig({ ...settings, revision: 8, account_selection_revision: 5 }), false);
  assert.equal(owner.setConfig({ ...settings, revision: 7, account_selection_revision: 6 }), false);
  assert.equal(store.config.value.username, 'Latest');
  assert.equal(owner.setConfig({ ...settings, revision: 9, account_selection_revision: 6 }), true);
});

test('a delayed import config read cannot replace a newer equal-settings-revision account selection', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  await h.workflow.prepareMetadataImport();
  const read = deferred<unknown>();
  h.setConfigRead(() => read.promise);
  const importing = h.workflow.importMetadata();
  await settle();
  h.store.config.value = { ...settings, revision: 8, account_selection_revision: 6, username: 'New_Selection' };
  h.accountSnapshot.value = { ...h.accountSnapshot.value, state: 'ready', selection_revision: 6 };
  read.resolve({ ...settings, revision: 8, account_selection_revision: 5, username: 'Old_Selection' });
  await importing;
  assert.equal(h.store.config.value.username, 'New_Selection');
  assert.equal(h.workflow.state.value.phase, 'metadata-imported');
  assert.match(h.workflow.state.value.error ?? '', /selection changed/);
  assert.equal(h.musicConfigs.length, 0);
  h.setConfigRead(async () => h.store.config.value);
  h.setAccountsRead(async () => undefined);
  await h.workflow.refreshImportedMetadata();
  assert.equal(h.workflow.state.value.error, null);
  assert.equal(h.musicConfigs[0][0].username, 'New_Selection');
  assert.equal(h.musicConfigs[0][1], true);
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
});

test('applying imported music synchronizes playback, volume and track without persisting or changing bootstrap autoplay', async () => {
  const players: FakeAudio[] = [];
  const frames = new Map<number, (time: number) => void>();
  const timers = new Map<number, () => void>();
  let sequence = 0;
  let writes = 0;
  class FakeAudio {
    paused = true;
    volume = 0;
    src = '';
    constructor() {
      players.push(this);
    }
    addEventListener() {}
    async play() {
      this.paused = false;
    }
    pause() {
      this.paused = true;
    }
  }
  const finishFade = () => {
    const callbacks = [...frames.values()];
    frames.clear();
    callbacks.forEach((callback) => callback(1000));
  };
  const { Music } = source<typeof import('../../src/music')>(
    'music.ts',
    {
      './api': { apiResourceUrl: (path: string) => path },
      './hooks/use-autosave': {
        saveConfigPatch: async () => {
          writes += 1;
        },
      },
      './store': { config: { value: settings } },
      './toast': { toast() {} },
    },
    {
      Audio: FakeAudio,
      performance: { now: () => 0 },
      requestAnimationFrame: (callback: (time: number) => void) => {
        frames.set(++sequence, callback);
        return sequence;
      },
      cancelAnimationFrame: (id: number) => frames.delete(id),
      setTimeout: (callback: () => void) => {
        timers.set(++sequence, callback);
        return sequence;
      },
      clearTimeout: (id: number) => timers.delete(id),
    },
  );
  Music.applyConfig({ music_enabled: true, music_volume: 20, music_track: 0 });
  assert.equal(players.length, 0);
  await Music.play();
  finishFade();
  assert.equal(players[0].paused, false);
  Music.setVolume(80);
  Music.applyConfig({ music_enabled: true, music_volume: 35, music_track: 1 }, true);
  await settle();
  finishFade();
  assert.equal(players[0].src, '/music/track?t=1');
  assert.equal(players[0].volume, 0.35);
  assert.equal(Music.volume, 35);
  assert.equal(Music.track, 1);
  Music.applyConfig({ music_enabled: false, music_volume: 15, music_track: 1 }, true);
  finishFade();
  assert.equal(players[0].paused, true);
  assert.equal(Music.enabled, false);
  assert.equal(Music.volume, 15);
  assert.equal(timers.size, 0);
  assert.equal(writes, 0);
});

test('an old music save queued behind a delayed settings response cannot overwrite a completed import', async () => {
  const h = harness();
  let backend = { ...settings, music_enabled: false, music_volume: 20 };
  const delayed = deferred<unknown>();
  const writes: Array<Record<string, unknown>> = [];
  let firstReply: Config | null = null;
  const actions = source<typeof import('../../src/actions')>('actions.ts', {
    './store': h.store,
    './launch-response-adapters': {},
  });
  const writer = source<typeof import('../../src/hooks/use-autosave')>('hooks/use-autosave.ts', {
    'preact/hooks': {},
    '../store': h.store,
    '../actions': actions,
    '../dto-core': coreDto,
    '../toast': {},
    '../utils': {},
    '../api': {
      api: async (method: string, path: string, patch: Record<string, unknown>) => {
        assert.equal(method, 'PUT');
        assert.equal(path, '/config');
        assert.equal(patch.expected_revision, backend.revision);
        writes.push(patch);
        const { expected_revision, ...values } = patch;
        backend = { ...backend, ...values, revision: backend.revision + 1 };
        if (writes.length === 1) {
          firstReply = { ...backend };
          return delayed.promise;
        }
        return backend;
      },
    },
  });
  const musicNotices: string[] = [];
  const { Music } = source<typeof import('../../src/music')>('music.ts', {
    './api': {},
    './hooks/use-autosave': writer,
    './store': h.store,
    './toast': { toast: (message: string) => musicNotices.push(message) },
  });
  h.store.config.value = { ...backend };
  Music.applyConfig(backend);
  h.setMusicApply((config, sync) => Music.applyConfig(config, sync));
  const first = writer.saveConfigPatch({ max_memory_mb: 8192 });
  await settle();
  assert.equal(backend.revision, 8);
  Music.enabled = true;
  Music.volume = 80;
  Music.persist();
  const ordinaryQueuedWrite = writer.saveConfigPatch({ window_width: 1600 });
  h.setConfigRead(async () => backend);
  h.setMetadataPost(async () => {
    backend = { ...backend, revision: 9, account_selection_revision: 5, music_enabled: false, music_volume: 35 };
    h.setSettings(backend);
    return { receipt: { ...receipt, settings_revision: 9 }, already_imported: false, cutover_available: false };
  });
  await h.workflow.chooseProfile();
  await h.workflow.prepareMetadataImport();
  await h.workflow.importMetadata();
  assert.equal(h.workflow.state.value.error, null);
  assert.equal(Music.enabled, false);
  assert.equal(Music.volume, 35);
  delayed.resolve(firstReply);
  await first;
  await ordinaryQueuedWrite;
  await settle();
  assert.equal(writes.length, 2);
  assert.equal(writes[1].window_width, 1600);
  assert.equal(writes[1].expected_revision, 9);
  assert.equal(
    writes.some((patch) => 'music_enabled' in patch),
    false,
  );
  assert.equal(backend.music_enabled, false);
  assert.equal(backend.music_volume, 35);
  assert.equal(h.store.config.value.revision, 10);
  assert.equal(Music.volume, 35);
  assert.deepEqual(musicNotices, []);
});

test('imported themes use the retained default fallback without replacing explicit local preferences or writing config', () => {
  const local = { theme: 'obsidian', customHue: 140, customVibrancy: 65, lightness: 20 };
  let writes = 0;
  let cssChanges = 0;
  const owner = source<typeof import('../../src/theme')>(
    'theme.ts',
    {
      './state': {
        local,
        defaults: { ...local },
        PRESET_HUES: { obsidian: 140, nether: 20 },
        canEditPreferences: () => true,
        saveLocalState: () => {
          writes += 1;
        },
      },
      './hooks/use-autosave': {
        saveConfigPatch: async () => {
          writes += 1;
        },
      },
      './store': { config: { value: settings } },
      './sound': {},
      './toast': {},
      './tokens': { buildTheme: (input: unknown) => input },
      './native': { windowSetResizeBackground: async () => undefined, hasNativeDesktopRuntime: () => false },
      './preferences/persistence': {},
    },
    {
      document: {
        documentElement: {
          style: {
            setProperty: () => {
              cssChanges += 1;
            },
          },
          setAttribute() {},
        },
      },
    },
  );
  owner.applyConfigTheme(settings);
  assert.equal(local.theme, 'obsidian');
  assert.equal(cssChanges, 0);
  owner.applyConfigTheme({ ...settings, theme: 'nether' });
  assert.equal(local.theme, 'nether');
  assert.ok(cssChanges > 0);
  const applied = cssChanges;
  owner.applyConfigTheme({ ...settings, theme: 'custom', custom_hue: 240 });
  assert.equal(local.theme, 'nether');
  assert.equal(cssChanges, applied);
  assert.equal(writes, 0);
});

test('metadata import is explicit, source-bound and revision-fenced even when the profile has no instances', async () => {
  const h = harness();
  h.setPreview(preview({ instances: [] }));
  await h.workflow.chooseProfile();
  assert.equal(button(h, 'Review accounts and settings').props.disabled, false);
  assert.equal(button(h, 'Import instance').props.disabled, true);
  await h.workflow.prepareMetadataImport();
  assert.equal(h.workflow.state.value.phase, 'confirming-metadata');
  const copy = h.view().map(textOf).join(' ');
  assert.match(copy, /replaces current launcher settings, including telemetry consent and feature overrides/);
  assert.match(copy, /Existing accounts remain/);
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 0);
  await h.workflow.importMetadata();
  const writes = h.calls.filter(([method]) => method === 'POST');
  assert.equal(writes.length, 1);
  assert.equal(writes[0][1], '/import/metadata');
  assert.equal(
    JSON.stringify(writes[0][2]),
    JSON.stringify({
      metadata_import_id: metadataId,
      fingerprint: 'a'.repeat(64),
      expected_settings_revision: 7,
      expected_account_selection_revision: 4,
    }),
  );
  assert.equal(h.workflow.state.value.phase, 'metadata-imported');
  assert.equal(h.workflow.state.value.error, null);
  assert.equal(h.store.config.value.username, 'Imported_Player');
  assert.equal(JSON.stringify(h.accountRefreshes), JSON.stringify([{ fresh: true }]));
  assert.equal(JSON.stringify(h.flagRefreshes), JSON.stringify([{ fresh: true }]));
  assert.equal(h.readinessRefreshes.length, 1);
  assert.equal(h.themeConfigs[0].username, 'Imported_Player');
  assert.equal(h.navigation.length, 0);
});

test('metadata refresh waits for acknowledged theme preferences and keeps refresh-only recovery after a save failure', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  await h.workflow.prepareMetadataImport();
  const held = deferred<void>();
  const preferenceVersion = h.localState.localStateVersion.value;
  let refuseSave = true;
  h.setThemeApply(async (_settings, version) => {
    assert.equal(version, preferenceVersion);
    await held.promise;
    if (refuseSave) throw new Error('Interface preferences could not be saved.');
  });
  const importing = h.workflow.importMetadata();
  await settle();
  assert.equal(h.workflow.state.value.phase, 'refreshing-metadata');
  assert.equal(h.readinessRefreshes.length, 0);
  held.resolve();
  await importing;
  assert.equal(h.workflow.state.value.phase, 'metadata-imported');
  assert.match(h.workflow.state.value.error ?? '', /Refresh current data.*preferences could not be saved/);
  assert.ok(h.workflow.state.value.metadataReceipt);
  assert.equal(h.readinessRefreshes.length, 0);
  refuseSave = false;
  await h.workflow.refreshImportedMetadata();
  assert.equal(h.workflow.state.value.error, null);
  assert.equal(h.readinessRefreshes.length, 1);
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
});

test('metadata eligibility is owned by the preview, not inferred from counts or broad blockers', async () => {
  const h = harness();
  h.setPreview(
    preview({ metadata_import_available: false, offline_account_count: 2, microsoft_reauthentication_count: 1 }),
  );
  await h.workflow.chooseProfile();
  assert.equal(button(h, 'Review accounts and settings').props.disabled, true);
  assert.match(h.view().map(textOf).join(' '), /source data is incomplete or unsupported/);
  await h.workflow.prepareMetadataImport();
  await h.workflow.importMetadata();
  assert.equal(h.calls.length, 0);
  h.setPreview(
    preview({
      metadata_import_available: true,
      blockers: ['cutover_not_implemented', 'saved_skins_require_conversion'],
    }),
  );
  await h.workflow.chooseProfile();
  assert.equal(button(h, 'Review accounts and settings').props.disabled, false);
});

test('Microsoft-only and mixed metadata wire receipts retain identity mappings but never sign in automatically', async () => {
  for (const mixed of [false, true]) {
    const h = harness();
    const mapping: Record<string, string> = { [microsoftSource]: microsoftId };
    if (mixed) Object.assign(mapping, { [offlineId]: offlineId, [secondOfflineId]: secondOfflineId });
    const result: MetadataImportReceipt = {
      ...receipt,
      imported_offline_account_count: mixed ? 2 : 0,
      imported_microsoft_account_count: 1,
      account_id_mapping: mapping,
    };
    h.setPreview(preview({ offline_account_count: mixed ? 2 : 0, microsoft_reauthentication_count: 1 }));
    await h.workflow.chooseProfile();
    assert.equal(button(h, 'Review accounts and settings').props.disabled, false);
    await h.workflow.prepareMetadataImport();
    const copy = h.view().map(textOf).join(' ');
    assert.match(copy, new RegExp(`${mixed ? 2 : 0} offline and 1 Microsoft identities`));
    assert.match(copy, /Microsoft identities require sign-in in Accounts/);
    assert.match(copy, /Credentials are never copied/);
    h.setMetadataPost(async () => {
      h.setSettings({ ...settings, revision: 8, account_selection_revision: 5, launch_auth_mode: 'online' });
      if (mixed) throw new Error('Response lost');
      return { receipt: result, already_imported: false, cutover_available: false };
    });
    h.setMetadataRead(async () => ({ receipt: result, cutover_available: false }));
    await h.workflow.importMetadata();
    assert.equal(h.workflow.state.value.phase, 'metadata-imported');
    assert.equal(h.workflow.state.value.error, null);
    assert.equal(JSON.stringify(h.workflow.state.value.metadataReceipt), JSON.stringify(result));
    assert.equal(h.store.config.value.launch_auth_mode, 'online');
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
    assert.equal(
      h.calls.some(([, path]) => path.startsWith('/auth')),
      false,
    );
    assert.equal(
      h.nativeCalls.some(([command]) => command !== 'pick_import_profile'),
      false,
    );
  }
});

test('a legacy offline completion receipt retains its explicitly unavailable identity mapping', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  await h.workflow.prepareMetadataImport();
  h.setMetadataPost(async () => {
    throw new Error('Response lost');
  });
  h.setMetadataRead(async () => ({ receipt: { ...receipt, account_id_mapping: null }, cutover_available: false }));
  h.setSettings({ ...settings, revision: 8, account_selection_revision: 5 });
  await h.workflow.importMetadata();
  assert.equal(h.workflow.state.value.phase, 'metadata-imported');
  assert.equal(h.workflow.state.value.metadataReceipt?.account_id_mapping, null);
  assert.equal(h.workflow.state.value.metadataReceipt?.imported_microsoft_account_count, 0);
});

test('metadata confirmation cancellation and late preflight completion cannot submit', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  await h.workflow.prepareMetadataImport();
  await (button(h, 'Cancel').props.onClick as () => void)();
  await h.workflow.importMetadata();
  assert.equal(h.workflow.state.value.phase, 'preview');
  for (const cancel of ['close', 'dispose'] as const) {
    const attempt = harness();
    await attempt.workflow.chooseProfile();
    const read = deferred<unknown>();
    attempt.setConfigRead(() => read.promise);
    const preparing = attempt.workflow.prepareMetadataImport();
    attempt.workflow[cancel]();
    read.resolve(settings);
    await preparing;
    await attempt.workflow.importMetadata();
    assert.equal(attempt.calls.filter(([method]) => method === 'POST').length, 0);
  }
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 0);
});

test('source-bound metadata identity changes require review even with the same content fingerprint', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  h.setPreview(preview({ metadata_import_id: 'd'.repeat(64) }));
  await h.workflow.prepareMetadataImport();
  assert.equal(h.workflow.state.value.phase, 'preview');
  assert.match(h.workflow.state.value.error ?? '', /source changed/);
  await h.workflow.importMetadata();
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 0);
});

test('metadata acceptance ignores duplicate submission and closing cannot cancel accepted work', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  await h.workflow.prepareMetadataImport();
  const response = deferred<unknown>();
  h.setMetadataPost(() => response.promise);
  const importing = h.workflow.importMetadata();
  assert.equal(button(h, 'Cancel').props.disabled, true);
  h.workflow.close();
  await h.workflow.importMetadata();
  assert.equal(h.workflow.state.value.phase, 'importing-metadata');
  h.setSettings({ ...settings, revision: 8, account_selection_revision: 5 });
  response.resolve({ receipt, already_imported: false, cutover_available: false });
  await importing;
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  assert.equal(h.workflow.state.value.phase, 'metadata-imported');
});

test('unknown metadata outcomes use only source-independent receipt reads and null never means safe to replay', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  await h.workflow.prepareMetadataImport();
  h.setMetadataPost(async () => {
    throw new Error('Response lost');
  });
  await h.workflow.importMetadata();
  assert.equal(h.workflow.state.value.phase, 'metadata-uncertain');
  assert.match(h.workflow.state.value.error ?? '', /may still finish/);
  assert.equal(h.calls[h.calls.length - 1][1], `/import/metadata/${metadataId}`);
  assert.equal(
    h.view().some((node) => node.type === 'Button' && node.props.children === 'Import accounts and settings'),
    false,
  );
  await h.workflow.importMetadata();
  h.setRead(async () => {
    throw new Error('Native preview forgotten');
  });
  h.setMetadataRead(async () => ({ receipt, cutover_available: false }));
  h.setSettings({ ...settings, revision: 11, account_selection_revision: 7, username: 'Later_Edit' });
  await h.workflow.checkMetadataImport();
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  assert.equal(h.calls.filter(([, path]) => path === '/import/preview').length, 1);
  assert.equal(h.workflow.state.value.phase, 'metadata-imported');
  assert.equal(h.workflow.state.value.metadataReceipt?.settings_revision, 8);
  assert.equal(h.store.config.value.revision, 11);
  assert.equal(h.store.config.value.username, 'Later_Edit');
});

test('metadata history completion remains optional, bounded and separate from instance eligibility', async () => {
  for (const historyCount of [undefined, 0, 128]) {
    const h = harness();
    h.setPreview(preview({ instances: [], blockers: ['cutover_not_implemented', 'unsettled_operation'] }));
    await h.workflow.chooseProfile();
    await h.workflow.prepareMetadataImport();
    const result = {
      ...receipt,
      ...(historyCount === undefined ? {} : { global_install_history_count: historyCount }),
    };
    h.setMetadataPost(async () => ({ receipt: result, already_imported: false, cutover_available: false }));
    await h.workflow.importMetadata();
    assert.equal(h.workflow.state.value.phase, 'metadata-imported');
    assert.equal(JSON.stringify(h.workflow.state.value.metadataReceipt), JSON.stringify(result));
    assert.equal(h.workflow.state.value.preview?.cutover_available, false);
    assert.equal(h.navigation.length, 0);
  }
});

test('archived launch report counts survive metadata receipts and status reads with optional compatibility', async () => {
  for (const reportCount of [undefined, null, 0, 1024]) {
    for (const lost of [false, true]) {
      const h = harness();
      h.setPreview(preview({ instances: [], blockers: ['cutover_not_implemented', 'unsettled_operation'] }));
      await h.workflow.chooseProfile();
      await h.workflow.prepareMetadataImport();
      const result = {
        ...receipt,
        global_install_history_count: 128,
        ...(reportCount === undefined ? {} : { archived_launch_report_count: reportCount }),
      };
      h.setSettings({ ...settings, revision: 8, account_selection_revision: 5 });
      h.setMetadataPost(async () => {
        if (lost) throw new Error('Response lost');
        return JSON.parse(JSON.stringify({ receipt: result, already_imported: false, cutover_available: false }));
      });
      h.setMetadataRead(async () => JSON.parse(JSON.stringify({ receipt: result, cutover_available: false })));
      await h.workflow.importMetadata();
      const expected = {
        ...receipt,
        global_install_history_count: 128,
        ...(reportCount == null ? {} : { archived_launch_report_count: reportCount }),
      };
      assert.equal(h.workflow.state.value.phase, 'metadata-imported');
      assert.equal(h.workflow.state.value.error, null);
      assert.equal(JSON.stringify(h.workflow.state.value.metadataReceipt), JSON.stringify(expected));
      assert.equal(
        Object.prototype.hasOwnProperty.call(h.workflow.state.value.metadataReceipt, 'archived_launch_report_count'),
        reportCount != null,
      );
      assert.equal(h.workflow.state.value.preview?.cutover_available, false);
      assert.equal(h.navigation.length, 0);
      assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
      assert.equal(h.calls.filter(([, path]) => path === `/import/metadata/${metadataId}`).length, lost ? 1 : 0);
    }
  }
});

test('invalid archived launch report counts cannot confirm metadata imports or refresh destination state', async () => {
  for (const reportCount of [-1, 0.5, 1025, Number.MAX_SAFE_INTEGER + 1, '2', true, {}, []]) {
    const h = harness();
    await h.workflow.chooseProfile();
    await h.workflow.prepareMetadataImport();
    const result = { ...receipt, archived_launch_report_count: reportCount };
    h.setMetadataPost(async () =>
      JSON.parse(JSON.stringify({ receipt: result, already_imported: false, cutover_available: false })),
    );
    h.setMetadataRead(async () => JSON.parse(JSON.stringify({ receipt: result, cutover_available: false })));
    await h.workflow.importMetadata();
    assert.equal(h.workflow.state.value.phase, 'metadata-uncertain', JSON.stringify(reportCount));
    assert.equal(h.workflow.state.value.metadataReceipt, null);
    assert.equal(h.accountRefreshes.length, 0);
    assert.equal(h.flagRefreshes.length, 0);
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  }
});

test('invalid or mismatched metadata receipts cannot claim completion or refresh destination state', async () => {
  for (const invalid of [
    { ...receipt, metadata_import_id: 'd'.repeat(64) },
    { ...receipt, imported_offline_account_count: -1 },
    { ...receipt, imported_offline_account_count: 0 },
    { ...receipt, imported_microsoft_account_count: -1 },
    { ...receipt, imported_microsoft_account_count: 0.5 },
    { ...receipt, imported_offline_account_count: 256, imported_microsoft_account_count: 1 },
    { ...receipt, imported_microsoft_account_count: undefined },
    { ...receipt, imported_microsoft_account_count: 1, account_id_mapping: null },
    { ...receipt, account_id_mapping: undefined },
    { ...receipt, account_id_mapping: [] },
    { ...receipt, account_id_mapping: {} },
    { ...receipt, account_id_mapping: { ['a'.repeat(129)]: offlineId } },
    { ...receipt, account_id_mapping: { [offlineId]: 'a'.repeat(129) } },
    { ...receipt, account_id_mapping: { [offlineId]: {} } },
    {
      ...receipt,
      imported_offline_account_count: 2,
      account_id_mapping: { [offlineId]: offlineId, [secondOfflineId]: offlineId },
    },
    { ...receipt, settings_revision: 0 },
    { ...receipt, account_selection_revision: 0.5 },
    ...[-1, 0.5, 129, '2', true].map((global_install_history_count) => ({ ...receipt, global_install_history_count })),
  ]) {
    const h = harness();
    await h.workflow.chooseProfile();
    await h.workflow.prepareMetadataImport();
    h.setMetadataPost(async () => ({ receipt: invalid, already_imported: false, cutover_available: false }));
    h.setMetadataRead(async () => ({ receipt: invalid, cutover_available: false }));
    await h.workflow.importMetadata();
    assert.equal(h.workflow.state.value.phase, 'metadata-uncertain');
    assert.equal(h.workflow.state.value.metadataReceipt, null);
    assert.equal(h.accountRefreshes.length, 0);
  }
});

test('confirmed metadata refresh failures preserve the receipt and retry reads without another POST', async () => {
  for (const owner of ['config', 'accounts', 'flags'] as const) {
    const h = harness();
    await h.workflow.chooseProfile();
    await h.workflow.prepareMetadataImport();
    if (owner === 'config')
      h.setConfigRead(async () => {
        throw new Error('Config unavailable');
      });
    if (owner === 'accounts')
      h.setAccountsRead(async () => {
        h.accountSnapshot.value.state = 'unavailable';
      });
    if (owner === 'flags')
      h.setFlagsRead(async () => {
        throw new Error('Flags unavailable');
      });
    await h.workflow.importMetadata();
    assert.equal(h.workflow.state.value.phase, 'metadata-imported');
    assert.equal(h.workflow.state.value.metadataReceipt?.metadata_import_id, metadataId);
    assert.match(h.workflow.state.value.error ?? '', /import is complete/);
    assert.ok(button(h, 'Refresh imported settings'));
    h.setConfigRead(null);
    h.setAccountsRead(null);
    h.setFlagsRead(null);
    await h.workflow.refreshImportedMetadata();
    assert.equal(h.workflow.state.value.error, null);
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  }
});

test('late metadata receipts refresh shared owners without overwriting a newer dialog preview', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  await h.workflow.prepareMetadataImport();
  h.setMetadataPost(async () => {
    throw new Error('Response lost');
  });
  await h.workflow.importMetadata();
  const read = deferred<unknown>();
  h.setMetadataRead(() => read.promise);
  const checking = h.workflow.checkMetadataImport();
  h.workflow.close();
  h.setPreview(preview({ fingerprint: 'b'.repeat(64), metadata_import_id: 'd'.repeat(64) }));
  await h.workflow.chooseProfile();
  h.setSettings({ ...settings, revision: 8, account_selection_revision: 5 });
  read.resolve({ receipt, cutover_available: false });
  await checking;
  assert.equal(h.workflow.state.value.preview?.metadata_import_id, 'd'.repeat(64));
  assert.equal(h.workflow.state.value.phase, 'preview');
  assert.equal(h.workflow.state.value.metadataReceipt, null);
  assert.equal(h.accountRefreshes.length, 1);
});

test('saved skins import independently of accounts and retain a historical receipt without applying a skin', async () => {
  for (const keys of [skinReceipt.texture_keys, []]) {
    const h = harness();
    h.setPreview(preview({ instances: [], metadata_import_available: false, saved_skin_count: keys.length }));
    await h.workflow.chooseProfile();
    assert.equal(button(h, 'Import saved skins').props.disabled, false);
    assert.match(JSON.stringify(h.view()), /saved metadata/);
    h.setSkinPost(async () => ({
      receipt: { ...skinReceipt, texture_keys: keys },
      already_imported: true,
      cutover_available: false,
    }));
    await (button(h, 'Import saved skins').props.onClick as () => Promise<void>)();
    await settle();
    assert.equal(
      JSON.stringify(h.calls),
      JSON.stringify([
        ['GET', '/import/preview', undefined],
        ['POST', '/import/skins', { skin_import_id: skinImportId, fingerprint: 'a'.repeat(64) }],
      ]),
    );
    assert.equal(h.workflow.state.value.phase, 'skins-imported');
    assert.equal(h.workflow.state.value.skinReceipt?.texture_keys.length, keys.length);
    assert.equal(h.wardrobeRefreshes.length, 1);
    assert.equal(h.wardrobe.value.skins.length, 0); // A receipt must not resurrect later deletions.
    assert.equal(h.accountRefreshes.length, 0);
    assert.equal(h.nativeCalls.filter(([command]) => command.includes('sign')).length, 0);
    assert.match(JSON.stringify(h.view()), /No skin was applied to an account/);
  }
});

test('skin availability and exact source identity are rechecked before a mutation', async () => {
  const unavailable = harness();
  unavailable.setPreview(preview({ skin_import_available: false, saved_skin_count: 4 }));
  await unavailable.workflow.chooseProfile();
  assert.equal(button(unavailable, 'Import saved skins').props.disabled, true);
  await unavailable.workflow.importSkins();
  assert.equal(unavailable.calls.length, 0);
  for (const change of [
    { fingerprint: 'b'.repeat(64) },
    { skin_import_id: 'f'.repeat(64) },
    { skin_import_available: false },
  ]) {
    const h = harness();
    await h.workflow.chooseProfile();
    h.setPreview(preview(change));
    await h.workflow.importSkins();
    assert.equal(h.workflow.state.value.phase, 'preview');
    assert.ok(h.workflow.state.value.error);
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 0);
  }
});

test('closing or unmounting skin preflight prevents submission; accepted work cannot be canceled or duplicated', async () => {
  for (const cancel of ['close', 'dispose'] as const) {
    const h = harness();
    await h.workflow.chooseProfile();
    const read = deferred<unknown>();
    h.setRead(() => read.promise);
    const importing = h.workflow.importSkins();
    h.workflow[cancel]();
    read.resolve(preview());
    await importing;
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 0);
  }
  const h = harness();
  await h.workflow.chooseProfile();
  const post = deferred<unknown>();
  h.setSkinPost(() => post.promise);
  const importing = h.workflow.importSkins();
  await settle();
  h.workflow.close();
  await h.workflow.importSkins();
  assert.equal(h.workflow.state.value.phase, 'importing-skins');
  assert.equal(button(h, 'Close').props.disabled, true);
  h.workflow.dispose();
  post.resolve({ receipt: skinReceipt, already_imported: false, cutover_available: false });
  await importing;
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  assert.equal(h.wardrobeRefreshes.length, 1);
  assert.equal(h.workflow.state.value.skinReceipt, null);
});

test('unknown saved-skin outcomes reconcile only by source-independent GET, never by replaying the command', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  h.setSkinPost(async () => {
    throw new Error('Response lost');
  });
  await h.workflow.importSkins();
  assert.equal(h.workflow.state.value.phase, 'skins-uncertain');
  assert.match(h.workflow.state.value.error ?? '', /No completion receipt yet.*may still finish/);
  assert.ok(button(h, 'Check import status'));
  assert.equal(h.wardrobeRefreshes.length, 0);
  h.setRead(async () => {
    throw new Error('Preview forgotten');
  });
  h.setSkinRead(async () => ({ receipt: skinReceipt, cutover_available: false }));
  await h.workflow.importSkins();
  await h.workflow.checkSkinImport();
  assert.equal(h.workflow.state.value.phase, 'skins-imported');
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  assert.equal(h.calls.filter(([, path]) => path === '/import/preview').length, 1);
  assert.equal(h.calls.filter(([, path]) => path === `/import/skins/${skinImportId}`).length, 2);
});

test('invalid saved-skin receipts cannot claim completion or refresh the wardrobe', async () => {
  for (const invalid of [
    null,
    { ...skinReceipt, skin_import_id: 'f'.repeat(64) },
    { ...skinReceipt, fingerprint: 'b'.repeat(64) },
    { ...skinReceipt, texture_keys: {} },
    { ...skinReceipt, texture_keys: ['../skin'] },
    { ...skinReceipt, texture_keys: ['E'.repeat(64)] },
    { ...skinReceipt, texture_keys: [skinReceipt.texture_keys[0], skinReceipt.texture_keys[0]] },
    { ...skinReceipt, texture_keys: Array(32_769).fill('e'.repeat(64)) },
  ]) {
    const h = harness();
    await h.workflow.chooseProfile();
    h.setSkinPost(async () => ({ receipt: invalid, already_imported: false, cutover_available: false }));
    h.setSkinRead(async () => ({ receipt: invalid, cutover_available: false }));
    await h.workflow.importSkins();
    assert.equal(h.workflow.state.value.phase, 'skins-uncertain');
    assert.equal(h.workflow.state.value.skinReceipt, null);
    assert.equal(h.wardrobeRefreshes.length, 0);
  }
});

test('confirmed skin import retries a failed or superseded wardrobe read without another POST', async () => {
  for (const result of ['failed', 'superseded'] as const) {
    const h = harness();
    await h.workflow.chooseProfile();
    h.setWardrobeRead(async () => {
      if (result === 'failed') h.wardrobe.value = { ...h.wardrobe.value, error: 'Read unavailable' };
    });
    await h.workflow.importSkins();
    assert.equal(h.workflow.state.value.phase, 'skins-imported');
    assert.equal(h.workflow.state.value.skinReceipt?.skin_import_id, skinImportId);
    assert.match(h.workflow.state.value.error ?? '', /import is complete/);
    assert.ok(button(h, 'Refresh skin library'));
    h.setWardrobeRead(null);
    await h.workflow.refreshImportedSkins();
    assert.equal(h.workflow.state.value.error, null);
    assert.equal(h.wardrobeRefreshes.length, 2);
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  }
});

test('a late skin status refreshes the shared wardrobe without replacing a newer import preview', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  h.setSkinPost(async () => {
    throw new Error('Response lost');
  });
  await h.workflow.importSkins();
  const read = deferred<unknown>();
  h.setSkinRead(() => read.promise);
  const checking = h.workflow.checkSkinImport();
  h.workflow.close();
  h.setPreview(preview({ skin_import_id: 'f'.repeat(64) }));
  await h.workflow.chooseProfile();
  read.resolve({ receipt: skinReceipt, cutover_available: false });
  await checking;
  assert.equal(h.workflow.state.value.phase, 'preview');
  assert.equal(h.workflow.state.value.preview?.skin_import_id, 'f'.repeat(64));
  assert.equal(h.workflow.state.value.skinReceipt, null);
  assert.equal(h.wardrobeRefreshes.length, 1);
});

function rulesPreview(available = false): ImportPreview {
  return preview({
    rules_import_available: true,
    instances: [
      {
        ...preview().instances[0],
        loader_key: 'fabric',
        ordinary_import_available: available,
        blockers: available ? [] : ['managed_state_requires_conversion'],
      },
    ],
  });
}

function rulesResponse(receipt: unknown = rulesReceipt, matches = true) {
  return { receipt, already_imported: false, stored_cache_matches_import: matches, cutover_available: false };
}

test('rules import refreshes the existing preview before enabling eligible Managed rows and keeps its historical receipt', async () => {
  const h = harness();
  h.setPreview(rulesPreview());
  await h.workflow.chooseProfile();
  assert.equal(button(h, 'Import performance rules').props.disabled, false);
  assert.equal(button(h, 'Import instance').props.disabled, true);
  h.setRulesPost(async () => {
    h.setPreview(rulesPreview(true));
    return rulesResponse();
  });
  (button(h, 'Import performance rules').props.onClick as () => void)();
  await settle();
  assert.deepEqual(
    h.calls.map(([method, path]) => [method, path]),
    [
      ['GET', '/import/preview'],
      ['POST', '/import/rules'],
      ['GET', '/import/preview'],
    ],
  );
  assert.equal(
    JSON.stringify(h.calls[1][2]),
    JSON.stringify({ fingerprint: 'a'.repeat(64), rules_import_id: rulesImportId }),
  );
  assert.equal(h.workflow.state.value.phase, 'preview');
  assert.equal(button(h, 'Import instance').props.disabled, false);
  assert.equal(button(h, 'Import performance rules').props.disabled, true);
  assert.equal(JSON.stringify(h.workflow.state.value.rulesReceipt), JSON.stringify(rulesReceipt));
  const completed = h.workflow.state.value.rulesReceipt;
  assert.match(h.view().map(textOf).join(' '), /Current rules trust and launch readiness are checked separately/);
  assert.match(h.view().map(textOf).join(' '), /Full profile migration remains unavailable/);
  h.setPreview(rulesPreview());
  await h.workflow.refreshPreview();
  assert.equal(h.workflow.state.value.rulesReceipt, completed);
  assert.equal(button(h, 'Import instance').props.disabled, true);
  await h.workflow.importRules();
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  assert.equal(h.readinessRefreshes.length, 0);
  assert.equal(h.navigation.length, 0);
});

test('history-only receipts and later cache differences do not grant current instance readiness', async () => {
  for (const cache of [null, rulesReceipt.cache_sha256]) {
    const h = harness();
    h.setPreview(rulesPreview());
    h.setRulesPost(async () => rulesResponse({ ...rulesReceipt, cache_sha256: cache }, false));
    await h.workflow.chooseProfile();
    await h.workflow.importRules();
    assert.equal(h.workflow.state.value.phase, 'preview');
    assert.equal(h.workflow.state.value.rulesReceipt?.cache_sha256, cache);
    assert.equal(button(h, 'Import instance').props.disabled, true);
    assert.equal(h.workflow.state.value.preview?.cutover_available, false);
    if (cache === null) assert.match(h.view().map(textOf).join(' '), /without a cached ruleset/);
  }
});

test('rules refresh history retains canonical u64 sequences beyond JavaScript integer precision', async () => {
  const receipt: RulesImportReceipt = {
    ...rulesReceipt,
    refresh_history: [
      { ...rulesReceipt.refresh_history[0], sequence: '9007199254740993' },
      { ...rulesReceipt.refresh_history[1], sequence: '18446744073709551614' },
      {
        ...rulesReceipt.refresh_history[0],
        operation_id: 'op-94d4ec3a-4a90-4d70-9e97-a5774f1c0a8b',
        sequence: '18446744073709551615',
      },
    ],
  };
  for (const lost of [false, true]) {
    const h = harness();
    h.setPreview(rulesPreview());
    h.setRulesPost(async () => {
      if (lost) throw new Error('Response lost');
      return JSON.parse(JSON.stringify(rulesResponse(receipt)));
    });
    h.setRulesRead(async () =>
      JSON.parse(
        JSON.stringify({
          receipt,
          stored_cache_matches_import: false,
          cutover_available: false,
        }),
      ),
    );
    await h.workflow.chooseProfile();
    await h.workflow.importRules();
    assert.equal(h.workflow.state.value.phase, 'preview');
    assert.equal(h.workflow.state.value.error, null);
    assert.equal(JSON.stringify(h.workflow.state.value.rulesReceipt), JSON.stringify(receipt));
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  }
});

test('rules availability and exact source identity are checked before submission', async () => {
  const unavailable = harness();
  await unavailable.workflow.chooseProfile();
  assert.equal(button(unavailable, 'Import performance rules').props.disabled, true);
  await unavailable.workflow.importRules();
  assert.equal(unavailable.calls.length, 0);
  for (const change of [
    { fingerprint: 'b'.repeat(64) },
    { rules_import_id: 'f'.repeat(64) },
    { rules_import_available: false },
  ]) {
    const h = harness();
    h.setPreview(rulesPreview());
    await h.workflow.chooseProfile();
    h.setPreview({ ...rulesPreview(), ...change });
    await h.workflow.importRules();
    assert.equal(h.workflow.state.value.phase, 'preview');
    assert.ok(h.workflow.state.value.error);
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 0);
  }
});

test('definitive trust and conflict refusals preserve their safe causes without replay or status waiting', async () => {
  for (const [status, message] of [
    [422, 'The predecessor rules are not trusted by this destination.'],
    [422, 'The destination rules signing key and remote policy must be configured.'],
    [409, 'The rules import conflicts with existing destination data.'],
  ] as const) {
    const h = harness();
    h.setPreview(rulesPreview());
    h.setRulesPost(async () => {
      throw Object.assign(new Error(message), { name: 'ApiError', status });
    });
    await h.workflow.chooseProfile();
    await h.workflow.importRules();
    assert.equal(h.workflow.state.value.phase, 'preview');
    assert.equal(h.workflow.state.value.rulesReceipt, null);
    assert.equal(h.workflow.state.value.error, message);
    assert.match(h.view().map(textOf).join(' '), new RegExp(message));
    assert.equal(button(h, 'Import instance').props.disabled, true);
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
    assert.equal(h.calls.filter(([, path]) => path.startsWith('/import/rules/')).length, 0);
  }
});

test('unknown rules outcomes use source-independent status reads and never repeat a write', async () => {
  for (const failure of [
    new Error('Response lost'),
    Object.assign(new Error('Storage unavailable'), { name: 'ApiError', status: 500 }),
  ]) {
    const h = harness();
    h.setPreview(rulesPreview());
    h.setRulesPost(async () => {
      throw failure;
    });
    await h.workflow.chooseProfile();
    await h.workflow.importRules();
    assert.equal(h.workflow.state.value.phase, 'rules-uncertain');
    assert.match(h.workflow.state.value.error ?? '', /No completion receipt yet.*may still finish/);
    assert.ok(button(h, 'Check import status'));
    await h.workflow.importRules();
    h.setRead(async () => {
      throw new Error('Source is disconnected');
    });
    h.setRulesRead(async () => rulesResponse(rulesReceipt, false));
    (button(h, 'Check import status').props.onClick as () => void)();
    await settle();
    assert.equal(h.workflow.state.value.phase, 'rules-imported');
    assert.equal(h.workflow.state.value.rulesReceipt?.rules_import_id, rulesImportId);
    assert.match(h.workflow.state.value.error ?? '', /import is complete.*Source is disconnected/);
    assert.equal(h.calls.filter(([, path]) => path === `/import/rules/${rulesImportId}`).length, 2);
    h.setRead(null);
    h.setPreview(rulesPreview(true));
    (button(h, 'Refresh preview').props.onClick as () => void)();
    await settle();
    assert.equal(h.workflow.state.value.phase, 'preview');
    assert.equal(button(h, 'Import instance').props.disabled, false);
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  }
});

test('confirmed rules import retries only preview reads after a failed or mismatched refresh', async () => {
  for (const failure of ['unavailable', 'different-source']) {
    const h = harness();
    h.setPreview(rulesPreview());
    h.setRulesPost(async () => {
      h.setRead(async () => {
        if (failure === 'unavailable') throw new Error('Preview unavailable');
        return { ...rulesPreview(true), rules_import_id: 'f'.repeat(64) };
      });
      return rulesResponse();
    });
    await h.workflow.chooseProfile();
    await h.workflow.importRules();
    assert.equal(h.workflow.state.value.phase, 'rules-imported');
    assert.ok(h.workflow.state.value.rulesReceipt);
    assert.match(h.workflow.state.value.error ?? '', /import is complete/);
    await h.workflow.importRules();
    await h.workflow.refreshPreview();
    h.setRead(null);
    h.setPreview(rulesPreview(true));
    await h.workflow.refreshImportedRules();
    assert.equal(h.workflow.state.value.error, null);
    assert.equal(h.workflow.state.value.phase, 'preview');
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  }
});

test('strict rules receipts reject malformed identities, omitted fields, nonterminal history and invalid bounds', async () => {
  const refresh = rulesReceipt.refresh_history[0];
  const invalidReceipts: unknown[] = [
    null,
    { ...rulesReceipt, rules_import_id: 'f'.repeat(64) },
    { ...rulesReceipt, fingerprint: 'b'.repeat(64) },
    { ...rulesReceipt, cache_sha256: undefined },
    { ...rulesReceipt, cache_sha256: 'F'.repeat(64) },
    { ...rulesReceipt, cache_sha256: null, refresh_history: [] },
    { ...rulesReceipt, refresh_history: undefined },
    { ...rulesReceipt, refresh_history: Array(129).fill(refresh) },
    ...[
      { ...refresh, operation_id: 'op-94d4ec3a-4a90-1d70-9e97-a5774f1c0a8a' },
      { ...refresh, operation_id: 'op-94d4ec3a-4a90-4d70-7e97-a5774f1c0a8a' },
      { ...refresh, operation_id: refresh.operation_id.toUpperCase() },
      { ...refresh, sequence: 0 },
      { ...refresh, sequence: 1.5 },
      { ...refresh, sequence: Number.MAX_SAFE_INTEGER + 1 },
      ...['0', '-1', '+1', '01', '1.0', '1e3', ' 1', '1 ', '18446744073709551616', '9'.repeat(21)].map((sequence) => ({
        ...refresh,
        sequence,
      })),
      { ...refresh, sequence: undefined },
      { ...refresh, outcome: { state: 'running' } },
      { ...refresh, outcome: { state: 'succeeded' } },
      { ...refresh, outcome: { state: 'succeeded', cache_changed: 'true' } },
      { ...refresh, outcome: { state: 'failed', failure_point: 'unknown' } },
      { ...refresh, outcome: { state: 'failed' } },
    ].map((refresh) => ({ ...rulesReceipt, refresh_history: [refresh] })),
    { ...rulesReceipt, refresh_history: [refresh, { ...refresh, sequence: '8' }] },
    { ...rulesReceipt, refresh_history: [...rulesReceipt.refresh_history].reverse() },
  ];
  const invalidResponses = [
    ...invalidReceipts.map((receipt) => rulesResponse(receipt)),
    { ...rulesResponse(), receipt: undefined },
    { ...rulesResponse(), stored_cache_matches_import: undefined },
    { ...rulesResponse(), stored_cache_matches_import: 'true' },
    { ...rulesResponse(), cutover_available: undefined },
    { ...rulesResponse(), cutover_available: true },
    rulesResponse({ ...rulesReceipt, cache_sha256: null }, true),
    rulesResponse(null, true),
  ];
  for (const response of invalidResponses) {
    const h = harness();
    h.setPreview(rulesPreview());
    const wire = JSON.parse(JSON.stringify(response));
    h.setRulesPost(async () => wire);
    h.setRulesRead(async () => wire);
    await h.workflow.chooseProfile();
    await h.workflow.importRules();
    assert.equal(h.workflow.state.value.phase, 'rules-uncertain', JSON.stringify(response));
    assert.equal(h.workflow.state.value.rulesReceipt, null);
    assert.equal(h.calls.filter(([, path]) => path === '/import/preview').length, 1);
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  }
  const h = harness();
  h.setPreview(rulesPreview());
  h.setRulesPost(async () => ({ ...rulesResponse(), already_imported: undefined }));
  await h.workflow.chooseProfile();
  await h.workflow.importRules();
  assert.equal(h.workflow.state.value.phase, 'rules-uncertain');
  assert.equal(h.workflow.state.value.rulesReceipt, null);
});

test('rules preflight closes safely while accepted work cannot be canceled or duplicated', async () => {
  for (const cancel of ['close', 'dispose'] as const) {
    const h = harness();
    h.setPreview(rulesPreview());
    await h.workflow.chooseProfile();
    const read = deferred<unknown>();
    h.setRead(() => read.promise);
    const importing = h.workflow.importRules();
    h.workflow[cancel]();
    read.resolve(rulesPreview());
    await importing;
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 0);
  }
  const h = harness();
  h.setPreview(rulesPreview());
  await h.workflow.chooseProfile();
  const post = deferred<unknown>();
  h.setRulesPost(() => post.promise);
  const importing = h.workflow.importRules();
  await settle();
  h.workflow.close();
  await h.workflow.importRules();
  assert.equal(h.workflow.state.value.phase, 'importing-rules');
  assert.equal(button(h, 'Close').props.disabled, true);
  h.workflow.dispose();
  post.resolve(rulesResponse());
  await importing;
  assert.equal(h.workflow.state.value.rulesReceipt, null);
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  assert.equal(h.calls.filter(([, path]) => path === '/import/preview').length, 1);
});

test('late rules status and preview reads cannot overwrite a closed, disposed or newer dialog', async () => {
  for (const pending of ['status', 'preview'])
    for (const end of ['close', 'dispose'] as const) {
      const h = harness();
      h.setPreview(rulesPreview());
      await h.workflow.chooseProfile();
      const read = deferred<unknown>();
      let checking: Promise<void>;
      if (pending === 'status') {
        h.setRulesPost(async () => {
          throw new Error('Response lost');
        });
        await h.workflow.importRules();
        h.setRulesRead(() => read.promise);
        checking = h.workflow.checkRulesImport();
      } else {
        h.setRulesPost(async () => {
          h.setRead(() => read.promise);
          return rulesResponse();
        });
        checking = h.workflow.importRules();
        await settle();
      }
      h.workflow[end]();
      if (end === 'close') {
        h.setPreview(preview({ rules_import_id: 'f'.repeat(64) }));
        await h.workflow.chooseProfile();
      }
      const before = h.workflow.state.value;
      const reads = h.calls.length;
      read.resolve(pending === 'status' ? rulesResponse() : rulesPreview(true));
      await checking;
      assert.equal(h.workflow.state.value, before);
      assert.equal(h.calls.length, reads);
    }
});

test('native profile admission previews first, then copies only the selected fingerprint and identity', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  assert.equal(h.nativeCalls[0][0], 'pick_import_profile');
  assert.equal(h.calls.length, 0);
  assert.equal(button(h, 'Import instance').props.disabled, false);
  await h.workflow.importSelected();
  assert.deepEqual(
    h.calls.map(([method, path]) => [method, path]),
    [
      ['GET', '/import/preview'],
      ['POST', '/import/instances'],
      ['GET', '/instances'],
    ],
  );
  assert.equal(JSON.stringify(h.calls[1][2]), JSON.stringify({ fingerprint: 'a'.repeat(64), legacy_id: legacyId }));
  assert.equal(h.store.instances.value[0].id, instanceId);
  assert.equal(JSON.stringify(h.navigation), JSON.stringify([{ name: 'instance', id: instanceId }]));
  assert.equal(h.workflow.state.value.phase, 'closed');
});

test('owner actionability disables a blocked row and an exact source picker can update it', async () => {
  const h = harness();
  h.setPreview(
    preview({
      instances: [
        { ...preview().instances[0], ordinary_import_available: false, blockers: ['missing_instance_source'] },
      ],
    }),
  );
  await h.workflow.chooseProfile();
  assert.equal(button(h, 'Import instance').props.disabled, true);
  await h.workflow.importSelected();
  assert.equal(h.calls.length, 0);
  assert.ok(button(h, 'Choose instance folder'));
  h.setPicker(async () => preview({ fingerprint: 'b'.repeat(64) }));
  await h.workflow.chooseInstanceFolder();
  assert.equal(h.nativeCalls[1][0], 'pick_import_instance_source');
  assert.equal(JSON.stringify(h.nativeCalls[1][1]), JSON.stringify({ fingerprint: 'a'.repeat(64), legacyId }));
  assert.equal(button(h, 'Import instance').props.disabled, false);
});

test('native picker cancellation and late picker results never admit an import action after closing', async () => {
  const h = harness();
  h.setPicker(async () => null);
  await h.workflow.chooseProfile();
  assert.equal(h.workflow.state.value.phase, 'closed');
  const late = deferred<unknown>();
  h.setPicker(() => late.promise);
  const choosing = h.workflow.chooseProfile();
  await settle();
  h.workflow.close();
  late.resolve(preview());
  await choosing;
  assert.equal(h.workflow.state.value.phase, 'closed');
  assert.equal(h.calls.length, 0);
  assert.ok(h.nativeCalls.some(([command]) => command === 'forget_import_profile'));
});

test('a changed or unavailable preview blocks POST until the user reviews a fresh preview', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  h.setPreview(preview({ fingerprint: 'b'.repeat(64) }));
  await h.workflow.importSelected();
  assert.match(h.workflow.state.value.error ?? '', /source changed/);
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 0);
  h.setRead(async () => {
    throw new Error('The predecessor profile changed. Create a new preview.');
  });
  await h.workflow.importSelected();
  assert.equal(h.workflow.state.value.phase, 'stale');
  assert.equal(button(h, 'Import instance').props.disabled, true);
  h.setRead(null);
  await h.workflow.refreshPreview();
  await h.workflow.importSelected();
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
});

test('closing or unmounting during the preflight read prevents a later POST', async () => {
  for (const cancel of ['close', 'dispose'] as const) {
    const h = harness();
    await h.workflow.chooseProfile();
    const read = deferred<unknown>();
    h.setRead(() => read.promise);
    const importing = h.workflow.importSelected();
    h.workflow[cancel]();
    read.resolve(preview());
    await importing;
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 0);
  }
});

test('an accepted import remains owned, ignores duplicate submit and cannot be canceled through the dialog', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  const post = deferred<unknown>();
  h.setPost(() => post.promise);
  const importing = h.workflow.importSelected();
  await settle();
  assert.equal(h.workflow.state.value.phase, 'importing');
  assert.equal(button(h, 'Cancel').props.disabled, true);
  h.workflow.close();
  await h.workflow.importSelected();
  assert.equal(h.workflow.state.value.phase, 'importing');
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  post.resolve({ legacy_id: legacyId, cutover_available: false, instance });
  await importing;
  assert.equal(h.navigation.length, 1);
});

test('a confirmed import with a failed registry read retries only the read and never copies again', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  h.setRegistry(async () => {
    throw new Error('Library unavailable.');
  });
  await h.workflow.importSelected();
  assert.equal(h.workflow.state.value.phase, 'imported');
  assert.equal(h.workflow.state.value.imported?.id, instanceId);
  assert.equal(h.navigation.length, 0);
  assert.ok(button(h, 'Open instance'));
  h.setRegistry(null);
  await h.workflow.openImportedInstance();
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  assert.equal(h.store.instances.value.length, 1);
  assert.equal(h.navigation.length, 1);
});

test('a lost import response warns that the copy may finish and offers only a library read', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  h.setPost(async () => {
    throw new Error('Response lost.');
  });
  await h.workflow.importSelected();
  assert.equal(h.workflow.state.value.phase, 'uncertain');
  assert.match(h.workflow.state.value.error ?? '', /may still finish; closing this dialog does not cancel it/);
  assert.ok(button(h, 'Close'));
  assert.ok(button(h, 'Check instance library'));
  assert.equal(
    h.view().some((node) => node.type === 'Button' && node.props.children === 'Cancel'),
    false,
  );
  await h.workflow.importSelected();
  await h.workflow.checkInstanceLibrary();
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  assert.equal(h.store.instances.value[0].id, instanceId);
  assert.equal(JSON.stringify(h.navigation), JSON.stringify([{ name: 'instances' }]));
  assert.equal(
    h.notices.some((message) => message.startsWith('Imported')),
    false,
  );
});

test('browser mode offers a desktop explanation and malformed previews remain blocked', async () => {
  const browser = harness(false);
  assert.equal(button(browser, 'Choose profile').props.disabled, true);
  assert.equal(button(browser, 'Choose profile').props.title, 'Available in the desktop app');
  const h = harness();
  for (const invalid of [
    null,
    { ...preview(), fingerprint: '../path' },
    { ...preview(), skin_import_id: '../path' },
    { ...preview(), skin_import_available: undefined },
    { ...preview(), rules_import_id: '../path' },
    { ...preview(), rules_import_available: undefined },
    { ...preview(), instances: [{ ...preview().instances[0], ordinary_import_available: 'true' }] },
    { ...preview(), instances: [preview().instances[0], preview().instances[0]] },
  ]) {
    assert.throws(() => h.helpers.importPreviewResponse(invalid));
  }
  h.setPreview({ ...preview(), instances: [{ ...preview().instances[0], ordinary_import_available: undefined }] });
  await h.workflow.chooseProfile();
  assert.equal(h.workflow.state.value.phase, 'stale');
  assert.equal(button(h, 'Import instance').props.disabled, true);
});

test('the reused modal traps Tab and Escape cancels the preview through its normal close callback', async () => {
  const h = harness();
  await h.workflow.chooseProfile();
  const modal = h.view().find((node) => node.type === 'Modal');
  assert.ok(modal);
  let onKey:
    | ((event: { key: string; shiftKey?: boolean; stopPropagation(): void; preventDefault(): void }) => void)
    | null = null;
  let active: unknown;
  const first = {
    offsetParent: {},
    focus() {
      active = first;
    },
  };
  const last = {
    offsetParent: {},
    focus() {
      active = last;
    },
  };
  const panel = { querySelector: () => first, querySelectorAll: () => [first, last], focus() {} };
  const { ModalContent } = source<typeof import('../../src/ui/Modal')>(
    'ui/Modal.tsx',
    {
      preact: { createContext: () => ({}) },
      'preact/compat': { createPortal: (children: unknown) => children },
      'preact/hooks': {
        useContext: () => ({ close: () => (modal.props.onOpenChange as (open: boolean) => void)(false) }),
        useRef: () => ({ current: panel }),
        useEffect: (effect: () => unknown) => effect(),
      },
      './Icons': { Icon: 'Icon' },
      '../utils': { cn: (...names: string[]) => names.filter(Boolean).join(' ') },
      './Dialog': { dialogOpen: { value: false } },
      'preact/jsx-runtime': jsx,
    },
    {
      document: {
        get activeElement() {
          return active;
        },
        addEventListener(_name: string, listener: typeof onKey) {
          onKey = listener;
        },
        removeEventListener() {},
        body: {},
      },
    },
  );
  ModalContent({ children: null });
  const sendKey = onKey as unknown as (event: {
    key: string;
    shiftKey?: boolean;
    stopPropagation(): void;
    preventDefault(): void;
  }) => void;
  active = last;
  sendKey({ key: 'Tab', preventDefault() {}, stopPropagation() {} });
  assert.equal(active, first);
  sendKey({ key: 'Escape', preventDefault() {}, stopPropagation() {} });
  assert.equal(h.workflow.state.value.phase, 'closed');
  assert.equal(h.calls.length, 0);
});

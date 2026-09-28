import { signal } from '@preact/signals';
import { api, isApiError } from '../../api';
import { addInstance, setConfig, updateInstanceInList } from '../../actions';
import { configResponse, instancesResponse } from '../../dto-core';
import { dtoArray, dtoBoolean, dtoEnum, dtoNumber, dtoRecord, dtoString } from '../../dto-contract';
import type { ImportBlocker } from '../../generated/ImportBlocker';
import type { ImportPreview } from '../../generated/ImportPreview';
import type { InstanceImportRequest } from '../../generated/InstanceImportRequest';
import type { InstanceImportResponse } from '../../generated/InstanceImportResponse';
import type { InstancePreview } from '../../generated/InstancePreview';
import type { InstanceImportMappings } from '../../generated/InstanceImportMappings';
import type { MetadataImportRequest } from '../../generated/MetadataImportRequest';
import type { MetadataImportReceipt } from '../../generated/MetadataImportReceipt';
import type { SkinImportRequest } from '../../generated/SkinImportRequest';
import type { SkinImportResponse } from '../../generated/SkinImportResponse';
import type { RulesImportRequest } from '../../generated/RulesImportRequest';
import type { RulesImportReceipt } from '../../generated/RulesImportReceipt';
import type { RulesImportStatus } from '../../generated/RulesImportStatus';
import { refreshFlags } from '../../flags';
import { refreshInstanceReadiness } from '../../instance-readiness';
import { accountsSnapshot, refreshAccountsData } from '../../machines/accounts';
import { refreshWardrobe, wardrobeData } from '../../machines/skin-wardrobe';
import { Music } from '../../music';
import {
  forgetNativeImportProfile,
  hasNativeDesktopRuntime,
  pickNativeImportInstanceSource,
  pickNativeImportProfile,
} from '../../native';
import { PREFERENCE_BYTES_LIMIT } from '../../preferences/local';
import { nativePreferenceBaseline, reloadApplication, replaceNativePreferences } from '../../preferences/persistence';
import {
  importPreferenceProfile,
  preferenceImportNeedsReload,
  preferenceReferences,
  preferenceSnapshot,
  previewPreferenceImport,
  resolvePreferenceProfile,
  type PreferenceProfile,
} from '../../profile-transfer';
import { local, localStateVersion, suspendLocalStatePersistence } from '../../state';
import { instances } from '../../store';
import { toast } from '../../toast';
import { applyImportedConfigTheme } from '../../theme';
import { navigate, route, suspendRoutePersistence } from '../../ui-state';
import { errMessage } from '../../utils';

function currentPreferenceSnapshot(): string {
  return hasNativeDesktopRuntime()
    ? nativePreferenceBaseline(local, route.value)
    : preferenceSnapshot(localStorage, local, route.value);
}

export const importBlockerText: Record<ImportBlocker, string> = {
  cutover_not_implemented: 'Full profile migration is not available.',
  missing_required_record: 'Required profile data is missing.',
  unsupported_schema: 'This profile uses an unsupported data format.',
  missing_instance_source: 'Choose the original folder for this instance.',
  unsupported_loader: 'This instance uses an unsupported loader.',
  pending_deletion: 'A deletion is unfinished in the original profile.',
  unsettled_operation: 'An operation is unfinished in the original profile.',
  managed_state_requires_conversion: 'Managed performance data needs conversion.',
  content_provenance_requires_conversion: 'Installed content records need conversion.',
  browser_preferences_required: 'Browser preferences have not been exported.',
  retained_preference_requires_conversion: 'Saved preferences need conversion.',
  instance_metadata_requires_conversion: 'Instance settings need conversion.',
  saved_skins_require_conversion: 'Saved skins need conversion.',
  retained_history_requires_conversion: 'Saved history needs conversion.',
  account_requires_conversion: 'Account records need conversion.',
  unknown_retained_record: 'The source contains an unsupported record.',
  unsafe_file: 'A source file cannot be imported safely.',
};

function blockers(value: unknown): ImportBlocker[] {
  return dtoArray(value, 'Import blockers').map((item) =>
    dtoEnum(item, 'Import blocker', Object.keys(importBlockerText) as ImportBlocker[]),
  );
}

function count(value: unknown, label: string): number {
  const number = dtoNumber(value, label);
  if (!Number.isSafeInteger(number) || number < 0) throw new Error(`${label} response was invalid.`);
  return number;
}

function legacyId(value: unknown): string {
  const id = dtoString(value, 'Import instance identity');
  if (!/^[0-9a-f]{16}$/.test(id)) throw new Error('Import instance identity response was invalid.');
  return id;
}

function importId(value: unknown): string {
  const id = dtoString(value, 'Import identity');
  if (!/^[0-9a-f]{64}$/.test(id)) throw new Error('Import identity response was invalid.');
  return id;
}

function instancePreview(value: unknown): InstancePreview {
  const record = dtoRecord(value, 'Instance import preview');
  return {
    legacy_id: legacyId(record.legacy_id),
    name: dtoString(record.name, 'Import instance name'),
    loader_key: dtoString(record.loader_key, 'Import instance loader'),
    ordinary_import_available: dtoBoolean(record.ordinary_import_available, 'Instance import availability'),
    blockers: blockers(record.blockers),
  };
}

export function importPreviewResponse(value: unknown): ImportPreview {
  const record = dtoRecord(value, 'Import preview');
  const fingerprint = dtoString(record.fingerprint, 'Import fingerprint');
  if (!/^[0-9a-f]{64}$/.test(fingerprint)) throw new Error('Import fingerprint response was invalid.');
  const rows = dtoArray(record.instances, 'Import instances').map(instancePreview);
  if (new Set(rows.map((row) => row.legacy_id)).size !== rows.length) {
    throw new Error('Import preview returned duplicate instance identities.');
  }
  return {
    cutover_available: dtoBoolean(record.cutover_available, 'Profile migration availability'),
    fingerprint,
    metadata_import_id: importId(record.metadata_import_id),
    metadata_import_available: dtoBoolean(record.metadata_import_available, 'Account and settings import availability'),
    skin_import_id: importId(record.skin_import_id),
    skin_import_available: dtoBoolean(record.skin_import_available, 'Saved skin import availability'),
    rules_import_id: importId(record.rules_import_id),
    rules_import_available: dtoBoolean(record.rules_import_available, 'Performance rules import availability'),
    instances: rows,
    file_count: count(record.file_count, 'Import file count'),
    byte_count: count(record.byte_count, 'Import byte count'),
    offline_account_count: count(record.offline_account_count, 'Import offline account count'),
    microsoft_reauthentication_count: count(record.microsoft_reauthentication_count, 'Import Microsoft account count'),
    saved_skin_count: count(record.saved_skin_count, 'Import saved skin count'),
    retained_obligation_count: count(record.retained_obligation_count, 'Import retained operation count'),
    retained_records: dtoArray(record.retained_records, 'Import retained records').map((value) => {
      const retained = dtoRecord(value, 'Import retained record');
      return {
        record_id: dtoString(retained.record_id, 'Import retained record identity'),
        instance_ids: dtoArray(retained.instance_ids, 'Import retained instance identities').map(legacyId),
        blocker: dtoEnum(
          retained.blocker,
          'Import retained blocker',
          Object.keys(importBlockerText) as ImportBlocker[],
        ),
      };
    }),
    blockers: blockers(record.blockers),
  };
}

function metadataReceipt(value: unknown, expectedId: string): MetadataImportReceipt {
  const record = dtoRecord(value, 'Metadata import receipt');
  const id = importId(record.metadata_import_id);
  if (id !== expectedId) throw new Error('The completion receipt belongs to a different metadata import.');
  const importedCount = count(record.imported_offline_account_count, 'Imported offline account count');
  const microsoftCount = count(record.imported_microsoft_account_count, 'Imported Microsoft account count');
  const total = importedCount + microsoftCount;
  const settingsRevision = count(record.settings_revision, 'Imported settings revision');
  if (
    total === 0 ||
    total > 256 ||
    settingsRevision === 0 ||
    (record.account_id_mapping === null && microsoftCount > 0)
  ) {
    throw new Error('The metadata import receipt was invalid.');
  }
  let mapping: MetadataImportReceipt['account_id_mapping'] = null;
  if (record.account_id_mapping !== null) {
    const entries = Object.entries(dtoRecord(record.account_id_mapping, 'Imported account mapping'));
    if (entries.length !== total) throw new Error('The imported account mapping count was invalid.');
    mapping = Object.fromEntries(
      entries.map(([source, value]) => {
        const destination = dtoString(value, 'Imported account identity');
        if (![source, destination].every((id) => /^[A-Za-z0-9_-]{1,128}$/.test(id))) {
          throw new Error('The imported account mapping was invalid.');
        }
        return [source, destination];
      }),
    );
    if (new Set(Object.values(mapping)).size !== total) throw new Error('Imported account identities were duplicated.');
  }
  return {
    metadata_import_id: id,
    imported_offline_account_count: importedCount,
    imported_microsoft_account_count: microsoftCount,
    account_id_mapping: mapping,
    settings_revision: settingsRevision,
    account_selection_revision: count(record.account_selection_revision, 'Imported account selection revision'),
  };
}

function importReceipt(value: unknown, command: boolean): unknown {
  const record = dtoRecord(value, 'Import status');
  if (dtoBoolean(record.cutover_available, 'Profile migration availability')) {
    throw new Error('Full profile migration is unsupported.');
  }
  if (command) dtoBoolean(record.already_imported, 'Previous import');
  return record.receipt;
}

function metadataResult(value: unknown, expectedId: string, command = false): MetadataImportReceipt | null {
  const receipt = importReceipt(value, command);
  return !command && receipt === null ? null : metadataReceipt(receipt, expectedId);
}

function skinResult(value: unknown, request: SkinImportRequest, command = false): SkinImportResponse['receipt'] | null {
  const result = importReceipt(value, command);
  if (!command && result === null) return null;
  const receipt = dtoRecord(result, 'Saved skin import receipt');
  const id = importId(receipt.skin_import_id);
  const fingerprint = importId(receipt.fingerprint);
  const keys = dtoArray(receipt.texture_keys, 'Imported skin identities');
  if (id !== request.skin_import_id || fingerprint !== request.fingerprint || keys.length > 32_768) {
    throw new Error('The completion receipt did not match this saved skin import.');
  }
  const textureKeys = keys.map(importId);
  if (new Set(textureKeys).size !== textureKeys.length) throw new Error('Imported skin identities were duplicated.');
  return { skin_import_id: id, fingerprint, texture_keys: textureKeys };
}

function rulesResult(value: unknown, request: RulesImportRequest, command = false): RulesImportStatus {
  const record = dtoRecord(value, 'Performance rules import');
  const result = importReceipt(record, command);
  const matches = dtoBoolean(record.stored_cache_matches_import, 'Imported rules cache match');
  if (!command && result === null) {
    if (matches) throw new Error('An absent rules receipt cannot match the stored cache.');
    return { receipt: null, stored_cache_matches_import: false, cutover_available: false };
  }
  const receipt = dtoRecord(result, 'Performance rules import receipt');
  const id = importId(receipt.rules_import_id);
  const fingerprint = importId(receipt.fingerprint);
  const cache = receipt.cache_sha256 === null ? null : importId(receipt.cache_sha256);
  const history = dtoArray(receipt.refresh_history, 'Imported rules refresh history');
  if (
    id !== request.rules_import_id ||
    fingerprint !== request.fingerprint ||
    history.length > 128 ||
    (cache === null && (history.length === 0 || matches))
  )
    throw new Error('The completion receipt did not match this performance rules import.');
  const identities = new Set<string>();
  let sequence = 0n;
  const refreshHistory = history.map((value): RulesImportReceipt['refresh_history'][number] => {
    const refresh = dtoRecord(value, 'Imported rules refresh');
    const id = dtoString(refresh.operation_id, 'Imported rules refresh identity');
    const next = dtoString(refresh.sequence, 'Imported rules refresh sequence');
    if (!/^[1-9][0-9]{0,19}$/.test(next)) throw new Error('Imported rules refresh sequence was invalid.');
    const ordinal = BigInt(next);
    if (
      !/^op-[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(id) ||
      identities.has(id) ||
      ordinal > 18446744073709551615n ||
      ordinal <= sequence
    )
      throw new Error('Imported rules refresh history was invalid.');
    identities.add(id);
    sequence = ordinal;
    const outcome = dtoRecord(refresh.outcome, 'Imported rules refresh outcome');
    const state = dtoEnum(outcome.state, 'Imported rules refresh state', ['succeeded', 'failed']);
    return {
      operation_id: id,
      sequence: next,
      outcome:
        state === 'succeeded'
          ? { state, cache_changed: dtoBoolean(outcome.cache_changed, 'Imported rules cache change') }
          : {
              state,
              failure_point: dtoEnum(outcome.failure_point, 'Imported rules refresh failure', [
                'refresh_remote_rules',
                'refresh_rules_journal_reconciliation',
              ]),
            },
    };
  });
  return {
    receipt: { rules_import_id: id, fingerprint, cache_sha256: cache, refresh_history: refreshHistory },
    stored_cache_matches_import: matches,
    cutover_available: false,
  };
}

async function refreshMetadataDestination(receipt: MetadataImportReceipt): Promise<void> {
  const preferenceVersion = localStateVersion.value;
  const [settings] = await Promise.all([
    api('GET', '/config').then(configResponse),
    refreshAccountsData({ fresh: true }),
    refreshFlags({ fresh: true }),
  ]);
  const accounts = accountsSnapshot.value;
  if (
    accounts.state !== 'ready' ||
    accounts.selection_revision === null ||
    accounts.selection_revision < receipt.account_selection_revision
  ) {
    throw new Error('The imported accounts could not be refreshed.');
  }
  if (
    settings.revision < receipt.settings_revision ||
    settings.account_selection_revision < accounts.selection_revision ||
    !setConfig(settings)
  ) {
    throw new Error('Settings or account selection changed. Refresh again.');
  }
  Music.applyConfig(settings, true);
  await applyImportedConfigTheme(settings, preferenceVersion);
  await refreshInstanceReadiness();
}

type ImportedInstance = Pick<InstanceImportResponse['instance'], 'id' | 'name'>;

function destinationInstanceId(value: unknown): string {
  const id = dtoString(value, 'Imported instance identity');
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(id)) {
    throw new Error('Imported instance identity response was invalid.');
  }
  return id;
}

function importedInstanceResponse(value: unknown, expectedLegacyId: string): ImportedInstance {
  const record = dtoRecord(value, 'Instance import');
  if (
    legacyId(record.legacy_id) !== expectedLegacyId ||
    dtoBoolean(record.cutover_available, 'Profile migration availability')
  ) {
    throw new Error('Instance import returned an unexpected result. Check the instance library before trying again.');
  }
  const instance = dtoRecord(record.instance, 'Imported instance');
  return { id: destinationInstanceId(instance.id), name: dtoString(instance.name, 'Imported instance name') };
}

function instanceMappings(value: unknown, preview: ImportPreview): InstanceImportMappings {
  const record = dtoRecord(value, 'Imported instance mappings');
  const entries = Object.entries(dtoRecord(record.instance_id_mapping, 'Imported instance identities'));
  if (
    record.fingerprint !== preview.fingerprint ||
    record.metadata_import_id !== preview.metadata_import_id ||
    dtoBoolean(record.cutover_available, 'Profile migration availability') ||
    entries.length > 4096
  ) {
    throw new Error('The instance mappings did not match the selected source.');
  }
  const mapping = Object.fromEntries(entries.map(([id, value]) => [legacyId(id), destinationInstanceId(value)]));
  if (new Set(Object.values(mapping)).size !== entries.length)
    throw new Error('Imported instance identities were duplicated.');
  return {
    fingerprint: preview.fingerprint,
    metadata_import_id: preview.metadata_import_id,
    instance_id_mapping: mapping,
    cutover_available: false,
  };
}

async function resolvePreferences(profile: PreferenceProfile, preview: ImportPreview) {
  const required = preferenceReferences(profile);
  const oldAccounts = accountsSnapshot.value;
  const oldWardrobe = wardrobeData.value;
  const currentInstances = instances.value;
  const [current, metadata, skins] = await Promise.all([
    api('GET', '/import/preview').then(importPreviewResponse),
    required.accounts
      ? api('GET', `/import/metadata/${preview.metadata_import_id}`).then((value) =>
          metadataResult(value, preview.metadata_import_id),
        )
      : null,
    required.skins
      ? api('GET', `/import/skins/${preview.skin_import_id}`).then((value) =>
          skinResult(value, { skin_import_id: preview.skin_import_id, fingerprint: preview.fingerprint }),
        )
      : null,
    required.accounts ? refreshAccountsData({ fresh: true }) : null,
    required.skins ? refreshWardrobe() : null,
  ]);
  if (
    current.fingerprint !== preview.fingerprint ||
    current.metadata_import_id !== preview.metadata_import_id ||
    current.skin_import_id !== preview.skin_import_id
  ) {
    throw new Error('The source changed. Choose the export again after reviewing the profile.');
  }
  const accounts = accountsSnapshot.value;
  const wardrobe = wardrobeData.value;
  if (
    (required.accounts && (accounts === oldAccounts || accounts.state !== 'ready')) ||
    (required.skins && (wardrobe === oldWardrobe || wardrobe.state !== 'ready' || wardrobe.error))
  ) {
    throw new Error('Current account or skin references could not be refreshed. Try again.');
  }
  // Read completed/live instance bindings last, after potentially slower owners.
  const mapping = required.instances
    ? instanceMappings(await api('GET', `/import/instances/${preview.fingerprint}`), preview)
    : null;
  return {
    profile: resolvePreferenceProfile(profile, {
      accounts: metadata?.account_id_mapping ?? null,
      instances: mapping?.instance_id_mapping ?? {},
      skins: skins?.texture_keys ?? [],
      currentAccounts: accounts.accounts.map((account) => account.account_id),
      currentSkins: wardrobe.skins.map((skin) => skin.texture_key),
    }),
    current: () =>
      (!required.accounts || accountsSnapshot.value === accounts) &&
      (!required.skins || wardrobeData.value === wardrobe) &&
      (!required.instances || instances.value === currentInstances),
  };
}

export interface InstanceImportState {
  phase:
    | 'closed'
    | 'picking'
    | 'preview'
    | 'checking'
    | 'importing'
    | 'stale'
    | 'imported'
    | 'uncertain'
    | 'checking-library'
    | 'confirming-metadata'
    | 'importing-metadata'
    | 'metadata-uncertain'
    | 'checking-metadata'
    | 'metadata-imported'
    | 'refreshing-metadata'
    | 'importing-skins'
    | 'skins-uncertain'
    | 'checking-skins'
    | 'skins-imported'
    | 'refreshing-skins'
    | 'importing-rules'
    | 'rules-uncertain'
    | 'checking-rules'
    | 'rules-imported'
    | 'refreshing-rules'
    | 'checking-preferences'
    | 'confirming-preferences'
    | 'preference-recovery'
    | 'reloading';
  preview: ImportPreview | null;
  selectedId: string | null;
  imported: ImportedInstance | null;
  metadataRequest: MetadataImportRequest | null;
  metadataReceipt: MetadataImportReceipt | null;
  skinRequest: SkinImportRequest | null;
  skinReceipt: SkinImportResponse['receipt'] | null;
  rulesRequest: RulesImportRequest | null;
  rulesReceipt: RulesImportReceipt | null;
  preferences: { source: PreferenceProfile; resolved: PreferenceProfile; baseline: string } | null;
  error: string | null;
}

function initialState(): InstanceImportState {
  return {
    phase: 'closed',
    preview: null,
    selectedId: null,
    imported: null,
    metadataRequest: null,
    metadataReceipt: null,
    skinRequest: null,
    skinReceipt: null,
    rulesRequest: null,
    rulesReceipt: null,
    preferences: null,
    error: null,
  };
}

async function refreshImportedInstance(imported: ImportedInstance): Promise<void> {
  const registry = instancesResponse(await api('GET', '/instances'));
  const instance = registry.instances.find((instance) => instance.id === imported.id);
  if (!instance) throw new Error('The imported instance has not appeared in the instance library yet.');
  if (instances.value.some((current) => current.id === instance.id)) updateInstanceInList(instance);
  else addInstance(instance);
}

export function createInstanceImportWorkflow() {
  const state = signal<InstanceImportState>(initialState());
  let revision = 0;
  let disposed = false;
  let closing: Promise<void> = Promise.resolve();
  const current = (capture: number): boolean => !disposed && revision === capture;
  const importUnconfirmed =
    'Import unconfirmed. Accepted work may still finish; closing does not cancel it. Check status before trying again.';

  function publishPreview(preview: ImportPreview, selectedId?: string | null, error: string | null = null): void {
    const selected =
      preview.instances.find((row) => row.legacy_id === selectedId) ??
      preview.instances.find((row) => row.ordinary_import_available) ??
      preview.instances[0];
    const receipt = state.value.rulesReceipt;
    const rulesReceipt =
      receipt?.rules_import_id === preview.rules_import_id && receipt.fingerprint === preview.fingerprint
        ? receipt
        : null;
    state.value = {
      ...initialState(),
      phase: 'preview',
      preview,
      selectedId: selected?.legacy_id ?? null,
      rulesReceipt,
      error,
    };
  }

  function forget(): Promise<void> {
    closing = forgetNativeImportProfile().catch((error) => {
      toast(`Could not close the import preview: ${errMessage(error)}`, 'error');
    });
    return closing;
  }

  function close(): void {
    if (
      [
        'importing',
        'importing-metadata',
        'importing-skins',
        'importing-rules',
        'reloading',
        'preference-recovery',
      ].includes(state.value.phase)
    )
      return;
    revision += 1;
    state.value = initialState();
    void forget();
  }

  async function chooseProfile(): Promise<void> {
    if (disposed || !['closed', 'preview', 'stale'].includes(state.value.phase)) return;
    const capture = ++revision;
    state.value = { ...initialState(), phase: 'picking' };
    try {
      await closing;
      if (!current(capture)) return;
      const payload = await pickNativeImportProfile();
      if (!current(capture)) return;
      if (payload === null) {
        close();
        return;
      }
      publishPreview(importPreviewResponse(payload));
    } catch (error) {
      if (current(capture)) state.value = { ...state.value, phase: 'stale', error: errMessage(error) };
    }
  }

  function select(legacyId: string): void {
    if (state.value.phase !== 'preview' || !state.value.preview?.instances.some((row) => row.legacy_id === legacyId))
      return;
    state.value = { ...state.value, selectedId: legacyId, error: null };
  }

  async function refreshPreview(): Promise<void> {
    if (disposed || !['preview', 'stale'].includes(state.value.phase)) return;
    const capture = ++revision;
    const selectedId = state.value.selectedId;
    state.value = { ...state.value, phase: 'checking', error: null };
    try {
      const preview = importPreviewResponse(await api('GET', '/import/preview'));
      if (current(capture)) publishPreview(preview, selectedId);
    } catch (error) {
      if (current(capture)) state.value = { ...state.value, phase: 'stale', error: errMessage(error) };
    }
  }

  async function chooseInstanceFolder(): Promise<void> {
    const before = state.value;
    const row = before.preview?.instances.find((row) => row.legacy_id === before.selectedId);
    if (disposed || before.phase !== 'preview' || !before.preview || !row?.blockers.includes('missing_instance_source'))
      return;
    const capture = ++revision;
    state.value = { ...before, phase: 'picking', error: null };
    try {
      const payload = await pickNativeImportInstanceSource(before.preview.fingerprint, row.legacy_id);
      if (!current(capture)) return;
      if (payload === null) {
        state.value = before;
        return;
      }
      publishPreview(importPreviewResponse(payload), row.legacy_id);
    } catch (error) {
      if (current(capture)) state.value = { ...before, phase: 'stale', error: errMessage(error) };
    }
  }

  async function openImportedInstance(): Promise<void> {
    const imported = state.value.imported;
    if (!imported || state.value.phase !== 'imported') return;
    const capture = ++revision;
    state.value = { ...state.value, phase: 'checking', error: null };
    try {
      await refreshImportedInstance(imported);
      if (!current(capture)) return;
      close();
      navigate({ name: 'instance', id: imported.id });
    } catch (error) {
      if (current(capture)) state.value = { ...state.value, phase: 'imported', error: errMessage(error) };
    }
  }

  async function importSelected(): Promise<void> {
    const before = state.value;
    const row = before.preview?.instances.find((row) => row.legacy_id === before.selectedId);
    if (disposed || before.phase !== 'preview' || !before.preview || !row?.ordinary_import_available) return;
    const capture = ++revision;
    state.value = { ...before, phase: 'checking', error: null };
    let submitted = false;
    try {
      const preview = importPreviewResponse(await api('GET', '/import/preview'));
      if (!current(capture)) return;
      if (preview.fingerprint !== before.preview.fingerprint) {
        publishPreview(preview, row.legacy_id, 'The source changed. Review this preview before importing.');
        return;
      }
      if (!preview.instances.find((candidate) => candidate.legacy_id === row.legacy_id)?.ordinary_import_available) {
        publishPreview(preview, row.legacy_id, 'This instance is no longer available for import.');
        return;
      }
      state.value = { ...state.value, phase: 'importing' };
      const request: InstanceImportRequest = { fingerprint: preview.fingerprint, legacy_id: row.legacy_id };
      submitted = true;
      const imported = importedInstanceResponse(await api('POST', '/import/instances', request), row.legacy_id);
      // The accepted import has its own lifetime even if this view was unmounted.
      if (!current(capture)) {
        await refreshImportedInstance(imported);
        return;
      }
      state.value = { ...state.value, phase: 'imported', imported, error: null };
      toast(`Imported "${imported.name}"`);
      await openImportedInstance();
    } catch (error) {
      if (current(capture))
        state.value = {
          ...state.value,
          phase: submitted ? 'uncertain' : 'stale',
          error: submitted
            ? `The import result could not be confirmed. It may still finish; closing this dialog does not cancel it. Check the instance library before trying again. ${errMessage(error)}`
            : errMessage(error),
        };
    }
  }

  async function checkInstanceLibrary(): Promise<void> {
    if (state.value.phase !== 'uncertain') return;
    const capture = ++revision;
    state.value = { ...state.value, phase: 'checking-library' };
    try {
      const registry = instancesResponse(await api('GET', '/instances'));
      if (!current(capture)) return;
      instances.value = registry.instances;
      close();
      navigate({ name: 'instances' });
      toast('The import may still be running. Refresh the launcher if it has not appeared yet.', 'info');
    } catch (error) {
      if (current(capture))
        state.value = {
          ...state.value,
          phase: 'uncertain',
          error: `The import may still finish; closing does not cancel it. Could not refresh the instance library: ${errMessage(error)}`,
        };
    }
  }

  async function prepareMetadataImport(): Promise<void> {
    const before = state.value;
    if (disposed || before.phase !== 'preview' || !before.preview?.metadata_import_available) return;
    const capture = ++revision;
    state.value = { ...before, phase: 'checking', error: null };
    try {
      const [preview, settings] = await Promise.all([
        api('GET', '/import/preview').then(importPreviewResponse),
        api('GET', '/config').then(configResponse),
      ]);
      if (!current(capture)) return;
      if (
        preview.fingerprint !== before.preview.fingerprint ||
        preview.metadata_import_id !== before.preview.metadata_import_id
      ) {
        publishPreview(preview, before.selectedId, 'The source changed. Review this preview before importing.');
        return;
      }
      if (!preview.metadata_import_available) {
        publishPreview(
          preview,
          before.selectedId,
          'Accounts and settings are no longer available for import from this profile.',
        );
        return;
      }
      state.value = {
        ...before,
        phase: 'confirming-metadata',
        error: null,
        metadataRequest: {
          metadata_import_id: preview.metadata_import_id,
          fingerprint: preview.fingerprint,
          expected_settings_revision: settings.revision,
          expected_account_selection_revision: settings.account_selection_revision,
        },
      };
    } catch (error) {
      if (current(capture)) state.value = { ...before, phase: 'stale', error: errMessage(error) };
    }
  }

  function cancelMetadataImport(): void {
    if (state.value.phase !== 'confirming-metadata' || !state.value.preview) return;
    revision += 1;
    publishPreview(state.value.preview, state.value.selectedId);
  }

  async function refreshMetadata(receipt: MetadataImportReceipt, capture: number): Promise<void> {
    if (current(capture))
      state.value = { ...state.value, phase: 'refreshing-metadata', metadataReceipt: receipt, error: null };
    await finishImportRefresh(capture, 'metadata-imported', () => refreshMetadataDestination(receipt));
  }

  async function finishImportRefresh(
    capture: number,
    phase: 'metadata-imported' | 'skins-imported',
    refresh: () => Promise<void>,
  ): Promise<void> {
    let error: string | null = null;
    try {
      await refresh();
    } catch (failure) {
      error = `The import is complete. Refresh current data; this will not import again. ${errMessage(failure)}`;
    }
    if (current(capture)) state.value = { ...state.value, phase, error };
  }

  async function refreshImportedMetadata(): Promise<void> {
    const receipt = state.value.metadataReceipt;
    if (!disposed && state.value.phase === 'metadata-imported' && receipt) await refreshMetadata(receipt, ++revision);
  }

  async function checkImport(kind: 'metadata' | 'skins'): Promise<void> {
    const request = kind === 'metadata' ? state.value.metadataRequest : state.value.skinRequest;
    if (disposed || state.value.phase !== `${kind}-uncertain` || !request) return;
    const capture = ++revision;
    state.value = { ...state.value, phase: `checking-${kind}` };
    try {
      if ('skin_import_id' in request) {
        const receipt = skinResult(await api('GET', `/import/skins/${request.skin_import_id}`), request);
        if (receipt) return await refreshSkins(receipt, capture);
      } else {
        const receipt = metadataResult(
          await api('GET', `/import/metadata/${request.metadata_import_id}`),
          request.metadata_import_id,
        );
        if (receipt) return await refreshMetadata(receipt, capture);
      }
      if (current(capture))
        state.value = {
          ...state.value,
          phase: `${kind}-uncertain`,
          error: `No completion receipt yet. ${importUnconfirmed}`,
        };
    } catch (error) {
      if (current(capture))
        state.value = {
          ...state.value,
          phase: `${kind}-uncertain`,
          error: `${importUnconfirmed} ${errMessage(error)}`,
        };
    }
  }

  function checkMetadataImport(): Promise<void> {
    return checkImport('metadata');
  }

  function checkSkinImport(): Promise<void> {
    return checkImport('skins');
  }

  async function importMetadata(): Promise<void> {
    const request = state.value.metadataRequest;
    if (disposed || state.value.phase !== 'confirming-metadata' || !request) return;
    const capture = ++revision;
    state.value = { ...state.value, phase: 'importing-metadata', error: null };
    try {
      const receipt = metadataResult(await api('POST', '/import/metadata', request), request.metadata_import_id, true);
      if (receipt) await refreshMetadata(receipt, capture);
    } catch {
      if (!current(capture)) return;
      state.value = {
        ...state.value,
        phase: 'metadata-uncertain',
        error: importUnconfirmed,
      };
      await checkMetadataImport();
    }
  }

  async function refreshSkins(receipt: SkinImportResponse['receipt'], capture: number): Promise<void> {
    if (current(capture))
      state.value = { ...state.value, phase: 'refreshing-skins', skinReceipt: receipt, error: null };
    await finishImportRefresh(capture, 'skins-imported', async () => {
      const previous = wardrobeData.value;
      await refreshWardrobe();
      const refreshed = wardrobeData.value;
      if (refreshed === previous || refreshed.state !== 'ready' || refreshed.error) {
        throw new Error(refreshed.error || 'The skin library could not be refreshed.');
      }
    });
  }

  async function refreshImportedSkins(): Promise<void> {
    const receipt = state.value.skinReceipt;
    if (!disposed && state.value.phase === 'skins-imported' && receipt) await refreshSkins(receipt, ++revision);
  }

  async function importSkins(): Promise<void> {
    const before = state.value;
    if (disposed || before.phase !== 'preview' || !before.preview?.skin_import_available) return;
    const capture = ++revision;
    state.value = { ...before, phase: 'checking', error: null };
    let submitted = false;
    try {
      const preview = importPreviewResponse(await api('GET', '/import/preview'));
      if (!current(capture)) return;
      if (
        preview.fingerprint !== before.preview.fingerprint ||
        preview.skin_import_id !== before.preview.skin_import_id
      ) {
        publishPreview(preview, before.selectedId, 'The source changed. Review this preview before importing.');
        return;
      }
      if (!preview.skin_import_available) {
        publishPreview(preview, before.selectedId, 'Saved skins are no longer available for import from this profile.');
        return;
      }
      const request: SkinImportRequest = { skin_import_id: preview.skin_import_id, fingerprint: preview.fingerprint };
      state.value = { ...before, phase: 'importing-skins', skinRequest: request, error: null };
      submitted = true;
      const receipt = skinResult(await api('POST', '/import/skins', request), request, true);
      if (receipt) await refreshSkins(receipt, capture);
    } catch (error) {
      if (!current(capture)) return;
      state.value = {
        ...state.value,
        phase: submitted ? 'skins-uncertain' : 'stale',
        error: submitted ? importUnconfirmed : errMessage(error),
      };
      if (submitted) await checkSkinImport();
    }
  }

  async function refreshRules(receipt: RulesImportReceipt, capture: number): Promise<void> {
    if (!current(capture)) return;
    state.value = { ...state.value, phase: 'refreshing-rules', rulesReceipt: receipt, error: null };
    try {
      const preview = importPreviewResponse(await api('GET', '/import/preview'));
      if (!current(capture)) return;
      if (preview.fingerprint !== receipt.fingerprint || preview.rules_import_id !== receipt.rules_import_id) {
        throw new Error('The source changed. Close this dialog and choose the profile again.');
      }
      publishPreview(preview, state.value.selectedId);
    } catch (error) {
      if (current(capture))
        state.value = {
          ...state.value,
          phase: 'rules-imported',
          error: `The rules import is complete. Refresh the preview; this will not import again. ${errMessage(error)}`,
        };
    }
  }

  async function refreshImportedRules(): Promise<void> {
    const receipt = state.value.rulesReceipt;
    if (!disposed && state.value.phase === 'rules-imported' && receipt) await refreshRules(receipt, ++revision);
  }

  async function checkRulesImport(): Promise<void> {
    const request = state.value.rulesRequest;
    if (disposed || state.value.phase !== 'rules-uncertain' || !request) return;
    const capture = ++revision;
    state.value = { ...state.value, phase: 'checking-rules' };
    try {
      const { receipt } = rulesResult(await api('GET', `/import/rules/${request.rules_import_id}`), request);
      if (receipt) return await refreshRules(receipt, capture);
      if (current(capture))
        state.value = {
          ...state.value,
          phase: 'rules-uncertain',
          error: `No completion receipt yet. ${importUnconfirmed}`,
        };
    } catch (error) {
      if (current(capture))
        state.value = { ...state.value, phase: 'rules-uncertain', error: `${importUnconfirmed} ${errMessage(error)}` };
    }
  }

  async function importRules(): Promise<void> {
    const before = state.value;
    if (disposed || before.phase !== 'preview' || !before.preview?.rules_import_available || before.rulesReceipt)
      return;
    const capture = ++revision;
    state.value = { ...before, phase: 'checking', error: null };
    let submitted = false;
    try {
      const preview = importPreviewResponse(await api('GET', '/import/preview'));
      if (!current(capture)) return;
      if (
        preview.fingerprint !== before.preview.fingerprint ||
        preview.rules_import_id !== before.preview.rules_import_id
      ) {
        publishPreview(preview, before.selectedId, 'The source changed. Review this preview before importing.');
        return;
      }
      if (!preview.rules_import_available) {
        publishPreview(
          preview,
          before.selectedId,
          'Performance rules are no longer available for import from this profile.',
        );
        return;
      }
      const request: RulesImportRequest = {
        fingerprint: preview.fingerprint,
        rules_import_id: preview.rules_import_id,
      };
      state.value = { ...before, phase: 'importing-rules', rulesRequest: request, error: null };
      submitted = true;
      const { receipt } = rulesResult(await api('POST', '/import/rules', request), request, true);
      if (receipt) await refreshRules(receipt, capture);
    } catch (error) {
      if (!current(capture)) return;
      if (submitted && isApiError(error) && (error.status === 409 || error.status === 422)) {
        state.value = { ...before, error: errMessage(error) };
        return;
      }
      state.value = {
        ...state.value,
        phase: submitted ? 'rules-uncertain' : 'stale',
        error: submitted ? importUnconfirmed : errMessage(error),
      };
      if (submitted) await checkRulesImport();
    }
  }

  async function choosePreferences(file: Pick<File, 'size' | 'text'>): Promise<void> {
    const before = state.value;
    if (disposed || before.phase !== 'preview' || !before.preview) return;
    const capture = ++revision;
    state.value = { ...before, phase: 'checking-preferences', error: null };
    try {
      if (file.size > PREFERENCE_BYTES_LIMIT) throw new Error('The preference export exceeds the supported size.');
      const source = previewPreferenceImport(await file.text());
      if (!current(capture)) return;
      const resolved = await resolvePreferences(source, before.preview);
      if (!current(capture)) return;
      if (!resolved.current()) throw new Error('Imported references changed. Choose the export again.');
      state.value = {
        ...before,
        phase: 'confirming-preferences',
        error: null,
        preferences: {
          source,
          resolved: resolved.profile,
          baseline: currentPreferenceSnapshot(),
        },
      };
    } catch (error) {
      if (current(capture)) state.value = { ...before, error: errMessage(error) };
    }
  }

  function cancelPreferences(): void {
    if (state.value.phase !== 'confirming-preferences' || !state.value.preview) return;
    revision += 1;
    publishPreview(state.value.preview, state.value.selectedId);
  }

  async function applyPreferences(): Promise<void> {
    const before = state.value;
    if (disposed || before.phase !== 'confirming-preferences' || !before.preview || !before.preferences) return;
    const capture = ++revision;
    state.value = { ...before, phase: 'checking-preferences', error: null };
    try {
      const resolved = await resolvePreferences(before.preferences.source, before.preview);
      if (!current(capture)) return;
      if (
        !resolved.current() ||
        JSON.stringify(resolved.profile) !== JSON.stringify(before.preferences.resolved) ||
        currentPreferenceSnapshot() !== before.preferences.baseline
      ) {
        throw new Error('References or current preferences changed. Cancel and review the export again.');
      }
      if (hasNativeDesktopRuntime()) {
        const baseline = before.preferences.baseline;
        state.value = { ...state.value, phase: 'reloading' };
        try {
          await replaceNativePreferences(
            { version: 1, preferences: resolved.profile.preferences, route: resolved.profile.route },
            () => current(capture) && resolved.current() && currentPreferenceSnapshot() === baseline,
          );
        } catch (error) {
          if (preferenceImportNeedsReload(error)) {
            state.value = { ...state.value, phase: 'preference-recovery', error: errMessage(error) };
            return;
          }
          throw error;
        }
        return;
      }
      // No await after the final witness check: fence this document's writers
      // through the synchronous write/readback/reload, or restore both on failure.
      const resumeLocal = suspendLocalStatePersistence();
      const resumeRoute = suspendRoutePersistence();
      state.value = { ...state.value, phase: 'reloading' };
      try {
        importPreferenceProfile(JSON.stringify(resolved.profile), localStorage, reloadApplication);
      } catch (error) {
        if (preferenceImportNeedsReload(error)) {
          state.value = { ...state.value, phase: 'preference-recovery', error: errMessage(error) };
          return;
        }
        resumeRoute();
        resumeLocal();
        throw error;
      }
    } catch (error) {
      if (current(capture)) state.value = { ...before, error: errMessage(error) };
    }
  }

  async function reloadPreferences(): Promise<void> {
    if (state.value.phase !== 'preference-recovery') return;
    try {
      if (await reloadApplication()) state.value = { ...state.value, phase: 'reloading' };
    } catch (error) {
      state.value = { ...state.value, error: `Reload could not start. ${errMessage(error)}` };
    }
  }

  function dispose(): void {
    disposed = true;
    revision += 1;
    if (state.value.phase !== 'closed') void forget();
  }

  return {
    state,
    chooseProfile,
    select,
    refreshPreview,
    chooseInstanceFolder,
    importSelected,
    openImportedInstance,
    checkInstanceLibrary,
    prepareMetadataImport,
    cancelMetadataImport,
    importMetadata,
    checkMetadataImport,
    refreshImportedMetadata,
    importSkins,
    checkSkinImport,
    refreshImportedSkins,
    importRules,
    checkRulesImport,
    refreshImportedRules,
    choosePreferences,
    cancelPreferences,
    applyPreferences,
    reloadPreferences,
    close,
    dispose,
  };
}

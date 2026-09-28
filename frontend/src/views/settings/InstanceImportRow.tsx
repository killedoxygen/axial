import type { JSX } from 'preact';
import { useEffect, useMemo, useRef } from 'preact/hooks';
import { hasNativeDesktopRuntime } from '../../native';
import { Button } from '../../ui/Atoms';
import { Modal, ModalContent, ModalHeader, ModalTitle } from '../../ui/Modal';
import { SelectField } from '../../ui/Select';
import { SettingRow } from '../../ui/SettingsSheet';
import { LOADER_LABELS } from '../create/defaults';
import { createInstanceImportWorkflow, importBlockerText } from './instance-import';

export function InstanceImportRow(): JSX.Element {
  const workflow = useMemo(createInstanceImportWorkflow, []);
  const preferenceFile = useRef<HTMLInputElement>(null);
  useEffect(() => () => workflow.dispose(), [workflow]);
  const state = workflow.state.value;
  const desktop = hasNativeDesktopRuntime();
  const busy = [
    'picking',
    'checking',
    'importing',
    'checking-library',
    'importing-metadata',
    'checking-metadata',
    'refreshing-metadata',
    'importing-skins',
    'checking-skins',
    'refreshing-skins',
    'importing-rules',
    'checking-rules',
    'refreshing-rules',
    'checking-preferences',
    'reloading',
  ].includes(state.phase);
  const uncertain = state.phase === 'uncertain' || state.phase === 'checking-library';
  const metadataActive = state.metadataRequest !== null;
  const metadataUncertain = state.phase === 'metadata-uncertain' || state.phase === 'checking-metadata';
  const skinsActive = state.skinRequest !== null;
  const skinsUncertain = state.phase === 'skins-uncertain' || state.phase === 'checking-skins';
  const rulesActive = state.rulesRequest !== null;
  const rulesUncertain = state.phase === 'rules-uncertain' || state.phase === 'checking-rules';
  const extraImportActive = metadataActive || skinsActive || rulesActive || state.preferences !== null;
  const accepted = [
    'importing',
    'importing-metadata',
    'importing-skins',
    'importing-rules',
    'reloading',
    'preference-recovery',
  ].includes(state.phase);
  const row = state.preview?.instances.find((instance) => instance.legacy_id === state.selectedId);
  const canImport = state.phase === 'preview' && row?.ordinary_import_available === true;
  const primaryAction = (label: string, run: () => Promise<void>, disabled = busy): JSX.Element => (
    <Button variant="primary" disabled={disabled} onClick={() => void run()}>
      {label}
    </Button>
  );

  return (
    <>
      <SettingRow
        title="Import from an older profile"
        description="Import instances, accounts/settings, saved skins or performance rules separately. Full profile migration is unavailable."
        control={
          <Button
            variant="secondary"
            icon="folder"
            disabled={!desktop || state.phase !== 'closed'}
            title={desktop ? 'Choose an older Axial profile' : 'Available in the desktop app'}
            onClick={() => void workflow.chooseProfile()}
          >
            Choose profile
          </Button>
        }
      />
      <Modal
        open={state.phase !== 'closed'}
        onOpenChange={(open) => {
          if (!open) workflow.close();
        }}
      >
        <ModalContent
          className="cp-dialog"
          style={{ overflowY: 'auto' }}
          aria-labelledby="cp-instance-import-title"
          aria-describedby="cp-instance-import-description"
          showCloseButton={false}
        >
          <ModalHeader>
            <ModalTitle id="cp-instance-import-title">Import from an older profile</ModalTitle>
          </ModalHeader>
          <p id="cp-instance-import-description" class="cp-dialog-body">
            Imports are separate; source files stay unchanged. Credentials are never copied; Microsoft identities
            require sign-in in Accounts. Library location is excluded. Full profile migration is unavailable.
          </p>
          {state.preview && !state.imported && !extraImportActive && (
            <>
              {state.preview.instances.length > 0 ? (
                <SelectField
                  value={state.selectedId ?? ''}
                  onChange={workflow.select}
                  ariaLabel="Instance to import"
                  disabled={busy || state.phase !== 'preview'}
                  width="100%"
                  options={state.preview.instances.map((instance) => ({
                    value: instance.legacy_id,
                    label: `${instance.name} (${
                      instance.loader_key ||
                      (instance.blockers.includes('unsupported_loader') ? 'Unknown loader' : LOADER_LABELS.vanilla)
                    })`,
                  }))}
                />
              ) : (
                <p class="cp-dialog-body">This profile has no instances to import.</p>
              )}
              {row && !row.ordinary_import_available && (
                <div role="status">
                  <p class="cp-dialog-body">This instance is not available for import.</p>
                  {row.blockers.length > 0 && (
                    <ul class="cp-dialog-body">
                      {row.blockers.map((blocker) => (
                        <li key={blocker}>{importBlockerText[blocker]}</li>
                      ))}
                    </ul>
                  )}
                </div>
              )}
              {row?.blockers.includes('missing_instance_source') && (
                <Button
                  variant="secondary"
                  icon="folder"
                  disabled={state.phase !== 'preview'}
                  onClick={() => void workflow.chooseInstanceFolder()}
                >
                  Choose instance folder
                </Button>
              )}
              <div>
                <strong>Accounts and settings</strong>
                <p class="cp-dialog-body">
                  {state.preview.metadata_import_available
                    ? `${state.preview.offline_account_count} offline and ${state.preview.microsoft_reauthentication_count} Microsoft identities and settings are available.`
                    : 'Accounts/settings are unavailable: source data is incomplete or unsupported.'}
                </p>
                <Button
                  variant="secondary"
                  disabled={state.phase !== 'preview' || !state.preview.metadata_import_available}
                  onClick={() => void workflow.prepareMetadataImport()}
                >
                  Review accounts and settings
                </Button>
              </div>
              <div>
                <strong>Interface preferences</strong>
                <p class="cp-dialog-body">
                  Choose an export made in the older Axial window. Referenced accounts, skins and instances must already
                  be imported.
                </p>
                <input
                  ref={preferenceFile}
                  type="file"
                  accept="application/json,.json"
                  style={{ display: 'none' }}
                  onChange={(event) => {
                    const file = event.currentTarget.files?.[0];
                    event.currentTarget.value = '';
                    if (file) void workflow.choosePreferences(file);
                  }}
                />
                <Button
                  variant="secondary"
                  disabled={state.phase !== 'preview'}
                  onClick={() => preferenceFile.current?.click()}
                >
                  Choose preference export
                </Button>
              </div>
              <div>
                <strong>Saved skins</strong>
                <p class="cp-dialog-body">
                  {state.preview.skin_import_available
                    ? `${state.preview.saved_skin_count} saved skins are available with their names, variants and saved metadata. No skin is applied to an account.`
                    : 'Saved skins are unavailable: source data is incomplete or unsupported.'}
                </p>
                <Button
                  variant="secondary"
                  disabled={state.phase !== 'preview' || !state.preview.skin_import_available}
                  onClick={() => void workflow.importSkins()}
                >
                  Import saved skins
                </Button>
              </div>
              <div>
                <strong>Performance rules</strong>
                <p class="cp-dialog-body">
                  {state.preview.rules_import_available
                    ? "Import signed rules and completed refresh history. Cached rules must pass this launcher's configured signing key and policy."
                    : 'Performance rules are unavailable: source data is missing, incomplete or unsupported.'}
                </p>
                <Button
                  variant="secondary"
                  disabled={
                    state.phase !== 'preview' || !state.preview.rules_import_available || state.rulesReceipt !== null
                  }
                  onClick={() => void workflow.importRules()}
                >
                  Import performance rules
                </Button>
              </div>
              {state.preview.blockers.length > 0 && (
                <details>
                  <summary class="cp-dialog-body">Profile migration limitations</summary>
                  <ul class="cp-dialog-body">
                    {state.preview.blockers.map((blocker) => (
                      <li key={blocker}>{importBlockerText[blocker]}</li>
                    ))}
                  </ul>
                </details>
              )}
            </>
          )}
          {state.phase === 'confirming-metadata' && (
            <p class="cp-dialog-body" role="status">
              Import {state.preview?.offline_account_count} offline and{' '}
              {state.preview?.microsoft_reauthentication_count} Microsoft identities and select the source profile's
              active account? This replaces current launcher settings, including telemetry consent and feature
              overrides. Existing accounts remain. This action does not import instances.
            </p>
          )}
          {state.phase === 'confirming-preferences' && (
            <p class="cp-dialog-body" role="status">
              Confirm that this export belongs to the selected older profile. Its source is not recorded in the export.
              This replaces interface preferences and the saved route, then reloads the interface. Unsaved drafts will
              be discarded. Close other Axial tabs first; their preference writes are not coordinated.
            </p>
          )}
          {busy && (
            <p class="cp-dialog-body" role="status">
              {state.phase === 'reloading'
                ? 'Reloading the interface...'
                : state.phase === 'picking'
                  ? 'Choose a folder in the system dialog.'
                  : accepted
                    ? 'Importing. An accepted import cannot be canceled. Keep this dialog open until it finishes.'
                    : 'Checking the source or refreshing current launcher data...'}
            </p>
          )}
          {state.metadataReceipt && (
            <p class="cp-dialog-body" role="status">
              Import confirmed for {state.metadataReceipt.imported_offline_account_count} offline and{' '}
              {state.metadataReceipt.imported_microsoft_account_count} Microsoft identities and settings. Later
              destination edits are preserved.
            </p>
          )}
          {state.imported && (
            <p class="cp-dialog-body" role="status">
              Imported "{state.imported.name}". Refresh the instance library to open it.
            </p>
          )}
          {state.skinReceipt && (
            <p class="cp-dialog-body" role="status">
              Saved-skin import confirmed for {state.skinReceipt.texture_keys.length} records. Later edits and deletions
              are preserved. No skin was applied to an account.
            </p>
          )}
          {state.rulesReceipt && (
            <p class="cp-dialog-body" role="status">
              Performance rules import confirmed.{' '}
              {state.rulesReceipt.cache_sha256 === null
                ? 'Refresh history was retained without a cached ruleset.'
                : 'The signed cache and its available refresh history were recorded.'}{' '}
              Current rules trust and launch readiness are checked separately. Full profile migration remains
              unavailable.
            </p>
          )}
          {state.error && (
            <p class="cp-dialog-error" role="alert">
              {state.error}
            </p>
          )}
          <div class="cp-dialog-actions cp-dialog-actions--choice">
            <Button
              variant="ghost"
              disabled={accepted}
              onClick={
                state.phase === 'confirming-preferences'
                  ? workflow.cancelPreferences
                  : state.phase === 'confirming-metadata'
                    ? workflow.cancelMetadataImport
                    : workflow.close
              }
            >
              {state.imported ||
              uncertain ||
              metadataUncertain ||
              state.metadataReceipt ||
              skinsActive ||
              rulesActive ||
              state.rulesReceipt
                ? 'Close'
                : 'Cancel'}
            </Button>
            {!busy && !state.imported && !uncertain && !extraImportActive && (
              <Button variant="secondary" onClick={() => void workflow.chooseProfile()}>
                Choose profile
              </Button>
            )}
            {!busy && state.preview && !state.imported && !uncertain && !extraImportActive && (
              <Button variant="secondary" onClick={() => void workflow.refreshPreview()}>
                Refresh preview
              </Button>
            )}
            {state.phase === 'preference-recovery'
              ? primaryAction('Reload interface', workflow.reloadPreferences)
              : rulesActive
                ? rulesUncertain
                  ? primaryAction('Check import status', workflow.checkRulesImport)
                  : state.rulesReceipt && state.error && primaryAction('Refresh preview', workflow.refreshImportedRules)
                : state.preferences
                  ? primaryAction('Apply preferences and reload', workflow.applyPreferences)
                  : skinsActive
                    ? skinsUncertain
                      ? primaryAction('Check import status', workflow.checkSkinImport)
                      : state.skinReceipt &&
                        state.error &&
                        primaryAction('Refresh skin library', workflow.refreshImportedSkins)
                    : state.phase === 'confirming-metadata'
                      ? primaryAction('Import accounts and settings', workflow.importMetadata)
                      : metadataUncertain
                        ? primaryAction('Check import status', workflow.checkMetadataImport)
                        : state.metadataReceipt
                          ? state.error && primaryAction('Refresh imported settings', workflow.refreshImportedMetadata)
                          : metadataActive
                            ? null
                            : uncertain
                              ? primaryAction('Check instance library', workflow.checkInstanceLibrary)
                              : state.imported
                                ? primaryAction('Open instance', workflow.openImportedInstance)
                                : primaryAction('Import instance', workflow.importSelected, !canImport)}
          </div>
        </ModalContent>
      </Modal>
    </>
  );
}

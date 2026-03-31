import type { JSX } from 'preact';
import { useEffect } from 'preact/hooks';
import { signal, useSignal } from '@preact/signals';
import { catalog, runningSessions, versions } from '../store';
import type { VersionInfo } from '../types';
import { api } from '../api';
import { Sound } from '../sound';
import { showError, formatBytes, errMessage } from '../utils';

type DeleteTarget = { id?: string };
type DeleteStep = 'analyze' | 'summary' | 'progress' | 'done';

export const showDeleteWizard = signal(false);
export const deleteTarget = signal<DeleteTarget | null>(null);

export function openDeleteWizard(version: DeleteTarget): void {
  if (!version.id) return;
  const runningWithVersion = Object.values(runningSessions.value).some((session) => session.versionId === version.id);
  if (runningWithVersion) {
    showError(`Cannot delete ${version.id} while it's running. Stop the game first.`);
    return;
  }
  deleteTarget.value = version;
  showDeleteWizard.value = true;
}

export function closeDeleteWizard(): void {
  showDeleteWizard.value = false;
  deleteTarget.value = null;
  Sound.ui('soft');
}

export function bindDeleteWizard(): void {}

export function DeleteWizard(): JSX.Element | null {
  const target = deleteTarget.value;
  const isOpen = showDeleteWizard.value;
  const versionId = target?.id ?? null;

  const step = useSignal<DeleteStep>('analyze');
  const info = useSignal<VersionInfo | null>(null);
  const confirmInput = useSignal('');
  const cascade = useSignal(false);
  const doneText = useSignal('');

  useEffect(() => {
    if (!isOpen || !versionId) return;
    let cancelled = false;
    step.value = 'analyze';
    info.value = null;
    confirmInput.value = '';
    cascade.value = false;
    doneText.value = '';
    Sound.ui('click');

    (async () => {
      try {
        const res = await api('GET', `/versions/${encodeURIComponent(versionId)}/info`);
        if (cancelled) return;
        if (res.error) {
          closeDeleteWizard();
          showError(res.error);
          return;
        }
        info.value = res as VersionInfo;
        step.value = 'summary';
        Sound.ui('bright');
      } catch (err: unknown) {
        if (cancelled) return;
        closeDeleteWizard();
        showError(`Failed to analyze version: ${errMessage(err)}`);
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [isOpen, versionId]);

  if (!isOpen || !versionId) return null;

  const targetId = versionId;

  const executeDelete = async () => {
    step.value = 'progress';
    Sound.ui('click');

    try {
      const res = await api('DELETE', `/versions/${encodeURIComponent(versionId)}`, {
        cascade_dependents: cascade.value,
      });
      if (res.error) {
        closeDeleteWizard();
        showError(res.error);
        return;
      }

      const deleted: string[] = res.deleted || [versionId];
      doneText.value = deleted.length === 1
        ? `${deleted[0]} has been removed.`
        : `Removed ${deleted.length} versions: ${deleted.join(', ')}`;
      step.value = 'done';
      Sound.ui('affirm');

      try {
        const versionsRes = await api('GET', '/versions');
        if (versionsRes.error) throw new Error(versionsRes.error);
        const nextVersions = versionsRes.versions || [];
        versions.value = nextVersions;

        if (catalog.value) {
          const installed = new Set<string>(nextVersions.filter((version: { launchable: boolean }) => version.launchable).map((version: { id: string }) => version.id));
          catalog.value = {
            ...catalog.value,
            versions: catalog.value.versions.map((version) => ({
              ...version,
              installed: installed.has(version.id),
            })),
          };
        }
      } catch (err: unknown) {
        showError(`Deleted ${versionId}, but failed to refresh versions: ${errMessage(err)}`);
      }
    } catch (err: unknown) {
      closeDeleteWizard();
      showError(`Delete failed: ${errMessage(err)}`);
    }
  };

  const handleOverlayClick = (e: MouseEvent) => {
    if (e.target === e.currentTarget) closeDeleteWizard();
  };

  const currentInfo = info.value;
  const dependents = currentInfo?.dependents || [];
  const worlds = currentInfo?.worlds || [];
  const sharedData = currentInfo?.shared_data || [];

  return (
    <div class="modal-overlay" id="delete-modal" onClick={(e: MouseEvent) => handleOverlayClick(e)}>
      <div class="modal delete-modal-size">
        <div class="modal-header">
          <span class="modal-title" id="delete-modal-title">Delete {targetId}</span>
          <button class="icon-btn modal-close" id="delete-close" data-action="close" aria-label="Close delete dialog" onClick={() => closeDeleteWizard()}>
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" /></svg>
          </button>
        </div>
        <div class="delete-wizard" id="delete-wizard">
          {step.value === 'analyze' && (
            <div class="delete-step" id="delete-step-analyze">
              <div class="delete-loading">
                <div class="spinner" />
                <span>Analyzing version data...</span>
              </div>
            </div>
          )}

          {step.value === 'summary' && currentInfo && (
            <div class="delete-step" id="delete-step-summary">
              <div class="delete-version-header">
                <span class="delete-version-name" id="delete-version-name">{targetId}</span>
                <span class="delete-version-size" id="delete-version-size">{formatBytes(currentInfo.folder_size)}</span>
              </div>

              {dependents.length > 0 && (
                <div class="delete-card delete-card-warn" id="delete-dependents-card">
                  <div class="delete-card-icon">
                    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3Z" /><line x1="12" y1="9" x2="12" y2="13" /><line x1="12" y1="17" x2="12.01" y2="17" /></svg>
                  </div>
                  <div class="delete-card-body">
                    <strong class="delete-card-title">Dependent Versions</strong>
                    <p class="delete-card-text">These modded versions rely on <strong id="delete-dep-parent">{targetId}</strong> as their base. Deleting it will make them unlaunchable.</p>
                    <div class="delete-dep-list" id="delete-dep-list">
                      {dependents.map((dependent) => <span key={dependent} class="delete-dep-tag">{dependent}</span>)}
                    </div>
                    <label class="delete-checkbox">
                      <input type="checkbox" id="delete-cascade-check" checked={cascade.value} onChange={(e) => { cascade.value = (e.currentTarget as HTMLInputElement).checked; }} />
                      <span>Also delete dependent versions</span>
                    </label>
                  </div>
                </div>
              )}

              {worlds.length > 0 && (
                <div class="delete-card delete-card-info" id="delete-worlds-card">
                  <div class="delete-card-icon">
                    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="10" /><path d="M2 12h20" /><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z" /></svg>
                  </div>
                  <div class="delete-card-body">
                    <strong class="delete-card-title">Your Worlds Are Safe</strong>
                    <p class="delete-card-text">Worlds are stored in the shared <code>saves/</code> folder and won't be deleted. You have <strong id="delete-world-count">{worlds.length}</strong> world(s):</p>
                    <div class="delete-world-list" id="delete-world-list">
                      {worlds.slice(0, 12).map((world) => (
                        <span key={`${world.name}:${world.size}`} class="delete-world-tag">
                          {world.name} <span class="delete-world-tag-size">{formatBytes(world.size)}</span>
                        </span>
                      ))}
                      {worlds.length > 12 && (
                        <span class="delete-world-tag">+{worlds.length - 12} more</span>
                      )}
                    </div>
                  </div>
                </div>
              )}

              {sharedData.length > 0 && (
                <div class="delete-card delete-card-info" id="delete-shared-card">
                  <div class="delete-card-icon">
                    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z" /></svg>
                  </div>
                  <div class="delete-card-body">
                    <strong class="delete-card-title">Shared Data Intact</strong>
                    <p class="delete-card-text" id="delete-shared-text">Your mods, resource packs, and shader packs are shared across all versions and won't be affected.</p>
                    <div class="delete-shared-list" id="delete-shared-list">
                      {sharedData.map((item) => (
                        <span key={`${item.name}:${item.count}`} class="delete-shared-tag">
                          {item.name} <span class="delete-shared-tag-count">{item.count} items</span>
                        </span>
                      ))}
                    </div>
                  </div>
                </div>
              )}

              <div class="delete-card delete-card-delete">
                <div class="delete-card-icon">
                  <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><polyline points="3 6 5 6 21 6" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" /></svg>
                </div>
                <div class="delete-card-body">
                  <strong class="delete-card-title">Will Be Deleted</strong>
                  <p class="delete-card-text">The version folder <code id="delete-folder-path">versions/{targetId}/</code> containing the version JSON, JAR, and any extracted natives.</p>
                </div>
              </div>

              <div class="delete-confirm-area">
                <p class="delete-confirm-hint">Type <strong id="delete-confirm-target">{targetId}</strong> to confirm</p>
                <input
                  type="text"
                  id="delete-confirm-input"
                  class="setting-input delete-confirm-input"
                  placeholder="Type version name..."
                  spellcheck={false}
                  autocomplete="off"
                  value={confirmInput.value}
                  onInput={(e) => { confirmInput.value = (e.currentTarget as HTMLInputElement).value; }}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' && confirmInput.value === targetId) {
                      e.preventDefault();
                      executeDelete();
                    }
                  }}
                />
                <div class="delete-actions">
                  <button class="btn-secondary" id="delete-cancel" onClick={() => closeDeleteWizard()}>Cancel</button>
                  <button class="btn-danger delete-btn" id="delete-confirm-btn" disabled={confirmInput.value !== targetId} onClick={() => executeDelete()}>
                    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><polyline points="3 6 5 6 21 6" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" /></svg>
                    Delete Version
                  </button>
                </div>
              </div>
            </div>
          )}

          {step.value === 'progress' && (
            <div class="delete-step" id="delete-step-progress">
              <div class="delete-loading">
                <div class="spinner" />
                <span id="delete-progress-text">{cascade.value ? 'Deleting version and dependents...' : 'Deleting version...'}</span>
              </div>
            </div>
          )}

          {step.value === 'done' && (
            <div class="delete-step" id="delete-step-done">
              <div class="delete-done">
                <div class="delete-done-icon">
                  <svg width="32" height="32" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><polyline points="20 6 9 17 4 12" /></svg>
                </div>
                <strong class="delete-done-title">Version Deleted</strong>
                <p class="delete-done-text" id="delete-done-text">{doneText.value}</p>
                <button class="btn-primary" id="delete-done-close" onClick={() => closeDeleteWizard()}>Done</button>
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

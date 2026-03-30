import { state } from './state.js';
import { api } from './api.js';
import { Sound } from './sound.js';
import { esc, showError, formatBytes } from './utils.js';
import { renderInstanceList } from './sidebar.js';
import { renderSelectedInstance } from './instance.js';

let deleteTarget = null;
let deleteInfo = null;

export function openDeleteWizard(version) {
  // Prevent deleting a running version
  const runningWithVersion = Object.values(state.runningSessions).some(s => s.versionId === version.id);
  if (runningWithVersion) {
    showError(`Cannot delete ${version.id} while it's running. Stop the game first.`);
    return;
  }

  deleteTarget = version;
  deleteInfo = null;
  const modal = document.getElementById('delete-modal');
  if (!modal) return;

  // Reset all steps
  document.getElementById('delete-step-analyze')?.classList.remove('hidden');
  document.getElementById('delete-step-summary')?.classList.add('hidden');
  document.getElementById('delete-step-progress')?.classList.add('hidden');
  document.getElementById('delete-step-done')?.classList.add('hidden');

  const titleEl = document.getElementById('delete-modal-title');
  if (titleEl) titleEl.textContent = `Delete ${version.id}`;

  modal.classList.remove('hidden');
  Sound.ui('click');

  // Fetch version info
  fetchDeleteInfo(version.id);
}

export function closeDeleteWizard() {
  const modal = document.getElementById('delete-modal');
  if (modal) modal.classList.add('hidden');
  deleteTarget = null;
  deleteInfo = null;
  const input = document.getElementById('delete-confirm-input');
  if (input) input.value = '';
}

async function fetchDeleteInfo(versionId) {
  try {
    const info = await api('GET', `/versions/${encodeURIComponent(versionId)}/info`);
    if (info.error) {
      closeDeleteWizard();
      showError(info.error);
      return;
    }
    deleteInfo = info;
    renderDeleteSummary();
  } catch (err) {
    closeDeleteWizard();
    showError('Failed to analyze version: ' + err.message);
  }
}

function renderDeleteSummary() {
  if (!deleteInfo || !deleteTarget) return;

  document.getElementById('delete-step-analyze')?.classList.add('hidden');
  document.getElementById('delete-step-summary')?.classList.remove('hidden');

  const nameEl = document.getElementById('delete-version-name');
  if (nameEl) nameEl.textContent = deleteTarget.id;

  const sizeEl = document.getElementById('delete-version-size');
  if (sizeEl) sizeEl.textContent = formatBytes(deleteInfo.folder_size);

  // Dependents
  const deps = deleteInfo.dependents || [];
  const depCard = document.getElementById('delete-dependents-card');
  if (depCard) {
    depCard.classList.toggle('hidden', deps.length === 0);
    const depParent = document.getElementById('delete-dep-parent');
    if (depParent) depParent.textContent = deleteTarget.id;
    const depList = document.getElementById('delete-dep-list');
    if (depList) {
      depList.innerHTML = deps.map(d => `<span class="delete-dep-tag">${esc(d)}</span>`).join('');
    }
  }
  // Reset cascade checkbox
  const cascadeCheck = document.getElementById('delete-cascade-check');
  if (cascadeCheck) cascadeCheck.checked = false;

  // Worlds
  const worlds = deleteInfo.worlds || [];
  const worldCard = document.getElementById('delete-worlds-card');
  if (worldCard) {
    worldCard.classList.toggle('hidden', worlds.length === 0);
    const countEl = document.getElementById('delete-world-count');
    if (countEl) countEl.textContent = worlds.length;
    const worldList = document.getElementById('delete-world-list');
    if (worldList) {
      worldList.innerHTML = worlds.slice(0, 12).map(w =>
        `<span class="delete-world-tag">${esc(w.name)} <span class="delete-world-tag-size">${formatBytes(w.size)}</span></span>`
      ).join('') + (worlds.length > 12 ? `<span class="delete-world-tag">+${worlds.length - 12} more</span>` : '');
    }
  }

  // Shared data
  const shared = deleteInfo.shared_data || [];
  const sharedCard = document.getElementById('delete-shared-card');
  if (sharedCard) {
    sharedCard.classList.toggle('hidden', shared.length === 0);
    const sharedList = document.getElementById('delete-shared-list');
    if (sharedList) {
      sharedList.innerHTML = shared.map(s =>
        `<span class="delete-shared-tag">${esc(s.name)} <span class="delete-shared-tag-count">${s.count} items</span></span>`
      ).join('');
    }
  }

  // Folder path
  const folderEl = document.getElementById('delete-folder-path');
  if (folderEl) folderEl.textContent = `versions/${deleteTarget.id}/`;

  // Confirm target
  const confirmTarget = document.getElementById('delete-confirm-target');
  if (confirmTarget) confirmTarget.textContent = deleteTarget.id;

  // Reset confirm input and button
  const input = document.getElementById('delete-confirm-input');
  if (input) { input.value = ''; input.focus(); }
  const btn = document.getElementById('delete-confirm-btn');
  if (btn) btn.disabled = true;

  Sound.ui('bright');
}

export function bindDeleteWizard() {
  // Confirm input validation
  document.getElementById('delete-confirm-input')?.addEventListener('input', (e) => {
    const btn = document.getElementById('delete-confirm-btn');
    if (!btn || !deleteTarget) return;
    btn.disabled = e.target.value !== deleteTarget.id;
  });

  // Enter key in confirm input
  document.getElementById('delete-confirm-input')?.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      const btn = document.getElementById('delete-confirm-btn');
      if (btn && !btn.disabled) executeDelete();
    }
  });

  // Delete button
  document.getElementById('delete-confirm-btn')?.addEventListener('click', executeDelete);

  // Cancel
  document.getElementById('delete-cancel')?.addEventListener('click', closeDeleteWizard);
  document.getElementById('delete-close')?.addEventListener('click', closeDeleteWizard);
  document.getElementById('delete-done-close')?.addEventListener('click', closeDeleteWizard);

  // Overlay click to close
  document.getElementById('delete-modal')?.addEventListener('click', (e) => {
    if (e.target.id === 'delete-modal') closeDeleteWizard();
  });
}

async function executeDelete() {
  if (!deleteTarget) return;

  const cascade = document.getElementById('delete-cascade-check')?.checked || false;
  const versionId = deleteTarget.id;

  // Show progress
  document.getElementById('delete-step-summary')?.classList.add('hidden');
  document.getElementById('delete-step-progress')?.classList.remove('hidden');

  const progressText = document.getElementById('delete-progress-text');
  if (progressText) progressText.textContent = cascade ? 'Deleting version and dependents...' : 'Deleting version...';

  Sound.ui('click');

  try {
    const res = await api('DELETE', `/versions/${encodeURIComponent(versionId)}`, { cascade_dependents: cascade });
    if (res.error) {
      closeDeleteWizard();
      showError(res.error);
      return;
    }

    // Show done
    document.getElementById('delete-step-progress')?.classList.add('hidden');
    document.getElementById('delete-step-done')?.classList.remove('hidden');

    const deleted = res.deleted || [versionId];
    const doneText = document.getElementById('delete-done-text');
    if (doneText) {
      if (deleted.length === 1) {
        doneText.textContent = `${deleted[0]} has been removed.`;
      } else {
        doneText.textContent = `Removed ${deleted.length} versions: ${deleted.join(', ')}`;
      }
    }

    Sound.ui('affirm');

    // Refresh version list
    try {
      const versionsRes = await api('GET', '/versions');
      state.versions = versionsRes.versions || [];
      // Refresh instance state after version deletion
      renderInstanceList();
      if (state.selectedInstance) renderSelectedInstance();
    } catch {}
  } catch (err) {
    closeDeleteWizard();
    showError('Delete failed: ' + err.message);
  }
}

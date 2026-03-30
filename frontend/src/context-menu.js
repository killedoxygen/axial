import { state, dom } from './state.js';
import { api } from './api.js';
import { Sound } from './sound.js';
import { showError } from './utils.js';
import { renderInstanceList } from './sidebar.js';
import { renderSelectedInstance } from './instance.js';
import { openDeleteWizard } from './delete-wizard.js';
import { showConfirm, showPrompt } from './dialogs.js';

let ctxMenuVersion = null;

export function showInstanceContextMenu(e, inst) {
  e.preventDefault();
  ctxMenuVersion = { id: inst.id, _instance: inst }; // reuse ctxMenuVersion slot
  const menu = document.getElementById('ctx-menu');
  if (!menu) return;
  menu.classList.remove('hidden');
  const mw = menu.offsetWidth || 180;
  const mh = menu.offsetHeight || 120;
  let x = e.clientX;
  let y = e.clientY;
  if (x + mw > window.innerWidth - 8) x = window.innerWidth - mw - 8;
  if (y + mh > window.innerHeight - 8) y = window.innerHeight - mh - 8;
  if (x < 4) x = 4;
  if (y < 4) y = 4;
  menu.style.left = x + 'px';
  menu.style.top = y + 'px';
  Sound.ui('soft');
}

export function showContextMenu(e, version) {
  e.preventDefault();
  ctxMenuVersion = version;
  const menu = document.getElementById('ctx-menu');
  if (!menu) return;
  menu.classList.remove('hidden');

  // Position: appear at cursor, but clamp to viewport
  const mw = menu.offsetWidth || 180;
  const mh = menu.offsetHeight || 120;
  let x = e.clientX;
  let y = e.clientY;
  if (x + mw > window.innerWidth - 8) x = window.innerWidth - mw - 8;
  if (y + mh > window.innerHeight - 8) y = window.innerHeight - mh - 8;
  if (x < 4) x = 4;
  if (y < 4) y = 4;
  menu.style.left = x + 'px';
  menu.style.top = y + 'px';

  Sound.ui('soft');
}

export function hideContextMenu() {
  const menu = document.getElementById('ctx-menu');
  if (menu) menu.classList.add('hidden');
  ctxMenuVersion = null;
}

export function bindContextMenu() {
  document.addEventListener('click', (e) => {
    const menu = document.getElementById('ctx-menu');
    if (menu && !menu.contains(e.target)) hideContextMenu();
  });
  document.addEventListener('contextmenu', (e) => {
    const menu = document.getElementById('ctx-menu');
    if (menu && !menu.classList.contains('hidden') && !menu.contains(e.target)) hideContextMenu();
  });

  document.getElementById('ctx-open-folder')?.addEventListener('click', () => {
    if (!ctxMenuVersion) return;
    const inst = ctxMenuVersion._instance;
    if (inst) {
      api('POST', `/instances/${encodeURIComponent(inst.id)}/open-folder`).catch(() => {});
    } else {
      api('POST', `/versions/${encodeURIComponent(ctxMenuVersion.id)}/open-folder`).catch(() => {});
    }
    hideContextMenu();
    Sound.ui('click');
  });

  document.getElementById('ctx-copy-id')?.addEventListener('click', () => {
    if (!ctxMenuVersion) return;
    const inst = ctxMenuVersion._instance;
    const text = inst ? inst.version_id : ctxMenuVersion.id;
    navigator.clipboard?.writeText(text).then(() => {
      Sound.ui('affirm');
    }).catch(() => {});
    hideContextMenu();
  });

  document.getElementById('ctx-rename')?.addEventListener('click', async () => {
    if (!ctxMenuVersion?._instance) return;
    const inst = ctxMenuVersion._instance;
    hideContextMenu();
    const newName = await showPrompt('Rename instance:', inst.name, {
      validate(val) {
        if (val === inst.name) return null;
        if (state.instances.some(i => i.id !== inst.id && i.name === val)) return 'An instance with this name already exists';
        return null;
      },
    });
    if (!newName || newName === inst.name) return;
    api('PUT', `/instances/${encodeURIComponent(inst.id)}`, { name: newName }).then(() => {
      inst.name = newName;
      renderInstanceList();
      if (state.selectedInstance?.id === inst.id) renderSelectedInstance();
    });
    Sound.ui('affirm');
  });

  document.getElementById('ctx-delete')?.addEventListener('click', async () => {
    if (!ctxMenuVersion) return;
    const inst = ctxMenuVersion._instance;
    if (inst) {
      hideContextMenu();
      const ok = await showConfirm(`Delete instance "${inst.name}"?\nThis will remove saves, mods, and all instance data.`, { confirmText: 'Delete', destructive: true });
      if (!ok) return;
      api('DELETE', `/instances/${encodeURIComponent(inst.id)}`).then(res => {
        if (res.error) { showError(res.error); return; }
        state.instances = state.instances.filter(i => i.id !== inst.id);
        if (state.selectedInstance?.id === inst.id) {
          state.selectedInstance = null;
          dom.versionDetail?.classList.add('hidden');
          dom.emptyState?.classList.remove('hidden');
        }
        renderInstanceList();
        Sound.ui('affirm');
      });
    } else {
      const version = ctxMenuVersion;
      hideContextMenu();
      openDeleteWizard(version);
    }
  });
}

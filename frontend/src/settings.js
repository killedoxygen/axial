import { state, dom, local, saveLocalState } from './state.js';
import { api } from './api.js';
import { Sound } from './sound.js';
import { esc, setPage } from './utils.js';
import { positionFieldMarker } from './theme.js';
import { renderShortcutEditor } from './shortcuts.js';
import { renderSelectedInstance } from './instance.js';
import { toast } from './toast.js';

let restoreFocusEl = null;

export function openSettings() {
  restoreFocusEl = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  syncSettingsForm();
  setPage('settings');
  if (dom.settingsContent) dom.settingsContent.scrollTop = 0;
  syncSettingsSectionNav();
  loadJavaRuntimes();
  setTimeout(() => dom.settingsNav?.querySelector('.settings-nav-btn.active')?.focus(), 0);
}

export function closeSettings() {
  setPage('launcher');
  renderSelectedInstance();
  restoreFocusEl?.focus?.();
}

function syncSettingsForm() {
  if (state.config) {
    if (dom.settingJavaPath) dom.settingJavaPath.value = state.config.java_path_override || '';
    if (dom.settingWidth) dom.settingWidth.value = state.config.window_width || '';
    if (dom.settingHeight) dom.settingHeight.value = state.config.window_height || '';
    if (dom.jvmPresetGroup) {
      const preset = state.config.jvm_preset || '';
      const radio = dom.jvmPresetGroup.querySelector(`input[value="${preset}"]`);
      if (radio) radio.checked = true;
    }
  }
  dom.themePicker?.querySelectorAll('.theme-swatch').forEach(s => s.classList.toggle('active', s.dataset.theme === local.theme));
  positionFieldMarker(dom.colorField, dom.colorFieldMarker, local.customHue, local.customVibrancy);
  if (dom.soundsToggle) dom.soundsToggle.checked = Sound.enabled;
  renderShortcutEditor();
}

export async function saveSettings() {
  const updates = {};
  const jp = dom.settingJavaPath?.value.trim() || '';
  if (jp !== (state.config?.java_path_override || '')) updates.java_path_override = jp;

  const presetRadio = dom.jvmPresetGroup?.querySelector('input[name="jvm-preset"]:checked');
  const preset = presetRadio?.value || '';
  if (preset !== (state.config?.jvm_preset || '')) updates.jvm_preset = preset;

  const widthRaw = dom.settingWidth?.value.trim() || '';
  const heightRaw = dom.settingHeight?.value.trim() || '';
  const w = widthRaw === '' ? 0 : parseInt(widthRaw, 10) || 0;
  const h = heightRaw === '' ? 0 : parseInt(heightRaw, 10) || 0;
  if (w !== (state.config?.window_width || 0)) updates.window_width = w;
  if (h !== (state.config?.window_height || 0)) updates.window_height = h;

  if (Object.keys(updates).length) {
    try {
      const r = await api('PUT', '/config', updates);
      if (!r.error) { state.config = r; toast('Settings saved'); }
      else toast(r.error, 'error');
    } catch (err) {
      toast('Failed to save settings', 'error');
    }
  } else {
    toast('No changes to save');
  }
  Sound.ui('affirm');
}

export function syncSettingsSectionNav() {
  if (!dom.settingsContent || !dom.settingsNav) return;
  const sections = [...dom.settingsContent.querySelectorAll('.settings-section-card')].filter(section => !section.classList.contains('hidden'));
  if (!sections.length) return;
  const contentTop = dom.settingsContent.getBoundingClientRect().top;
  let activeId = sections[0].id;
  let best = Number.POSITIVE_INFINITY;
  sections.forEach(section => {
    const distance = Math.abs(section.getBoundingClientRect().top - contentTop - 18);
    if (distance < best) {
      best = distance;
      activeId = section.id;
    }
  });
  dom.settingsNav.querySelectorAll('.settings-nav-btn').forEach(btn => btn.classList.toggle('active', btn.dataset.settingsTarget === activeId));
}

async function loadJavaRuntimes() {
  if (!dom.javaRuntimes) return;
  try {
    const res = await api('GET', '/java');
    const rt = res.runtimes || [];
    dom.javaRuntimes.innerHTML = rt.length === 0 ? '<span class="setting-hint">No runtimes detected</span>' :
      rt.map(r => `<div class="java-runtime-item"><span class="java-runtime-component">${esc(r.Component || r.component)}</span><span class="java-runtime-source">${esc(r.Source || r.source)}</span></div>`).join('');
  } catch {
    dom.javaRuntimes.innerHTML = '<span class="setting-hint">Failed to load</span>';
  }
}

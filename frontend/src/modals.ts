import { local } from './state';
import { api } from './api';
import { byId } from './dom';
import { Sound } from './sound';
import { Music } from './music';
import { fmtMem, getMemoryRecommendation, errMessage, showError } from './utils';
import { positionFieldMarker } from './theme';
import { showNewInstanceModal } from './components/NewInstanceModal';
import { browseDirectory } from './native';
import { config, systemInfo } from './store';

export async function openNewInstanceFlow(): Promise<void> {
  showNewInstanceModal.value = true;
}

export function closeNewInstanceFlow(): void {
  if (!showNewInstanceModal.value) return;
  showNewInstanceModal.value = false;
  Sound.ui('soft');
}

export function showSetup(): Promise<void> {
  return new Promise((resolve: () => void, reject: (reason?: unknown) => void) => {
    const overlay = byId<HTMLElement>('setup-overlay');
    const setupUseBtn = byId<HTMLButtonElement>('setup-use-btn');
    const setupBrowseBtn = byId<HTMLButtonElement>('setup-browse-btn');
    const setupInitBtn = byId<HTMLButtonElement>('setup-init-btn');
    if (!overlay || !setupUseBtn || !setupBrowseBtn || !setupInitBtn) {
      overlay?.classList.add('hidden');
      reject(new Error('setup UI is missing required elements'));
      return;
    }
    const overlayEl = overlay;
    const setupUseBtnEl = setupUseBtn;
    const setupBrowseBtnEl = setupBrowseBtn;
    const setupInitBtnEl = setupInitBtn;
    overlayEl.classList.remove('hidden');

    void (async () => {
      try {
        const defaults: any = await api('GET', '/setup/defaults');
        const setupNewPath = byId<HTMLInputElement>('setup-new-path');
        if (setupNewPath) setupNewPath.value = defaults.default_path || '';
      } catch (err: unknown) {}
    })();

    function hideSetup(): void {
      setupUseBtnEl.onclick = null;
      setupBrowseBtnEl.onclick = null;
      setupInitBtnEl.onclick = null;
      overlayEl.classList.add('hidden');
      resolve();
    }

    function showPathError(msg: string): void {
      const setupPathError = byId<HTMLElement>('setup-path-error');
      if (setupPathError) {
        setupPathError.textContent = msg;
        setupPathError.classList.remove('hidden');
      }
    }
    function clearPathError(): void {
      byId<HTMLElement>('setup-path-error')?.classList.add('hidden');
    }

    // "Use this path" flow
    setupUseBtnEl.onclick = async () => {
      clearPathError();
      const path: string | undefined = byId<HTMLInputElement>('setup-path-input')?.value.trim();
      if (!path) { showPathError('Please enter a path'); return; }
      setupUseBtnEl.disabled = true;
      setupUseBtnEl.textContent = 'Checking...';
      try {
        const res: any = await api('POST', '/setup/set-dir', { path });
        if (res.error) { showPathError(res.error); return; }
        hideSetup();
      } catch (err: unknown) {
        showPathError(errMessage(err) || 'Failed to set directory');
      } finally {
        setupUseBtnEl.disabled = false;
        setupUseBtnEl.textContent = 'Use this path';
      }
    };

    // "Browse" button
    setupBrowseBtnEl.onclick = async () => {
      setupBrowseBtnEl.disabled = true;
      setupBrowseBtnEl.textContent = 'Opening...';
      try {
        const setupPathInput = byId<HTMLInputElement>('setup-path-input');
        const currentPath = setupPathInput?.value.trim() || '';
        const nativePath = await browseDirectory(currentPath);
        if (nativePath !== null) {
          if (nativePath) {
            if (setupPathInput) setupPathInput.value = nativePath;
            clearPathError();
          }
          return;
        }

        const res: any = await api('POST', '/setup/browse');
        if (res.path) {
          if (setupPathInput) setupPathInput.value = res.path;
          clearPathError();
        }
      } catch (err: unknown) {
        showPathError(errMessage(err) || 'Failed to browse for a folder');
      } finally {
        setupBrowseBtnEl.disabled = false;
        setupBrowseBtnEl.textContent = 'Browse';
      }
    };

    // "Create & Continue" flow
    setupInitBtnEl.onclick = async () => {
      const path: string | undefined = byId<HTMLInputElement>('setup-new-path')?.value.trim();
      if (!path) return;
      setupInitBtnEl.disabled = true;
      setupInitBtnEl.textContent = 'Creating...';
      try {
        const res: any = await api('POST', '/setup/init', { path });
        if (res.error) { showPathError(res.error); return; }
        hideSetup();
      } catch (err: unknown) {
        showPathError(errMessage(err) || 'Failed to create directory');
      } finally {
        setupInitBtnEl.disabled = false;
        setupInitBtnEl.textContent = 'Create & Continue';
      }
    };
  });
}

export function showOnboarding(): void {
  byId<HTMLElement>('onboarding')?.classList.remove('hidden');
  if (systemInfo.value?.total_memory_mb) {
    const gb: number = Math.floor(systemInfo.value.total_memory_mb / 1024);
    const onboardingRamInfo = byId<HTMLElement>('onboarding-ram-info');
    const onboardingMemorySlider = byId<HTMLInputElement>('onboarding-memory-slider');
    const onboardingMemoryValue = byId<HTMLElement>('onboarding-memory-value');
    const onboardingRec = byId<HTMLElement>('onboarding-rec');
    if (onboardingRamInfo) onboardingRamInfo.textContent = `Your system has ${gb} GB of RAM`;
    if (onboardingMemorySlider) {
      onboardingMemorySlider.max = String(gb);
      const { rec, text } = getMemoryRecommendation(gb);
      onboardingMemorySlider.value = String(rec);
      if (onboardingMemoryValue) onboardingMemoryValue.textContent = fmtMem(rec);
      if (onboardingRec) onboardingRec.textContent = text;
    }
  }
  positionFieldMarker(byId('ob-color-field'), byId('ob-color-field-marker'), local.customHue, local.customVibrancy);
}

let obStep: number = 1;
const OB_STEPS: number = 5;

export function getObStep(): number { return obStep; }

export function onboardingStep(n: number): void {
  obStep = Math.max(1, Math.min(OB_STEPS, n));
  const steps: Array<HTMLElement | null> = [
    byId('onboarding-step-1'), byId('onboarding-step-2'), byId('onboarding-step-3'),
    byId('onboarding-step-4'), byId('onboarding-step-5'),
  ];
  const dots: Array<HTMLElement | null> = [byId('dot-1'), byId('dot-2'), byId('dot-3'), byId('dot-4'), byId('dot-5')];
  steps.forEach((s: HTMLElement | null, i: number) => { if (s) s.classList.toggle('hidden', i !== obStep - 1); });
  dots.forEach((d: HTMLElement | null, i: number) => { if (d) d.classList.toggle('active', i === obStep - 1); });
  byId<HTMLElement>('onboarding-back')?.classList.toggle('hidden', obStep === 1);
  const onboardingNextBtn = byId<HTMLElement>('onboarding-next');
  if (onboardingNextBtn) onboardingNextBtn.textContent = obStep === OB_STEPS ? "Let's go" : 'Continue';
  if (obStep === 1) byId<HTMLInputElement>('onboarding-username')?.focus();
}

export function onboardingNext(): void {
  if (obStep < OB_STEPS) onboardingStep(obStep + 1);
  else finishOnboarding();
}

export function onboardingBack(): void {
  if (obStep > 1) onboardingStep(obStep - 1);
}

export async function finishOnboarding(): Promise<void> {
  const username: string = byId<HTMLInputElement>('onboarding-username')?.value.trim() || 'Player';
  const rawMemGB = parseFloat(byId<HTMLInputElement>('onboarding-memory-slider')?.value || '4');
  const sliderMax = parseFloat(byId<HTMLInputElement>('onboarding-memory-slider')?.max || '0');
  const maxMemGB = Number.isFinite(sliderMax) && sliderMax >= 1 ? sliderMax : 64;
  const memGB = Number.isFinite(rawMemGB) ? Math.min(maxMemGB, Math.max(1, rawMemGB)) : Math.min(4, maxMemGB);
  const musicEnabled: boolean = byId<HTMLElement>('ob-music-yes')?.classList.contains('active') ?? false;
  const usernameInput = byId<HTMLInputElement>('username-input');
  const memorySlider = byId<HTMLInputElement>('memory-slider');
  const memoryValue = byId<HTMLElement>('memory-value');
  try {
    const r: any = await api('PUT', '/config', { username, max_memory_mb: Math.round(memGB * 1024), music_enabled: musicEnabled, music_volume: 5 });
    if (r.error) throw new Error(r.error);
    config.value = r;
  } catch (err: unknown) {
    showError(`Failed to save onboarding settings: ${errMessage(err)}`);
    return;
  }
  try {
    const res: any = await api('POST', '/onboarding/complete');
    if (res?.error) throw new Error(res.error);
  } catch (err: unknown) {
    showError(`Failed to finish onboarding: ${errMessage(err)}`);
    return;
  }
  if (usernameInput) usernameInput.value = username;
  if (memorySlider) {
    memorySlider.value = String(memGB);
    if (memoryValue) memoryValue.textContent = fmtMem(memGB);
  }
  byId<HTMLElement>('onboarding')?.classList.add('hidden');
  Music.applyConfig({ music_enabled: musicEnabled, music_volume: 5 });
  if (musicEnabled) Music.play();
}

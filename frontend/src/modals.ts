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
    const setupAdvancedToggle = byId<HTMLButtonElement>('setup-advanced-toggle');
    const setupAdvanced = byId<HTMLElement>('setup-advanced');
    const setupManagedPath = byId<HTMLElement>('setup-managed-path');
    const setupProgressCopy = byId<HTMLElement>('setup-progress-copy');
    if (!overlay || !setupUseBtn || !setupBrowseBtn || !setupInitBtn || !setupAdvancedToggle || !setupAdvanced || !setupManagedPath || !setupProgressCopy) {
      overlay?.classList.add('hidden');
      reject(new Error('setup UI is missing required elements'));
      return;
    }
    const overlayEl = overlay;
    const setupUseBtnEl = setupUseBtn;
    const setupBrowseBtnEl = setupBrowseBtn;
    const setupInitBtnEl = setupInitBtn;
    const setupAdvancedToggleEl = setupAdvancedToggle;
    const setupAdvancedEl = setupAdvanced;
    const setupManagedPathEl = setupManagedPath;
    const setupProgressCopyEl = setupProgressCopy;
    let managedPath = '';
    let setupRunning = false;
    overlayEl.classList.remove('hidden');

    function hideSetup(): void {
      setupUseBtnEl.onclick = null;
      setupBrowseBtnEl.onclick = null;
      setupInitBtnEl.onclick = null;
      setupAdvancedToggleEl.onclick = null;
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

    async function runManagedSetup(isRetry = false): Promise<void> {
      if (setupRunning || !managedPath) return;
      setupRunning = true;
      clearPathError();
      setupInitBtnEl.disabled = true;
      setupInitBtnEl.textContent = isRetry ? 'Retrying setup...' : 'Setting up Croopor...';
      setupProgressCopyEl.textContent = isRetry
        ? 'Retrying managed library setup...'
        : 'Creating the Croopor library...';
      try {
        const res: any = await api('POST', '/setup/init', { path: managedPath });
        if (res.error) { showPathError(res.error); return; }
        hideSetup();
      } catch (err: unknown) {
        showPathError(errMessage(err) || 'Failed to set up the Croopor library');
      } finally {
        setupRunning = false;
        setupInitBtnEl.disabled = false;
        setupInitBtnEl.textContent = 'Retry setup';
        setupProgressCopyEl.textContent = 'Croopor could not finish setup. You can retry or use an existing library instead.';
      }
    }

    setupAdvancedToggleEl.onclick = () => {
      const open = setupAdvancedEl.classList.toggle('hidden');
      setupAdvancedToggleEl.textContent = open
        ? 'Use an existing Minecraft folder instead'
        : 'Hide existing-library option';
    };

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
        setupUseBtnEl.textContent = 'Use existing library';
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

    setupInitBtnEl.onclick = () => {
      void runManagedSetup(true);
    };

    void (async () => {
      try {
        const defaults: any = await api('GET', '/setup/defaults');
        managedPath = defaults.managed_default_path || '';
        setupManagedPathEl.textContent = managedPath || 'Could not determine a default library path.';
        const setupPathInput = byId<HTMLInputElement>('setup-path-input');
        if (setupPathInput && defaults.existing_default_path) {
          setupPathInput.value = defaults.existing_default_path;
        }
        if (!managedPath) {
          setupProgressCopyEl.textContent = 'Croopor could not determine a managed library path. Use an existing library or retry.';
          setupInitBtnEl.disabled = false;
          setupInitBtnEl.textContent = 'Retry setup';
          return;
        }
        void runManagedSetup(false);
      } catch (err: unknown) {
        setupProgressCopyEl.textContent = 'Croopor could not start setup automatically. Retry or use an existing library instead.';
        setupInitBtnEl.disabled = false;
        setupInitBtnEl.textContent = 'Retry setup';
      }
    })();
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

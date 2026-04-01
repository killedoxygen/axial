import type { JSX } from 'preact';
import {
  appVersion, bootstrapError, bootstrapState, collapsedLogSeverity, currentPage, instances, selectedInstance, updateInfo,
} from '../store';
import { localStateVersion } from '../state';
import { SettingsView } from './SettingsView';
import { InstanceList } from './InstanceList';
import { DetailPanel } from './DetailPanel';
import { NewInstanceModal } from './NewInstanceModal';
import { DeleteWizard } from './DeleteWizard';
import { ToastViewport } from './ToastViewport';
import { dismissAvailableUpdate, hasVisibleUpdate, openUpdateAction } from '../updater';

function displayVersion(version: string): string {
  return version.startsWith('v') ? version : `v${version}`;
}

function LogSeverityIndicator(): JSX.Element | null {
  const severity = collapsedLogSeverity.value;
  if (!severity) return null;

  const label = severity === 'error' ? 'Errors in game output' : severity === 'system' ? 'New system messages' : 'New game output';
  return (
    <span id="log-severity-icon" class={`log-severity-icon severity-${severity}`} role="img" aria-label={label} title={label}>
      {severity === 'error' ? (
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="M3 12a9 9 0 1 0 18 0a9 9 0 0 0 -18 0" />
          <path d="M12 8v4" />
          <path d="M12 16h.01" />
        </svg>
      ) : severity === 'system' ? (
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="M12 9v4" />
          <path d="M10.363 3.591l-8.106 13.534a1.914 1.914 0 0 0 1.636 2.871h16.214a1.914 1.914 0 0 0 1.636 -2.87l-8.106 -13.536a1.914 1.914 0 0 0 -3.274 0" />
          <path d="M12 16h.01" />
        </svg>
      ) : (
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="M3 12a9 9 0 1 0 18 0a9 9 0 0 0 -18 0" />
          <path d="M12 9h.01" />
          <path d="M11 12h1v4h1" />
        </svg>
      )}
    </span>
  );
}

function EmptyState(): JSX.Element {
  const boot = bootstrapState.value;
  const error = bootstrapError.value;
  const hasInstances = instances.value.length > 0;

  let title = 'Select a version';
  let subtitle = 'Choose a Minecraft version from the sidebar to launch';
  let showAdd = false;

  if (boot === 'loading') {
    title = 'Scanning instances...';
    subtitle = 'Loading local data and launcher status';
  } else if (boot === 'error') {
    title = 'Failed to connect';
    subtitle = error || 'The launcher could not load its initial state';
  } else if (!hasInstances) {
    title = 'No instances yet';
    subtitle = 'Create a Minecraft instance to get started';
    showAdd = true;
  }

  return (
    <div class={`empty-state${selectedInstance.value ? ' hidden' : ''}`} id="empty-state">
      <div class="empty-icon">
        <svg width="48" height="48" viewBox="0 0 48 48" fill="none">
          <rect x="4" y="4" width="16" height="16" rx="2" fill="var(--surface-2)" stroke="var(--border)" stroke-width="1.5" />
          <rect x="28" y="4" width="16" height="16" rx="2" fill="var(--surface-2)" stroke="var(--border)" stroke-width="1.5" />
          <rect x="4" y="28" width="16" height="16" rx="2" fill="var(--surface-2)" stroke="var(--border)" stroke-width="1.5" />
          <rect x="28" y="28" width="16" height="16" rx="2" fill="var(--accent)" opacity="0.2" stroke="var(--accent)" stroke-width="1.5" stroke-opacity="0.4" />
        </svg>
      </div>
      <p class="empty-title" id="empty-title">{title}</p>
      <p class="empty-sub" id="empty-sub">{subtitle}</p>
      <button class={`btn-primary${showAdd ? '' : ' hidden'}`} id="empty-add-btn" style="margin-top: 12px;" data-action="newInstance">New Instance</button>
    </div>
  );
}

export function App(): JSX.Element {
  const page = currentPage.value;
  const info = updateInfo.value;
  localStateVersion.value;
  const visibleUpdate = hasVisibleUpdate();

  return (
    <>
      <header class="topbar">
        <div class="topbar-left">
          <div class="logo">
            <img class="logo-img" src="logo.svg" alt="Croopor" width="26" height="26" />
            <span class="logo-text">Croopor</span>
            <span class="logo-version">{displayVersion(appVersion.value)}</span>
            {visibleUpdate && info && (
              <div class="topbar-update">
                <button type="button" class="update-chip" onClick={() => { void openUpdateAction(); }}>
                  update {displayVersion(info.latest_version)}
                </button>
                <button
                  type="button"
                  class="update-chip-dismiss"
                  aria-label={`Hide update ${displayVersion(info.latest_version)}`}
                  onClick={() => dismissAvailableUpdate()}
                >
                  hide
                </button>
              </div>
            )}
          </div>
        </div>
        <div class="topbar-center">
          <div class="topbar-field">
            <label class="field-label">Player</label>
            <input type="text" id="username-input" class="field-input" defaultValue="Player" spellcheck={false} autocomplete="off" />
          </div>
          <div class="topbar-divider" />
          <div class="topbar-field">
            <label class="field-label">Memory</label>
            <div class="memory-control">
              <input type="range" id="memory-slider" min="1" max="16" step="0.5" defaultValue="4" />
              <span id="memory-value" class="memory-value">4 GB</span>
              <span id="memory-rec" class="memory-rec" aria-live="polite" />
            </div>
          </div>
        </div>
        <div class="topbar-right">
          <div class="music-eq hidden" id="music-eq" title="Next track"><span /><span /><span /><span /><span /></div>
          <button class="icon-btn" id="music-btn" title="Music off" aria-label="Toggle background music">
            <svg class="music-icon-on" width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" style="display:none">
              <path d="M9 18V5l12-2v13" /><circle cx="6" cy="18" r="3" /><circle cx="18" cy="16" r="3" />
            </svg>
            <svg class="music-icon-off" width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
              <path d="M9 18V5l12-2v13" opacity="0.4" /><circle cx="6" cy="18" r="3" opacity="0.4" /><circle cx="18" cy="16" r="3" opacity="0.4" />
              <line x1="3" y1="3" x2="21" y2="21" stroke-width="2.5" />
            </svg>
          </button>
          <button class={`icon-btn${page === 'settings' ? ' active' : ''}`} id="settings-btn" title="Settings" aria-label="Open settings" data-action="settings">
            <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
              <circle cx="12" cy="12" r="3" />
              <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
            </svg>
          </button>
        </div>
      </header>

      <div class="main">
        <aside class="sidebar">
          <div class={`sidebar-launcher-panel${page === 'launcher' ? '' : ' hidden'}`} id="sidebar-launcher-panel">
            <div class="sidebar-header">
              <div class="search-box" data-action="search">
                <svg class="search-icon" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><circle cx="11" cy="11" r="8" /><path d="m21 21-4.3-4.3" /></svg>
                <input type="text" id="version-search" class="search-input" placeholder="Search instances..." spellcheck={false} autocomplete="off" aria-label="Search instances" />
              </div>
              <div class="filter-chips">
                <button class="chip active" data-filter="all">All</button>
                <button class="chip" data-filter="release">Release</button>
                <button class="chip" data-filter="snapshot">Snapshot</button>
                <button class="chip" data-filter="modded">Modded</button>
              </div>
            </div>
            <div class="version-list" id="version-list" aria-label="Installed versions">
              <InstanceList />
            </div>
            <button class="add-version-btn" id="add-version-btn" data-action="newInstance">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><line x1="12" y1="5" x2="12" y2="19" /><line x1="5" y1="12" x2="19" y2="12" /></svg>
              <span>New Instance</span>
            </button>
          </div>

          <div class={`sidebar-settings-panel${page === 'settings' ? '' : ' hidden'}`} id="sidebar-settings-panel">
            <div class="sidebar-settings-head">
              <h2 class="sidebar-settings-title">Settings</h2>
            </div>
            <nav class="settings-nav" id="settings-nav">
              <button class="settings-nav-btn active" data-settings-target="settings-section-appearance">Appearance</button>
              <button class="settings-nav-btn" data-settings-target="settings-section-launch">Launch</button>
              <button class="settings-nav-btn" data-settings-target="settings-section-java">Java</button>
              <button class="settings-nav-btn" data-settings-target="settings-section-shortcuts">Shortcuts</button>
              <button class="settings-nav-btn" data-settings-target="settings-section-advanced">Advanced</button>
            </nav>
            <div class="sidebar-settings-actions">
              <button class="btn-secondary" id="settings-cancel" data-action="close">Back</button>
              <button class="btn-primary" id="settings-save" data-action="save">Save</button>
            </div>
          </div>
        </aside>

        <main class="center-panel" id="center-panel">
          <div class="page-stack" id="page-stack">
            <section class={`launcher-view${page === 'launcher' ? '' : ' hidden'}`} id="launcher-view">
              <EmptyState />
              <div class={`version-detail${selectedInstance.value ? '' : ' hidden'}`} id="version-detail">
                <DetailPanel />
              </div>
            </section>

            <section class={`settings-view${page === 'settings' ? '' : ' hidden'}`} id="settings-view">
              <SettingsView />
            </section>
          </div>
        </main>
      </div>

      <div class="log-panel" id="log-panel">
        <div class="log-resize" id="log-resize" />
        <div class="log-toggle" id="log-toggle">
          <svg class="log-chevron" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><polyline points="18 15 12 9 6 15" /></svg>
          <span id="log-title">Game Output</span>
          <LogSeverityIndicator />
          <select class="log-filter hidden" id="log-filter"><option value="all">All instances</option></select>
          <span class="log-count" id="log-count">0 lines</span>
        </div>
        <div class="log-content" id="log-content">
          <div class="log-lines" id="log-lines" />
        </div>
      </div>

      <div class="ctx-menu hidden" id="ctx-menu" role="menu">
        <button class="ctx-item" id="ctx-rename" role="menuitem">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7" /><path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z" /></svg>
          <span>Rename</span>
        </button>
        <button class="ctx-item" id="ctx-open-folder" role="menuitem">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" /></svg>
          <span>Open Folder</span>
        </button>
        <button class="ctx-item" id="ctx-copy-id" role="menuitem">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><rect x="9" y="9" width="13" height="13" rx="2" /><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" /></svg>
          <span>Copy Version ID</span>
        </button>
        <div class="ctx-divider" />
        <button class="ctx-item ctx-item-danger" id="ctx-delete" role="menuitem">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><polyline points="3 6 5 6 21 6" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" /><line x1="10" y1="11" x2="10" y2="17" /><line x1="14" y1="11" x2="14" y2="17" /></svg>
          <span>Delete Instance</span>
        </button>
      </div>

      <div class="setup-overlay hidden" id="setup-overlay">
        <div class="setup-card">
          <img src="logo.svg" alt="Croopor" width="64" height="64" class="onboarding-logo" />
          <h1 class="onboarding-title">Minecraft Not Found</h1>
          <p class="onboarding-sub">Croopor couldn't detect a Minecraft installation on this computer.</p>

          <div class="setup-options">
            <div class="setup-option">
              <h3 class="setup-option-title">I have Minecraft installed</h3>
              <p class="setup-option-sub">Point Croopor to your .minecraft folder</p>
              <div class="setup-path-row">
                <input type="text" id="setup-path-input" class="setting-input setup-path-input" placeholder="C:\\Users\\...\\.minecraft" spellcheck={false} autocomplete="off" />
                <button class="btn-secondary setup-browse-btn" id="setup-browse-btn">Browse</button>
              </div>
              <span class="setup-error hidden" id="setup-path-error" />
              <button class="btn-primary setup-action-btn" id="setup-use-btn">Use this path</button>
            </div>

            <div class="setup-divider"><span>or</span></div>

            <div class="setup-option">
              <h3 class="setup-option-title">Set up Minecraft for me</h3>
              <p class="setup-option-sub">Create a fresh installation and download any version you want</p>
              <input type="text" id="setup-new-path" class="setting-input setup-path-input" spellcheck={false} autocomplete="off" />
              <button class="btn-primary setup-action-btn" id="setup-init-btn">Create &amp; Continue</button>
            </div>
          </div>
        </div>
      </div>

      <div class="onboarding-overlay hidden" id="onboarding">
        <div class="onboarding-card">
          <div class="onboarding-body">
            <div class="onboarding-step" id="onboarding-step-1">
              <img src="logo.svg" alt="Croopor" width="64" height="64" class="onboarding-logo" />
              <h1 class="onboarding-title">Welcome to Croopor</h1>
              <p class="onboarding-sub">Your offline Minecraft launcher</p>
              <div class="onboarding-field">
                <label class="setting-label">Choose your player name</label>
                <input type="text" id="onboarding-username" class="setting-input onboarding-input" defaultValue="Player" spellcheck={false} autocomplete="off" autoFocus />
              </div>
            </div>

            <div class="onboarding-step hidden" id="onboarding-step-2">
              <h2 class="onboarding-title">Memory Allocation</h2>
              <p class="onboarding-sub" id="onboarding-ram-info">Detecting system memory...</p>
              <div class="onboarding-memory">
                <input type="range" id="onboarding-memory-slider" min="1" max="16" step="0.5" defaultValue="4" class="onboarding-slider" />
                <span id="onboarding-memory-value" class="memory-value">4 GB</span>
              </div>
              <p class="onboarding-rec" id="onboarding-rec" />
            </div>

            <div class="onboarding-step hidden" id="onboarding-step-3">
              <h2 class="onboarding-title">Pick your style</h2>
              <p class="onboarding-sub">Choose a preset or craft your own palette</p>
              <div class="ob-theme-presets" id="ob-theme-presets">
                <button class="ob-theme-btn active" data-ob-theme="obsidian">
                  <span class="ob-swatch" style="background:linear-gradient(135deg,#0c0e11 60%,#3dd68c)" />
                  <span>Obsidian</span>
                </button>
                <button class="ob-theme-btn" data-ob-theme="deepslate">
                  <span class="ob-swatch" style="background:linear-gradient(135deg,#101218 60%,#6ea8fe)" />
                  <span>Deepslate</span>
                </button>
                <button class="ob-theme-btn" data-ob-theme="nether">
                  <span class="ob-swatch" style="background:linear-gradient(135deg,#140a0a 60%,#ff6b4a)" />
                  <span>Nether</span>
                </button>
                <button class="ob-theme-btn" data-ob-theme="end">
                  <span class="ob-swatch" style="background:linear-gradient(135deg,#0d0b14 60%,#c4a3ff)" />
                  <span>The End</span>
                </button>
                <button class="ob-theme-btn" data-ob-theme="birch">
                  <span class="ob-swatch" style="background:linear-gradient(135deg,#f5f0e8 60%,#5a8f4a)" />
                  <span>Birch</span>
                </button>
              </div>
              <div class="ob-custom-section">
                <label class="setting-label" style="text-align:center;width:100%">Or create your own</label>
                <div class="color-field" id="ob-color-field">
                  <div class="color-field-marker" id="ob-color-field-marker" />
                </div>
                <div class="lightness-row">
                  <svg class="lightness-icon" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" /></svg>
                  <input type="range" id="ob-lightness-slider" class="lightness-slider" min="0" max="100" step="1" defaultValue="0" />
                  <svg class="lightness-icon" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="5" /><path d="M12 1v2M12 21v2M4.22 4.22l1.42 1.42M18.36 18.36l1.42 1.42M1 12h2M21 12h2M4.22 19.78l1.42-1.42M18.36 5.64l1.42-1.42" /></svg>
                </div>
              </div>
            </div>

            <div class="onboarding-step hidden" id="onboarding-step-4">
              <h2 class="onboarding-title">Background Music</h2>
              <p class="onboarding-sub">Ambient track while you're in the launcher</p>
              <div class="ob-music-toggle">
                <button class="ob-music-btn active" id="ob-music-yes">
                  <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
                    <path d="M9 18V5l12-2v13" /><circle cx="6" cy="18" r="3" /><circle cx="18" cy="16" r="3" />
                  </svg>
                  <span>Enable music</span>
                </button>
                <button class="ob-music-btn" id="ob-music-no">
                  <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
                    <path d="M9 18V5l12-2v13" opacity="0.4" /><circle cx="6" cy="18" r="3" opacity="0.4" /><circle cx="18" cy="16" r="3" opacity="0.4" />
                    <line x1="3" y1="3" x2="21" y2="21" stroke-width="2.5" />
                  </svg>
                  <span>No thanks</span>
                </button>
              </div>
              <p class="onboarding-hint">Downloaded on first play (~12 MB), works offline after that</p>
            </div>

            <div class="onboarding-step hidden" id="onboarding-step-5">
              <div class="onboarding-check">&#10003;</div>
              <h2 class="onboarding-title">You're all set!</h2>
              <p class="onboarding-sub">Scroll the wheel to pick a version and hit Launch.<br />Croopor handles everything else.</p>
            </div>
          </div>

          <div class="onboarding-footer">
            <button class="onboarding-back hidden" id="onboarding-back">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><polyline points="15 18 9 12 15 6" /></svg>
            </button>
            <div class="onboarding-dots">
              <span class="dot active" id="dot-1" />
              <span class="dot" id="dot-2" />
              <span class="dot" id="dot-3" />
              <span class="dot" id="dot-4" />
              <span class="dot" id="dot-5" />
            </div>
            <button class="btn-primary onboarding-next" id="onboarding-next">Continue</button>
          </div>
        </div>
      </div>

      <NewInstanceModal />
      <DeleteWizard />
      <ToastViewport />
    </>
  );
}

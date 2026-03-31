import type { JSX } from 'preact';
import { useEffect, useRef } from 'preact/hooks';
import { signal } from '@preact/signals';
import { useSignal } from '@preact/signals';
import { catalog, instances } from '../store';
import { addInstance, selectInstance } from '../actions';
import { api } from '../api';
import { Sound } from '../sound';
import { showError, esc, parseVersionDisplay } from '../utils';
import { installVersion, installLoaderVersion } from '../install';
import {
  fetchGameVersions, fetchLoaderVersions,
  filterByLoaderSupport, latestStable,
} from '../loaders';
import type {
  CatalogVersion, GameVersion, LoaderVersion, LoaderType,
} from '../types';

export const showNewInstanceModal = signal(false);

const PAGE_SIZE = 50;

const LOADER_OPTIONS: { value: LoaderType; label: string }[] = [
  { value: 'fabric', label: 'Fabric' },
  { value: 'quilt', label: 'Quilt' },
  { value: 'forge', label: 'Forge' },
  { value: 'neoforge', label: 'NeoForge' },
];

const FILTER_CHIPS: { value: string; label: string }[] = [
  { value: 'release', label: 'Release' },
  { value: 'snapshot', label: 'Snapshot' },
  { value: 'old_beta', label: 'Beta' },
  { value: 'old_alpha', label: 'Alpha' },
];

function defaultName(): string {
  const base = 'Instance';
  const names = new Set(instances.value.map(i => i.name));
  if (!names.has(base)) return base;
  for (let n = 2; ; n++) {
    const alt = `${base} ${n}`;
    if (!names.has(alt)) return alt;
  }
}

function isAutoName(val: string): boolean {
  return !val || /^Instance( \d+)?$/.test(val);
}

function validateName(name: string): string | null {
  if (!name) return 'Name is required';
  if (instances.value.some(i => i.name === name)) return 'An instance with this name already exists';
  return null;
}

function buildCompositeId(loaderType: string, mcVersion: string, loaderVersion: string): string {
  switch (loaderType) {
    case 'fabric': return `fabric-loader-${loaderVersion}-${mcVersion}`;
    case 'quilt': return `quilt-loader-${loaderVersion}-${mcVersion}`;
    case 'forge': return `${mcVersion}-forge-${loaderVersion}`;
    case 'neoforge': return `neoforge-${loaderVersion}`;
    default: return mcVersion;
  }
}

export function NewInstanceModal(): JSX.Element | null {
  if (!showNewInstanceModal.value) return null;

  const filter = useSignal('release');
  const search = useSignal('');
  const selectedVersionId = useSignal<string | null>(null);
  const page = useSignal(0);
  const loaderEnabled = useSignal(false);
  const selectedLoader = useSignal<LoaderType>('fabric');
  const loaderGameVersions = useSignal<GameVersion[] | null>(null);
  const loaderVersionsList = useSignal<LoaderVersion[] | null>(null);
  const selectedLoaderVersion = useSignal<string | null>(null);
  const loaderLoading = useSignal(false);
  const name = useSignal(defaultName());
  const nameError = useSignal<string | null>(null);

  const overlayRef = useRef<HTMLDivElement>(null);
  const nameRef = useRef<HTMLInputElement>(null);

  // Load catalog on first open, auto-focus name, play sound
  useEffect(() => {
    Sound.ui('soft');
    (async () => {
      if (!catalog.value) {
        try {
          catalog.value = await api('GET', '/catalog');
        } catch {
          showError('Failed to load version catalog');
          showNewInstanceModal.value = false;
          return;
        }
      }
      // Auto-select first version
      const allVersions = catalog.value?.versions ?? [];
      const first = allVersions.filter(v => v.type === filter.value);
      if (first.length > 0) {
        selectedVersionId.value = first[0].id;
        name.value = defaultName();
      }
    })();
    requestAnimationFrame(() => nameRef.current?.focus());
  }, []);

  const allVersions: CatalogVersion[] = catalog.value?.versions ?? [];

  // Filter versions
  let filteredVersions = allVersions.filter(v => v.type === filter.value);
  if (loaderEnabled.value && loaderGameVersions.value) {
    filteredVersions = filterByLoaderSupport(filteredVersions, loaderGameVersions.value);
  }
  if (search.value) {
    const q = search.value.toLowerCase();
    filteredVersions = filteredVersions.filter(v => {
      const pd = parseVersionDisplay(v.id, v, allVersions);
      return v.id.toLowerCase().includes(q) || pd.name.toLowerCase().includes(q);
    });
  }

  const total = filteredVersions.length;
  const totalPages = Math.ceil(total / PAGE_SIZE);
  const start = page.value * PAGE_SIZE;
  const display = filteredVersions.slice(start, start + PAGE_SIZE);

  const close = () => {
    showNewInstanceModal.value = false;
    Sound.ui('soft');
  };

  const handleOverlayClick = (e: MouseEvent) => {
    if (e.target === overlayRef.current) close();
  };

  const handleFilterClick = (f: string) => {
    filter.value = f;
    page.value = 0;
  };

  const handleSearchInput = (e: JSX.TargetedEvent<HTMLInputElement>) => {
    search.value = e.currentTarget.value;
    page.value = 0;
  };

  const handleNameInput = (e: JSX.TargetedEvent<HTMLInputElement>) => {
    name.value = e.currentTarget.value;
    nameError.value = null;
  };

  const handleNameKeyDown = (e: KeyboardEvent) => {
    if (e.key === 'Enter') handleCreate();
  };

  const selectVersion = async (vid: string) => {
    selectedVersionId.value = vid;
    if (isAutoName(name.value.trim())) name.value = defaultName();
    nameError.value = null;
    Sound.ui('click');

    if (loaderEnabled.value) {
      await updateLoaderVersionInfo(vid);
    }
  };

  const loadLoaderGameVersions = async (): Promise<void> => {
    loaderLoading.value = true;
    try {
      loaderGameVersions.value = await fetchGameVersions(selectedLoader.value);
    } catch {
      loaderGameVersions.value = [];
    }
    loaderLoading.value = false;
  };

  const updateLoaderVersionInfo = async (mcVersion: string): Promise<void> => {
    if (!loaderEnabled.value) return;
    try {
      loaderVersionsList.value = await fetchLoaderVersions(selectedLoader.value, mcVersion);
      const best = latestStable(loaderVersionsList.value);
      selectedLoaderVersion.value = best?.version ?? null;
    } catch {
      selectedLoaderVersion.value = null;
    }
  };

  const autoSelectFirstVersion = (list: CatalogVersion[]) => {
    if (list.length > 0) {
      selectedVersionId.value = list[0].id;
      if (isAutoName(name.value.trim())) name.value = defaultName();
      if (loaderEnabled.value) updateLoaderVersionInfo(list[0].id);
    }
  };

  const handleLoaderToggle = async (e: JSX.TargetedEvent<HTMLInputElement>) => {
    const enabled = e.currentTarget.checked;
    loaderEnabled.value = enabled;
    if (enabled) {
      await loadLoaderGameVersions();
    } else {
      loaderGameVersions.value = null;
      loaderVersionsList.value = null;
      selectedLoaderVersion.value = null;
    }
    page.value = 0;
    selectedVersionId.value = null;

    // Re-compute filtered list for auto-select
    let list = allVersions.filter(v => v.type === filter.value);
    if (enabled && loaderGameVersions.value) {
      list = filterByLoaderSupport(list, loaderGameVersions.value);
    }
    autoSelectFirstVersion(list);
  };

  const handleLoaderChange = async (e: JSX.TargetedEvent<HTMLSelectElement>) => {
    selectedLoader.value = e.currentTarget.value as LoaderType;
    loaderVersionsList.value = null;
    selectedLoaderVersion.value = null;
    await loadLoaderGameVersions();
    page.value = 0;
    selectedVersionId.value = null;

    let list = allVersions.filter(v => v.type === filter.value);
    if (loaderGameVersions.value) {
      list = filterByLoaderSupport(list, loaderGameVersions.value);
    }
    autoSelectFirstVersion(list);
    Sound.ui('soft');
  };

  const handleCreate = async () => {
    const trimmed = name.value.trim();
    const err = validateName(trimmed);
    if (err) {
      nameError.value = err;
      nameRef.current?.focus();
      return;
    }
    if (!selectedVersionId.value) return;

    if (loaderEnabled.value && !selectedLoaderVersion.value) {
      nameError.value = 'No loader version available for this Minecraft version';
      return;
    }

    const compositeId = loaderEnabled.value
      ? buildCompositeId(selectedLoader.value, selectedVersionId.value, selectedLoaderVersion.value!)
      : selectedVersionId.value;

    try {
      const res = await api('POST', '/instances', { name: trimmed, version_id: compositeId });
      if (res.error) { showError(res.error); return; }
      addInstance(res);
      close();
      selectInstance(res.id);
      Sound.ui('affirm');

      // Auto-install
      if (loaderEnabled.value) {
        installLoaderVersion(selectedLoader.value, selectedVersionId.value, selectedLoaderVersion.value!, compositeId);
      } else {
        const needsInstall = !allVersions.find(v => v.id === selectedVersionId.value)?.installed;
        if (needsInstall) installVersion(selectedVersionId.value);
      }
    } catch (err: unknown) {
      showError((err as Error).message);
    }
  };

  // Loader info text
  const loaderInfoText = loaderEnabled.value && selectedLoaderVersion.value
    ? `Loader: ${selectedLoaderVersion.value}`
    : null;

  return (
    <div
      class="modal-overlay"
      id="new-instance-modal"
      ref={overlayRef}
      onClick={handleOverlayClick}
    >
      <div class="modal" style="width:480px">
        <div class="modal-header">
          <span class="modal-title">New Instance</span>
          <button class="icon-btn modal-close" onClick={close}>&times;</button>
        </div>
        <div style="padding:16px 18px;display:flex;flex-direction:column;gap:14px">
          {/* Name */}
          <div>
            <label class="detail-prop-label" style="display:block;margin-bottom:6px;padding:0">Name</label>
            <input
              type="text"
              ref={nameRef}
              class="field-input"
              placeholder="My Instance"
              spellcheck={false}
              autocomplete="off"
              style="width:100%;box-sizing:border-box"
              value={name.value}
              onInput={handleNameInput}
              onKeyDown={handleNameKeyDown}
            />
            {nameError.value && (
              <div style="font-size:11px;color:var(--red);margin-top:4px">{nameError.value}</div>
            )}
          </div>

          {/* Mod Loader */}
          <div>
            <label class="detail-prop-label" style="display:block;margin-bottom:6px;padding:0">Mod Loader</label>
            <div class="ni-loader-row">
              <label class="ni-loader-toggle">
                <input
                  type="checkbox"
                  checked={loaderEnabled.value}
                  onChange={handleLoaderToggle}
                />
                <span class="ni-toggle-track"><span class="ni-toggle-thumb"></span></span>
              </label>
              {loaderEnabled.value && (
                <select
                  class="ni-loader-select"
                  autocomplete="off"
                  value={selectedLoader.value}
                  onChange={handleLoaderChange}
                >
                  {LOADER_OPTIONS.map(opt => (
                    <option key={opt.value} value={opt.value}>{opt.label}</option>
                  ))}
                </select>
              )}
              {loaderInfoText && (
                <span class="ni-loader-info">{loaderInfoText}</span>
              )}
            </div>
          </div>

          {/* Version */}
          <div>
            <label class="detail-prop-label" style="display:block;margin-bottom:6px;padding:0">Version</label>
            <input
              type="text"
              class="field-input"
              placeholder="Search versions..."
              spellcheck={false}
              autocomplete="off"
              style="width:100%;box-sizing:border-box;margin-bottom:8px"
              value={search.value}
              onInput={handleSearchInput}
            />
            <div class="filter-chips">
              {FILTER_CHIPS.map(chip => (
                <button
                  key={chip.value}
                  class={`chip${filter.value === chip.value ? ' active' : ''}`}
                  onClick={() => handleFilterClick(chip.value)}
                >
                  {chip.label}
                </button>
              ))}
            </div>
            <div class="ni-version-list">
              {loaderLoading.value ? (
                <div style="padding:12px;text-align:center;color:var(--text-muted);font-size:12px">
                  Loading versions...
                </div>
              ) : display.length === 0 ? (
                <div style="padding:12px;text-align:center;color:var(--text-muted);font-size:12px">
                  No versions found
                </div>
              ) : (
                <>
                  {display.map(v => {
                    const selected = v.id === selectedVersionId.value;
                    const pd = parseVersionDisplay(v.id, v, allVersions);
                    return (
                      <div
                        key={v.id}
                        class={`ni-version-item${selected ? ' selected' : ''}`}
                        onClick={() => selectVersion(v.id)}
                      >
                        <span
                          class="ni-version-id"
                          dangerouslySetInnerHTML={{
                            __html: pd.hint
                              ? `${esc(pd.name)} <span class="version-hint">${esc(pd.hint)}</span>`
                              : esc(pd.name),
                          }}
                        />
                        {v.installed && <span class="ni-installed-badge">Installed</span>}
                      </div>
                    );
                  })}
                  {totalPages > 1 && (
                    <div class="ni-pagination">
                      <button
                        class="ni-page-btn"
                        disabled={page.value === 0}
                        onClick={() => { if (page.value > 0) page.value--; }}
                      >
                        &lsaquo;
                      </button>
                      <span class="ni-page-info">{page.value + 1} / {totalPages}</span>
                      <button
                        class="ni-page-btn"
                        disabled={page.value >= totalPages - 1}
                        onClick={() => { if (page.value < totalPages - 1) page.value++; }}
                      >
                        &rsaquo;
                      </button>
                    </div>
                  )}
                </>
              )}
            </div>
          </div>

          <button
            class="btn-primary"
            style="align-self:flex-end;margin-top:4px"
            onClick={handleCreate}
          >
            Create
          </button>
        </div>
      </div>
    </div>
  );
}

import type { JSX } from 'preact';
import { useEffect, useMemo, useRef } from 'preact/hooks';
import { signal } from '@preact/signals';
import { useSignal } from '@preact/signals';
import { catalog, instances, versions } from '../store';
import { addInstance, selectInstance } from '../actions';
import { api } from '../api';
import { Sound } from '../sound';
import { showError, esc, parseVersionDisplay, errMessage } from '../utils';
import { installVersion, installLoaderVersion } from '../install';
import { createNewInstanceLoaderMachine } from '../machines/new-instance-loader';
import type {
  CatalogVersion, LoaderBuildRecord, LoaderComponentId, LoaderComponentRecord,
} from '../types';

export const showNewInstanceModal = signal(false);

const PAGE_SIZE = 50;

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

export function NewInstanceModal(): JSX.Element | null {
  const isOpen = showNewInstanceModal.value;

  const filter = useSignal('release');
  const search = useSignal('');
  const selectedVersionId = useSignal<string | null>(null);
  const page = useSignal(0);
  const name = useSignal(defaultName());
  const nameError = useSignal<string | null>(null);

  const overlayRef = useRef<HTMLDivElement>(null);
  const nameRef = useRef<HTMLInputElement>(null);
  const loaderMachineRef = useRef<ReturnType<typeof createNewInstanceLoaderMachine> | null>(null);
  if (!loaderMachineRef.current) {
    loaderMachineRef.current = createNewInstanceLoaderMachine();
  }
  const loaderMachine = loaderMachineRef.current;
  const loaderState = loaderMachine.state;
  const loaderEnabled = loaderState.value.kind !== 'disabled';
  const loaderLoading = loaderState.value.kind === 'loading_components' || loaderState.value.kind === 'loading_builds';
  const loaderComponents: LoaderComponentRecord[] | null = loaderState.value.context.components;
  const selectedLoader = loaderState.value.context.selectedComponentId;
  const loaderBuilds: LoaderBuildRecord[] | null = loaderState.value.context.builds;
  const selectedLoaderBuildId: string | null = loaderState.value.context.selectedBuildId;
  const loaderError = loaderState.value.kind === 'error' ? loaderState.value.context.errorMessage : null;
  const selectedLoaderBuild = loaderBuilds?.find((build) => build.build_id === selectedLoaderBuildId) ?? null;

  // Reset modal state on each open, then ensure catalog exists
  useEffect(() => {
    if (!isOpen) return;

    filter.value = 'release';
    search.value = '';
    selectedVersionId.value = null;
    page.value = 0;
    loaderMachine.reset();
    name.value = defaultName();
    nameError.value = null;

    Sound.ui('soft');
    (async () => {
      if (!catalog.value) {
        try {
          const res = await api('GET', '/catalog');
          if (res.error) throw new Error(res.error);
          catalog.value = res;
        } catch (err: unknown) {
          showError(`Failed to load version catalog: ${err instanceof Error ? err.message : String(err)}`);
          showNewInstanceModal.value = false;
          return;
        }
      }
      const allVersions = catalog.value?.versions ?? [];
      const first = allVersions.filter(v => v.type === filter.value);
      if (first.length > 0) {
        selectedVersionId.value = first[0].id;
        if (isAutoName(name.value.trim())) name.value = defaultName();
      }
    })();
    requestAnimationFrame(() => nameRef.current?.focus());
  }, [isOpen]);

  const allVersions: CatalogVersion[] = catalog.value?.versions ?? [];

  // Filter versions
  const filteredVersions = useMemo(() => {
    let next = allVersions.filter(v => v.type === filter.value);
    if (search.value) {
      const q = search.value.toLowerCase();
      next = next.filter(v => {
        const pd = parseVersionDisplay(v.id, v, allVersions);
        return v.id.toLowerCase().includes(q) || pd.name.toLowerCase().includes(q);
      });
    }
    return next;
  }, [allVersions, filter.value, search.value]);

  const loaderInstalledFor = useMemo(() => {
    if (!loaderEnabled) return null;
    const loader = selectedLoader;
    const set = new Set<string>();
    for (const ver of versions.value) {
      if (!ver.launchable || !ver.inherits_from || !loader) continue;
      if (ver.loader_component_id === loader) {
        set.add(ver.inherits_from);
      }
    }
    return set;
  }, [loaderEnabled, selectedLoader, versions.value]);

  const total = filteredVersions.length;
  const totalPages = Math.ceil(total / PAGE_SIZE);
  const start = page.value * PAGE_SIZE;
  const display = filteredVersions.slice(start, start + PAGE_SIZE);

  if (!isOpen) return null;

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

  useEffect(() => {
    if (filteredVersions.length === 0) {
      selectedVersionId.value = null;
      return;
    }
    if (selectedVersionId.value && filteredVersions.some((version) => version.id === selectedVersionId.value)) return;

    const nextId = filteredVersions[0].id;
    selectedVersionId.value = nextId;
    if (isAutoName(name.value.trim())) name.value = defaultName();
    nameError.value = null;
    if (loaderEnabled) void loaderMachine.changeMcVersion(nextId);
  }, [filteredVersions, loaderEnabled]);

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

    if (loaderEnabled) {
      await loaderMachine.changeMcVersion(vid);
    }
  };

  const autoSelectFirstVersion = (list: CatalogVersion[], loadBuilds = loaderMachine.state.value.kind !== 'disabled') => {
    if (list.length > 0) {
      selectedVersionId.value = list[0].id;
      if (isAutoName(name.value.trim())) name.value = defaultName();
      if (loadBuilds) {
        void loaderMachine.changeMcVersion(list[0].id);
      }
    }
  };

  const handleLoaderToggle = async (e: JSX.TargetedEvent<HTMLInputElement>) => {
    const enabled = e.currentTarget.checked;
    if (enabled) {
      await loaderMachine.enable(selectedVersionId.value);
    } else {
      loaderMachine.disable();
    }
    page.value = 0;
    autoSelectFirstVersion(allVersions.filter(v => v.type === filter.value), enabled);
  };

  const handleLoaderChange = async (e: JSX.TargetedEvent<HTMLSelectElement>) => {
    await loaderMachine.changeComponent(
      e.currentTarget.value as LoaderComponentId,
      selectedVersionId.value,
    );
    page.value = 0;
    autoSelectFirstVersion(allVersions.filter(v => v.type === filter.value), true);
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

    if (loaderEnabled && (loaderState.value.kind !== 'ready' || !selectedLoaderBuild)) {
      nameError.value = 'No loader build available for this Minecraft version';
      return;
    }

    const compositeId = loaderEnabled ? selectedLoaderBuild!.version_id : selectedVersionId.value;

    try {
      const res = await api('POST', '/instances', { name: trimmed, version_id: compositeId });
      if (res.error) { showError(res.error); return; }
      addInstance(res);
      close();
      selectInstance(res.id);
      Sound.ui('affirm');

      // Auto-install
      if (loaderEnabled) {
        installLoaderVersion(selectedLoaderBuild!);
      } else {
        const needsInstall = !allVersions.find(v => v.id === selectedVersionId.value)?.installed;
        if (needsInstall) installVersion(selectedVersionId.value);
      }
    } catch (err: unknown) {
      showError(errMessage(err));
    }
  };

  // Loader info text
  const loaderInfoText = loaderEnabled && selectedLoaderBuild
    ? `Loader: ${selectedLoaderBuild.loader_version}`
    : null;
  const createDisabled = !selectedVersionId.value || (loaderEnabled && loaderState.value.kind !== 'ready');

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
                  checked={loaderEnabled}
                  onChange={handleLoaderToggle}
                />
                <span class="ni-toggle-track"><span class="ni-toggle-thumb"></span></span>
              </label>
              {loaderEnabled && (
                <>
                  <select
                    class="ni-loader-select"
                    autocomplete="off"
                    value={selectedLoader ?? ''}
                    onChange={handleLoaderChange}
                  >
                    {(loaderComponents ?? []).map(opt => (
                      <option key={opt.id} value={opt.id}>{opt.name}</option>
                    ))}
                  </select>
                  <select
                    class="ni-loader-select"
                    autocomplete="off"
                    value={selectedLoaderBuildId ?? ''}
                    disabled={loaderBuilds === null || loaderBuilds.length === 0}
                    onChange={(event) => loaderMachine.selectBuild(event.currentTarget.value)}
                  >
                    {(loaderBuilds ?? []).map(build => (
                      <option key={build.build_id} value={build.build_id}>
                        {build.loader_version}{build.recommended ? ' (recommended)' : build.latest ? ' (latest)' : ''}
                      </option>
                    ))}
                  </select>
                </>
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
              {loaderLoading ? (
                <div style="padding:12px;text-align:center;color:var(--text-muted);font-size:12px">
                  Loading versions...
                </div>
              ) : loaderError ? (
                <div style="padding:12px;text-align:center;color:var(--red);font-size:12px">
                  {loaderError}
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
                      <button
                        key={v.id}
                        type="button"
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
                        {(loaderInstalledFor ? loaderInstalledFor.has(v.id) : v.installed) && <span class="ni-installed-badge">Installed</span>}
                      </button>
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
            disabled={createDisabled}
            onClick={handleCreate}
          >
            Create
          </button>
        </div>
      </div>
    </div>
  );
}

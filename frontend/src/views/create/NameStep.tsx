import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { Input } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { Slider } from '../../ui/Slider';
import {
  InstanceArt,
  artPresetForSeed,
  loaderTraitForComponentId,
  versionIdentityForVersion,
  versionIdentityForVersionId,
  type VersionIdentitySource,
} from '../../art/InstanceArt';
import { fmtMem } from '../../utils';
import { Sound } from '../../sound';
import type { LoaderBuildRecord } from '../../types';
import { LOADER_COMPONENT_IDS, LOADER_LABELS, type LoaderKey } from './defaults';
import { LoaderLogo } from './loader-logos';
import {
  JVM_PRESET_HINTS,
  JVM_PRESET_LABELS,
  type JvmPreset,
} from './jvm-presets';
import type { WindowPresetSpec } from './screen-presets';

type IdleHandle = number;
type IdleDeadline = { didTimeout: boolean; timeRemaining: () => number };
type IdleCapableWindow = Window & {
  requestIdleCallback?: (cb: (d: IdleDeadline) => void, opts?: { timeout: number }) => IdleHandle;
  cancelIdleCallback?: (handle: IdleHandle) => void;
};

const COMPACT_JVM_HINTS: Record<JvmPreset, string> = {
  '': 'Safe flags.',
  smooth: 'Steady frames.',
  performance: 'More throughput.',
  ultra_low_latency: 'Fewer pauses.',
  graalvm: 'Graal flags.',
  legacy: 'Old Java.',
  legacy_pvp: 'Input bias.',
  legacy_heavy: 'Old modpacks.',
};

function useDeferredFlag(): boolean {
  const [ready, setReady] = useState(false);
  useEffect(() => {
    let idleHandle: IdleHandle | null = null;
    let rafA = 0;
    let rafB = 0;
    const idleWin = window as IdleCapableWindow;
    if (idleWin.requestIdleCallback) {
      idleHandle = idleWin.requestIdleCallback(() => { setReady(true); }, { timeout: 400 });
    } else {
      rafA = requestAnimationFrame(() => {
        rafB = requestAnimationFrame(() => { setReady(true); });
      });
    }
    return () => {
      if (idleHandle != null && idleWin.cancelIdleCallback) idleWin.cancelIdleCallback(idleHandle);
      if (rafA) cancelAnimationFrame(rafA);
      if (rafB) cancelAnimationFrame(rafB);
    };
  }, []);
  return ready;
}

export function NameStep({
  source,
  mcVersionId,
  name,
  suggestedName,
  onNameChange,
  nameInputRef,
  alreadyInstalled,
  selectedBuild,
  minecraftVersion,
  previewSeed,
  onReroll,
  memoryGB,
  onMemoryChange,
  memoryRec,
  totalGB,
  windowPresets,
  windowPresetId,
  onCycleWindow,
  jvmPreset,
  onCycleJvm,
}: {
  source: LoaderKey;
  mcVersionId: string;
  name: string;
  suggestedName: string;
  onNameChange: (value: string) => void;
  nameInputRef: { current: HTMLInputElement | null };
  alreadyInstalled: boolean;
  selectedBuild: LoaderBuildRecord | null;
  minecraftVersion?: VersionIdentitySource | null;
  previewSeed: number;
  onReroll: () => void;
  memoryGB: number;
  onMemoryChange: (v: number) => void;
  memoryRec: number;
  totalGB: number;
  windowPresets: WindowPresetSpec[];
  windowPresetId: string;
  onCycleWindow: () => void;
  jvmPreset: JvmPreset;
  onCycleJvm: () => void;
}): JSX.Element {
  const displayName = name.trim() || suggestedName || 'Untitled';
  const previewPreset = artPresetForSeed(previewSeed);
  const serial = String(previewSeed % 10000).padStart(4, '0');
  const previewInstance = {
    id: `preview:${source}:${mcVersionId}`,
    name: displayName,
    version_id: mcVersionId,
    art_seed: previewSeed,
  };
  const loaderLabel = source === 'vanilla' ? 'Vanilla' : LOADER_LABELS[source];
  const buildLabel = source !== 'vanilla' && selectedBuild ? selectedBuild.loader_version : null;
  const loaderTrait = source === 'vanilla' ? null : loaderTraitForComponentId(LOADER_COMPONENT_IDS[source]);
  const versionIdentity = (() => {
    const fromVersion = versionIdentityForVersion(minecraftVersion);
    if (fromVersion) return { ...fromVersion, loaderTrait };
    return versionIdentityForVersionId(mcVersionId, loaderTrait);
  })();

  const artReady = useDeferredFlag();
  const memReady = useDeferredFlag();

  // Reroll feedback: immediate visual response while the procedural art
  // regenerates. The blob placeholder + spinning icon cover the ~300ms it
  // takes for the canvas to redraw, so the click feels live.
  const [rerolling, setRerolling] = useState(false);
  const rerollTimer = useRef<number | null>(null);
  useEffect(() => () => {
    if (rerollTimer.current != null) window.clearTimeout(rerollTimer.current);
  }, []);
  const handleReroll = (): void => {
    if (rerollTimer.current != null) window.clearTimeout(rerollTimer.current);
    setRerolling(true);
    Sound.ui('click');
    onReroll();
    rerollTimer.current = window.setTimeout(() => {
      setRerolling(false);
      rerollTimer.current = null;
    }, 480);
  };

  const showArt = artReady && !rerolling;

  const winSpec = windowPresets.find((p) => p.id === windowPresetId)
    ?? windowPresets[windowPresets.length - 1]
    ?? { id: 'default', label: 'Default', w: 0, h: 0 };
  const winSubtitle = winSpec.id === 'default'
    ? 'Game default'
    : `${winSpec.w} × ${winSpec.h}`;

  return (
    <section class="cp-cr-step cp-cr-step--name">
      <header class="cp-cr-head">
        <h1 class="cp-cr-headline">Name it.</h1>
        <p class="cp-cr-subline">A few defaults you can tune before the instance lands.</p>
      </header>

      <label class="cp-cr-name-row">
        <span class="cp-cr-name-label">Name</span>
        <Input
          value={name}
          onChange={onNameChange}
          placeholder={suggestedName || 'Aurora Adventure'}
          inputRef={nameInputRef}
          autoFocus
        />
      </label>

      <div class="cp-cr-bento">
        {/* Identity tile: heaviest visual, 3x2 */}
        <article class="cp-cr-tile cp-cr-tile--identity" aria-label="Identity preview">
          <div class="cp-cr-card-bg" aria-hidden="true">
            {showArt ? (
              <InstanceArt instance={previewInstance} aspect="banner" />
            ) : (
              <div class="cp-cr-blob cp-cr-blob--banner" />
            )}
            <div class="cp-cr-card-veil" />
          </div>
          <div class="cp-cr-card-top">
            <div class="cp-cr-card-avatar">
              {showArt ? (
                <InstanceArt instance={previewInstance} versionIdentity={versionIdentity} aspect="square" radius={14} />
              ) : (
                <div class="cp-cr-blob cp-cr-blob--avatar" />
              )}
            </div>
            <div class="cp-cr-card-stamp">
              <span class="cp-cr-card-stamp-key">{previewPreset}</span>
              <span class="cp-cr-card-stamp-sep" aria-hidden="true">·</span>
              <span class="cp-cr-card-stamp-num">{serial}</span>
            </div>
          </div>
          <div class="cp-cr-card-foot">
            <span class="cp-cr-card-eyebrow">Minecraft Instance</span>
            <h2 class="cp-cr-card-name" title={displayName}>{displayName}</h2>
            <dl class="cp-cr-card-meta">
              <div class="cp-cr-card-meta-col">
                <dt>Loader</dt>
                <dd class="cp-cr-card-meta-loader">
                  {source !== 'vanilla' && (
                    <LoaderLogo loader={source} size={12} class="cp-cr-loader-mark" />
                  )}
                  <span>{loaderLabel}{buildLabel ? ` ${buildLabel}` : ''}</span>
                </dd>
              </div>
              <div class="cp-cr-card-meta-col">
                <dt>Version</dt>
                <dd>{mcVersionId}</dd>
              </div>
              <div class="cp-cr-card-meta-col">
                <dt>Status</dt>
                <dd>
                  <span class={alreadyInstalled ? 'cp-cr-card-status is-ok' : 'cp-cr-card-status'}>
                    {alreadyInstalled ? 'Ready' : 'Will download'}
                  </span>
                </dd>
              </div>
            </dl>
          </div>
        </article>

        {/* Memory tile: medium, 3x1 */}
        <article class="cp-cr-tile cp-cr-tile--memory" aria-label="Memory">
          <div class="cp-cr-tile-head">
            <span class="cp-cr-tile-eyebrow">Memory</span>
            <span class="cp-cr-tile-reading" aria-live="polite">{fmtMem(memoryGB)}</span>
          </div>
          {memReady ? (
            <Slider
              value={memoryGB}
              min={1}
              max={totalGB}
              step={0.5}
              recommended={[Math.max(2, memoryRec - 2), Math.min(totalGB, memoryRec + 2)]}
              sound="memory"
              onChange={onMemoryChange}
              ariaLabel="Max memory in gigabytes"
            />
          ) : (
            <div class="cp-cr-blob cp-cr-blob--slider" />
          )}
          <span class="cp-cr-tile-hint">
            {memoryGB < 2
              ? 'Low. May stutter.'
              : memoryGB > totalGB * 0.75
                ? 'High. Leave room for the OS.'
                : `Comfortable start: ${memoryRec} GB.`}
          </span>
        </article>

        {/* Reroll tile: light, 1x1 */}
        <button
          type="button"
          class="cp-cr-tile cp-cr-tile--reroll"
          onClick={handleReroll}
          aria-label="Reroll artwork"
          data-rerolling={rerolling ? 'true' : 'false'}
        >
          <span class="cp-cr-tile-eyebrow">Artwork</span>
          <span class="cp-cr-reroll-icon" aria-hidden="true">
            <Icon name="refresh" size={20} stroke={2} />
          </span>
          <span class="cp-cr-reroll-label">{rerolling ? 'Rerolling' : 'Reroll'}</span>
          <span class="cp-cr-reroll-sub">{previewPreset}</span>
        </button>

        {/* Window tile: light, 1x1 */}
        <button
          type="button"
          class="cp-cr-tile cp-cr-tile--window"
          onClick={onCycleWindow}
          aria-label="Cycle window size"
          data-preset={windowPresetId}
        >
          <span class="cp-cr-tile-eyebrow">Window</span>
          <span class="cp-cr-window-frame" aria-hidden="true">
            <span class="cp-cr-window-frame-inner" data-preset={windowPresetId} />
          </span>
          <span class="cp-cr-window-label">{winSpec.label}</span>
          <span class="cp-cr-window-sub">{winSubtitle}</span>
        </button>

        {/* Profile tile: light, 1x1 */}
        <button
          type="button"
          class="cp-cr-tile cp-cr-tile--profile"
          onClick={onCycleJvm}
          aria-label="Cycle performance profile"
          data-profile={jvmPreset || 'auto'}
          title={JVM_PRESET_HINTS[jvmPreset]}
        >
          <span class="cp-cr-tile-eyebrow">Profile</span>
          <span class="cp-cr-profile-glyph" aria-hidden="true">
            <span class="cp-cr-profile-bar" data-bar="1" />
            <span class="cp-cr-profile-bar" data-bar="2" />
            <span class="cp-cr-profile-bar" data-bar="3" />
            <span class="cp-cr-profile-bar" data-bar="4" />
          </span>
          <span class="cp-cr-profile-label">{JVM_PRESET_LABELS[jvmPreset]}</span>
          <span class="cp-cr-profile-sub" title={JVM_PRESET_HINTS[jvmPreset]}>
            {COMPACT_JVM_HINTS[jvmPreset]}
          </span>
        </button>
      </div>
    </section>
  );
}

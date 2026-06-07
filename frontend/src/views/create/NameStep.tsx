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

      <div class="cp-cr-identity">
        <div class="cp-cr-identity-avatar">
          {showArt ? (
            <InstanceArt instance={previewInstance} versionIdentity={versionIdentity} aspect="square" radius={14} />
          ) : (
            <div class="cp-cr-blob cp-cr-blob--avatar" />
          )}
        </div>
        <div class="cp-cr-identity-text">
          <div class="cp-cr-identity-pills">
            <span class="cp-pill">
              {source !== 'vanilla' && (
                <LoaderLogo loader={source} size={11} class="cp-cr-loader-mark" />
              )}
              {loaderLabel}{buildLabel ? ` ${buildLabel}` : ''}
            </span>
            <span class="cp-pill">Minecraft {mcVersionId}</span>
            <span class={alreadyInstalled ? 'cp-pill cp-pill--ok' : 'cp-pill'}>
              <span class="cp-cr-identity-dot" aria-hidden="true" />
              {alreadyInstalled ? 'Already installed' : 'Downloads on create'}
            </span>
          </div>
          <h2 class="cp-cr-identity-name" title={displayName}>{displayName}</h2>
        </div>
      </div>

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

      <div class="cp-cr-defaults">
        <section class="cp-cr-mem" aria-label="Memory">
          <div class="cp-cr-mem-head">
            <span class="cp-cr-field-label">Memory</span>
            <span class="cp-cr-mem-reading" aria-live="polite">{fmtMem(memoryGB)}</span>
          </div>
          <div class="cp-cr-mem-slider">
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
          </div>
          <span class="cp-cr-mem-hint">
            {memoryGB < 2
              ? 'Low. May stutter.'
              : memoryGB > totalGB * 0.75
                ? 'High. Leave room for the OS.'
                : `Comfortable start: ${memoryRec} GB.`}
          </span>
        </section>

        <div class="cp-cr-rows" role="group" aria-label="Instance defaults">
          <button
            type="button"
            class="cp-cr-row cp-cr-row--reroll"
            onClick={handleReroll}
            aria-label="Reroll artwork"
            data-rerolling={rerolling ? 'true' : 'false'}
          >
            <span class="cp-cr-row-key">Artwork</span>
            <span class="cp-cr-row-val">
              <span class="cp-cr-row-value">{rerolling ? 'Rerolling…' : previewPreset}</span>
              <span class="cp-cr-row-act cp-cr-row-act--reroll" aria-hidden="true">
                <Icon name="refresh" size={14} stroke={2} />
              </span>
            </span>
          </button>

          <button
            type="button"
            class="cp-cr-row"
            onClick={onCycleWindow}
            aria-label="Cycle window size"
            data-preset={windowPresetId}
          >
            <span class="cp-cr-row-key">Window</span>
            <span class="cp-cr-row-val">
              <span class="cp-cr-row-value">{winSpec.label}</span>
              <span class="cp-cr-row-sub">{winSubtitle}</span>
              <span class="cp-cr-row-act" aria-hidden="true">
                <Icon name="chevron-right" size={14} stroke={2} />
              </span>
            </span>
          </button>

          <button
            type="button"
            class="cp-cr-row"
            onClick={onCycleJvm}
            aria-label="Cycle performance profile"
            data-profile={jvmPreset || 'auto'}
            title={JVM_PRESET_HINTS[jvmPreset]}
          >
            <span class="cp-cr-row-key">Profile</span>
            <span class="cp-cr-row-val">
              <span class="cp-cr-row-value">{JVM_PRESET_LABELS[jvmPreset]}</span>
              <span class="cp-cr-row-sub">{COMPACT_JVM_HINTS[jvmPreset]}</span>
              <span class="cp-cr-row-act" aria-hidden="true">
                <Icon name="chevron-right" size={14} stroke={2} />
              </span>
            </span>
          </button>
        </div>
      </div>

      <div class="cp-cr-horizon" aria-hidden="true">
        {showArt ? (
          <InstanceArt instance={previewInstance} aspect="banner" className="cp-cr-horizon-art" />
        ) : (
          <div class="cp-cr-blob" />
        )}
        <div class="cp-cr-horizon-glow" />
        <div class="cp-cr-horizon-stamp">
          <span class="cp-cr-horizon-stamp-key">{previewPreset}</span>
          <span class="cp-cr-horizon-stamp-sep">·</span>
          <span class="cp-cr-horizon-stamp-num">{serial}</span>
        </div>
      </div>
    </section>
  );
}

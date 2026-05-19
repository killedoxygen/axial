import type { JSX } from 'preact';
import { useLayoutEffect, useRef } from 'preact/hooks';
import { useTheme } from '../hooks/use-theme';
import { hashStr } from '../tokens';
import type { Instance, LoaderComponentId, Version } from '../types';
import { renderInstanceArt, type VersionIdentity, type VersionLoaderTrait } from './art-engine';
import './instance-art.css';

export type ArtPreset = 'aurora' | 'silk' | 'mineral' | 'ember' | 'vapor' | 'topo' | 'prism' | 'dune' | 'orbit';
export type ArtAspect = 'square' | 'banner' | 'thumb';

export interface VersionIdentitySource {
  id: string;
  minecraft_meta: Version['minecraft_meta'];
  lifecycle: Version['lifecycle'];
  loader?: Version['loader'];
}

// Order is part of the deterministic seed contract. Keep this list in sync
// with `ART_PRESETS` in `core/config/src/instances/mod.rs`.
export const ART_PRESETS: ArtPreset[] = ['aurora', 'silk', 'mineral', 'ember', 'vapor', 'topo', 'prism', 'dune', 'orbit'];

interface ArtInput {
  seed: number;
  preset: ArtPreset;
  dark: boolean;
  aspect: ArtAspect;
  renderSize?: { width: number; height: number } | null;
  versionIdentity?: VersionIdentity | null;
}

function pickPreset(seed: number): ArtPreset {
  return ART_PRESETS[seed % ART_PRESETS.length];
}

/**
 * Instance artwork is deterministic from one number:
 *
 * - `art_seed` is the source of truth for both the composition and the preset.
 * - the preset is `ART_PRESETS[seed % ART_PRESETS.length]`.
 * - all renderer randomness is derived from this seed plus stable labels.
 * - changing the variant changes the seed to the nearest value that maps to
 *   that preset, so the identity remains reproducible from the seed alone.
 */
export function artPresetForSeed(seed: number): ArtPreset {
  return pickPreset(seed >>> 0 || 1);
}

export function artSeedForPreset(seed: number, preset: ArtPreset): number {
  const current = seed >>> 0 || 1;
  const target = ART_PRESETS.indexOf(preset);
  if (target < 0) return current;
  for (let offset = 0; offset < ART_PRESETS.length; offset += 1) {
    const up = current + offset;
    if (up <= 0xffffffff && up % ART_PRESETS.length === target) return up >>> 0;
    const down = current - offset;
    if (down > 0 && down % ART_PRESETS.length === target) return down >>> 0;
  }
  return (target + ART_PRESETS.length) >>> 0;
}

export function artSeedFor(instance: Pick<Instance, 'id' | 'name' | 'version_id' | 'art_seed'>): number {
  const seed = instance.art_seed ?? 0;
  if (seed > 0) return seed >>> 0;
  return hashStr(`${instance.id}:${instance.name}:${instance.version_id}`) || 1;
}

export function nextArtSeed(seed: number): number {
  const next = Math.imul((seed >>> 0) ^ 0x9e3779b9, 2654435761) >>> 0;
  return next || 1;
}

function numericAnchorFrom(...values: Array<string | undefined | null>): string {
  for (const value of values) {
    const trimmed = value?.trim() ?? '';
    if (!trimmed) continue;
    if (/^\d+(?:\.\d+)*$/.test(trimmed)) return trimmed;
    const match = trimmed.match(/\d+(?:\.\d+)+/);
    if (match) return match[0];
  }
  return '';
}

function lifecycleTraitForVersion(version: VersionIdentitySource): VersionIdentity['lifecycleTrait'] {
  const labels = version.lifecycle?.labels ?? [];
  if (labels.includes('old_alpha')) return 'old_alpha';
  if (labels.includes('old_beta')) return 'old_beta';
  if (labels.includes('release_candidate')) return 'release_candidate';
  if (labels.includes('pre_release')) return 'pre_release';
  if (version.lifecycle?.channel === 'experimental') return 'experimental';
  if (labels.includes('snapshot') || version.lifecycle?.channel === 'preview') return 'snapshot';
  return null;
}

export function loaderTraitForComponentId(componentId: LoaderComponentId | string | undefined | null): VersionLoaderTrait | null {
  if (componentId === 'net.fabricmc.fabric-loader') return 'fabric';
  if (componentId === 'org.quiltmc.quilt-loader') return 'quilt';
  if (componentId === 'net.minecraftforge') return 'forge';
  if (componentId === 'net.neoforged') return 'neoforge';
  return null;
}

export function versionIdentityForVersionId(
  versionId: string,
  loaderTrait?: VersionLoaderTrait | null,
): VersionIdentity | null {
  const label = numericAnchorFrom(versionId);
  if (!label && !loaderTrait) return null;
  return { label, loaderTrait: loaderTrait ?? null };
}

export function versionIdentityForVersion(version: VersionIdentitySource | null | undefined): VersionIdentity | null {
  if (!version) return null;
  const label = numericAnchorFrom(
    version.minecraft_meta?.effective_version,
    version.minecraft_meta?.display_hint,
    version.minecraft_meta?.display_name,
    version.minecraft_meta?.base_id,
    version.id,
  );
  const lifecycleTrait = lifecycleTraitForVersion(version);
  const loaderTrait = loaderTraitForComponentId(version.loader?.component_id);
  if (!label && !lifecycleTrait && !loaderTrait) return null;
  return { label, lifecycleTrait, loaderTrait };
}

function measuredRenderSize(root: HTMLDivElement | null): ArtInput['renderSize'] {
  if (!root) return null;
  const rect = root.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0) return null;
  const ratio = Math.max(1, Math.min(window.devicePixelRatio || 1, 1.5));
  return {
    width: rect.width * ratio,
    height: rect.height * ratio,
  };
}

function drawArt(root: HTMLDivElement | null, canvas: HTMLCanvasElement | null, input: ArtInput): void {
  if (!canvas) return;
  const art = renderInstanceArt({ ...input, renderSize: input.renderSize ?? measuredRenderSize(root) });
  const ctx = canvas.getContext('2d');
  if (!ctx) return;
  canvas.width = art.width;
  canvas.height = art.height;
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.drawImage(art.source, 0, 0);
}

export function InstanceArt({
  instance,
  aspect = 'square',
  radius,
  className,
  style,
  version,
  versionIdentity,
}: {
  instance: Pick<Instance, 'id' | 'name' | 'version_id' | 'art_seed'>;
  aspect?: ArtAspect;
  radius?: number;
  className?: string;
  style?: JSX.CSSProperties;
  version?: Version | null;
  versionIdentity?: VersionIdentity | null;
}): JSX.Element {
  const theme = useTheme();
  const rootRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const seed = artSeedFor(instance);
  const preset = artPresetForSeed(seed);
  const classValue = `cp-instance-art cp-instance-art--${aspect}${className ? ` ${className}` : ''}`;
  const identity = aspect === 'square'
    ? versionIdentity ?? versionIdentityForVersion(version)
    : null;

  useLayoutEffect(() => {
    drawArt(rootRef.current, canvasRef.current, { seed, preset, dark: theme.dark, aspect, versionIdentity: identity });
  }, [seed, preset, theme.dark, aspect, identity?.label, identity?.lifecycleTrait, identity?.loaderTrait]);

  return (
    <div
      ref={rootRef}
      class={classValue}
      style={{ borderRadius: radius, ...style }}
      aria-hidden="true"
      data-preset={preset}
    >
      <canvas ref={canvasRef} />
    </div>
  );
}

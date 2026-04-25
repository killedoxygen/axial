import type { JSX } from 'preact';
import { useLayoutEffect, useRef } from 'preact/hooks';
import { useTheme } from '../hooks/use-theme';
import { hashStr } from '../tokens';
import type { Instance } from '../types';
import { renderInstanceArt } from './art-engine';
import './instance-art.css';

export type ArtPreset = 'aurora' | 'silk' | 'mineral' | 'ember' | 'vapor' | 'topo' | 'prism' | 'dune' | 'orbit';
export type ArtAspect = 'square' | 'banner' | 'thumb';

// Order is part of the deterministic seed contract. Keep this list in sync
// with `ART_PRESETS` in `core/config/src/instances/mod.rs`.
export const ART_PRESETS: ArtPreset[] = ['aurora', 'silk', 'mineral', 'ember', 'vapor', 'topo', 'prism', 'dune', 'orbit'];

interface ArtInput {
  seed: number;
  preset: ArtPreset;
  dark: boolean;
  aspect: ArtAspect;
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

function drawArt(canvas: HTMLCanvasElement | null, input: ArtInput): void {
  if (!canvas) return;
  const source = renderInstanceArt(input);
  const ctx = canvas.getContext('2d');
  if (!ctx) return;
  canvas.width = source.width;
  canvas.height = source.height;
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.drawImage(source, 0, 0);
}

export function InstanceArt({
  instance,
  aspect = 'square',
  radius,
  className,
  style,
}: {
  instance: Pick<Instance, 'id' | 'name' | 'version_id' | 'art_seed'>;
  aspect?: ArtAspect;
  radius?: number;
  className?: string;
  style?: JSX.CSSProperties;
}): JSX.Element {
  const theme = useTheme();
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const seed = artSeedFor(instance);
  const preset = artPresetForSeed(seed);
  const classValue = `cp-instance-art cp-instance-art--${aspect}${className ? ` ${className}` : ''}`;

  useLayoutEffect(() => {
    drawArt(canvasRef.current, { seed, preset, dark: theme.dark, aspect });
  }, [seed, preset, theme.dark, aspect]);

  return (
    <div
      class={classValue}
      style={{ borderRadius: radius, ...style }}
      aria-hidden="true"
      data-preset={preset}
    >
      <canvas ref={canvasRef} />
    </div>
  );
}

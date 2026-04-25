import type { JSX } from 'preact';
import { useLayoutEffect, useRef } from 'preact/hooks';
import { useTheme } from '../hooks/use-theme';
import { hashStr } from '../tokens';
import type { Instance } from '../types';
import { renderInstanceArt } from './art-engine';
import './instance-art.css';

export type ArtPreset = 'aurora' | 'silk' | 'mineral' | 'ember';
export type ArtAspect = 'square' | 'banner' | 'thumb';

export const ART_PRESETS: ArtPreset[] = ['aurora', 'silk', 'mineral', 'ember'];

interface ArtInput {
  seed: number;
  preset: ArtPreset;
  dark: boolean;
  aspect: ArtAspect;
}

function pickPreset(seed: number): ArtPreset {
  return ART_PRESETS[seed % ART_PRESETS.length];
}

export function artSeedFor(instance: Pick<Instance, 'id' | 'name' | 'version_id' | 'art_seed'>): number {
  const seed = instance.art_seed ?? 0;
  if (seed > 0) return seed >>> 0;
  return hashStr(`${instance.id}:${instance.name}:${instance.version_id}`) || 1;
}

export function artPresetFor(instance: Pick<Instance, 'art_preset'>, seed: number): ArtPreset {
  const preset = instance.art_preset;
  return ART_PRESETS.includes(preset as ArtPreset) ? preset as ArtPreset : pickPreset(seed);
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
  instance: Pick<Instance, 'id' | 'name' | 'version_id' | 'art_seed' | 'art_preset'>;
  aspect?: ArtAspect;
  radius?: number;
  className?: string;
  style?: JSX.CSSProperties;
}): JSX.Element {
  const theme = useTheme();
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const seed = artSeedFor(instance);
  const preset = artPresetFor(instance, seed);
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

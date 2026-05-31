import type { JSX } from 'preact';
import { useEffect, useMemo, useState } from 'preact/hooks';
import { hashStr } from '../tokens';
import './player-head.css';

type PixelTone = 'skin' | 'skinLight' | 'skinShade' | 'hair' | 'hairLight' | 'hairShade' | 'eye' | 'mouth';

interface HeadPalette {
  skin: string;
  skinLight: string;
  skinShade: string;
  hair: string;
  hairLight: string;
  hairShade: string;
  eye: string;
  mouth: string;
}

interface HeadPixel {
  x: number;
  y: number;
  tone: PixelTone;
}

const HAIR_HUES = [48, 36, 72, 28, 58, 82];
const EYE_HUES = [42, 118, 205, 225, 155];

function clampLightness(value: number): number {
  return Math.max(0.18, Math.min(0.86, value));
}

function color(l: number, c: number, h: number, alpha = 1): string {
  return `oklch(${clampLightness(l).toFixed(3)} ${c.toFixed(3)} ${h}${alpha < 1 ? ` / ${alpha}` : ''})`;
}

function buildPalette(hash: number): HeadPalette {
  const skinHue = 42 + ((hash >>> 3) % 20);
  const skinLightness = 0.61 + ((hash >>> 8) % 17) / 100;
  const skinChroma = 0.045 + ((hash >>> 15) % 4) / 1000;
  const hairHue = HAIR_HUES[(hash >>> 19) % HAIR_HUES.length];
  const hairLightness = 0.27 + ((hash >>> 23) % 19) / 100;
  const hairChroma = hairHue === 82 ? 0.075 : 0.055;
  const eyeHue = EYE_HUES[(hash >>> 11) % EYE_HUES.length];

  return {
    skin: color(skinLightness, skinChroma, skinHue),
    skinLight: color(skinLightness + 0.055, skinChroma * 0.74, skinHue),
    skinShade: color(skinLightness - 0.075, skinChroma, skinHue),
    hair: color(hairLightness, hairChroma, hairHue),
    hairLight: color(hairLightness + 0.07, hairChroma * 0.86, hairHue),
    hairShade: color(hairLightness - 0.075, hairChroma * 0.9, hairHue),
    eye: color(0.38, 0.06, eyeHue),
    mouth: color(skinLightness - 0.19, 0.07, 24),
  };
}

function pixelTone(x: number, y: number, hash: number): PixelTone {
  const fringe = ((hash >>> (x + 1)) & 1) === 1;

  if (y === 0) return x === 0 || x === 7 ? 'hairShade' : (x + hash) % 3 === 0 ? 'hairLight' : 'hair';
  if (y === 1) return x === 0 || x === 7 ? 'hairShade' : 'hair';
  if (y === 2 && (x === 0 || x === 7 || (x > 1 && x < 6 && fringe))) return x === 0 || x === 7 ? 'hairShade' : 'hair';
  if (y === 3 && (x === 0 || x === 7)) return 'hairShade';
  if (y === 3 && (x === 2 || x === 5)) return 'eye';
  if (y === 4 && (x === 0 || x === 7)) return 'hair';
  if (y === 4 && (x === 3 || x === 4)) return 'skinShade';
  if (y === 5 && (x === 3 || x === 4)) return 'mouth';
  if (y === 5 && (x === 1 || x === 6)) return 'skinShade';
  if (y === 5 && (x === 2 || x === 5)) return 'skinLight';
  if (y === 6 && (x === 0 || x === 7 || x === 3 || x === 4)) return 'skinShade';
  if (y === 7) return x === 0 || x === 7 ? 'skinShade' : 'skin';
  return 'skin';
}

function buildPixels(username: string): { palette: HeadPalette; pixels: HeadPixel[]; overlays: HeadPixel[] } {
  const hash = hashStr(username.trim().toLowerCase() || 'player');
  const palette = buildPalette(hash);
  const pixels: HeadPixel[] = [];
  const overlays: HeadPixel[] = [];

  for (let y = 0; y < 8; y += 1) {
    for (let x = 0; x < 8; x += 1) {
      pixels.push({ x, y, tone: pixelTone(x, y, hash) });
    }
  }

  for (let x = 1; x < 7; x += 1) {
    if (((hash >>> (x + 8)) & 1) === 1) overlays.push({ x, y: 0, tone: 'hairLight' });
  }
  overlays.push({ x: 0, y: 1, tone: 'hairShade' }, { x: 7, y: 1, tone: 'hairShade' });

  return { palette, pixels, overlays };
}

export function PlayerHeadPreview({
  username,
  src,
  textureSrc,
  size = 48,
  radius = 8,
  ariaLabel,
  title,
  class: className,
  style,
}: {
  username: string;
  src?: string;
  textureSrc?: string;
  size?: number | string;
  radius?: number;
  ariaLabel?: string;
  title?: string;
  class?: string;
  style?: JSX.CSSProperties;
}): JSX.Element {
  const { palette, pixels, overlays } = useMemo(() => buildPixels(username), [username]);
  const [imageFailed, setImageFailed] = useState(false);
  const [textureReady, setTextureReady] = useState(false);
  const [textureFailed, setTextureFailed] = useState(false);
  const dim = typeof size === 'number' ? `${size}px` : size;
  const imageSrc = src && !imageFailed ? src : null;
  const skinTextureSrc = textureSrc && textureReady && !textureFailed ? textureSrc : null;

  useEffect(() => {
    setImageFailed(false);
  }, [src]);

  useEffect(() => {
    setTextureReady(false);
    setTextureFailed(false);
  }, [textureSrc]);

  return (
    <div
      class={className ? `cp-player-head ${className}` : 'cp-player-head'}
      role={ariaLabel ? 'img' : undefined}
      aria-label={ariaLabel}
      aria-hidden={ariaLabel ? undefined : true}
      title={title}
      style={{ width: dim, height: dim, borderRadius: radius, ...style }}
    >
      {textureSrc && !textureReady && !textureFailed && (
        <img
          class="cp-player-head-preload"
          src={textureSrc}
          alt=""
          aria-hidden="true"
          draggable={false}
          onLoad={() => setTextureReady(true)}
          onError={() => setTextureFailed(true)}
        />
      )}
      {skinTextureSrc ? (
        <svg viewBox="0 0 8 8" width="100%" height="100%" preserveAspectRatio="none" shapeRendering="crispEdges">
          <image href={skinTextureSrc} x="-8" y="-8" width="64" height="64" />
          <image href={skinTextureSrc} x="-40" y="-8" width="64" height="64" />
          <rect x="0" y="0" width="8" height="8" fill="oklch(0.98 0.006 70 / 0.08)" />
          <rect x="0" y="7" width="8" height="1" fill="oklch(0.16 0.008 70 / 0.13)" />
        </svg>
      ) : imageSrc ? (
        <img
          src={imageSrc}
          alt=""
          aria-hidden="true"
          draggable={false}
          onError={() => setImageFailed(true)}
          style={{ display: 'block', width: '100%', height: '100%', objectFit: 'cover' }}
        />
      ) : (
        <svg viewBox="0 0 8 8" width="100%" height="100%" preserveAspectRatio="none" shapeRendering="crispEdges">
          {pixels.map(pixel => (
            <rect
              key={`${pixel.x}-${pixel.y}`}
              x={pixel.x}
              y={pixel.y}
              width="1"
              height="1"
              fill={palette[pixel.tone]}
            />
          ))}
          {overlays.map((pixel, index) => (
            <rect
              key={`overlay-${index}`}
              x={pixel.x}
              y={pixel.y}
              width="1"
              height="1"
              fill={palette[pixel.tone]}
              opacity="0.72"
            />
          ))}
          <rect x="0" y="0" width="8" height="8" fill="oklch(0.98 0.006 70 / 0.08)" />
          <rect x="0" y="7" width="8" height="1" fill="oklch(0.16 0.008 70 / 0.13)" />
        </svg>
      )}
    </div>
  );
}

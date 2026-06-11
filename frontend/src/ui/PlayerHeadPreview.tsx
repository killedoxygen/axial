import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { DEFAULT_SKINS } from '../default-skins';

export function PlayerHeadPreview({
  textureSrc,
  size = 48,
  radius = 8,
  ariaLabel,
  title,
  class: className,
  style,
}: {
  username?: string;
  textureSrc?: string;
  size?: number | string;
  radius?: number;
  ariaLabel?: string;
  title?: string;
  class?: string;
  style?: JSX.CSSProperties;
}): JSX.Element {
  const [failedTextureSrcs, setFailedTextureSrcs] = useState<Set<string>>(() => new Set());
  const fallbackSrc = DEFAULT_SKINS[0].src;
  const dim = typeof size === 'number' ? `${size}px` : size;
  const requestedSrc = textureSrc && !failedTextureSrcs.has(textureSrc) ? textureSrc : null;
  const skinTextureSrc = requestedSrc ?? fallbackSrc;
  const headSource = requestedSrc ? 'texture' : 'default';

  const markTextureFailed = (failedSrc: string): void => {
    if (failedSrc === fallbackSrc) return;
    setFailedTextureSrcs((current) => {
      if (current.has(failedSrc)) return current;
      const next = new Set(current);
      next.add(failedSrc);
      return next;
    });
  };

  return (
    <div
      class={className ? `cp-player-head ${className}` : 'cp-player-head'}
      data-player-head-source={headSource}
      role={ariaLabel ? 'img' : undefined}
      aria-label={ariaLabel}
      aria-hidden={ariaLabel ? undefined : true}
      title={title}
      style={{ width: dim, height: dim, borderRadius: radius, ...style }}
    >
      <div class="cp-player-head-texture" data-player-head-texture="minecraft" aria-hidden="true">
        <div class="cp-player-head-texture-layer cp-player-head-texture-face">
          <img
            class="cp-player-head-texture-img"
            src={skinTextureSrc}
            alt=""
            aria-hidden="true"
            draggable={false}
            data-player-head-texture-layer="face"
            onError={() => markTextureFailed(skinTextureSrc)}
          />
        </div>
        <div class="cp-player-head-texture-layer cp-player-head-texture-hat">
          <img
            class="cp-player-head-texture-img"
            src={skinTextureSrc}
            alt=""
            aria-hidden="true"
            draggable={false}
            data-player-head-texture-layer="hat"
            onError={() => markTextureFailed(skinTextureSrc)}
          />
        </div>
      </div>
    </div>
  );
}

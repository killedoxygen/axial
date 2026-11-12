import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { Icon, type IconName } from './Icons';

/**
 * A project's own art in a holder, or the glyph that stands for its kind when it has none or it fails to load.
 * Small pixel art stays crisp.
 */
export function Art({ url, icon, size }: { url?: string; icon: IconName; size?: number }): JSX.Element {
  const [failed, setFailed] = useState<string | null>(null);
  return (
    <span class="cp-art" style={size ? { '--art': `${size}px` } : undefined} aria-hidden="true">
      {url && failed !== url ? (
        <img
          src={url}
          alt=""
          loading="lazy"
          decoding="async"
          draggable={false}
          onLoad={(event) => {
            const image = event.currentTarget;
            if (image.naturalWidth <= 64) image.dataset.pixel = 'true';
            image.dataset.loaded = 'true';
          }}
          onError={() => setFailed(url)}
        />
      ) : (
        <Icon name={icon} size={16} />
      )}
    </span>
  );
}

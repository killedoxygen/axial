import type { JSX } from 'preact';
import { useEffect, useMemo, useState } from 'preact/hooks';
import { getSkinSnapshot, requestSkinSnapshot, subscribeSkinSnapshot, type SkinSnapshotInput } from './skin-snapshot';
import type { SkinVariant } from './types';

/** A skin as the app's own bust render, drawn once and cached. */
export function SkinSnapshotImg({
  cacheKey,
  src,
  variant,
  capeSrc,
  textureIdentity,
  capeIdentity,
  alt,
}: {
  cacheKey: string;
  src: string;
  variant: SkinVariant;
  capeSrc?: string;
  textureIdentity?: string;
  capeIdentity?: string;
  alt: string;
}): JSX.Element {
  const [, setVersion] = useState(0);
  const input = useMemo<SkinSnapshotInput>(
    () => ({ cacheKey, src, variant, capeSrc, textureIdentity, capeIdentity }),
    [cacheKey, capeIdentity, capeSrc, src, textureIdentity, variant],
  );
  const front = getSkinSnapshot(input);

  useEffect(() => {
    const update = (): void => setVersion((value) => value + 1);
    const unsubscribe = subscribeSkinSnapshot(input, update);
    requestSkinSnapshot(input, 4);
    update();
    return unsubscribe;
  }, [input]);

  return (
    <span class="cp-bust">
      {front.status === 'ready' ? (
        <img src={front.url} alt={alt} draggable={false} />
      ) : (
        <span data-snapshot={front.status === 'error' ? 'error' : 'loading'} role="img" aria-label={alt} />
      )}
    </span>
  );
}

import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { getSkinSnapshot, requestSkinSnapshot, subscribeSkinSnapshot, type SkinSnapshotInput } from './skin-snapshot';
import type { SkinVariant } from './types';

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
  const rootRef = useRef<HTMLSpanElement>(null);
  const [, setVersion] = useState(0);
  const input = useMemo<SkinSnapshotInput>(
    () => ({
      cacheKey,
      src,
      variant,
      capeSrc,
      textureIdentity,
      capeIdentity,
    }),
    [cacheKey, capeIdentity, capeSrc, src, textureIdentity, variant],
  );
  const front = getSkinSnapshot(input, 'front');
  const back = getSkinSnapshot(input, 'back');

  useEffect(() => {
    const update = (): void => setVersion((value) => value + 1);
    const unsubscribeFront = subscribeSkinSnapshot(input, 'front', update);
    const unsubscribeBack = subscribeSkinSnapshot(input, 'back', update);
    requestSkinSnapshot(input, 'front', 4);
    update();
    return () => {
      unsubscribeFront();
      unsubscribeBack();
    };
  }, [input]);

  const requestBack = (): void => {
    requestSkinSnapshot(input, 'back', 8);
  };

  useEffect(() => {
    const tile = rootRef.current?.closest('button.cp-skin-tile');
    if (!tile) return undefined;
    const onFocus = (): void => requestBack();
    tile.addEventListener('focus', onFocus);
    return () => tile.removeEventListener('focus', onFocus);
  }, [input]);

  return (
    <span
      ref={rootRef}
      class="cp-skin-tile__flip"
      data-front={front.status === 'ready' ? 'ready' : 'pending'}
      data-back={back.status === 'ready' ? 'ready' : 'none'}
      onPointerEnter={requestBack}
    >
      {front.status === 'ready' ? (
        <img class="cp-skin-tile__img" src={front.url} alt={alt} draggable={false} />
      ) : (
        <span
          class="cp-skin-tile__img"
          data-snapshot={front.status === 'error' ? 'error' : 'loading'}
          role="img"
          aria-label={alt}
        />
      )}
      {back.status === 'ready' && (
        <img
          class="cp-skin-tile__img cp-skin-tile__img--back"
          src={back.url}
          alt=""
          aria-hidden="true"
          draggable={false}
        />
      )}
    </span>
  );
}

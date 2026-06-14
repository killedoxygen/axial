import { h } from 'preact';
import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import type { SkinThreePreviewProps } from './SkinThreePreview';

type SkinThreePreviewComponent = (typeof import('./SkinThreePreview'))['SkinThreePreview'];

let loadedSkinThreePreview: SkinThreePreviewComponent | null = null;
let loadingSkinThreePreview: Promise<SkinThreePreviewComponent> | null = null;

function loadSkinThreePreview(): Promise<SkinThreePreviewComponent> {
  loadingSkinThreePreview ??= import('./SkinThreePreview')
    .then((module) => {
      loadedSkinThreePreview = module.SkinThreePreview;
      return module.SkinThreePreview;
    })
    .catch((err: unknown) => {
      loadingSkinThreePreview = null;
      throw err;
    });
  return loadingSkinThreePreview;
}

export function LazySkinThreePreview(props: SkinThreePreviewProps): JSX.Element {
  const [Preview, setPreview] = useState<SkinThreePreviewComponent | null>(() => loadedSkinThreePreview);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    if (Preview) return;
    let mounted = true;
    setFailed(false);
    void loadSkinThreePreview()
      .then((component) => {
        if (mounted) setPreview(() => component);
      })
      .catch(() => {
        if (mounted) setFailed(true);
      });
    return () => {
      mounted = false;
    };
  }, [Preview]);

  return Preview ? (
    h(Preview, props)
  ) : (
    <SkinThreePreviewFallback name={props.name} capeSrc={props.capeSrc} failed={failed} />
  );
}

function SkinThreePreviewFallback({
  name,
  capeSrc,
  failed,
}: {
  name: string;
  capeSrc?: string;
  failed: boolean;
}): JSX.Element {
  return (
    <div
      class="cp-skin-three cp-skin-three-sized"
      data-skin-three-preview={failed ? 'error' : 'loading'}
      data-skin-three-interaction="idle"
      data-skin-three-cape={capeSrc ? 'loading' : 'none'}
      data-skin-three-fit="pending"
      aria-label={`${name} 3D skin preview`}
    >
      <div class="cp-skin-three__status">{failed ? '3D preview unavailable' : 'Preparing 3D preview...'}</div>
    </div>
  );
}

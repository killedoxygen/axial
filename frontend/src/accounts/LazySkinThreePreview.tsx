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
    <div
      class="cp-skin-three"
      data-skin-three-preview={failed ? 'error' : 'loading'}
      aria-label={`${props.name} 3D skin preview`}
    >
      {failed && <div class="cp-skin-three__status">3D preview unavailable</div>}
    </div>
  );
}

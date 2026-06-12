import {
  addSceneLighting,
  buildSkinModel,
  loadBitmap,
  loadOptionalBitmap,
  type ThreeModule,
} from './SkinThreePreview';
import type { SkinVariant } from './types';

const SNAPSHOT_WIDTH = 320;
const SNAPSHOT_HEIGHT = 378;
const SNAPSHOT_FOV = 34;
const SNAPSHOT_ROTATION = -Math.PI / 9;
const SNAPSHOT_CENTER_Y = 21.4;
const SNAPSHOT_HALF_HEIGHT = 13.2;

export type SnapshotSide = 'front' | 'back';

interface SnapshotRig {
  THREE: ThreeModule;
  renderer: import('three').WebGLRenderer;
  canvas: HTMLCanvasElement;
}

let rigPromise: Promise<SnapshotRig> | null = null;
let queue: Promise<unknown> = Promise.resolve();
const cache = new Map<string, Promise<string>>();

async function snapshotRig(): Promise<SnapshotRig> {
  if (!rigPromise) {
    rigPromise = (async () => {
      const THREE = await import('three');
      const canvas = document.createElement('canvas');
      canvas.width = SNAPSHOT_WIDTH;
      canvas.height = SNAPSHOT_HEIGHT;
      const renderer = new THREE.WebGLRenderer({
        canvas,
        alpha: true,
        antialias: true,
        preserveDrawingBuffer: true,
      });
      renderer.outputColorSpace = THREE.SRGBColorSpace;
      renderer.setPixelRatio(1);
      renderer.setSize(SNAPSHOT_WIDTH, SNAPSHOT_HEIGHT, false);
      return { THREE, renderer, canvas };
    })();
  }
  return rigPromise;
}

async function renderSnapshot(
  src: string,
  variant: SkinVariant,
  capeSrc: string | undefined,
  side: SnapshotSide,
): Promise<string> {
  const { THREE, renderer, canvas } = await snapshotRig();
  const skinBitmap = await loadBitmap(src);
  const capeBitmap = await loadOptionalBitmap(capeSrc, 'cape snapshot');
  const disposables: Array<() => void> = [];

  try {
    const scene = new THREE.Scene();
    addSceneLighting(THREE, scene, disposables);

    const group = new THREE.Group();
    group.rotation.y = side === 'back' ? SNAPSHOT_ROTATION + Math.PI : SNAPSHOT_ROTATION;
    scene.add(group);
    const parts = buildSkinModel({
      THREE,
      group,
      skinBitmap,
      capeBitmap,
      variant,
      showOuterLayers: true,
      disposables,
    });
    parts.rightArm.rotation.x = 0.1;
    parts.leftArm.rotation.x = -0.1;
    parts.rightArm.rotation.z = 0.03;
    parts.leftArm.rotation.z = -0.03;
    parts.rightLeg.rotation.x = -0.06;
    parts.leftLeg.rotation.x = 0.06;

    const camera = new THREE.PerspectiveCamera(
      SNAPSHOT_FOV,
      SNAPSHOT_WIDTH / SNAPSHOT_HEIGHT,
      0.1,
      500,
    );
    const distance = SNAPSHOT_HALF_HEIGHT / Math.tan(THREE.MathUtils.degToRad(SNAPSHOT_FOV) / 2);
    camera.position.set(0, SNAPSHOT_CENTER_Y, distance);
    camera.lookAt(0, SNAPSHOT_CENTER_Y, 0);
    camera.updateProjectionMatrix();

    renderer.clear();
    renderer.render(scene, camera);
    return canvas.toDataURL('image/png');
  } finally {
    disposables.forEach((dispose) => dispose());
    skinBitmap.close();
    capeBitmap?.close();
  }
}

export function skinSnapshot(
  cacheKey: string,
  src: string,
  variant: SkinVariant,
  capeSrc?: string,
  side: SnapshotSide = 'front',
): Promise<string> {
  const key = side === 'back' ? `${cacheKey}:back` : cacheKey;
  const existing = cache.get(key);
  if (existing) return existing;

  const job = queue
    .catch(() => undefined)
    .then(() => renderSnapshot(src, variant, capeSrc, side));
  queue = job;
  cache.set(key, job);
  job.catch(() => {
    if (cache.get(key) === job) cache.delete(key);
  });
  return job;
}

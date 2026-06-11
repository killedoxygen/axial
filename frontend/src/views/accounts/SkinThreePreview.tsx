import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { Icon } from '../../ui/Icons';

export type ThreeModule = typeof import('three');
type SkinVariant = 'classic' | 'slim';
type SkinPreviewSide = 'front' | 'back';
type SkinPreviewBadgeState = 'previewing' | 'queued';
type SkinThreeCapeState = 'loading' | 'none' | 'loaded' | 'omitted';
type SkinThreeFitState = 'pending' | 'fitted';
const CLICK_PULSE_DURATION_MS = 420;
const DRAG_THRESHOLD_PX = 4;
const FIT_FOV_DEGREES = 34;
const FIT_ZOOM = 0.96;

interface SkinThreePreviewProps {
  src: string;
  capeSrc?: string;
  name: string;
  nametag?: string | null;
  onNametagEdit?: () => void;
  badge?: {
    state: SkinPreviewBadgeState;
    label: string;
  } | null;
  variant: SkinVariant;
  side: SkinPreviewSide;
  showOuterLayers: boolean;
}

interface Region {
  x: number;
  y: number;
  w: number;
  h: number;
}

interface FaceRegions {
  px: Region;
  nx: Region;
  py: Region;
  ny: Region;
  pz: Region;
  nz: Region;
}

interface SceneHandle {
  renderer: import('three').WebGLRenderer;
  dispose: () => void;
}

interface SkinThreeFitMetrics {
  overlayTopPx: number;
  hintBottomPx: number;
}

interface SkinThreeModelBounds {
  centerY: number;
  halfWidth: number;
  halfHeight: number;
}

function region(x: number, y: number, w: number, h: number): Region {
  return { x, y, w, h };
}

function headFaces(overlay: boolean): FaceRegions {
  const ox = overlay ? 32 : 0;
  return {
    px: region(ox + 16, 8, 8, 8),
    nx: region(ox, 8, 8, 8),
    py: region(ox + 8, 0, 8, 8),
    ny: region(ox + 16, 0, 8, 8),
    pz: region(ox + 8, 8, 8, 8),
    nz: region(ox + 24, 8, 8, 8),
  };
}

function bodyFaces(overlay: boolean): FaceRegions {
  const y = overlay ? 36 : 20;
  const topY = overlay ? 32 : 16;
  return {
    px: region(28, y, 4, 12),
    nx: region(16, y, 4, 12),
    py: region(20, topY, 8, 4),
    ny: region(28, topY, 8, 4),
    pz: region(20, y, 8, 12),
    nz: region(32, y, 8, 12),
  };
}

function armFaces(frontX: number, rowX: number, topY: number, rowY: number, armWidth: number): FaceRegions {
  return {
    px: region(frontX + armWidth, rowY, 4, 12),
    nx: region(rowX, rowY, 4, 12),
    py: region(frontX, topY, armWidth, 4),
    ny: region(frontX + armWidth, topY, armWidth, 4),
    pz: region(frontX, rowY, armWidth, 12),
    nz: region(frontX + armWidth + 4, rowY, armWidth, 12),
  };
}

function legFaces(frontX: number, rowX: number, topY: number, rowY: number): FaceRegions {
  return {
    px: region(frontX + 4, rowY, 4, 12),
    nx: region(rowX, rowY, 4, 12),
    py: region(frontX, topY, 4, 4),
    ny: region(frontX + 4, topY, 4, 4),
    pz: region(frontX, rowY, 4, 12),
    nz: region(frontX + 8, rowY, 4, 12),
  };
}

const textureBlobCache = new Map<string, Promise<Blob>>();

function fetchTextureBlob(src: string): Promise<Blob> {
  const immutable = src.startsWith('data:') || src.includes('/skins/');
  const cached = immutable ? textureBlobCache.get(src) : undefined;
  if (cached) return cached;

  const pending = fetch(src).then((response) => {
    if (!response.ok) throw new Error(`texture HTTP ${response.status}`);
    return response.blob();
  });
  if (immutable) {
    textureBlobCache.set(src, pending);
    pending.catch(() => {
      if (textureBlobCache.get(src) === pending) textureBlobCache.delete(src);
    });
  }
  return pending;
}

export function loadBitmap(src: string): Promise<ImageBitmap> {
  return fetchTextureBlob(src).then((blob) => createImageBitmap(blob));
}

export async function loadOptionalBitmap(src: string | undefined, label: string): Promise<ImageBitmap | null> {
  if (!src) return null;
  try {
    return await loadBitmap(src);
  } catch (err) {
    console.warn(`Could not load optional ${label} texture for 3D skin preview.`, err);
    return null;
  }
}

function textureFromRegion(
  THREE: ThreeModule,
  image: ImageBitmap,
  source: Region,
  transparent: boolean,
): { texture: import('three').CanvasTexture; material: import('three').MeshLambertMaterial } {
  const canvas = document.createElement('canvas');
  canvas.width = Math.max(1, source.w);
  canvas.height = Math.max(1, source.h);
  const ctx = canvas.getContext('2d');
  if (!ctx) throw new Error('Could not create skin preview texture');
  ctx.imageSmoothingEnabled = false;
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.drawImage(image, source.x, source.y, source.w, source.h, 0, 0, source.w, source.h);

  const texture = new THREE.CanvasTexture(canvas);
  texture.colorSpace = THREE.SRGBColorSpace;
  texture.magFilter = THREE.NearestFilter;
  texture.minFilter = THREE.NearestFilter;
  texture.needsUpdate = true;

  return {
    texture,
    material: new THREE.MeshLambertMaterial({
      map: texture,
      transparent,
      alphaTest: transparent ? 0.1 : 0,
      side: THREE.FrontSide,
      emissive: new THREE.Color(0x101010),
      emissiveIntensity: 0.16,
    }),
  };
}

function addBox({
  THREE,
  group,
  image,
  faces,
  size,
  position,
  transparent,
  disposables,
}: {
  THREE: ThreeModule;
  group: import('three').Group;
  image: ImageBitmap;
  faces: FaceRegions;
  size: [number, number, number];
  position: [number, number, number];
  transparent: boolean;
  disposables: Array<() => void>;
}): void {
  const faceOrder: Region[] = [faces.px, faces.nx, faces.py, faces.ny, faces.pz, faces.nz];
  const materialPairs = faceOrder.map((face) => textureFromRegion(THREE, image, face, transparent));
  const geometry = new THREE.BoxGeometry(size[0], size[1], size[2]);
  const mesh = new THREE.Mesh(geometry, materialPairs.map((pair) => pair.material));
  mesh.position.set(position[0], position[1], position[2]);
  group.add(mesh);
  disposables.push(() => {
    geometry.dispose();
    for (const pair of materialPairs) {
      pair.texture.dispose();
      pair.material.dispose();
    }
  });
}

function addCape({
  THREE,
  group,
  image,
  disposables,
}: {
  THREE: ThreeModule;
  group: import('three').Group;
  image: ImageBitmap;
  disposables: Array<() => void>;
}): void {
  const { texture, material } = textureFromRegion(THREE, image, region(1, 1, 10, 16), true);
  material.side = THREE.DoubleSide;
  const geometry = new THREE.PlaneGeometry(10, 16);
  const mesh = new THREE.Mesh(geometry, material);
  mesh.position.set(0, 16, -3.05);
  mesh.rotation.x = -0.06;
  group.add(mesh);
  disposables.push(() => {
    geometry.dispose();
    texture.dispose();
    material.dispose();
  });
}

export function addSceneLighting(
  THREE: ThreeModule,
  scene: import('three').Scene,
  disposables: Array<() => void>,
): void {
  const ambient = new THREE.AmbientLight(0xffffff, 1.12);
  const key = new THREE.DirectionalLight(0xffffff, 1.45);
  const fill = new THREE.DirectionalLight(0xffffff, 0.32);
  key.position.set(-28, 48, 36);
  fill.position.set(30, 22, -28);
  scene.add(ambient, key, fill);
  disposables.push(() => scene.remove(ambient, key, fill));
}

function addFloorSpotlight(
  THREE: ThreeModule,
  scene: import('three').Scene,
  disposables: Array<() => void>,
): void {
  const canvas = document.createElement('canvas');
  canvas.width = 256;
  canvas.height = 256;
  const ctx = canvas.getContext('2d');
  if (!ctx) return;

  const shadow = ctx.createRadialGradient(128, 128, 6, 128, 128, 112);
  shadow.addColorStop(0, 'rgba(0, 0, 0, 0.34)');
  shadow.addColorStop(0.55, 'rgba(0, 0, 0, 0.16)');
  shadow.addColorStop(1, 'rgba(0, 0, 0, 0)');
  ctx.fillStyle = shadow;
  ctx.fillRect(0, 0, canvas.width, canvas.height);

  const texture = new THREE.CanvasTexture(canvas);
  texture.colorSpace = THREE.SRGBColorSpace;
  texture.needsUpdate = true;

  const geometry = new THREE.PlaneGeometry(24, 13);
  const material = new THREE.MeshBasicMaterial({
    map: texture,
    transparent: true,
    depthWrite: false,
    side: THREE.DoubleSide,
  });
  const mesh = new THREE.Mesh(geometry, material);
  mesh.position.set(0, -0.25, 0);
  mesh.rotation.x = -Math.PI / 2;
  scene.add(mesh);
  disposables.push(() => {
    scene.remove(mesh);
    geometry.dispose();
    texture.dispose();
    material.dispose();
  });
}

export interface SkinModelParts {
  rightArm: import('three').Group;
  leftArm: import('three').Group;
  rightLeg: import('three').Group;
  leftLeg: import('three').Group;
}

export function buildSkinModel({
  THREE,
  group,
  skinBitmap,
  capeBitmap,
  variant,
  showOuterLayers,
  disposables,
}: {
  THREE: ThreeModule;
  group: import('three').Group;
  skinBitmap: ImageBitmap;
  capeBitmap: ImageBitmap | null;
  variant: SkinVariant;
  showOuterLayers: boolean;
  disposables: Array<() => void>;
}): SkinModelParts {
  const armWidth = variant === 'slim' ? 3 : 4;
  const armX = 4 + armWidth / 2;

  const limbPivot = (x: number, y: number): import('three').Group => {
    const pivot = new THREE.Group();
    pivot.position.set(x, y, 0);
    group.add(pivot);
    disposables.push(() => group.remove(pivot));
    return pivot;
  };
  const rightArm = limbPivot(-armX, 22);
  const leftArm = limbPivot(armX, 22);
  const rightLeg = limbPivot(-2, 12);
  const leftLeg = limbPivot(2, 12);

  addBox({ THREE, group, image: skinBitmap, faces: headFaces(false), size: [8, 8, 8], position: [0, 28, 0], transparent: false, disposables });
  addBox({ THREE, group, image: skinBitmap, faces: bodyFaces(false), size: [8, 12, 4], position: [0, 18, 0], transparent: false, disposables });
  addBox({ THREE, group: rightArm, image: skinBitmap, faces: armFaces(44, 40, 16, 20, armWidth), size: [armWidth, 12, 4], position: [0, -4, 0], transparent: false, disposables });
  addBox({ THREE, group: leftArm, image: skinBitmap, faces: armFaces(36, 32, 48, 52, armWidth), size: [armWidth, 12, 4], position: [0, -4, 0], transparent: false, disposables });
  addBox({ THREE, group: rightLeg, image: skinBitmap, faces: legFaces(4, 0, 16, 20), size: [4, 12, 4], position: [0, -6, 0], transparent: false, disposables });
  addBox({ THREE, group: leftLeg, image: skinBitmap, faces: legFaces(20, 16, 48, 52), size: [4, 12, 4], position: [0, -6, 0], transparent: false, disposables });

  if (showOuterLayers) {
    addBox({ THREE, group, image: skinBitmap, faces: headFaces(true), size: [8.7, 8.7, 8.7], position: [0, 28, 0], transparent: true, disposables });
    addBox({ THREE, group, image: skinBitmap, faces: bodyFaces(true), size: [8.55, 12.55, 4.55], position: [0, 18, 0], transparent: true, disposables });
    addBox({ THREE, group: rightArm, image: skinBitmap, faces: armFaces(44, 40, 32, 36, armWidth), size: [armWidth + 0.5, 12.5, 4.5], position: [0, -4, 0], transparent: true, disposables });
    addBox({ THREE, group: leftArm, image: skinBitmap, faces: armFaces(52, 48, 48, 52, armWidth), size: [armWidth + 0.5, 12.5, 4.5], position: [0, -4, 0], transparent: true, disposables });
    addBox({ THREE, group: rightLeg, image: skinBitmap, faces: legFaces(4, 0, 32, 36), size: [4.5, 12.5, 4.5], position: [0, -6, 0], transparent: true, disposables });
    addBox({ THREE, group: leftLeg, image: skinBitmap, faces: legFaces(4, 0, 48, 52), size: [4.5, 12.5, 4.5], position: [0, -6, 0], transparent: true, disposables });
  }

  if (capeBitmap) {
    addCape({ THREE, group, image: capeBitmap, disposables });
  }

  return { rightArm, leftArm, rightLeg, leftLeg };
}

function modelBounds(props: SkinThreePreviewProps): SkinThreeModelBounds {
  const armWidth = props.variant === 'slim' ? 3 : 4;
  const armX = 4 + armWidth / 2;
  const outerAllowance = props.showOuterLayers ? 0.55 : 0;
  const modelWidth = Math.max(8 + outerAllowance, (armX * 2) + armWidth + outerAllowance);
  const modelDepth = props.showOuterLayers ? 8.7 : 8;
  const modelHeight = props.showOuterLayers ? 32.7 : 32;

  return {
    centerY: modelHeight / 2,
    halfWidth: Math.sqrt((modelWidth / 2) ** 2 + (modelDepth / 2) ** 2),
    halfHeight: modelHeight / 2,
  };
}

function fitCameraToCanvas({
  THREE,
  camera,
  canvas,
  bounds,
  hasOverlay,
}: {
  THREE: ThreeModule;
  camera: import('three').PerspectiveCamera;
  canvas: HTMLCanvasElement;
  bounds: SkinThreeModelBounds;
  hasOverlay: boolean;
}): SkinThreeFitMetrics {
  const width = Math.max(1, Math.round(canvas.getBoundingClientRect().width));
  const height = Math.max(1, Math.round(canvas.getBoundingClientRect().height));
  const aspect = width / height;
  const topPadding = hasOverlay ? 0.2 : 0.12;
  const bottomPadding = 0.18;
  const sidePadding = 0.12;
  const usableWidth = Math.max(width * (1 - sidePadding * 2), 1);
  const usableHeight = Math.max(height * (1 - topPadding - bottomPadding), 1);
  const verticalFov = THREE.MathUtils.degToRad(FIT_FOV_DEGREES);
  const horizontalFov = 2 * Math.atan(Math.tan(verticalFov / 2) * aspect);
  const paddedHalfHeight = bounds.halfHeight * (height / usableHeight);
  const paddedHalfWidth = bounds.halfWidth * (width / usableWidth);
  const distance = Math.max(
    paddedHalfHeight / Math.tan(verticalFov / 2),
    paddedHalfWidth / Math.tan(horizontalFov / 2),
  ) / FIT_ZOOM;
  const visibleHalfHeight = distance * Math.tan(verticalFov / 2);
  const targetY = bounds.centerY - ((bottomPadding - topPadding) * visibleHalfHeight);

  camera.fov = FIT_FOV_DEGREES;
  camera.aspect = aspect;
  camera.position.set(0, targetY, distance);
  camera.lookAt(0, targetY, 0);
  camera.updateProjectionMatrix();

  const projectY = (worldY: number): number => {
    const normalized = (worldY - targetY) / distance / Math.max(Math.tan(verticalFov / 2), 0.001);
    return THREE.MathUtils.clamp(((1 - normalized) / 2) * height, 0, height);
  };
  const modelTop = projectY(bounds.centerY + bounds.halfHeight);
  const modelBottom = projectY(bounds.centerY - bounds.halfHeight);

  return {
    overlayTopPx: Math.round(THREE.MathUtils.clamp(modelTop - 4, 8, Math.max(8, height * 0.18))),
    hintBottomPx: Math.round(THREE.MathUtils.clamp(height - modelBottom + 8, 8, 18)),
  };
}

async function setupScene(
  canvas: HTMLCanvasElement,
  props: SkinThreePreviewProps,
  setReady: (ready: boolean) => void,
  setInteracting: (interacting: boolean) => void,
  setCapeState: (state: SkinThreeCapeState) => void,
  setFitState: (state: SkinThreeFitState) => void,
  setFitMetrics: (metrics: SkinThreeFitMetrics) => void,
): Promise<SceneHandle> {
  const THREE = await import('three');
  const disposables: Array<() => void> = [];
  const skinBitmap = await loadBitmap(props.src);
  const capeBitmap = await loadOptionalBitmap(props.capeSrc, 'cape');
  setCapeState(props.capeSrc ? capeBitmap ? 'loaded' : 'omitted' : 'none');
  const renderer = new THREE.WebGLRenderer({
    canvas,
    alpha: true,
    antialias: true,
    preserveDrawingBuffer: true,
  });
  renderer.outputColorSpace = THREE.SRGBColorSpace;
  renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));

  const scene = new THREE.Scene();
  const camera = new THREE.PerspectiveCamera(FIT_FOV_DEGREES, 1, 0.1, 500);
  addSceneLighting(THREE, scene, disposables);
  addFloorSpotlight(THREE, scene, disposables);

  const group = new THREE.Group();
  group.rotation.y = props.side === 'back' ? Math.PI - 0.22 : 0.22;
  scene.add(group);

  const parts = buildSkinModel({
    THREE,
    group,
    skinBitmap,
    capeBitmap,
    variant: props.variant,
    showOuterLayers: props.showOuterLayers,
    disposables,
  });

  let frame = 0;
  let dragging = false;
  let hasDragged = false;
  let pointerStartX = 0;
  let pointerStartY = 0;
  let dragStartX = 0;
  let dragStartRotation = 0;
  let modelRotation = group.rotation.y;
  let clickPulseStart = -CLICK_PULSE_DURATION_MS;
  let interactionTimeout: number | null = null;
  const bounds = modelBounds(props);

  function resize(): void {
    const rect = canvas.getBoundingClientRect();
    const width = Math.max(1, Math.round(rect.width));
    const height = Math.max(1, Math.round(rect.height));
    renderer.setSize(width, height, false);
    setFitMetrics(fitCameraToCanvas({
      THREE,
      camera,
      canvas,
      bounds,
      hasOverlay: Boolean(props.badge || props.nametag),
    }));
    setFitState('fitted');
  }

  function render(time = 0): void {
    const pulseElapsed = time - clickPulseStart;
    const pulseProgress = pulseElapsed >= 0 && pulseElapsed < CLICK_PULSE_DURATION_MS
      ? pulseElapsed / CLICK_PULSE_DURATION_MS
      : 1;
    const pulse = pulseProgress < 1 ? Math.sin(pulseProgress * Math.PI) : 0;
    const pulseWobble = pulseProgress < 1 ? Math.sin(pulseProgress * Math.PI * 2) : 0;
    if (!dragging) {
      group.rotation.y = modelRotation + Math.sin(time / 1800) * 0.05;
    }
    const limbPhase = Math.sin(time / 520);
    group.position.y = dragging ? 0 : Math.abs(Math.cos(time / 520)) * 0.22 - 0.11;
    parts.rightArm.rotation.x = limbPhase * 0.34;
    parts.leftArm.rotation.x = -limbPhase * 0.34;
    parts.rightArm.rotation.z = 0.02 + limbPhase * 0.015;
    parts.leftArm.rotation.z = -0.02 - limbPhase * 0.015;
    parts.rightLeg.rotation.x = -limbPhase * 0.26;
    parts.leftLeg.rotation.x = limbPhase * 0.26;
    group.rotation.z = pulseWobble * 0.035;
    group.position.x = pulseWobble * 0.22;
    group.scale.set(1 - pulse * 0.012, 1 + pulse * 0.026, 1);
    renderer.render(scene, camera);
      frame = window.requestAnimationFrame(render);
  }

  const resizeObserver = new ResizeObserver(resize);
  resizeObserver.observe(canvas);
  resize();
  render();
  setReady(true);

  const onPointerDown = (event: PointerEvent): void => {
    dragging = true;
    hasDragged = false;
    pointerStartX = event.clientX;
    pointerStartY = event.clientY;
    dragStartX = event.clientX;
    dragStartRotation = modelRotation;
    canvas.setPointerCapture(event.pointerId);
  };
  const onPointerMove = (event: PointerEvent): void => {
    if (!dragging) return;
    if (
      Math.abs(event.clientX - pointerStartX) > DRAG_THRESHOLD_PX ||
      Math.abs(event.clientY - pointerStartY) > DRAG_THRESHOLD_PX
    ) {
      hasDragged = true;
    }
    modelRotation = dragStartRotation + (event.clientX - dragStartX) / 90;
    group.rotation.y = modelRotation;
    renderer.render(scene, camera);
  };

  const startClickPulse = (): void => {
    clickPulseStart = performance.now();
    setInteracting(true);
    if (interactionTimeout !== null) {
      window.clearTimeout(interactionTimeout);
    }
    interactionTimeout = window.setTimeout(() => {
      interactionTimeout = null;
      setInteracting(false);
    }, CLICK_PULSE_DURATION_MS);
  };

  const onPointerUp = (event: PointerEvent): void => {
    dragging = false;
    if (canvas.hasPointerCapture(event.pointerId)) {
      canvas.releasePointerCapture(event.pointerId);
    }
    if (!hasDragged) {
      startClickPulse();
    }
  };

  const onPointerCancel = (event: PointerEvent): void => {
    dragging = false;
    hasDragged = false;
    if (canvas.hasPointerCapture(event.pointerId)) {
      canvas.releasePointerCapture(event.pointerId);
    }
  };

  canvas.addEventListener('pointerdown', onPointerDown);
  canvas.addEventListener('pointermove', onPointerMove);
  canvas.addEventListener('pointerup', onPointerUp);
  canvas.addEventListener('pointercancel', onPointerCancel);

  return {
    renderer,
    dispose: () => {
      window.cancelAnimationFrame(frame);
      resizeObserver.disconnect();
      canvas.removeEventListener('pointerdown', onPointerDown);
      canvas.removeEventListener('pointermove', onPointerMove);
      canvas.removeEventListener('pointerup', onPointerUp);
      canvas.removeEventListener('pointercancel', onPointerCancel);
      if (interactionTimeout !== null) {
        window.clearTimeout(interactionTimeout);
      }
      disposables.forEach((dispose) => dispose());
      renderer.dispose();
      skinBitmap.close();
      capeBitmap?.close();
    },
  };
}

export function SkinThreePreview(props: SkinThreePreviewProps): JSX.Element {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [ready, setReady] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [interacting, setInteracting] = useState(false);
  const [capeState, setCapeState] = useState<SkinThreeCapeState>(props.capeSrc ? 'loading' : 'none');
  const [fitState, setFitState] = useState<SkinThreeFitState>('pending');
  const [fitMetrics, setFitMetrics] = useState<SkinThreeFitMetrics>({ overlayTopPx: 10, hintBottomPx: 8 });

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return undefined;
    let active = true;
    let handle: SceneHandle | null = null;
    setReady(false);
    setError(null);
    setInteracting(false);
    setCapeState(props.capeSrc ? 'loading' : 'none');
    setFitState('pending');

    void setupScene(canvas, props, (nextReady) => {
      if (active) setReady(nextReady);
    }, (nextInteracting) => {
      if (active) setInteracting(nextInteracting);
    }, (nextCapeState) => {
      if (active) setCapeState(nextCapeState);
    }, (nextFitState) => {
      if (active) setFitState(nextFitState);
    }, (nextFitMetrics) => {
      if (active) setFitMetrics(nextFitMetrics);
    })
      .then((nextHandle) => {
        if (!active) {
          nextHandle.dispose();
          return;
        }
        handle = nextHandle;
      })
      .catch((err: unknown) => {
        if (!active) return;
        setError(err instanceof Error ? err.message : '3D preview failed');
      });

    return () => {
      active = false;
      handle?.dispose();
    };
  }, [props.src, props.capeSrc, props.name, props.variant, props.side, props.showOuterLayers]);

  return (
    <div
      class="cp-skin-three"
      data-skin-three-preview={ready ? 'ready' : error ? 'error' : 'loading'}
      data-skin-three-interaction={interacting ? 'active' : 'idle'}
      data-skin-three-cape={capeState}
      data-skin-three-fit={fitState}
      aria-label={`${props.name} 3D skin preview`}
      style={{
        '--skin-three-overlay-top': `${fitMetrics.overlayTopPx}px`,
        '--skin-three-hint-bottom': `${fitMetrics.hintBottomPx}px`,
      } as JSX.CSSProperties}
    >
      <canvas ref={canvasRef} aria-hidden="true" />
      {!ready && !error && (
        <div class="cp-skin-three__status">Loading 3D preview...</div>
      )}
      {error && (
        <div class="cp-skin-three__status">3D preview unavailable</div>
      )}
      {(props.badge || props.nametag) && (
        <div class="cp-skin-three__overlays">
          {props.badge && (
            <div
              class="cp-skin-three__badge"
              data-skin-three-badge={props.badge.state}
              title={props.badge.state === 'queued' ? 'Queued for Minecraft profile apply' : 'Preview selection differs from the equipped skin'}
            >
              {props.badge.label}
            </div>
          )}
          {props.nametag && (
            props.onNametagEdit ? (
              <button
                type="button"
                class="cp-skin-three__nametag cp-skin-nametag cp-skin-nametag--editable"
                title="Rename player"
                aria-label={`Rename player ${props.nametag}`}
                data-skin-three-nametag="visible"
                onClick={props.onNametagEdit}
              >
                <span>{props.nametag}</span>
                <Icon name="edit" size={11} />
              </button>
            ) : (
              <div
                class="cp-skin-three__nametag cp-skin-nametag"
                title="Active player"
                aria-label={`Active player: ${props.nametag}`}
                data-skin-three-nametag="visible"
              >
                {props.nametag}
              </div>
            )
          )}
        </div>
      )}
      <div class="cp-skin-three__hint" data-skin-three-hint="drag-rotate" aria-hidden="true">
        <span class="cp-skin-three__hint-icons">
          <Icon name="arrow-left" size={10} />
          <Icon name="arrow-right" size={10} />
        </span>
        <span>Drag to rotate</span>
      </div>
    </div>
  );
}

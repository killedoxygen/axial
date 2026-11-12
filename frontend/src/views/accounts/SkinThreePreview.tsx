import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import {
  addFloorSpotlight,
  addSceneLighting,
  buildSkinModel,
  modelBounds,
  type SkinModelBounds,
  type SkinModelParts,
} from './skin-model';
import { loadThree, type ThreeModule } from './skin-three-loader';
import { loadOptionalBitmap, loadSkinBitmap } from './skin-textures';
import type { SkinVariant } from './types';

type SkinPreviewSide = 'front' | 'back';
const CLICK_PULSE_DURATION_MS = 420;
const DRAG_THRESHOLD_PX = 4;
const FIT_FOV_DEGREES = 34;
const FIT_ZOOM = 0.96;
const OVERLAY_MODEL_GAP_PX = 14;
const NAMETAG_HEIGHT_PX = 26;
const ASSEMBLE_MS = 900;
const ASSEMBLE_MARGIN = 2;

export interface SkinThreeFitPadding {
  top?: number;
  bottom?: number;
  side?: number;
}

export interface SkinThreePreviewProps {
  src: string;
  capeSrc?: string;
  name: string;
  nametag?: string | null;
  variant: SkinVariant;
  side: SkinPreviewSide;
  showOuterLayers: boolean;
  fitPadding?: SkinThreeFitPadding;
  /**
   * Staged mode: the model stands as a flat outline, in the canvas's CSS `color`, until this is
   * true, then assembles from the feet up. A new skin always assembles from the outline.
   */
  built?: boolean;
}

interface SceneHandle {
  setBuilt: (built: boolean) => void;
  setLook: (look: SkinLook) => Promise<void>;
  dispose: () => void;
}

/** Resolves any CSS colour, including oklch, through a 2D canvas. */
function cssColor(THREE: ThreeModule, element: Element): import('three').Color {
  const color = new THREE.Color(0x404040);
  const ctx = document.createElement('canvas').getContext('2d', { willReadFrequently: true });
  if (!ctx) return color;
  ctx.fillStyle = getComputedStyle(element).color;
  ctx.fillRect(0, 0, 1, 1);
  const [r, g, b] = ctx.getImageData(0, 0, 1, 1).data;
  return color.setRGB(r / 255, g / 255, b / 255, THREE.SRGBColorSpace);
}

interface SkinThreeFitMetrics {
  overlayTopPx: number;
}

function fitCameraToCanvas({
  THREE,
  camera,
  canvas,
  bounds,
  overlayHeightPx,
  fitPadding,
}: {
  THREE: ThreeModule;
  camera: import('three').PerspectiveCamera;
  canvas: HTMLCanvasElement;
  bounds: SkinModelBounds;
  overlayHeightPx: number;
  fitPadding?: SkinThreeFitPadding;
}): SkinThreeFitMetrics {
  const width = Math.max(1, Math.round(canvas.getBoundingClientRect().width));
  const height = Math.max(1, Math.round(canvas.getBoundingClientRect().height));
  const aspect = width / height;
  const hasOverlay = overlayHeightPx > 0;
  const topPadding = fitPadding?.top ?? (hasOverlay ? 0.2 : 0.12);
  const bottomPadding = fitPadding?.bottom ?? 0.18;
  const sidePadding = fitPadding?.side ?? 0.12;
  const usableWidth = Math.max(width * (1 - sidePadding * 2), 1);
  const usableHeight = Math.max(height * (1 - topPadding - bottomPadding), 1);
  const verticalFov = THREE.MathUtils.degToRad(FIT_FOV_DEGREES);
  const horizontalFov = 2 * Math.atan(Math.tan(verticalFov / 2) * aspect);
  const paddedHalfHeight = bounds.halfHeight * (height / usableHeight);
  const paddedHalfWidth = bounds.halfWidth * (width / usableWidth);
  const distance =
    Math.max(paddedHalfHeight / Math.tan(verticalFov / 2), paddedHalfWidth / Math.tan(horizontalFov / 2)) / FIT_ZOOM;
  const visibleHalfHeight = distance * Math.tan(verticalFov / 2);
  const targetY = bounds.centerY - (bottomPadding - topPadding) * visibleHalfHeight;

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

  return {
    overlayTopPx: Math.round(
      THREE.MathUtils.clamp(modelTop - overlayHeightPx - OVERLAY_MODEL_GAP_PX, 8, Math.max(8, height * 0.18)),
    ),
  };
}

function estimateOverlayHeightPx(props: SkinThreePreviewProps): number {
  return props.nametag ? NAMETAG_HEIGHT_PX : 0;
}

/** What a figure wears; the rest of the scene outlives it. */
interface SkinLook {
  src: string;
  capeSrc?: string;
  variant: SkinVariant;
}

interface LoadedLook {
  skin: ImageBitmap;
  cape: ImageBitmap | null;
  variant: SkinVariant;
}

function lookKey(look: SkinLook): string {
  return `${look.src}|${look.capeSrc ?? ''}|${look.variant}`;
}

async function loadLook(look: SkinLook): Promise<LoadedLook> {
  const [skin, cape] = await Promise.all([loadSkinBitmap(look.src), loadOptionalBitmap(look.capeSrc, 'cape')]);
  return { skin, cape, variant: look.variant };
}

async function setupScene(
  canvas: HTMLCanvasElement,
  props: () => SkinThreePreviewProps,
  setFitMetrics: (metrics: SkinThreeFitMetrics) => void,
  setError: (error: boolean) => void,
): Promise<SceneHandle> {
  const first = props();
  const THREE = await loadThree();
  const disposables: Array<() => void> = [];
  const firstLook = await loadLook(first);
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
  const floor = addFloorSpotlight(THREE, scene, disposables);

  const group = new THREE.Group();
  group.rotation.y = first.side === 'back' ? Math.PI - 0.22 : 0.22;
  scene.add(group);

  // The figure itself: rebuilt in place when the look changes, so the scene, its motion and the canvas stay.
  let worn: { key: string; parts: SkinModelParts; release: () => void } | null = null;
  let bounds: SkinModelBounds = modelBounds({ variant: first.variant, showOuterLayers: first.showOuterLayers });
  const wear = (key: string, look: LoadedLook): void => {
    const body = new THREE.Group();
    group.add(body);
    const owned: Array<() => void> = [() => group.remove(body)];
    const parts = buildSkinModel({
      THREE,
      group: body,
      skinBitmap: look.skin,
      capeBitmap: look.cape,
      variant: look.variant,
      showOuterLayers: first.showOuterLayers,
      disposables: owned,
    });
    worn?.release();
    worn = {
      key,
      parts,
      release: () => {
        owned.forEach((dispose) => dispose());
        look.skin.close();
        look.cape?.close();
      },
    };
    bounds = modelBounds({ variant: look.variant, showOuterLayers: first.showOuterLayers });
  };
  wear(lookKey(first), firstLook);

  let frame = 0;
  let dragging = false;
  let hasDragged = false;
  let pointerStartX = 0;
  let pointerStartY = 0;
  let dragStartX = 0;
  let dragStartRotation = 0;
  let modelRotation = group.rotation.y;
  let pulseStart = -CLICK_PULSE_DURATION_MS;

  function resize(): void {
    const rect = canvas.getBoundingClientRect();
    const width = Math.max(1, Math.round(rect.width));
    const height = Math.max(1, Math.round(rect.height));
    renderer.setSize(width, height, false);
    const current = props();
    setFitMetrics(
      fitCameraToCanvas({
        THREE,
        camera,
        canvas,
        bounds,
        overlayHeightPx: estimateOverlayHeightPx(current),
        fitPadding: current.fitPadding,
      }),
    );
    // A resize clears the canvas; draw now so no frame goes blank, even while the loop is paused.
    draw();
  }

  // Staged mode splits the model at a rising plane: outline above, skin below.
  const staged = first.built !== undefined;
  const outline = new THREE.MeshLambertMaterial({ color: cssColor(THREE, canvas) });
  disposables.push(() => outline.dispose());
  const above = [new THREE.Plane(new THREE.Vector3(0, 1, 0), 0)];
  const below = [new THREE.Plane(new THREE.Vector3(0, -1, 0), 0)];
  const instant = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  let built = false;
  let assembled = 0;
  let lastTime = 0;
  renderer.autoClear = !staged;

  function draw(): void {
    if (staged) {
      const eased = 1 - (1 - assembled) ** 3;
      const span = bounds.halfHeight * 2 + ASSEMBLE_MARGIN * 2;
      // Whole model units, so the skin lands one pixel row at a time.
      const height = Math.round(eased * span) - ASSEMBLE_MARGIN;
      above[0].constant = -height;
      below[0].constant = height;
      renderer.clear();
      if (assembled < 1) {
        if (floor) floor.visible = false;
        scene.overrideMaterial = outline;
        renderer.clippingPlanes = above;
        renderer.render(scene, camera);
        scene.overrideMaterial = null;
        if (floor) floor.visible = true;
      }
      renderer.clippingPlanes = below;
    }
    renderer.render(scene, camera);
  }

  function render(time = 0): void {
    if (staged) {
      const step = instant ? 1 : lastTime ? (time - lastTime) / ASSEMBLE_MS : 0;
      assembled = THREE.MathUtils.clamp(assembled + (built ? step : -2 * step), 0, 1);
      lastTime = time;
    }
    const parts = worn!.parts;
    const pulseElapsed = time - pulseStart;
    const pulseProgress =
      pulseElapsed >= 0 && pulseElapsed < CLICK_PULSE_DURATION_MS ? pulseElapsed / CLICK_PULSE_DURATION_MS : 1;
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
    draw();
    frame = paused ? 0 : window.requestAnimationFrame(render);
  }

  let paused = false;
  let canvasVisible = true;
  function syncPaused(): void {
    const next = document.hidden || !canvasVisible;
    if (paused === next) return;
    paused = next;
    if (paused) {
      if (frame !== 0) window.cancelAnimationFrame(frame);
      frame = 0;
    } else if (frame === 0) {
      frame = window.requestAnimationFrame(render);
    }
  }
  const onVisibilityChange = (): void => syncPaused();
  const intersectionObserver = new IntersectionObserver((entries) => {
    canvasVisible = entries[entries.length - 1]?.isIntersecting ?? true;
    syncPaused();
  });
  intersectionObserver.observe(canvas);
  document.addEventListener('visibilitychange', onVisibilityChange);

  const resizeObserver = new ResizeObserver(resize);
  resizeObserver.observe(canvas);
  resize();
  render();

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
    draw();
  };
  const onPointerUp = (event: PointerEvent): void => {
    dragging = false;
    if (canvas.hasPointerCapture(event.pointerId)) {
      canvas.releasePointerCapture(event.pointerId);
    }
    if (!hasDragged) pulseStart = performance.now();
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

  let disposed = false;
  let wanted = 0;
  return {
    setBuilt: (next) => {
      built = next;
    },
    // Only the latest look is put on, the moment its textures are in: the old one stays until then, so there is
    // no empty frame. The change lands with the same small bounce as a click; a staged figure rebuilds instead.
    setLook: async (look) => {
      const key = lookKey(look);
      const ticket = ++wanted;
      if (key === worn?.key) {
        setError(false);
        return;
      }
      let loaded: LoadedLook;
      try {
        loaded = await loadLook(look);
      } catch {
        if (ticket === wanted && !disposed) setError(true);
        return;
      }
      if (ticket !== wanted || disposed) {
        loaded.skin.close();
        loaded.cape?.close();
        return;
      }
      wear(key, loaded);
      setError(false);
      resize();
      if (staged) assembled = 0;
      else if (!instant) pulseStart = performance.now();
    },
    dispose: () => {
      disposed = true;
      window.cancelAnimationFrame(frame);
      resizeObserver.disconnect();
      intersectionObserver.disconnect();
      document.removeEventListener('visibilitychange', onVisibilityChange);
      canvas.removeEventListener('pointerdown', onPointerDown);
      canvas.removeEventListener('pointermove', onPointerMove);
      canvas.removeEventListener('pointerup', onPointerUp);
      canvas.removeEventListener('pointercancel', onPointerCancel);
      worn?.release();
      disposables.forEach((dispose) => dispose());
      renderer.dispose();
    },
  };
}

export function SkinThreePreview(props: SkinThreePreviewProps): JSX.Element {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [ready, setReady] = useState(false);
  const [error, setError] = useState(false);
  const [fitMetrics, setFitMetrics] = useState<SkinThreeFitMetrics>({ overlayTopPx: 10 });
  const handleRef = useRef<SceneHandle | null>(null);
  const propsRef = useRef(props);
  propsRef.current = props;
  const built = props.built === true;

  useEffect(() => {
    handleRef.current?.setBuilt(built);
  }, [built]);

  // One scene for the component's life; a new look is put on in it.
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return undefined;
    let active = true;
    setReady(false);
    setError(false);
    const guard =
      <T,>(set: (value: T) => void) =>
      (value: T): void => {
        if (active) set(value);
      };

    void setupScene(canvas, () => propsRef.current, guard(setFitMetrics), guard(setError))
      .then((handle) => {
        if (!active) {
          handle.dispose();
          return;
        }
        handle.setBuilt(propsRef.current.built === true);
        handleRef.current = handle;
        void handle.setLook(propsRef.current);
        setReady(true);
      })
      .catch(() => {
        if (active) setError(true);
      });

    return () => {
      active = false;
      handleRef.current?.dispose();
      handleRef.current = null;
    };
  }, [props.side, props.showOuterLayers]);

  useEffect(() => {
    void handleRef.current?.setLook(props);
  }, [props.src, props.capeSrc, props.variant]);

  return (
    <div
      class="cp-skin-three"
      data-skin-three-preview={error ? 'error' : ready ? 'ready' : 'loading'}
      aria-label={`${props.name} 3D skin preview`}
      style={
        {
          '--skin-three-overlay-top': `${fitMetrics.overlayTopPx}px`,
        } as JSX.CSSProperties
      }
    >
      <canvas ref={canvasRef} aria-hidden="true" />
      {error && <div class="cp-skin-three__status">3D preview unavailable</div>}
      {props.nametag && (
        <div class="cp-skin-three__overlays">
          <div class="cp-skin-nametag" title="Active player" aria-label={`Active player: ${props.nametag}`}>
            <span>{props.nametag}</span>
          </div>
        </div>
      )}
    </div>
  );
}

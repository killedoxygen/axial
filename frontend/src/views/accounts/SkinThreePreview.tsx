import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { addFloorSpotlight, addSceneLighting, buildSkinModel, modelBounds, type SkinModelBounds } from './skin-model';
import { loadThree, type ThreeModule } from './skin-three-loader';
import { loadOptionalBitmap, loadSkinBitmap } from './skin-textures';
import type { SkinVariant } from './types';

type SkinPreviewSide = 'front' | 'back';
type SkinThreeCapeState = 'loading' | 'none' | 'loaded' | 'omitted';
type SkinThreeFitState = 'pending' | 'fitted';
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
  renderer: import('three').WebGLRenderer;
  setBuilt: (built: boolean) => void;
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

async function setupScene(
  canvas: HTMLCanvasElement,
  props: SkinThreePreviewProps,
  setReady: (ready: boolean) => void,
  setInteracting: (interacting: boolean) => void,
  setCapeState: (state: SkinThreeCapeState) => void,
  setFitState: (state: SkinThreeFitState) => void,
  setFitMetrics: (metrics: SkinThreeFitMetrics) => void,
): Promise<SceneHandle> {
  const THREE = await loadThree();
  const disposables: Array<() => void> = [];
  const skinBitmap = await loadSkinBitmap(props.src);
  const capeBitmap = await loadOptionalBitmap(props.capeSrc, 'cape');
  setCapeState(props.capeSrc ? (capeBitmap ? 'loaded' : 'omitted') : 'none');
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
  const bounds = modelBounds({
    variant: props.variant,
    showOuterLayers: props.showOuterLayers,
  });

  function resize(): void {
    const rect = canvas.getBoundingClientRect();
    const width = Math.max(1, Math.round(rect.width));
    const height = Math.max(1, Math.round(rect.height));
    renderer.setSize(width, height, false);
    setFitMetrics(
      fitCameraToCanvas({
        THREE,
        camera,
        canvas,
        bounds,
        overlayHeightPx: estimateOverlayHeightPx(props),
        fitPadding: props.fitPadding,
      }),
    );
    setFitState('fitted');
  }

  // Staged mode splits the model at a rising plane: outline above, skin below.
  const staged = props.built !== undefined;
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
    const pulseElapsed = time - clickPulseStart;
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
    draw();
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
    setBuilt: (next) => {
      built = next;
    },
    dispose: () => {
      window.cancelAnimationFrame(frame);
      resizeObserver.disconnect();
      intersectionObserver.disconnect();
      document.removeEventListener('visibilitychange', onVisibilityChange);
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
  const [fitMetrics, setFitMetrics] = useState<SkinThreeFitMetrics>({ overlayTopPx: 10 });
  const handleRef = useRef<SceneHandle | null>(null);
  const built = props.built === true;
  const builtRef = useRef(built);
  builtRef.current = built;

  useEffect(() => {
    handleRef.current?.setBuilt(built);
  }, [built]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return undefined;
    let active = true;
    setReady(false);
    setError(null);
    setInteracting(false);
    setCapeState(props.capeSrc ? 'loading' : 'none');
    setFitState('pending');

    void setupScene(
      canvas,
      props,
      (nextReady) => {
        if (active) setReady(nextReady);
      },
      (nextInteracting) => {
        if (active) setInteracting(nextInteracting);
      },
      (nextCapeState) => {
        if (active) setCapeState(nextCapeState);
      },
      (nextFitState) => {
        if (active) setFitState(nextFitState);
      },
      (nextFitMetrics) => {
        if (active) setFitMetrics(nextFitMetrics);
      },
    )
      .then((nextHandle) => {
        if (!active) {
          nextHandle.dispose();
          return;
        }
        nextHandle.setBuilt(builtRef.current);
        handleRef.current = nextHandle;
      })
      .catch((err: unknown) => {
        if (!active) return;
        setError(err instanceof Error ? err.message : '3D preview failed');
      });

    return () => {
      active = false;
      handleRef.current?.dispose();
      handleRef.current = null;
    };
  }, [props.src, props.capeSrc, props.variant, props.side, props.showOuterLayers]);

  return (
    <div
      class="cp-skin-three"
      data-skin-three-preview={ready ? 'ready' : error ? 'error' : 'loading'}
      data-skin-three-interaction={interacting ? 'active' : 'idle'}
      data-skin-three-cape={capeState}
      data-skin-three-fit={fitState}
      aria-label={`${props.name} 3D skin preview`}
      style={
        {
          '--skin-three-overlay-top': `${fitMetrics.overlayTopPx}px`,
        } as JSX.CSSProperties
      }
    >
      <canvas ref={canvasRef} aria-hidden="true" />
      {!ready && !error && <div class="cp-skin-three__status">Preparing 3D preview...</div>}
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

import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { Icon } from '../../ui/Icons';
import { addFloorSpotlight, addSceneLighting, buildSkinModel, modelBounds, type SkinModelBounds } from './skin-model';
import { loadThree, type ThreeModule } from './skin-three-loader';
import { loadBitmap, loadOptionalBitmap } from './skin-textures';
import type { SkinVariant } from './types';

type SkinPreviewSide = 'front' | 'back';
type SkinPreviewBadgeState = 'previewing' | 'queued';
type SkinThreeCapeState = 'loading' | 'none' | 'loaded' | 'omitted';
type SkinThreeFitState = 'pending' | 'fitted';
const CLICK_PULSE_DURATION_MS = 420;
const DRAG_THRESHOLD_PX = 4;
const FIT_FOV_DEGREES = 34;
const FIT_ZOOM = 0.96;
const OVERLAY_MODEL_GAP_PX = 14;
const BADGE_HEIGHT_PX = 22;
const NAMETAG_HEIGHT_PX = 26;
const OVERLAY_ROW_GAP_PX = 6;

export interface SkinThreePreviewProps {
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

interface SceneHandle {
  renderer: import('three').WebGLRenderer;
  dispose: () => void;
}

interface SkinThreeFitMetrics {
  overlayTopPx: number;
  hintBottomPx: number;
}

function fitCameraToCanvas({
  THREE,
  camera,
  canvas,
  bounds,
  overlayHeightPx,
}: {
  THREE: ThreeModule;
  camera: import('three').PerspectiveCamera;
  canvas: HTMLCanvasElement;
  bounds: SkinModelBounds;
  overlayHeightPx: number;
}): SkinThreeFitMetrics {
  const width = Math.max(1, Math.round(canvas.getBoundingClientRect().width));
  const height = Math.max(1, Math.round(canvas.getBoundingClientRect().height));
  const aspect = width / height;
  const hasOverlay = overlayHeightPx > 0;
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
    overlayTopPx: Math.round(THREE.MathUtils.clamp(
      modelTop - overlayHeightPx - OVERLAY_MODEL_GAP_PX,
      8,
      Math.max(8, height * 0.18),
    )),
    hintBottomPx: Math.round(THREE.MathUtils.clamp(height - modelBottom + 8, 8, 18)),
  };
}

function estimateOverlayHeightPx(props: SkinThreePreviewProps): number {
  const rows = [
    props.badge ? BADGE_HEIGHT_PX : 0,
    props.nametag ? NAMETAG_HEIGHT_PX : 0,
  ].filter((height) => height > 0);
  return rows.reduce((total, height) => total + height, 0) + Math.max(0, rows.length - 1) * OVERLAY_ROW_GAP_PX;
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
  const bounds = modelBounds({
    variant: props.variant,
    showOuterLayers: props.showOuterLayers,
  });

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
      overlayHeightPx: estimateOverlayHeightPx(props),
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
  }, [props.src, props.capeSrc, props.variant, props.side, props.showOuterLayers]);

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
        <div class="cp-skin-three__status">Preparing 3D preview...</div>
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
                data-skin-three-nametag="editable"
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
                data-skin-three-nametag="static"
              >
                <span>{props.nametag}</span>
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

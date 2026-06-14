import type { ThreeModule } from './skin-three-loader';
import type { SkinVariant } from './types';

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

export interface SkinModelParts {
  rightArm: import('three').Group;
  leftArm: import('three').Group;
  rightLeg: import('three').Group;
  leftLeg: import('three').Group;
}

export interface SkinModelBounds {
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
  const mesh = new THREE.Mesh(
    geometry,
    materialPairs.map((pair) => pair.material),
  );
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

export function addFloorSpotlight(
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

  addBox({
    THREE,
    group,
    image: skinBitmap,
    faces: headFaces(false),
    size: [8, 8, 8],
    position: [0, 28, 0],
    transparent: false,
    disposables,
  });
  addBox({
    THREE,
    group,
    image: skinBitmap,
    faces: bodyFaces(false),
    size: [8, 12, 4],
    position: [0, 18, 0],
    transparent: false,
    disposables,
  });
  addBox({
    THREE,
    group: rightArm,
    image: skinBitmap,
    faces: armFaces(44, 40, 16, 20, armWidth),
    size: [armWidth, 12, 4],
    position: [0, -4, 0],
    transparent: false,
    disposables,
  });
  addBox({
    THREE,
    group: leftArm,
    image: skinBitmap,
    faces: armFaces(36, 32, 48, 52, armWidth),
    size: [armWidth, 12, 4],
    position: [0, -4, 0],
    transparent: false,
    disposables,
  });
  addBox({
    THREE,
    group: rightLeg,
    image: skinBitmap,
    faces: legFaces(4, 0, 16, 20),
    size: [4, 12, 4],
    position: [0, -6, 0],
    transparent: false,
    disposables,
  });
  addBox({
    THREE,
    group: leftLeg,
    image: skinBitmap,
    faces: legFaces(20, 16, 48, 52),
    size: [4, 12, 4],
    position: [0, -6, 0],
    transparent: false,
    disposables,
  });

  if (showOuterLayers) {
    addBox({
      THREE,
      group,
      image: skinBitmap,
      faces: headFaces(true),
      size: [8.7, 8.7, 8.7],
      position: [0, 28, 0],
      transparent: true,
      disposables,
    });
    addBox({
      THREE,
      group,
      image: skinBitmap,
      faces: bodyFaces(true),
      size: [8.55, 12.55, 4.55],
      position: [0, 18, 0],
      transparent: true,
      disposables,
    });
    addBox({
      THREE,
      group: rightArm,
      image: skinBitmap,
      faces: armFaces(44, 40, 32, 36, armWidth),
      size: [armWidth + 0.5, 12.5, 4.5],
      position: [0, -4, 0],
      transparent: true,
      disposables,
    });
    addBox({
      THREE,
      group: leftArm,
      image: skinBitmap,
      faces: armFaces(52, 48, 48, 52, armWidth),
      size: [armWidth + 0.5, 12.5, 4.5],
      position: [0, -4, 0],
      transparent: true,
      disposables,
    });
    addBox({
      THREE,
      group: rightLeg,
      image: skinBitmap,
      faces: legFaces(4, 0, 32, 36),
      size: [4.5, 12.5, 4.5],
      position: [0, -6, 0],
      transparent: true,
      disposables,
    });
    addBox({
      THREE,
      group: leftLeg,
      image: skinBitmap,
      faces: legFaces(4, 0, 48, 52),
      size: [4.5, 12.5, 4.5],
      position: [0, -6, 0],
      transparent: true,
      disposables,
    });
  }

  if (capeBitmap) {
    addCape({ THREE, group, image: capeBitmap, disposables });
  }

  return { rightArm, leftArm, rightLeg, leftLeg };
}

export function modelBounds({
  variant,
  showOuterLayers,
}: {
  variant: SkinVariant;
  showOuterLayers: boolean;
}): SkinModelBounds {
  const armWidth = variant === 'slim' ? 3 : 4;
  const armX = 4 + armWidth / 2;
  const outerAllowance = showOuterLayers ? 0.55 : 0;
  const modelWidth = Math.max(8 + outerAllowance, armX * 2 + armWidth + outerAllowance);
  const modelDepth = showOuterLayers ? 8.7 : 8;
  const modelHeight = showOuterLayers ? 32.7 : 32;

  return {
    centerY: modelHeight / 2,
    halfWidth: Math.sqrt((modelWidth / 2) ** 2 + (modelDepth / 2) ** 2),
    halfHeight: modelHeight / 2,
  };
}

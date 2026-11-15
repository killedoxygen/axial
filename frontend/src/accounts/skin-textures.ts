interface TextureCacheEntry {
  promise: Promise<Blob>;
  lastUsed: number;
}

interface TextureRegion {
  x: number;
  y: number;
  w: number;
  h: number;
}

const MAX_TEXTURE_BLOB_CACHE_SIZE = 96;
const SKIN_WIDTH = 64;
const SKIN_HEIGHT = 64;
const LEGACY_SKIN_HEIGHT = 32;
const textureBlobCache = new Map<string, TextureCacheEntry>();

function textureCacheKey(src: string, identity?: string): string | null {
  if (identity) return identity;
  if (src.startsWith('data:')) return src;
  try {
    const url = new URL(src, window.location.href);
    if (
      url.pathname.startsWith('/api/v1/skins/') ||
      url.pathname === '/api/v1/skin/cape/file' ||
      url.pathname === '/api/v1/skin/lookup/cape'
    ) {
      return url.href;
    }
  } catch {}
  return null;
}

function pruneTextureBlobCache(): void {
  if (textureBlobCache.size <= MAX_TEXTURE_BLOB_CACHE_SIZE) return;
  const entries = [...textureBlobCache.entries()].sort((left, right) => left[1].lastUsed - right[1].lastUsed);
  for (const [key] of entries.slice(0, textureBlobCache.size - MAX_TEXTURE_BLOB_CACHE_SIZE)) {
    textureBlobCache.delete(key);
  }
}

export function fetchTextureBlob(src: string, identity?: string): Promise<Blob> {
  const key = textureCacheKey(src, identity);
  const existing = key ? textureBlobCache.get(key) : undefined;
  if (existing) {
    existing.lastUsed = Date.now();
    return existing.promise;
  }

  const pending = fetch(src).then((response) => {
    if (!response.ok) throw new Error(`texture HTTP ${response.status}`);
    return response.blob();
  });
  if (key) {
    textureBlobCache.set(key, { promise: pending, lastUsed: Date.now() });
    pruneTextureBlobCache();
    pending.catch(() => {
      if (textureBlobCache.get(key)?.promise === pending) textureBlobCache.delete(key);
    });
  }
  return pending;
}

export function loadBitmap(src: string, identity?: string): Promise<ImageBitmap> {
  return fetchTextureBlob(src, identity).then((blob) => createImageBitmap(blob));
}

function copyRegion(ctx: CanvasRenderingContext2D, source: TextureRegion, target: TextureRegion): void {
  ctx.drawImage(ctx.canvas, source.x, source.y, source.w, source.h, target.x, target.y, target.w, target.h);
}

function isTransparentBottomHalf(ctx: CanvasRenderingContext2D): boolean {
  const pixels = ctx.getImageData(0, LEGACY_SKIN_HEIGHT, SKIN_WIDTH, SKIN_HEIGHT - LEGACY_SKIN_HEIGHT).data;
  for (let index = 3; index < pixels.length; index += 4) {
    if (pixels[index] !== 0) return false;
  }
  return true;
}

function legacyHeadOverlayIsFullyOpaque(ctx: CanvasRenderingContext2D): boolean {
  const pixels = ctx.getImageData(32, 0, 32, 16).data;
  for (let index = 0; index < pixels.length; index += 4) {
    if (pixels[index + 3] !== 255) return false;
  }
  return true;
}

function forceBaseSkinAlpha(ctx: CanvasRenderingContext2D): void {
  forceRegionAlpha(ctx, 0, 0, 32, 16);
  forceRegionAlpha(ctx, 0, 16, 64, 16);
  forceRegionAlpha(ctx, 16, 48, 32, 16);
}

function forceRegionAlpha(
  ctx: CanvasRenderingContext2D,
  startX: number,
  startY: number,
  width: number,
  height: number,
): void {
  const imageData = ctx.getImageData(startX, startY, width, height);
  for (let index = 3; index < imageData.data.length; index += 4) {
    imageData.data[index] = 255;
  }
  ctx.putImageData(imageData, startX, startY);
}

function regionsMatch(ctx: CanvasRenderingContext2D, source: TextureRegion, target: TextureRegion): boolean {
  const sourcePixels = ctx.getImageData(source.x, source.y, source.w, source.h).data;
  const targetPixels = ctx.getImageData(target.x, target.y, target.w, target.h).data;
  if (sourcePixels.length !== targetPixels.length) return false;
  for (let index = 0; index < sourcePixels.length; index += 1) {
    if (sourcePixels[index] !== targetPixels[index]) return false;
  }
  return true;
}

function hasLegacyCopiedLimbRegions(ctx: CanvasRenderingContext2D): boolean {
  return (
    regionsMatch(ctx, { x: 0, y: 20, w: 4, h: 12 }, { x: 24, y: 52, w: 4, h: 12 }) &&
    regionsMatch(ctx, { x: 4, y: 20, w: 4, h: 12 }, { x: 20, y: 52, w: 4, h: 12 }) &&
    regionsMatch(ctx, { x: 40, y: 20, w: 4, h: 12 }, { x: 40, y: 52, w: 4, h: 12 }) &&
    regionsMatch(ctx, { x: 44, y: 20, w: 4, h: 12 }, { x: 36, y: 52, w: 4, h: 12 })
  );
}

function copyLegacyLimbRegions(ctx: CanvasRenderingContext2D): void {
  ctx.clearRect(0, LEGACY_SKIN_HEIGHT, SKIN_WIDTH, SKIN_HEIGHT - LEGACY_SKIN_HEIGHT);

  copyRegion(ctx, { x: 4, y: 16, w: 4, h: 4 }, { x: 20, y: 48, w: 4, h: 4 });
  copyRegion(ctx, { x: 8, y: 16, w: 4, h: 4 }, { x: 24, y: 48, w: 4, h: 4 });
  copyRegion(ctx, { x: 0, y: 20, w: 4, h: 12 }, { x: 24, y: 52, w: 4, h: 12 });
  copyRegion(ctx, { x: 4, y: 20, w: 4, h: 12 }, { x: 20, y: 52, w: 4, h: 12 });
  copyRegion(ctx, { x: 8, y: 20, w: 4, h: 12 }, { x: 16, y: 52, w: 4, h: 12 });
  copyRegion(ctx, { x: 12, y: 20, w: 4, h: 12 }, { x: 28, y: 52, w: 4, h: 12 });

  copyRegion(ctx, { x: 44, y: 16, w: 4, h: 4 }, { x: 36, y: 48, w: 4, h: 4 });
  copyRegion(ctx, { x: 48, y: 16, w: 4, h: 4 }, { x: 40, y: 48, w: 4, h: 4 });
  copyRegion(ctx, { x: 40, y: 20, w: 4, h: 12 }, { x: 40, y: 52, w: 4, h: 12 });
  copyRegion(ctx, { x: 44, y: 20, w: 4, h: 12 }, { x: 36, y: 52, w: 4, h: 12 });
  copyRegion(ctx, { x: 48, y: 20, w: 4, h: 12 }, { x: 32, y: 52, w: 4, h: 12 });
  copyRegion(ctx, { x: 52, y: 20, w: 4, h: 12 }, { x: 44, y: 52, w: 4, h: 12 });
}

async function normalizeLegacySkinBitmap(bitmap: ImageBitmap): Promise<ImageBitmap> {
  const legacySkin = bitmap.width === SKIN_WIDTH && bitmap.height === LEGACY_SKIN_HEIGHT;
  const normalizedLegacySkin = bitmap.width === SKIN_WIDTH && bitmap.height === SKIN_HEIGHT;
  if (!legacySkin && !normalizedLegacySkin) return bitmap;

  const canvas = document.createElement('canvas');
  canvas.width = SKIN_WIDTH;
  canvas.height = SKIN_HEIGHT;
  const ctx = canvas.getContext('2d', { willReadFrequently: normalizedLegacySkin });
  if (!ctx) return bitmap;
  ctx.imageSmoothingEnabled = false;
  ctx.clearRect(0, 0, SKIN_WIDTH, SKIN_HEIGHT);
  ctx.drawImage(bitmap, 0, 0);

  const shouldRepairLimbs = legacySkin || isTransparentBottomHalf(ctx);
  const legacyShaped = legacySkin || shouldRepairLimbs || hasLegacyCopiedLimbRegions(ctx);
  if (!legacyShaped) return bitmap;

  if (shouldRepairLimbs) copyLegacyLimbRegions(ctx);
  if (legacyHeadOverlayIsFullyOpaque(ctx)) ctx.clearRect(32, 0, 32, 16);
  forceBaseSkinAlpha(ctx);
  const normalized = await createImageBitmap(canvas);
  bitmap.close();
  return normalized;
}

export function loadSkinBitmap(src: string, identity?: string): Promise<ImageBitmap> {
  return loadBitmap(src, identity).then(normalizeLegacySkinBitmap);
}

export async function loadOptionalBitmap(
  src: string | undefined,
  label: string,
  identity?: string,
): Promise<ImageBitmap | null> {
  if (!src) return null;
  try {
    return await loadBitmap(src, identity);
  } catch (err) {
    console.warn(`Could not load optional ${label} texture for 3D skin preview.`, err);
    return null;
  }
}

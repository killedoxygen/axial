interface TextureCacheEntry {
  promise: Promise<Blob>;
  lastUsed: number;
}

const MAX_TEXTURE_BLOB_CACHE_SIZE = 96;
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

export type ThreeModule = typeof import('three');

let threePromise: Promise<ThreeModule> | null = null;

export function loadThree(): Promise<ThreeModule> {
  if (!threePromise) {
    threePromise = import('three').catch((err: unknown) => {
      threePromise = null;
      throw err;
    });
  }
  return threePromise;
}

export function preloadSkinRenderer(): Promise<void> {
  return loadThree().then(() => undefined);
}

import type { JSX } from 'preact';
import type { LoaderKey } from './defaults';

const LOADER_LOGO_SRC: Record<LoaderKey, string> = {
  vanilla: 'loader-base.svg',
  fabric: 'loader-grid.svg',
  forge: 'loader-cross.svg',
  neoforge: 'loader-orbit.svg',
  quilt: 'loader-diamonds.svg',
};

export function loaderLogoSrc(loader: LoaderKey): string {
  return LOADER_LOGO_SRC[loader];
}

export function LoaderLogo({
  loader,
  size = 16,
  class: className,
}: {
  loader: LoaderKey;
  size?: number;
  class?: string;
}): JSX.Element {
  const src = loaderLogoSrc(loader);
  return (
    <span
      aria-hidden="true"
      class={className}
      style={{
        ['--cp-loader-src' as any]: `url("${src}")`,
        width: `${size}px`,
        height: `${size}px`,
      }}
    />
  );
}

import type { JSX } from 'preact';
import type { LoaderKey } from './defaults';

const LOADER_LOGO_SRC: Partial<Record<LoaderKey, string>> = {
  fabric: 'fabric_icon.svg',
  forge: 'forge_icon.svg',
  neoforge: 'neoforge_icon.svg',
  quilt: 'quilt_icon.svg',
};

export function loaderLogoSrc(loader: LoaderKey): string | null {
  return LOADER_LOGO_SRC[loader] ?? null;
}

export function LoaderLogo({
  loader,
  size = 16,
  class: className,
}: {
  loader: LoaderKey;
  size?: number;
  class?: string;
}): JSX.Element | null {
  const src = loaderLogoSrc(loader);
  if (!src) return null;
  return (
    <span
      aria-hidden="true"
      class={className}
      data-loader={loader}
      style={{
        ['--cp-loader-src' as any]: `url("${src}")`,
        width: `${size}px`,
        height: `${size}px`,
      }}
    />
  );
}

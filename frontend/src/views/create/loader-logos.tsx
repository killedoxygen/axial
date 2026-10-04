import type { JSX } from 'preact';
import type { LoaderKey } from './defaults';

const LOADER_LOGO_SRC: Record<LoaderKey, string> = {
  vanilla: 'vanilla_icon.svg',
  fabric: 'fabric_icon.svg',
  forge: 'forge_icon.svg',
  neoforge: 'neoforge_icon.svg',
  quilt: 'quilt_icon.svg',
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
      data-loader={loader}
      style={{
        ['--cp-loader-src' as any]: `url("${src}")`,
        width: `${size}px`,
        height: `${size}px`,
      }}
    />
  );
}

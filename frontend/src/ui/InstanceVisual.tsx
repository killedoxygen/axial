import type { JSX } from 'preact';
import { versionById } from '../store';
import { instanceInstallStatus } from '../instance-install-status';
import { hashStr } from '../tokens';
import { loaderKeyFromComponentId, loaderKeyFromVersion, type LoaderKey } from '../views/create/defaults';
import { loaderLogoSrc } from '../views/create/loader-logos';
import { Icon } from './Icons';
import type { Instance, Version } from '../types';

export type VisualInstance = Pick<Instance, 'id' | 'name' | 'version_id' | 'art_seed'>;

export function artSeedFor(instance: VisualInstance): number {
  const seed = instance.art_seed ?? 0;
  if (seed > 0) return seed >>> 0;
  return hashStr(`${instance.id}:${instance.name}:${instance.version_id}`) || 1;
}

export function nextArtSeed(seed: number): number {
  const next = Math.imul((seed >>> 0) ^ 0x9e3779b9, 2654435761) >>> 0;
  return next || 1;
}

function hueFor(inst: VisualInstance): number {
  return artSeedFor(inst) % 360;
}

function loaderKeyForInstance(inst: VisualInstance, version: Version | undefined): LoaderKey {
  const versionLoader = loaderKeyFromVersion(version);
  if (versionLoader !== 'vanilla') return versionLoader;
  const installLoader = instanceInstallStatus(inst, version).item.loader;
  return loaderKeyFromComponentId(installLoader?.componentId);
}

function GlyphMark({ loader, className }: { loader: LoaderKey; className: string }): JSX.Element {
  const src = loaderLogoSrc(loader);
  if (src) {
    return (
      <span
        aria-hidden="true"
        class={`${className} ${className}--mask`}
        data-loader={loader}
        style={{ ['--cp-loader-src' as any]: `url("${src}")` }}
      />
    );
  }
  return (
    <span aria-hidden="true" class={className}>
      <Icon name="stack" stroke={1.5} />
    </span>
  );
}

export function InstanceTile({
  inst,
  radius,
  className,
  style,
}: {
  inst: VisualInstance;
  radius?: number;
  className?: string;
  style?: JSX.CSSProperties;
}): JSX.Element {
  const version = versionById(inst.version_id);
  const loader = loaderKeyForInstance(inst, version);

  return (
    <div
      class={`cp-tile${className ? ` ${className}` : ''}`}
      style={{ ['--cp-tile-h' as any]: hueFor(inst), borderRadius: radius, ...style }}
      aria-hidden="true"
    >
      <div class="cp-tile-identity">
        <GlyphMark loader={loader} className="cp-tile-glyph" />
      </div>
    </div>
  );
}

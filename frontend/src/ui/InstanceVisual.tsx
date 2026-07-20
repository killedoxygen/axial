import type { JSX } from 'preact';
import { versionById } from '../store';
import { instanceInstallStatus } from '../instance-install-status';
import { hashStr } from '../tokens';
import { useTheme } from '../hooks/use-theme';
import { loaderKeyFromComponentId, loaderKeyFromVersion, type LoaderKey } from '../views/create/defaults';
import { loaderLogoSrc } from '../views/create/loader-logos';
import { resolveTileHue } from './look-guardian';
import type { Theme } from '../tokens';
import type { Version } from '../types-version';
import type { Instance, InstanceVersionDisplay } from '../types-instance';

export type VisualInstance = Pick<Instance, 'id' | 'name' | 'version_id' | 'art_seed'> & {
  version_display?: Pick<InstanceVersionDisplay, 'loader_key'>;
};

export function artSeedFor(instance: VisualInstance): number {
  const seed = instance.art_seed ?? 0;
  if (seed > 0) return seed >>> 0;
  return hashStr(`${instance.id}:${instance.name}:${instance.version_id}`) || 1;
}

export function nextArtSeed(seed: number): number {
  const next = Math.imul((seed >>> 0) ^ 0x9e3779b9, 2654435761) >>> 0;
  return next || 1;
}

export function instanceHue(inst: VisualInstance): number {
  return artSeedFor(inst) % 360;
}

export function guardedInstanceHue(inst: VisualInstance, theme: Theme): number {
  return resolveTileHue(instanceHue(inst), theme);
}

function isLoaderKey(value: string | undefined): value is LoaderKey {
  return value === 'vanilla' || value === 'fabric' || value === 'quilt' || value === 'forge' || value === 'neoforge';
}

function loaderKeyForInstance(inst: VisualInstance, version: Version | undefined): LoaderKey {
  if (isLoaderKey(inst.version_display?.loader_key)) {
    return inst.version_display.loader_key;
  }
  const versionLoader = loaderKeyFromVersion(version);
  if (versionLoader !== 'vanilla') return versionLoader;
  const installLoader = instanceInstallStatus(inst, version).item.loader;
  return loaderKeyFromComponentId(installLoader?.componentId);
}

function GlyphMark({ loader, className }: { loader: LoaderKey; className: string }): JSX.Element {
  const src = loaderLogoSrc(loader);
  return (
    <span
      aria-hidden="true"
      class={`${className} ${className}--mask`}
      style={{ ['--cp-loader-src' as any]: `url("${src}")` }}
    />
  );
}

export function InstanceGlyph({ inst, className }: { inst: VisualInstance; className: string }): JSX.Element {
  const version = versionById(inst.version_id);
  return <GlyphMark loader={loaderKeyForInstance(inst, version)} className={className} />;
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
  const theme = useTheme();
  const version = versionById(inst.version_id);
  const loader = loaderKeyForInstance(inst, version);

  return (
    <div
      class={`cp-tile${className ? ` ${className}` : ''}`}
      style={{ ['--cp-tile-h' as any]: guardedInstanceHue(inst, theme), borderRadius: radius, ...style }}
      aria-hidden="true"
    >
      <div class="cp-tile-identity">
        <GlyphMark loader={loader} className="cp-tile-glyph" />
      </div>
    </div>
  );
}

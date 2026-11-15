import type { JSX } from 'preact';
import { versionById } from '../instances/state';
import { hashStr } from '../app/tokens';
import { useTheme } from '../app/use-theme';
import { loaderKeyFromVersion, type LoaderKey } from '../create/defaults';
import { loaderLogoSrc } from '../create/loader-logos';
import { resolveTileHue } from './look-guardian';
import type { Theme } from '../app/tokens';
import type { VersionEntry } from '../generated/VersionEntry';
import type { EnrichedInstance } from '../generated/EnrichedInstance';
import type { InstanceVersionDisplay } from '../generated/InstanceVersionDisplay';

export type VisualInstance = Pick<EnrichedInstance, 'id' | 'name' | 'version_id' | 'art_seed'> & {
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

/** The seed's hue, moved off the accent's neighbourhood so a tile never reads as the theme. */
function tileHue(inst: VisualInstance, theme: Theme): number {
  return resolveTileHue(artSeedFor(inst) % 360, theme);
}

/** The tile's hue, for what stands around the tile and draws its marks in the tile's colours. */
export function useTileHue(inst: VisualInstance): number {
  return tileHue(inst, useTheme());
}

function isLoaderKey(value: string | undefined): value is LoaderKey {
  return value === 'vanilla' || value === 'fabric' || value === 'quilt' || value === 'forge' || value === 'neoforge';
}

function loaderKeyForInstance(inst: VisualInstance, version: VersionEntry | undefined): LoaderKey {
  if (isLoaderKey(inst.version_display?.loader_key)) {
    return inst.version_display.loader_key;
  }
  return loaderKeyFromVersion(version);
}

function GlyphMark({ loader, className }: { loader: LoaderKey; className: string }): JSX.Element {
  const src = loaderLogoSrc(loader);
  return (
    <span aria-hidden="true" class={`${className} ${className}--mask`} style={{ '--cp-loader-src': `url("${src}")` }} />
  );
}

export function InstanceGlyph({ inst, className }: { inst: VisualInstance; className: string }): JSX.Element {
  const version = versionById(inst.version_id);
  return <GlyphMark loader={loaderKeyForInstance(inst, version)} className={className} />;
}

export function InstanceTile({ inst, className }: { inst: VisualInstance; className?: string }): JSX.Element {
  const theme = useTheme();
  const loader = loaderKeyForInstance(inst, versionById(inst.version_id));
  return (
    <div
      class={className ? `cp-tile ${className}` : 'cp-tile'}
      style={{ '--cp-tile-h': tileHue(inst, theme) }}
      aria-hidden="true"
    >
      <GlyphMark loader={loader} className="cp-tile-glyph" />
    </div>
  );
}

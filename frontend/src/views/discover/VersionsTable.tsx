import type { JSX } from 'preact';
import { useMemo, useState } from 'preact/hooks';
import { Button, Chip } from '../../ui/Atoms';
import { SelectField, type SelectFieldOption } from '../../ui/Select';
import { formatBytes, formatCount, formatDate, plural } from '../../format';
import type { ContentDetail, ContentVersion, ReleaseChannel } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';
import { ContentAction } from './ContentAction';
import type { InstallFlow } from './install-flow';
import { compareMcDesc, tagLabel, usesLoaderFilter, versionFits } from './shared';

const CHANNELS: SelectFieldOption<'' | ReleaseChannel>[] = [
  { value: '', label: 'All channels' },
  { value: 'release', label: 'Release' },
  { value: 'beta', label: 'Beta' },
  { value: 'alpha', label: 'Alpha' },
];
const FIRST_PAGE = 10;
const GAME_VERSIONS_SHOWN = 3;

/** The distinct values across the versions, under an "any" choice. */
function valueOptions(
  versions: ContentVersion[],
  read: (version: ContentVersion) => string[],
  any: string,
  order: (a: string, b: string) => number,
  label: (value: string) => string = (value) => value,
): SelectFieldOption<string>[] {
  const values = [...new Set(versions.flatMap(read))].sort(order);
  return [{ value: '', label: any }, ...values.map((value) => ({ value, label: label(value) }))];
}

/** Every version of a project. A target instance starts the filters on what fits it; they stay editable. */
export function VersionsTable({
  detail,
  instance,
  flow,
}: {
  detail: ContentDetail;
  instance: EnrichedInstance | null;
  flow: InstallFlow;
}): JSX.Element {
  const versions = detail.versions;
  const [showAll, setShowAll] = useState(false);
  const [channel, setChannel] = useState<'' | ReleaseChannel>('');
  const [minecraft, setMinecraft] = useState(() => {
    const wanted = instance?.version_display.minecraft_label ?? '';
    return versions.some((version) => version.game_versions.includes(wanted)) ? wanted : '';
  });
  const [loader, setLoader] = useState(() => {
    const wanted = instance && usesLoaderFilter(detail.kind) ? instance.version_display.loader_key : '';
    return versions.some((version) => version.loaders.includes(wanted)) ? wanted : '';
  });

  const options = useMemo(
    () => ({
      channel: CHANNELS.filter((option) => !option.value || versions.some((v) => v.channel === option.value)),
      minecraft: valueOptions(versions, (v) => v.game_versions, 'Any version', compareMcDesc),
      loader: valueOptions(
        versions,
        (v) => v.loaders,
        'Any loader',
        (a, b) => a.localeCompare(b),
        tagLabel,
      ),
    }),
    [versions],
  );
  const filtered = versions.filter(
    (version) =>
      (!channel || version.channel === channel) &&
      (!minecraft || version.game_versions.includes(minecraft)) &&
      (!loader || version.loaders.includes(loader)),
  );
  const shown = showAll ? filtered : filtered.slice(0, FIRST_PAGE);
  const older = filtered.length - shown.length;

  return (
    <section class="cp-content-section">
      <div class="cp-head">
        <h2>Versions</h2>
        <span class="cp-tab-count" aria-live="polite" aria-label={plural(filtered.length, 'version', 'versions')}>
          {filtered.length}
        </span>
        <span class="cp-head-fill" />
        {options.channel.length > 2 && (
          <SelectField value={channel} onChange={setChannel} options={options.channel} ariaLabel="Channel" />
        )}
        {options.loader.length > 2 && (
          <SelectField value={loader} onChange={setLoader} options={options.loader} ariaLabel="Loader" />
        )}
        {options.minecraft.length > 1 && (
          <SelectField
            value={minecraft}
            onChange={setMinecraft}
            options={options.minecraft}
            ariaLabel="Minecraft version"
          />
        )}
      </div>
      <div class="cp-table cp-content-versions" role="table">
        <div class="cp-tr cp-th" role="row">
          <span role="columnheader">Version</span>
          <span role="columnheader">Supports</span>
          <span role="columnheader">Published</span>
          <span role="columnheader">Downloads</span>
          <span role="columnheader">Size</span>
          <span />
        </div>
        {shown.map((version) => {
          const fits = versionFits(version, detail.kind, instance);
          const file = version.files.find((entry) => entry.primary) ?? version.files[0];
          const hidden = version.game_versions.length - GAME_VERSIONS_SHOWN;
          return (
            <div key={version.id} class="cp-tr" role="row" data-fits={fits}>
              <span class="cp-content-version" role="cell">
                <strong title={version.name}>{version.version_number}</strong>
                {version.channel !== 'release' && (
                  <Chip tone={version.channel === 'beta' ? 'warn' : 'err'}>{tagLabel(version.channel)}</Chip>
                )}
              </span>
              <span class="cp-chips" role="cell">
                {version.loaders.map((entry) => (
                  <Chip key={entry}>{tagLabel(entry)}</Chip>
                ))}
                {version.game_versions.slice(0, GAME_VERSIONS_SHOWN).map((entry) => (
                  <Chip key={entry} mono>
                    {entry}
                  </Chip>
                ))}
                {hidden > 0 && (
                  <Chip mono title={version.game_versions.slice(GAME_VERSIONS_SHOWN).join(', ')}>
                    +{hidden}
                  </Chip>
                )}
              </span>
              <span role="cell">{formatDate(version.published)}</span>
              <span role="cell">{formatCount(version.downloads)}</span>
              <span role="cell">{file?.size ? formatBytes(file.size) : ''}</span>
              <span class="cp-discover-act" role="cell">
                <ContentAction
                  item={detail}
                  version={version}
                  instance={instance}
                  flow={flow}
                  unfit={instance && !fits ? `Does not fit ${instance.version_display.summary_label}` : undefined}
                />
              </span>
            </div>
          );
        })}
      </div>
      {filtered.length === 0 && (
        <p class="cp-content-none">
          Nothing matches these filters.
          <Button
            variant="ghost"
            size="sm"
            onClick={() => {
              setChannel('');
              setMinecraft('');
              setLoader('');
            }}
          >
            Clear filters
          </Button>
        </p>
      )}
      {older > 0 && (
        <Button variant="secondary" size="sm" onClick={() => setShowAll(true)}>
          Show {plural(older, 'older version', 'older versions')}
        </Button>
      )}
    </section>
  );
}

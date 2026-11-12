import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Chip, Empty } from '../../ui/Atoms';
import { Art } from '../../ui/Art';
import { Icon } from '../../ui/Icons';
import { formatAge } from '../../format';
import type { ContentDetail } from '../../types-content';
import { navigate, route } from '../../ui-state';
import { errMessage } from '../../utils';
import { ContentAction } from './ContentAction';
import { cachedDetail, loadDetail } from './detail-cache';
import { Gallery } from './Gallery';
import { InstallConflictSheet, useInstallFlow } from './install-flow';
import { InstallTarget } from './InstallTarget';
import { ExternalLink, ProjectBody } from './markdown';
import {
  ContentStats,
  installCandidate,
  InstanceTargetPicker,
  KIND_ICON,
  KIND_NOUN,
  tagLabel,
  versionFits,
} from './shared';
import { targetInstance } from './state';
import { Tray } from './Tray';
import { VersionsTable } from './VersionsTable';

interface Loaded {
  canonicalId: string;
  detail: ContentDetail | null;
  error: string | null;
}

/** One project, in one flow: what it is with its action, what it does, its screenshots, its versions. */
export function ContentDetailView(): JSX.Element {
  const current = route.value;
  const canonicalId = current.name === 'content' ? current.id : '';
  const instance = targetInstance.value;
  const flow = useInstallFlow(instance?.id);
  const [loaded, setLoaded] = useState<Loaded>(() => ({
    canonicalId,
    detail: cachedDetail(canonicalId) ?? null,
    error: null,
  }));

  useEffect(() => {
    const cached = cachedDetail(canonicalId) ?? null;
    let active = true;
    setLoaded({ canonicalId, detail: cached, error: null });
    loadDetail(canonicalId)
      .then((detail) => {
        if (active) setLoaded({ canonicalId, detail, error: null });
      })
      .catch((err) => {
        if (active && !cached) setLoaded({ canonicalId, detail: null, error: errMessage(err) });
      });
    return () => {
      active = false;
    };
  }, [canonicalId]);

  const { detail, error } = loaded.canonicalId === canonicalId ? loaded : { detail: null, error: null };

  if (!detail) {
    return (
      <div class="cp-view-page cp-content">
        {error ? (
          <Empty eyes="x" title="Could not load this page">
            {error}
          </Empty>
        ) : (
          <header class="cp-content-id" data-ghost aria-hidden="true">
            <span class="cp-art" />
            <div class="cp-content-copy">
              <span />
              <span />
            </div>
          </header>
        )}
        <Tray />
      </div>
    );
  }

  const versions = detail.versions ?? [];
  const gallery = detail.gallery ?? [];
  const body = detail.body?.trim();
  const candidate = instance ? installCandidate(versions, detail.kind, instance) : versions[0];
  const unfit =
    instance && versions.length > 0 && !versions.some((version) => versionFits(version, detail.kind, instance))
      ? `No version supports ${instance.version_display.summary_label}`
      : undefined;

  return (
    <div class="cp-view-page cp-content">
      <header class="cp-content-id">
        <Art url={detail.icon_url} icon={KIND_ICON[detail.kind]} size={56} />
        <div class="cp-content-copy">
          <div class="cp-content-name">
            <h1 class="cp-h1">{detail.title}</h1>
            {detail.author && <span class="cp-discover-by">by {detail.author}</span>}
          </div>
          <div class="cp-content-meta">
            <span class="cp-chips">
              <Chip>{tagLabel(KIND_NOUN[detail.kind])}</Chip>
              {detail.categories.slice(0, 3).map((category) => (
                <Chip key={category}>{tagLabel(category)}</Chip>
              ))}
            </span>
            <ContentStats downloads={detail.downloads} follows={detail.follows} />
            {detail.updated && (
              <Chip tone="bare" icon="clock">
                Updated {formatAge(detail.updated)}
              </Chip>
            )}
            {candidate && (
              <Chip
                tone="bare"
                title={instance ? `${candidate.version_number} would be added to ${instance.name}` : undefined}
              >
                {instance ? 'Adds' : 'Latest'} <b>{candidate.version_number}</b>
              </Chip>
            )}
            {detail.slug && (
              <ExternalLink href={`https://modrinth.com/project/${detail.slug}`} class="cp-chip cp-chip--bare">
                Modrinth
                <Icon name="expand" size={12} />
              </ExternalLink>
            )}
          </div>
        </div>
        <div class="cp-content-act">
          {instance ? (
            <InstallTarget instance={instance} facts />
          ) : (
            detail.kind !== 'modpack' && (
              <InstanceTargetPicker
                placeholder="Add to an instance…"
                onPick={(id) => navigate({ name: 'content', id: detail.canonical_id, target: id })}
              />
            )
          )}
          <ContentAction item={detail} instance={instance} flow={flow} unfit={unfit} lead />
        </div>
      </header>

      {body ? <ProjectBody body={body} /> : <p class="cp-content-body">{detail.summary}</p>}
      {gallery.length > 0 && <Gallery images={gallery} />}
      {versions.length > 0 && (
        <VersionsTable key={instance?.id ?? ''} detail={detail} instance={instance} flow={flow} />
      )}

      <InstallConflictSheet flow={flow} />
      <Tray />
    </div>
  );
}

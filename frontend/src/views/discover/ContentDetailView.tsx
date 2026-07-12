import type { JSX } from 'preact';
import { useEffect, useMemo, useState } from 'preact/hooks';
import { Button } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { Modal, ModalContent } from '../../ui/Modal';
import { SelectField } from '../../ui/Select';
import { navigate, route } from '../../ui-state';
import { getContentDetail } from '../../content';
import { errMessage } from '../../utils';
import type { ContentDetail, ContentSelection, ContentVersion, ResolutionPlan } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';
import { addToInstance, commitInstall, createFromModpack } from './actions';
import { ConflictSheet } from './Tray';
import { TargetBar } from './TargetBar';
import { contentTargets, isStaged, stage, targetInstance, unstage } from './state';
import { ProjectBody, ExternalLink } from './markdown';
import {
  ContentIcon,
  formatAge,
  formatBytes,
  formatCount,
  formatDate,
  isAddable,
  KIND_NOUN,
  plural,
  Spinner,
  Stat,
  tagLabel,
  versionFits,
} from './shared';

const VERSIONS_SHOWN = 6;

export function ContentDetailView(): JSX.Element {
  const current = route.value;
  const canonicalId = current.name === 'content' ? current.id : '';
  const instance = targetInstance.value;

  const [detail, setDetail] = useState<ContentDetail | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    setDetail(null);
    setError(null);
    getContentDetail(canonicalId)
      .then((resolved) => {
        if (active) setDetail(resolved);
      })
      .catch((err) => {
        if (active) setError(errMessage(err));
      });
    return () => {
      active = false;
    };
  }, [canonicalId]);

  const back = (): void => navigate({ name: 'discover', target: instance?.id });

  if (error) {
    return (
      <div class="cp-view-page cp-content">
        <BackLink onClick={back} />
        <div class="cp-discover-empty cp-discover-empty--pad">
          <Icon name="alert" size={22} />
          <div>{error}</div>
          <Button variant="secondary" onClick={back}>
            Back to Discover
          </Button>
        </div>
      </div>
    );
  }

  if (!detail) return <DetailSkeleton onBack={back} />;

  const latest = detail.versions[0];
  const projectUrl = detail.slug ? `https://modrinth.com/project/${detail.slug}` : '';

  return (
    <div class="cp-view-page cp-content">
      <BackLink onClick={back} />
      {instance && <TargetBar instance={instance} />}

      <header class="cp-content-hero">
        <div class="cp-content-icon" aria-hidden="true">
          <ContentIcon url={detail.icon_url} kind={detail.kind} size={34} />
        </div>
        <div class="cp-content-headings">
          <div class="cp-content-kicker">
            <span class="cp-content-kind">{KIND_NOUN[detail.kind]}</span>
            {detail.categories.slice(0, 3).map((category) => (
              <span key={category} class="cp-discover-tag">
                {tagLabel(category)}
              </span>
            ))}
          </div>
          <h1 class="cp-content-title">{detail.title}</h1>
          <div class="cp-content-author">{detail.author ? `by ${detail.author}` : 'Modrinth'}</div>
          <div class="cp-content-stats">
            <Stat icon="download" label="downloads" value={formatCount(detail.downloads)} />
            <Stat icon="user" label="followers" value={formatCount(detail.follows)} />
            <Stat icon="clock" label="updated" value={formatAge(detail.updated)} />
            {latest && <Stat icon="tag" label="latest" value={latest.version_number} />}
          </div>
        </div>
        {projectUrl && (
          <ExternalLink href={projectUrl} class="cp-content-hero-link">
            <Icon name="globe" size={13} />
            Modrinth
          </ExternalLink>
        )}
      </header>

      <div class="cp-content-layout">
        <div class="cp-content-main">
          <About detail={detail} />
          {detail.gallery.length > 0 && <Gallery detail={detail} />}
          <Versions detail={detail} instance={instance} />
        </div>

        <aside class="cp-content-rail">
          <InstallRail detail={detail} instance={instance} />
          <Facts detail={detail} />
        </aside>
      </div>
    </div>
  );
}

function BackLink({ onClick }: { onClick: () => void }): JSX.Element {
  return (
    <button class="cp-content-back" onClick={onClick}>
      <Icon name="arrow-left" size={13} /> Discover
    </button>
  );
}

function DetailSkeleton({ onBack }: { onBack: () => void }): JSX.Element {
  return (
    <div class="cp-view-page cp-content" aria-busy="true">
      <BackLink onClick={onBack} />
      <header class="cp-content-hero">
        <div class="cp-content-icon cp-skeleton" />
        <div class="cp-content-headings">
          <div class="cp-skeleton cp-skeleton-line" style={{ width: 120, height: 14 }} />
          <div class="cp-skeleton cp-skeleton-line" style={{ width: '38%', height: 24, marginTop: 10 }} />
          <div class="cp-skeleton cp-skeleton-line" style={{ width: '22%', marginTop: 10 }} />
          <div class="cp-skeleton cp-skeleton-line" style={{ width: '55%', height: 14, marginTop: 14 }} />
        </div>
      </header>
      <div class="cp-content-layout">
        <div class="cp-content-main">
          <div class="cp-skeleton cp-skeleton-line" style={{ width: '100%' }} />
          <div class="cp-skeleton cp-skeleton-line" style={{ width: '92%' }} />
          <div class="cp-skeleton cp-skeleton-line" style={{ width: '78%' }} />
        </div>
        <aside class="cp-content-rail">
          <div class="cp-skeleton" style={{ height: 148, borderRadius: 'var(--r-md)' }} />
        </aside>
      </div>
    </div>
  );
}

function About({ detail }: { detail: ContentDetail }): JSX.Element {
  const [expanded, setExpanded] = useState(false);
  const body = detail.body.trim();
  const long = body.length > 900;

  return (
    <section class="cp-content-section">
      <h2 class="cp-content-section-title">About</h2>
      <p class="cp-content-summary">{detail.summary}</p>
      {body && body !== detail.summary && (
        <div class="cp-content-body-wrap" data-clamped={long && !expanded}>
          <ProjectBody body={body} />
          {long && !expanded && (
            <button class="cp-content-more" onClick={() => setExpanded(true)}>
              Read more <Icon name="chevron-down" size={13} />
            </button>
          )}
        </div>
      )}
    </section>
  );
}

function Gallery({ detail }: { detail: ContentDetail }): JSX.Element {
  const [open, setOpen] = useState<number | null>(null);
  const image = open === null ? null : detail.gallery[open];

  return (
    <section class="cp-content-section">
      <h2 class="cp-content-section-title">Gallery</h2>
      <div class="cp-content-gallery">
        {detail.gallery.slice(0, 9).map((entry, index) => (
          <button
            key={entry.url}
            class="cp-content-shot"
            onClick={() => setOpen(index)}
            aria-label={entry.title ?? `Screenshot ${index + 1}`}
          >
            <img src={entry.url} alt={entry.title ?? ''} loading="lazy" />
            {entry.title && <span class="cp-content-shot-caption">{entry.title}</span>}
          </button>
        ))}
      </div>

      {image && (
        <Modal open onOpenChange={(next) => !next && setOpen(null)}>
          <ModalContent className="cp-content-lightbox" aria-label={image.title ?? 'Screenshot'}>
            <img src={image.url} alt={image.title ?? ''} />
            {image.title && <div class="cp-content-lightbox-caption">{image.title}</div>}
          </ModalContent>
        </Modal>
      )}
    </section>
  );
}

function Versions({
  detail,
  instance,
}: {
  detail: ContentDetail;
  instance: EnrichedInstance | null;
}): JSX.Element | null {
  const [showAll, setShowAll] = useState(false);
  if (detail.versions.length === 0) return null;

  const shown = showAll ? detail.versions : detail.versions.slice(0, VERSIONS_SHOWN);
  const hidden = detail.versions.length - shown.length;

  return (
    <section class="cp-content-section">
      <h2 class="cp-content-section-title">
        Versions
        <span class="cp-content-section-count">{detail.versions.length}</span>
      </h2>
      <div class="cp-content-versions">
        {shown.map((version) => (
          <VersionRow key={version.id} detail={detail} version={version} instance={instance} />
        ))}
      </div>
      {hidden > 0 && (
        <button class="cp-content-more cp-content-more--flush" onClick={() => setShowAll(true)}>
          Show {plural(hidden, 'more version', 'more versions')} <Icon name="chevron-down" size={13} />
        </button>
      )}
    </section>
  );
}

function VersionRow({
  detail,
  version,
  instance,
}: {
  detail: ContentDetail;
  version: ContentVersion;
  instance: EnrichedInstance | null;
}): JSX.Element {
  const [busy, setBusy] = useState(false);
  const fits = versionFits(version, detail.kind, instance);
  const size = version.files.find((file) => file.primary)?.size ?? version.files[0]?.size;
  const canAdd = !!instance && isAddable(detail.kind) && fits;

  const add = async (): Promise<void> => {
    if (!instance || busy) return;
    setBusy(true);
    await addToInstance(
      instance.id,
      [{ canonical_id: detail.canonical_id, kind: detail.kind, version_id: version.id }],
      `${detail.title} ${version.version_number}`,
    );
    setBusy(false);
  };

  return (
    <div
      class="cp-content-version"
      data-fits={fits}
      title={fits ? undefined : `Does not fit ${instance?.version_display.summary_label}`}
    >
      <span class="cp-content-version-channel" data-channel={version.channel} title={version.channel} />
      <div class="cp-content-version-main">
        <span class="cp-content-version-number">{version.version_number}</span>
        <span class="cp-content-version-name">{version.name}</span>
      </div>
      <div class="cp-content-version-tags">
        {version.loaders.slice(0, 2).map((loader) => (
          <span key={loader} class="cp-discover-tag">
            {tagLabel(loader)}
          </span>
        ))}
        <span class="cp-discover-tag">{version.game_versions[0] ?? '—'}</span>
      </div>
      <span class="cp-content-version-meta">
        {formatDate(version.published) || formatAge(version.published)}
        {size ? ` · ${formatBytes(size)}` : ''}
        {version.downloads ? ` · ${formatCount(version.downloads)} downloads` : ''}
      </span>
      {canAdd && (
        <button
          class="cp-content-version-add"
          onClick={add}
          disabled={busy}
          title={`Add this version to ${instance?.name}`}
        >
          {busy ? <Spinner size={12} /> : <Icon name="plus" size={13} />}
          {busy ? 'Adding…' : 'Add'}
        </button>
      )}
    </div>
  );
}

function Facts({ detail }: { detail: ContentDetail }): JSX.Element {
  const loaders = detail.loaders.slice(0, 6);
  const games = useMemo(() => detail.game_versions.slice(-6).reverse(), [detail.game_versions]);

  return (
    <div class="cp-content-facts">
      <div class="cp-content-facts-row">
        <span>Updated</span>
        <b>{formatAge(detail.updated)}</b>
      </div>
      <div class="cp-content-facts-row">
        <span>Downloads</span>
        <b>{detail.downloads.toLocaleString()}</b>
      </div>
      <div class="cp-content-facts-row">
        <span>Followers</span>
        <b>{detail.follows.toLocaleString()}</b>
      </div>
      {loaders.length > 0 && (
        <div class="cp-content-facts-block">
          <span>Loaders</span>
          <div class="cp-content-facts-tags">
            {loaders.map((loader) => (
              <span key={loader} class="cp-discover-tag">
                {tagLabel(loader)}
              </span>
            ))}
          </div>
        </div>
      )}
      {games.length > 0 && (
        <div class="cp-content-facts-block">
          <span>Minecraft</span>
          <div class="cp-content-facts-tags">
            {games.map((game) => (
              <span key={game} class="cp-discover-tag">
                {game}
              </span>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

function InstallRail({ detail, instance }: { detail: ContentDetail; instance: EnrichedInstance | null }): JSX.Element {
  const [busy, setBusy] = useState(false);
  const [conflictPlan, setConflictPlan] = useState<ResolutionPlan | null>(null);
  const staged = isStaged(detail.canonical_id);
  const selections: ContentSelection[] = [{ canonical_id: detail.canonical_id, kind: detail.kind }];
  const latest = detail.versions[0];

  if (detail.kind === 'modpack') {
    return (
      <div class="cp-content-rail-card">
        <div class="cp-content-rail-head">
          <Icon name="stack" size={15} />
          <div class="cp-content-rail-title">This is a modpack</div>
        </div>
        <p class="cp-content-rail-note">
          A pack is a whole instance, so it gets set up as its own. Nothing in your existing instances is touched.
        </p>
        <Button
          icon="plus"
          full
          disabled={busy}
          onClick={async () => {
            setBusy(true);
            await createFromModpack(detail.canonical_id);
            setBusy(false);
          }}
        >
          {busy ? 'Setting up…' : 'Set up an instance'}
        </Button>
        {latest && (
          <div class="cp-content-rail-foot">
            {latest.version_number} · {latest.game_versions[0] ?? 'unknown'}
          </div>
        )}
      </div>
    );
  }

  const fitting = instance ? detail.versions.find((version) => versionFits(version, detail.kind, instance)) : latest;
  const blocked = !!instance && !fitting;

  const add = async (): Promise<void> => {
    if (!instance || busy) return;
    setBusy(true);
    const outcome = await addToInstance(instance.id, selections, detail.title);
    setBusy(false);
    if (outcome.status === 'needs-confirmation' && outcome.plan) setConflictPlan(outcome.plan);
  };

  const confirm = async (): Promise<void> => {
    if (!instance) return;
    setBusy(true);
    await commitInstall(instance.id, selections, detail.title, conflictPlan ?? undefined);
    setBusy(false);
    setConflictPlan(null);
  };

  return (
    <div class="cp-content-rail-card">
      {instance ? (
        <>
          <div class="cp-content-rail-head">
            <Icon name="download" size={15} />
            <div class="cp-content-rail-title">Add to {instance.name}</div>
          </div>
          <p class="cp-content-rail-note">
            {blocked
              ? `No release of this ${KIND_NOUN[detail.kind]} fits ${instance.version_display.summary_label}.`
              : `${instance.version_display.summary_label} · ${fitting?.version_number ?? 'latest'}`}
          </p>
          <Button icon="download" full onClick={add} disabled={busy || blocked}>
            {busy ? 'Adding…' : blocked ? 'Nothing to add' : `Add to ${instance.name}`}
          </Button>
        </>
      ) : (
        <>
          <div class="cp-content-rail-head">
            <Icon name="compass" size={15} />
            <div class="cp-content-rail-title">Where should this go?</div>
          </div>
          <p class="cp-content-rail-note">
            Pick an instance to add it now, or stage it with a few more and build one that fits them all.
          </p>
          <InstancePicker />
          <Button
            icon={staged ? 'check' : 'plus'}
            variant={staged ? 'secondary' : 'primary'}
            full
            onClick={() =>
              staged
                ? unstage(detail.canonical_id)
                : stage({
                    canonical_id: detail.canonical_id,
                    kind: detail.kind,
                    title: detail.title,
                    icon_url: detail.icon_url,
                  })
            }
          >
            {staged ? 'Staged — remove' : 'Stage this'}
          </Button>
        </>
      )}

      {conflictPlan && (
        <ConflictSheet plan={conflictPlan} busy={busy} onCancel={() => setConflictPlan(null)} onConfirm={confirm} />
      )}
    </div>
  );
}

function InstancePicker(): JSX.Element | null {
  const current = route.value;
  const id = current.name === 'content' ? current.id : '';
  const options = contentTargets.value.map((instance) => ({
    value: instance.id,
    label: `${instance.name} · ${instance.version_display.summary_label}`,
  }));
  if (options.length === 0) return null;

  return (
    <SelectField
      value=""
      onChange={(target) => navigate({ name: 'content', id, target })}
      options={[{ value: '', label: 'Choose an instance…' }, ...options]}
      ariaLabel="Choose an instance"
      width="100%"
    />
  );
}

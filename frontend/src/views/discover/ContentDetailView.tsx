import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Button, Pill } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { SelectField } from '../../ui/Select';
import { navigate, route } from '../../ui-state';
import { getContentDetail } from '../../content';
import { errMessage } from '../../utils';
import type { ContentDetail, ResolutionPlan } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';
import { addToInstance, commitInstall, createFromModpack } from './actions';
import { ConflictSheet } from './Tray';
import { contentTargets, isStaged, stage, targetInstance, unstage } from './state';
import { ContentIcon, formatCount, KIND_NOUN, plural } from './shared';

/**
 * A content page is a destination, not an interruption: it has a description, a
 * gallery, a version history and a decision at the end of it. Making it a route
 * rather than a modal also means the back button returns to the search you came
 * from, with your results still there.
 */
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
      <div class="cp-view-page">
        <button class="cp-content-back" onClick={back}>
          <Icon name="arrow-left" size={13} /> Discover
        </button>
        <div class="cp-discover-empty cp-discover-empty--pad">
          <Icon name="alert" size={20} />
          <div>{error}</div>
        </div>
      </div>
    );
  }

  if (!detail) {
    return (
      <div class="cp-view-page">
        <button class="cp-content-back" onClick={back}>
          <Icon name="arrow-left" size={13} /> Discover
        </button>
        <div class="cp-content-head">
          <div class="cp-content-icon cp-skeleton" />
          <div style={{ flex: 1 }}>
            <div class="cp-skeleton cp-skeleton-line" style={{ width: '40%', height: 22 }} />
            <div class="cp-skeleton cp-skeleton-line" style={{ width: '25%', marginTop: 10 }} />
          </div>
        </div>
      </div>
    );
  }

  return (
    <div class="cp-view-page cp-content">
      <button class="cp-content-back" onClick={back}>
        <Icon name="arrow-left" size={13} /> Discover
      </button>

      <div class="cp-content-head">
        <div class="cp-content-icon" aria-hidden="true">
          <ContentIcon url={detail.icon_url} kind={detail.kind} size={30} />
        </div>
        <div class="cp-content-headings">
          <h1 class="cp-content-title">{detail.title}</h1>
          {detail.author && <div class="cp-content-author">by {detail.author}</div>}
          <div class="cp-content-meta">
            <Pill icon="download">{formatCount(detail.downloads)}</Pill>
            <Pill icon="user">{formatCount(detail.follows)}</Pill>
            <span class="cp-discover-tag">{KIND_NOUN[detail.kind]}</span>
            {detail.categories.slice(0, 4).map((category) => (
              <span key={category} class="cp-discover-tag">
                {category}
              </span>
            ))}
          </div>
        </div>
      </div>

      <div class="cp-content-layout">
        <div class="cp-content-main">
          <p class="cp-content-summary">{detail.summary}</p>

          {detail.gallery.length > 0 && (
            <div class="cp-content-gallery">
              {detail.gallery.slice(0, 8).map((image) => (
                <figure key={image.url}>
                  <img src={image.url} alt={image.title ?? ''} loading="lazy" />
                  {image.title && <figcaption>{image.title}</figcaption>}
                </figure>
              ))}
            </div>
          )}

          {detail.versions.length > 0 && (
            <section class="cp-content-versions">
              <h2 class="cp-content-section-title">Versions</h2>
              {detail.versions.slice(0, 8).map((version) => (
                <div key={version.id} class="cp-discover-version-row">
                  <span class="cp-discover-version-name" title={version.name}>
                    {version.version_number}
                  </span>
                  <span class="cp-discover-version-loaders">
                    {[...version.loaders, ...version.game_versions.slice(0, 2)].join(' · ')}
                  </span>
                  <span class="cp-discover-version-channel" data-channel={version.channel}>
                    {version.channel}
                  </span>
                </div>
              ))}
            </section>
          )}
        </div>

        <aside class="cp-content-rail">
          <InstallRail detail={detail} instance={instance} />
        </aside>
      </div>
    </div>
  );
}

function InstallRail({ detail, instance }: { detail: ContentDetail; instance: EnrichedInstance | null }): JSX.Element {
  const [busy, setBusy] = useState(false);
  const [conflictPlan, setConflictPlan] = useState<ResolutionPlan | null>(null);
  const staged = isStaged(detail.canonical_id);
  const selections = [{ canonical_id: detail.canonical_id, kind: detail.kind }];

  if (detail.kind === 'modpack') {
    return (
      <div class="cp-content-rail-card">
        <div class="cp-content-rail-title">This is a modpack</div>
        <p class="cp-content-rail-note">
          A pack is a whole instance, so it gets set up as its own — nothing in your existing instances is touched.
        </p>
        <Button
          icon="sparkles"
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
      </div>
    );
  }

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
          <div class="cp-content-rail-title">Add to {instance.name}</div>
          <p class="cp-content-rail-note">{instance.version_display.summary_label}</p>
          <Button icon="download" full onClick={add} disabled={busy}>
            {busy ? 'Adding…' : `Add to ${instance.name}`}
          </Button>
        </>
      ) : (
        <>
          <div class="cp-content-rail-title">Where should this go?</div>
          <p class="cp-content-rail-note">
            Pick an instance, or stage this and a few more and build one that fits them all.
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
            {staged ? 'Staged' : 'Stage this'}
          </Button>
        </>
      )}

      {detail.versions.length > 0 && (
        <div class="cp-content-rail-foot">
          {plural(detail.versions.length, 'version', 'versions')} · latest {detail.versions[0].version_number}
        </div>
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

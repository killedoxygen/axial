import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { Button, Chip, Kbd } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import type { ContentKind, ContentVersion, InstallState } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';
import { setUpModpack } from './actions';
import type { InstallFlow } from './install-flow';
import { ModpackPicker } from './ModpackPicker';
import { isStaged, stageContent, stagedItem, unstage } from './state';

export interface Actionable {
  canonical_id: string;
  kind: ContentKind;
  title: string;
  icon_url?: string;
  install_state?: InstallState;
}

/** What Enter does to this item, for the list's key legend. */
export function actionVerb(item: Actionable, instance: EnrichedInstance | null): string | null {
  if (item.install_state === 'installed') return null;
  if (item.kind === 'modpack') return instance ? 'choose files' : 'set up';
  if (instance) return 'add';
  return isStaged(item.canonical_id) ? 'unstage' : 'stage';
}

/**
 * The one thing to do with a piece of content, wherever it is listed: add it to the target instance, or with no
 * target stage it; a modpack is set up as an instance of its own, or opened to choose files for the target.
 * `version` pins all of these to one version.
 */
export function ContentAction({
  item,
  version,
  instance,
  flow,
  unfit,
  lead = false,
  inHand = false,
}: {
  item: Actionable;
  version?: ContentVersion;
  instance: EnrichedInstance | null;
  flow: InstallFlow;
  /** Why nothing here can go into the target instance. */
  unfit?: string;
  /** The page's main action rather than a row's. */
  lead?: boolean;
  /** Enter runs it: solid, with the key. */
  inHand?: boolean;
}): JSX.Element {
  const [busy, setBusy] = useState(false);
  const [picking, setPicking] = useState(false);
  const run = (work: Promise<unknown>): void => {
    setBusy(true);
    void work.finally(() => setBusy(false));
  };
  const look = {
    size: lead ? ('lg' as const) : ('sm' as const),
    variant: lead || inHand ? ('primary' as const) : ('secondary' as const),
    trailing: inHand ? <Kbd>↵</Kbd> : undefined,
  };

  if (item.install_state === 'installed' && !version) {
    return (
      <span class="cp-discover-installed" title="Already in this instance">
        <Icon name="check" size={13} stroke={2.6} />
        Installed
      </span>
    );
  }

  if (item.kind === 'modpack') {
    if (!instance) {
      return (
        <Button
          {...look}
          icon="stack"
          busy={busy}
          disabled={busy}
          title={version ? `Create an instance from ${version.version_number}` : 'Create an instance from this pack'}
          onClick={() => run(setUpModpack(item.canonical_id, version?.id, item.icon_url))}
        >
          Set up
        </Button>
      );
    }
    return (
      <>
        <Button {...look} icon="plus" title={`Choose files for ${instance.name}`} onClick={() => setPicking(true)}>
          Choose files
        </Button>
        <ModpackPicker
          open={picking}
          instanceId={instance.id}
          canonicalId={item.canonical_id}
          versionId={version?.id}
          onClose={() => setPicking(false)}
        />
      </>
    );
  }

  if (instance) {
    if (unfit) {
      return (
        <Chip tone="warn" icon="alert" title={unfit}>
          {version ? 'Does not fit' : 'No version fits'}
        </Chip>
      );
    }
    const label = version ? `${item.title} ${version.version_number}` : item.title;
    return (
      <Button
        {...look}
        icon="plus"
        busy={busy}
        disabled={busy || flow.busy}
        title={`Add ${label} to ${instance.name}`}
        onClick={() =>
          run(
            flow.add(
              [{ canonical_id: item.canonical_id, kind: item.kind, ...(version ? { version_id: version.id } : {}) }],
              label,
            ),
          )
        }
      >
        Add
      </Button>
    );
  }

  const stagedEntry = stagedItem(item.canonical_id);
  const staged = version ? stagedEntry?.version_id === version.id : isStaged(item.canonical_id);
  return (
    <Button
      {...look}
      variant={staged ? 'soft' : look.variant}
      icon={staged ? 'check' : 'plus'}
      pressed={staged}
      title={
        staged
          ? 'Remove from selection'
          : version && stagedEntry
            ? `Switch the staged version to ${version.version_number}`
            : 'Add to selection'
      }
      onClick={() => {
        if (staged) unstage(item.canonical_id);
        else stageContent(item, version);
      }}
    >
      {staged ? 'Staged' : 'Stage'}
    </Button>
  );
}

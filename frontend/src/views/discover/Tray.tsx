import type { JSX } from 'preact';
import { useRef, useState } from 'preact/hooks';
import { Button, Chip, IconButton, Ring } from '../../ui/Atoms';
import { Art } from '../../ui/Art';
import { FloatingTray, FloatingTrayLabel } from '../../ui/FloatingTray';
import { Icon } from '../../ui/Icons';
import { Popover } from '../../ui/Popover';
import { plural } from '../../format';
import type { ContentKind } from '../../types-content';
import { navigate } from '../../ui-state';
import { type Fit, recheck, staging } from './fit';
import { InstallConflictSheet, useInstallFlow } from './install-flow';
import { createEach, openStaged, type StagedPack } from './make';
import { FitFacts, InstanceTargetPicker, KIND_ICON, KIND_NOUN, tagLabel } from './shared';
import { clearTray, selectionOf, targetInstance, tray, unstage, unstageAll, type TrayItem } from './state';

const PREVIEW_LIMIT = 5;
const KIND_ORDER: ContentKind[] = ['modpack', 'mod', 'resource_pack', 'shader_pack'];

/** Why a pick that is not a pack would be left out, if it would. */
function leftOut(item: TrayItem, packs: StagedPack[], fit: Fit | null): { label: string; why: string } | null {
  if (fit) {
    return fit.drops?.has(item.canonical_id)
      ? { label: 'Will not fit', why: `No version for ${fit.loaderLabel} ${fit.gameVersion}` }
      : null;
  }
  const known = packs.filter((pack) => pack.fit?.drops);
  const taking = known.filter((pack) => !pack.fit!.drops!.has(item.canonical_id));
  if (known.length === 0 || taking.length === known.length) return null;
  return {
    label: taking.length === 0 ? 'Fits no pack' : `Fits ${taking.length} of ${known.length}`,
    why:
      taking.length === 0
        ? 'None of the staged packs has a version of this'
        : `Goes into ${taking.map((pack) => pack.item.title).join(', ')}`,
  };
}

/**
 * The picks, read left to right: what is staged (open it to review), what it runs on, then the one way to make it
 * real. Packs each become an instance and take the other picks that fit; with no pack the picks make one instance
 * or go into an existing one.
 */
export function Tray(): JSX.Element | null {
  const items = tray.value;
  const instance = targetInstance.value;
  const flow = useInstallFlow(instance?.id);
  const { packs, extras, fit, pending, apart, error } = staging.value;
  const [reviewing, setReviewing] = useState(false);
  const [creating, setCreating] = useState(false);
  const itemsRef = useRef<HTMLButtonElement>(null);
  if (items.length === 0) return null;

  const hidden = items.length - PREVIEW_LIMIT;
  const ordered = [...items].sort((a, b) => KIND_ORDER.indexOf(a.kind) - KIND_ORDER.indexOf(b.kind));
  // A pack's instance takes the picks that fit it, so it waits for that to be known. Picks alone go to a form
  // that checks them itself.
  const ready = packs.every((pack) => pack.target) && (packs.length === 0 || !error);
  const busy = flow.busy || creating;

  const addToInstance = async (): Promise<void> => {
    const taken = extras.map(selectionOf);
    const outcome = await flow.add(taken, plural(taken.length, 'pick', 'picks'));
    if (outcome.status === 'queued') unstageAll(taken);
  };
  const createAll = (): void => {
    setCreating(true);
    void createEach(packs, extras).finally(() => setCreating(false));
  };

  return (
    <>
      <FloatingTray class="cp-tray" ariaLabel={plural(items.length, 'pick', 'picks')} reserveSpace>
        <button
          ref={itemsRef}
          type="button"
          class="cp-tray-items"
          aria-haspopup="dialog"
          aria-expanded={reviewing}
          title="Review your picks"
          onClick={() => setReviewing((value) => !value)}
        >
          {ordered.slice(0, PREVIEW_LIMIT).map((item) => (
            <Art key={item.canonical_id} url={item.icon_url} icon={KIND_ICON[item.kind]} size={24} />
          ))}
          {hidden > 0 && <span class="cp-unit">+{hidden}</span>}
          <FloatingTrayLabel>{plural(items.length, 'pick', 'picks')}</FloatingTrayLabel>
          <Icon name="chevron-up" size={13} stroke={2.2} />
        </button>

        {!instance && (
          <span class="cp-chips cp-tray-fit" aria-live="polite">
            {pending ? (
              <Ring size={14} />
            ) : (
              <>
                {fit && <FitFacts fit={fit} />}
                {packs.length > 1 && <Chip icon="stack">{packs.length} instances</Chip>}
                {fit?.drops && fit.drops.size > 0 && (
                  <Chip tone="warn" icon="alert" title="Open your picks to see which">
                    {fit.drops.size} will not fit
                  </Chip>
                )}
                {error && (
                  <Chip tone="warn" icon="alert" title={error}>
                    Fit not checked
                  </Chip>
                )}
                {apart && (
                  <Chip tone="warn" icon="alert" title="No loader and Minecraft version runs all of these">
                    No shared version
                  </Chip>
                )}
              </>
            )}
          </span>
        )}

        {!instance && error && !pending && (
          <Button variant="ghost" size="sm" icon="refresh" title="Check again what fits" onClick={recheck}>
            Retry
          </Button>
        )}
        <Button variant="ghost" size="sm" onClick={clearTray} disabled={busy}>
          Clear
        </Button>
        {instance ? (
          <Button
            size="sm"
            icon="plus"
            busy={flow.busy}
            disabled={busy || extras.length === 0}
            title={extras.length === 0 ? 'A modpack becomes an instance of its own' : undefined}
            onClick={() => void addToInstance()}
          >
            Add to {instance.name}
          </Button>
        ) : (
          <>
            {packs.length === 0 && (
              <InstanceTargetPicker
                placeholder="Add to existing…"
                onPick={(id) => navigate({ name: 'discover', target: id })}
              />
            )}
            {packs.length > 1 ? (
              <Button size="sm" icon="plus" busy={creating} disabled={busy || pending || !ready} onClick={createAll}>
                Create {packs.length} instances
              </Button>
            ) : (
              <Button size="sm" icon="plus" disabled={busy || !ready} onClick={() => openStaged(packs[0], extras)}>
                Create instance
              </Button>
            )}
          </>
        )}
      </FloatingTray>

      {reviewing && (
        <Popover
          anchor={itemsRef}
          onDismiss={() => setReviewing(false)}
          gap={10}
          maxHeight={420}
          class="cp-pop cp-tray-review"
          role="dialog"
          aria-label="Your picks"
        >
          {ordered.map((item) => {
            const pack = packs.find((entry) => entry.item === item);
            const out = pack || instance ? null : leftOut(item, packs, fit);
            return (
              <div key={item.canonical_id} class="cp-row" data-out={out ? true : undefined}>
                <Art url={item.icon_url} icon={KIND_ICON[item.kind]} size={28} />
                <strong title={item.title}>{item.title}</strong>
                <Chip tone="bare" icon={KIND_ICON[item.kind]}>
                  {tagLabel(KIND_NOUN[item.kind])}
                </Chip>
                <span class="cp-head-fill" />
                {item.version_label && (
                  <Chip mono title="Pinned to this version">
                    {item.version_label}
                  </Chip>
                )}
                {pack?.fit && <FitFacts fit={pack.fit} />}
                {out && (
                  <Chip tone="warn" icon="alert" title={out.why}>
                    {out.label}
                  </Chip>
                )}
                <IconButton
                  icon="x"
                  size={24}
                  tooltip={`Remove ${item.title}`}
                  disabled={busy}
                  onClick={() => {
                    if (items.length === 1) setReviewing(false);
                    unstage(item.canonical_id);
                  }}
                />
              </div>
            );
          })}
        </Popover>
      )}
      <InstallConflictSheet flow={flow} onQueued={unstageAll} />
    </>
  );
}

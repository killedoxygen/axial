import type { JSX } from 'preact';
import { apiResourceUrl } from '../../api';
import type { DefaultSkin } from '../../default-skins';
import {
  applyDefaultSkin,
  applySkin,
  cancelPendingApply,
  flushPendingApply,
  resetProfileCape,
  resetProfileSkin,
  saveProfileSkinLocally,
  wardrobeContext,
  wardrobeOp,
} from '../../machines/skin-wardrobe';
import { profileCape } from '../../player-skin';
import { Button, Empty, IconButton, Status } from '../../ui/Atoms';
import { openContextMenu, type ContextMenuItem } from '../../ui/ContextMenu';
import { capeFileUrl, lookupCapeFileUrl, lookupSkinFileUrl, savedSkinFileUrl } from './api';
import { LazySkinThreePreview } from './LazySkinThreePreview';
import type { MinecraftSkinLookup, SavedSkinRecord, SkinVariant } from './types';

/** The figure fills its scene from head to feet; the scene is let out by these shares to make room for it. */
const FIT = { top: 0.15, bottom: 0.14, side: 0.08 };

export type SkinStageModel =
  | { kind: 'empty'; loading: boolean }
  | { kind: 'lookup'; profile: MinecraftSkinLookup; variant: SkinVariant }
  | { kind: 'default'; skin: DefaultSkin; worn: boolean }
  | { kind: 'profile' }
  | {
      kind: 'saved';
      skin: SavedSkinRecord;
      capeSrc?: string;
      queued: boolean;
      worn: boolean;
      /** While it is being edited the stage shows the edit, not the saved skin. */
      edit: { src: string | null; variant: SkinVariant; capeSrc?: string } | null;
      menuItems: ContextMenuItem[];
    };

/** From a skin that is only being tried on, the way back to the one worn. */
export interface StageBack {
  label: string;
  run: () => void;
}

interface Figure {
  name: string;
  src: string;
  variant: SkinVariant;
  capeSrc?: string;
}

/** The skin in hand, as tall as the page allows; under it its name and one line of state or actions. */
export function SkinStage({
  model,
  back,
  onSaveLookup,
  onOpenUploadPicker,
}: {
  model: SkinStageModel;
  back: StageBack | null;
  onSaveLookup: (wear: boolean) => void;
  onOpenUploadPicker: () => void;
}): JSX.Element {
  const op = wardrobeOp.value;
  const busy = op !== null;
  const { skinActionsEnabled: canWear, account } = wardrobeContext.value;

  const figure = (): Figure | null => {
    switch (model.kind) {
      case 'lookup':
        return {
          name: model.profile.username,
          src: lookupSkinFileUrl(model.profile),
          variant: model.variant,
          capeSrc: lookupCapeFileUrl(model.profile),
        };
      case 'default':
        return { name: model.skin.name, src: model.skin.src, variant: model.skin.variant };
      case 'profile': {
        const skin = account?.profile_skin;
        const cape = profileCape(account);
        if (!account || !skin) return null;
        return {
          name: account.display_name,
          src: apiResourceUrl('/skin/profile/file'),
          variant: skin.variant,
          capeSrc: cape ? capeFileUrl(cape) : undefined,
        };
      }
      case 'saved':
        return {
          name: model.skin.name,
          src: model.edit?.src ?? savedSkinFileUrl(model.skin),
          variant: model.edit?.variant ?? model.skin.variant,
          capeSrc: model.edit ? model.edit.capeSrc : model.capeSrc,
        };
      case 'empty':
        return null;
    }
  };

  const wear = (run: () => void, working: boolean): JSX.Element | false =>
    canWear && (
      <Button icon="shirt" busy={working} disabled={busy} sound="affirm" onClick={run}>
        Wear this skin
      </Button>
    );
  const goBack = back && (
    <Button variant="ghost" disabled={busy} onClick={back.run}>
      {back.label}
    </Button>
  );
  const more = (items: ContextMenuItem[]): JSX.Element | false =>
    items.length > 0 && (
      <IconButton icon="dots" tooltip="More actions" onClick={(event) => openContextMenu(event, items)} />
    );

  const state = (): JSX.Element | null => {
    switch (model.kind) {
      case 'lookup': {
        const saving = op?.kind === 'lookup';
        return (
          <>
            {canWear && (
              <Button icon="shirt" busy={saving} disabled={busy} sound="affirm" onClick={() => onSaveLookup(true)}>
                Save and wear
              </Button>
            )}
            <Button
              variant={canWear ? 'secondary' : 'primary'}
              icon={canWear ? undefined : 'download'}
              busy={!canWear && saving}
              disabled={busy}
              title="Keep a copy in your skins"
              onClick={() => onSaveLookup(false)}
            >
              Save
            </Button>
          </>
        );
      }
      case 'default':
        return model.worn ? (
          <Status eyes="happy" tone="accent">
            Wearing
          </Status>
        ) : (
          <>
            {wear(() => void applyDefaultSkin(model.skin), op?.kind === 'apply' || op?.kind === 'upload')}
            {goBack}
          </>
        );
      case 'profile': {
        const menu: ContextMenuItem[] = [];
        if (canWear) menu.push({ icon: 'x', label: 'Reset profile skin', onSelect: () => void resetProfileSkin() });
        if (canWear && profileCape(account)) {
          menu.push({ icon: 'x', label: 'Reset profile cape', onSelect: () => void resetProfileCape() });
        }
        return (
          <>
            <Status eyes="happy" tone="accent">
              Wearing
            </Status>
            {canWear && (
              <Button
                variant="secondary"
                icon="download"
                busy={op?.kind === 'save-profile'}
                disabled={busy}
                title="Keep a copy of this skin in your skins"
                onClick={() => void saveProfileSkinLocally()}
              >
                Save
              </Button>
            )}
            {goBack}
            {more(menu)}
          </>
        );
      }
      case 'saved': {
        if (model.edit) return null;
        const key = model.skin.texture_key;
        return (
          <>
            {model.queued ? (
              <>
                <Status busy>Queued for the next launch</Status>
                <Button
                  icon="check"
                  busy={op?.kind === 'flush'}
                  disabled={!canWear || busy}
                  sound="affirm"
                  onClick={() => void flushPendingApply()}
                >
                  Wear now
                </Button>
                <Button variant="ghost" disabled={busy} onClick={() => void cancelPendingApply()}>
                  Cancel
                </Button>
              </>
            ) : model.worn ? (
              <Status eyes="happy" tone="accent">
                Wearing
              </Status>
            ) : (
              <>
                {wear(() => void applySkin(key), op?.kind === 'apply' && op.key === key)}
                {goBack}
              </>
            )}
            {more(model.menuItems)}
          </>
        );
      }
      case 'empty':
        return null;
    }
  };

  const shown = figure();
  return (
    <section class="cp-stage" aria-label="Skin preview">
      {shown ? (
        <>
          <div class="cp-stage-figure">
            <LazySkinThreePreview
              src={shown.src}
              capeSrc={shown.capeSrc}
              name={shown.name}
              variant={shown.variant}
              side="front"
              showOuterLayers
              fitPadding={FIT}
            />
          </div>
          <div class="cp-stage-caption">
            <h2 title={shown.name}>{shown.name}</h2>
            <div class="cp-stage-state">{state()}</div>
          </div>
        </>
      ) : model.kind === 'empty' && model.loading ? (
        <Status busy>Loading skins</Status>
      ) : (
        <Empty
          title="No skin yet"
          actions={
            <Button variant="secondary" icon="plus" disabled={busy} onClick={onOpenUploadPicker}>
              Add skin
            </Button>
          }
        />
      )}
    </section>
  );
}

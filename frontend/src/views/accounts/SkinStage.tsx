import type { JSX } from 'preact';
import { apiResourceUrl } from '../../api';
import type { DefaultSkin } from '../../default-skins';
import {
  applyDefaultSkin,
  applySkin,
  cancelPendingApply,
  deleteSavedSkin,
  downloadSavedSkin,
  flushPendingApply,
  resetProfileCape,
  resetProfileSkin,
  resetWardrobePreview,
  returnFromProfilePreview,
  saveProfileSkinLocally,
  wardrobeContext,
  wardrobeOp,
} from '../../machines/skin-wardrobe';
import { Button } from '../../ui/Atoms';
import { openContextMenu, type ContextMenuItem } from '../../ui/ContextMenu';
import { Icon } from '../../ui/Icons';
import {
  activeMinecraftCape,
  activeMinecraftSkin,
  capeFileUrl,
  lookupCapeFileUrl,
  lookupSkinFileUrl,
  savedSkinFileUrl,
} from './api';
import { LazySkinThreePreview as SkinThreePreview } from './LazySkinThreePreview';
import type { MinecraftSkinLookup, SavedSkinRecord, SkinVariant } from './types';

const STAGE_FIT = { top: 0.15, bottom: 0.14, side: 0.08 };

export type SkinStageModel =
  | { kind: 'empty'; loading: boolean }
  | { kind: 'lookup'; profile: MinecraftSkinLookup; variant: SkinVariant }
  | { kind: 'default'; skin: DefaultSkin }
  | { kind: 'profile'; returnable: boolean }
  | {
      kind: 'saved';
      skin: SavedSkinRecord;
      capeSrc?: string;
      queued: boolean;
      worn: boolean;
      editing: boolean;
      editingSrc: string | null;
      editVariant: SkinVariant;
      editCapeSrc?: string;
    };

function stageMenuButton(items: ContextMenuItem[]): JSX.Element | null {
  if (items.length === 0) return null;
  return (
    <Button
      variant="ghost"
      size="lg"
      icon="dots"
      title="More actions"
      onClick={(event) => openContextMenu(event, items)}
    />
  );
}

export function SkinStage({
  model,
  nametag,
  onRenameNametag,
  onSaveLookup,
  onDismissLookup,
  onStartEdit,
  onOpenUploadPicker,
}: {
  model: SkinStageModel;
  nametag: string | null;
  onRenameNametag?: () => void;
  onSaveLookup: (applyAfterSave: boolean) => void;
  onDismissLookup: () => void;
  onStartEdit: (skin: SavedSkinRecord) => void;
  onOpenUploadPicker: () => void;
}): JSX.Element {
  const op = wardrobeOp.value;
  const busy = op !== null;
  const { skinActionsEnabled, profile } = wardrobeContext.value;

  const renderScene = (): JSX.Element | null => {
    switch (model.kind) {
      case 'lookup':
        return (
          <SkinThreePreview
            src={lookupSkinFileUrl(model.profile)}
            capeSrc={lookupCapeFileUrl(model.profile)}
            name={model.profile.username}
            nametag={model.profile.username}
            variant={model.variant}
            side="front"
            showOuterLayers
            fitPadding={STAGE_FIT}
            showHint={false}
          />
        );
      case 'default':
        return (
          <SkinThreePreview
            src={model.skin.src}
            name={model.skin.name}
            nametag={nametag}
            onNametagEdit={onRenameNametag}
            variant={model.skin.variant}
            side="front"
            showOuterLayers
            fitPadding={STAGE_FIT}
            showHint={false}
          />
        );
      case 'profile': {
        const profileSkin = activeMinecraftSkin(profile ?? undefined);
        const profileCape = activeMinecraftCape(profile ?? undefined);
        if (!profile || !profileSkin) return null;
        return (
          <SkinThreePreview
            src={apiResourceUrl('/skin/profile/file')}
            capeSrc={profileCape ? capeFileUrl(profileCape) : undefined}
            name={profile.name}
            nametag={nametag}
            onNametagEdit={onRenameNametag}
            variant={profileSkin.variant === 'slim' ? 'slim' : 'classic'}
            side="front"
            showOuterLayers
            fitPadding={STAGE_FIT}
            showHint={false}
          />
        );
      }
      case 'saved':
        return (
          <SkinThreePreview
            src={model.editing && model.editingSrc ? model.editingSrc : savedSkinFileUrl(model.skin)}
            capeSrc={model.editing ? model.editCapeSrc : model.capeSrc}
            name={model.skin.name}
            nametag={nametag}
            onNametagEdit={onRenameNametag}
            variant={model.editing ? model.editVariant : model.skin.variant}
            side="front"
            showOuterLayers
            fitPadding={STAGE_FIT}
            showHint={false}
          />
        );
      case 'empty':
        return null;
    }
  };

  const caption = (): { name: string; status: string | null } | null => {
    switch (model.kind) {
      case 'lookup':
        return { name: model.profile.username, status: 'Current skin of this player' };
      case 'default':
        return { name: model.skin.name, status: 'Minecraft default skin' };
      case 'profile':
        return { name: profile?.name ?? 'Minecraft profile', status: 'Current Minecraft profile skin' };
      case 'saved':
        if (model.queued) return { name: model.skin.name, status: 'Queued for the next launch' };
        if (model.worn) return { name: model.skin.name, status: 'Worn on your Minecraft profile' };
        return { name: model.skin.name, status: null };
      case 'empty':
        return null;
    }
  };

  const renderActions = (): JSX.Element | null => {
    switch (model.kind) {
      case 'lookup': {
        const saveBusy = op?.kind === 'lookup';
        return (
          <>
            <Button
              variant="ghost"
              size="lg"
              icon="x"
              disabled={saveBusy}
              onClick={onDismissLookup}
              title="Stop previewing this player skin"
            >
              Dismiss
            </Button>
            {skinActionsEnabled ? (
              <>
                <Button
                  variant="secondary"
                  size="lg"
                  icon={saveBusy ? 'refresh' : 'download'}
                  disabled={busy}
                  onClick={() => onSaveLookup(false)}
                  title="Keep a copy in your library without wearing it"
                >
                  Save
                </Button>
                <Button
                  variant="primary"
                  size="lg"
                  icon={saveBusy ? 'refresh' : 'check'}
                  disabled={busy}
                  onClick={() => onSaveLookup(true)}
                  title="Save to your library and wear this skin"
                  sound="affirm"
                >
                  Apply
                </Button>
              </>
            ) : (
              <Button
                variant="primary"
                size="lg"
                icon={saveBusy ? 'refresh' : 'download'}
                disabled={busy}
                onClick={() => onSaveLookup(false)}
                title="Keep a copy in your library"
                sound="affirm"
              >
                Save
              </Button>
            )}
          </>
        );
      }
      case 'default': {
        const applying = op?.kind === 'apply' || op?.kind === 'upload';
        return (
          <>
            {model.skin.id !== 'steve' && (
              <Button
                variant="secondary"
                size="lg"
                icon="refresh"
                disabled={busy}
                onClick={() => resetWardrobePreview()}
                title="Reset the account avatar to Steve"
              >
                Reset
              </Button>
            )}
            {skinActionsEnabled && (
              <Button
                variant="primary"
                size="lg"
                icon={applying ? 'refresh' : 'check'}
                disabled={busy}
                onClick={() => void applyDefaultSkin(model.skin)}
                title="Wear this default skin on the active Minecraft account"
                sound="affirm"
              >
                {applying ? 'Applying' : 'Apply'}
              </Button>
            )}
          </>
        );
      }
      case 'profile': {
        const saving = op?.kind === 'save-profile';
        const menu: ContextMenuItem[] = [
          ...(skinActionsEnabled
            ? [{ icon: 'x', label: 'Reset profile skin', onSelect: () => void resetProfileSkin() }]
            : []),
          ...(skinActionsEnabled && activeMinecraftCape(profile ?? undefined)
            ? [{ icon: 'x', label: 'Reset profile cape', onSelect: () => void resetProfileCape() }]
            : []),
        ];
        return (
          <>
            {model.returnable && (
              <Button
                variant="secondary"
                size="lg"
                icon="refresh"
                disabled={busy}
                onClick={() => returnFromProfilePreview()}
                title="Return to your skin"
              >
                Back
              </Button>
            )}
            {skinActionsEnabled && (
              <Button
                variant="primary"
                size="lg"
                icon={saving ? 'refresh' : 'download'}
                disabled={busy}
                onClick={() => void saveProfileSkinLocally()}
                title="Keep a copy of this skin in your library"
                sound="affirm"
              >
                Save
              </Button>
            )}
            {stageMenuButton(menu)}
          </>
        );
      }
      case 'saved': {
        if (model.editing) return null;
        const applying = op?.kind === 'apply' && op.key === model.skin.texture_key;
        const flushing = op?.kind === 'flush';
        const canceling = op?.kind === 'cancel-pending';
        const menu: ContextMenuItem[] = [
          { icon: 'refresh', label: 'Reset preview to Steve', onSelect: () => resetWardrobePreview() },
          { icon: 'download', label: 'Download PNG', onSelect: () => void downloadSavedSkin(model.skin) },
          ...(!model.worn
            ? [
                { label: '', onSelect: () => {}, divider: true },
                { icon: 'trash', label: 'Delete', onSelect: () => void deleteSavedSkin(model.skin), danger: true },
              ]
            : []),
        ];
        if (model.queued) {
          return (
            <>
              <Button
                variant="secondary"
                size="lg"
                icon={canceling ? 'refresh' : 'x'}
                disabled={busy}
                onClick={() => void cancelPendingApply()}
                title="Cancel the queued skin change"
              >
                Cancel
              </Button>
              <Button
                variant="primary"
                size="lg"
                icon={flushing ? 'refresh' : 'check'}
                disabled={!skinActionsEnabled || busy}
                onClick={() => void flushPendingApply()}
                title="Apply the queued skin change now"
                sound="affirm"
              >
                {flushing ? 'Applying' : 'Apply now'}
              </Button>
              {stageMenuButton(menu)}
            </>
          );
        }
        return (
          <>
            <Button
              variant="secondary"
              size="lg"
              icon="edit"
              disabled={busy}
              onClick={() => onStartEdit(model.skin)}
              title="Rename, retexture, or change this skin's cape"
            >
              Edit skin
            </Button>
            {skinActionsEnabled && !model.worn && (
              <Button
                variant="primary"
                size="lg"
                icon={applying ? 'refresh' : 'check'}
                disabled={busy}
                onClick={() => void applySkin(model.skin.texture_key)}
                title="Wear this skin on the active Minecraft account"
                sound="affirm"
              >
                {applying ? 'Applying' : 'Apply'}
              </Button>
            )}
            {stageMenuButton(menu)}
          </>
        );
      }
      case 'empty':
        return null;
    }
  };

  return (
    <section class="cp-skinhall__stage" aria-label="Skin preview">
      <div class="cp-skinhall__backdrop" aria-hidden="true" />
      {model.kind === 'empty' ? (
        <div class="cp-skinhall__empty">
          <Icon name="image" size={28} color="var(--text-mute)" />
          <span>{model.loading ? 'Loading skins' : 'No skin selected yet'}</span>
          {!model.loading && (
            <Button variant="secondary" icon="plus" disabled={busy} onClick={onOpenUploadPicker}>
              Add skin
            </Button>
          )}
        </div>
      ) : (
        <>
          <div class="cp-skinhall__scene">{renderScene()}</div>
          <div class="cp-skinhall__foot">
            {(() => {
              const line = caption();
              if (!line) return null;
              return (
                <div class="cp-skinhall__caption">
                  <span class="cp-skinhall__caption-name">{line.name}</span>
                  {line.status && <span class="cp-skinhall__caption-status">{line.status}</span>}
                </div>
              );
            })()}
            <div class="cp-skinhall__actions">{renderActions()}</div>
            <div class="cp-skinhall__hint" aria-hidden="true">
              <Icon name="arrow-left" size={10} />
              <Icon name="arrow-right" size={10} />
              <span>Drag to rotate</span>
            </div>
          </div>
        </>
      )}
    </section>
  );
}

import type { JSX } from 'preact';
import { deleteSavedSkin, wardrobeContext } from '../../machines/skin-wardrobe';
import { Button, Input, Notice, Status } from '../../ui/Atoms';
import { Segmented } from '../../ui/Segmented';
import { savedSkinFileUrl, stagedSkinPreviewSrc } from './api';
import { SkinDialog, SkinField } from './SkinDialog';
import { NO_CAPE_VALUE, type MinecraftCape, type SavedSkinRecord, type SkinVariant } from './types';
import type { useSavedSkinEditWorkflow } from './use-saved-skin-edit-workflow';

const ARM_MODELS: Array<{ value: SkinVariant; label: string }> = [
  { value: 'classic', label: 'Classic' },
  { value: 'slim', label: 'Slim' },
];

/** A saved skin's name, arm model, cape and texture. A PNG dropped on the dialog replaces the texture. */
export function SkinEditDialog({
  skin,
  edit,
  capes,
  capeSrc,
  deleting,
  disabledReason,
}: {
  skin: SavedSkinRecord | null;
  edit: ReturnType<typeof useSavedSkinEditWorkflow>;
  capes: MinecraftCape[];
  capeSrc: (capeId: string | null) => string | undefined;
  deleting: boolean;
  disabledReason: string;
}): JSX.Element {
  const busy = edit.editBusyKey !== null;
  const locked = busy || edit.editDetectBusyKey !== null;
  const replacement = edit.editReplacement;
  return (
    <SkinDialog
      title="Edit skin"
      open={skin !== null}
      busy={busy}
      preview={
        skin && {
          src: replacement ? stagedSkinPreviewSrc(replacement) : savedSkinFileUrl(skin),
          name: edit.trimmedEditName || skin.name,
          variant: edit.editVariant,
          capeSrc: capeSrc(edit.editCapeId === NO_CAPE_VALUE ? null : edit.editCapeId),
        }
      }
      capes={capes}
      capeId={edit.editCapeId}
      onCapeChange={edit.setEditCapeId}
      canSave={Boolean(
        skin &&
        !locked &&
        edit.editReplacementReady &&
        edit.savedSkinEditHasChanges(skin) &&
        edit.trimmedEditName.length > 0,
      )}
      canWear={wardrobeContext.value.skinActionsEnabled}
      disabledReason={disabledReason}
      extraAction={
        skin &&
        !skin.applied_at && (
          <Button variant="ghost" icon="trash" disabled={busy || deleting} onClick={() => void deleteSavedSkin(skin)}>
            Delete
          </Button>
        )
      }
      onClose={edit.cancelEdit}
      onSave={(wear) => skin && void edit.saveSkinMetadata(skin.texture_key, wear)}
      dropKey={skin?.texture_key}
      data-drop={edit.editReplacementDragActive}
      onDragEnter={edit.editReplacementDrop.onDragEnter}
      onDragOver={edit.editReplacementDrop.onDragOver}
      onDragLeave={edit.editReplacementDrop.onDragLeave}
      onDrop={edit.editReplacementDrop.onDrop}
    >
      <SkinField label="Name">
        <Input
          value={edit.editName}
          onChange={(value) => edit.setEditName(value.slice(0, 64))}
          placeholder="Skin name"
          ariaLabel="Skin name"
        />
      </SkinField>
      <SkinField label="Arm model">
        <Segmented<SkinVariant>
          options={ARM_MODELS}
          value={edit.editVariant}
          onChange={(value) => {
            edit.setEditVariant(value);
            edit.setEditDetectError(null);
          }}
        />
        <Button
          variant="ghost"
          size="sm"
          busy={skin !== null && edit.editDetectBusyKey === skin.texture_key}
          disabled={locked}
          title="Detect the arm model from the texture"
          onClick={() => skin && void edit.detectSavedSkinModel(skin)}
        >
          Detect
        </Button>
      </SkinField>
      <SkinField label="Texture">
        <Button
          variant="secondary"
          size="sm"
          icon="image"
          disabled={locked}
          title="Replace this skin's PNG, or drop a file on this dialog"
          onClick={edit.openEditTexturePicker}
        >
          Replace PNG
        </Button>
        {replacement &&
          (replacement.normalizeStatus === 'checking' ? (
            <Status busy>Checking</Status>
          ) : (
            replacement.normalizeStatus === 'ready' && (
              <Status eyes="happy" tone="accent">
                Ready
              </Status>
            )
          ))}
        {replacement && (
          <Button variant="ghost" size="sm" disabled={busy} onClick={edit.clearEditReplacement}>
            Remove
          </Button>
        )}
      </SkinField>
      {(edit.editDetectError || replacement?.normalizeStatus === 'error') && (
        <Notice tone="error" role="alert">
          <p>{edit.editDetectError || replacement?.normalizeError || 'Skin validation failed.'}</p>
        </Notice>
      )}
    </SkinDialog>
  );
}

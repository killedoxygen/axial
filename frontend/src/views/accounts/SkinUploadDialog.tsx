import type { JSX } from 'preact';
import { setWardrobeNotice, wardrobeContext } from '../../machines/skin-wardrobe';
import { Input, Notice, Status } from '../../ui/Atoms';
import { Segmented } from '../../ui/Segmented';
import { stagedSkinPreviewSrc } from './api';
import { SkinDialog, SkinField } from './SkinDialog';
import { NO_CAPE_VALUE, type MinecraftCape, type UploadSkinVariant } from './types';
import type { useSavedSkinUploadWorkflow } from './use-saved-skin-upload-workflow';

const ARM_MODELS: Array<{ value: UploadSkinVariant; label: string }> = [
  { value: 'auto', label: 'Auto' },
  { value: 'classic', label: 'Classic' },
  { value: 'slim', label: 'Slim' },
];

/** A PNG that was picked or dropped, before it is saved: its name, arm model and cape. */
export function SkinUploadDialog({
  upload,
  capes,
  capeSrc,
  disabledReason,
}: {
  upload: ReturnType<typeof useSavedSkinUploadWorkflow>;
  capes: MinecraftCape[];
  capeSrc: (capeId: string | null) => string | undefined;
  disabledReason: string;
}): JSX.Element {
  const staged = upload.stagedUpload;
  return (
    <SkinDialog
      title="Add skin"
      open={staged !== null}
      busy={upload.busy}
      preview={
        staged && upload.stagedVariant
          ? {
              src: stagedSkinPreviewSrc(staged),
              name: upload.stagedName,
              variant: upload.stagedVariant,
              capeSrc: capeSrc(upload.stagedCapeId === NO_CAPE_VALUE ? null : upload.stagedCapeId),
            }
          : null
      }
      capes={capes}
      capeId={upload.stagedCapeId}
      onCapeChange={upload.setStagedCapeId}
      canSave={upload.stagedCanSave}
      canWear={wardrobeContext.value.skinActionsEnabled}
      disabledReason={disabledReason}
      onClose={upload.clearStagedUpload}
      onSave={upload.saveStagedUpload}
    >
      <SkinField label="Name">
        <Input
          value={upload.skinName}
          onChange={(value) => {
            upload.setSkinName(value.slice(0, 64));
            setWardrobeNotice(null);
          }}
          placeholder={upload.stagedName || 'Skin name'}
          ariaLabel="Skin name"
        />
      </SkinField>
      <SkinField label="Arm model">
        <Segmented<UploadSkinVariant>
          options={ARM_MODELS}
          value={upload.uploadVariant}
          onChange={(value) => {
            upload.setUploadVariant(value);
            setWardrobeNotice(null);
          }}
        />
      </SkinField>
      {staged?.normalizeStatus === 'error' ? (
        <Notice tone="error" role="alert">
          <p>{staged.normalizeError || 'This file is not a valid Minecraft skin.'}</p>
        </Notice>
      ) : (
        staged?.normalizeStatus === 'checking' && <Status busy>Checking skin file</Status>
      )}
    </SkinDialog>
  );
}

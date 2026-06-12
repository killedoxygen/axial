import type { JSX } from 'preact';
import { Button, Input, Segmented } from '../../ui/Atoms';
import { Modal, ModalContent, ModalHeader, ModalTitle } from '../../ui/Modal';
import { CapePicker } from './CapePicker';
import { stagedSkinPreviewSrc, uploadSkinName } from './api';
import { SkinThreePreview } from './SkinThreePreview';
import type { MinecraftCape, SkinVariant, StagedSkinUpload, UploadSkinVariant } from './types';

export function SkinUploadDialog({
  stagedUpload,
  stagedVariant,
  stagedName,
  stagedCapeSrc,
  stagedCapeId,
  availableCapes,
  skinName,
  uploadVariant,
  busy,
  onlineReady,
  stagedCanSave,
  onClose,
  onSkinNameChange,
  onUploadVariantChange,
  onStagedCapeChange,
  onSave,
}: {
  stagedUpload: StagedSkinUpload | null;
  stagedVariant: SkinVariant | null;
  stagedName: string;
  stagedCapeSrc?: string;
  stagedCapeId: string;
  availableCapes: MinecraftCape[];
  skinName: string;
  uploadVariant: UploadSkinVariant;
  busy: boolean;
  onlineReady: boolean;
  stagedCanSave: boolean;
  onClose: () => void;
  onSkinNameChange: (value: string) => void;
  onUploadVariantChange: (value: UploadSkinVariant) => void;
  onStagedCapeChange: (value: string) => void;
  onSave: (applyAfterSave: boolean) => void;
}): JSX.Element {
  return (
    <Modal
      open={Boolean(stagedUpload)}
      onOpenChange={(next) => {
        if (!next && !busy) onClose();
      }}
    >
      <ModalContent className="cp-skinedit-modal" aria-label="Add skin" aria-describedby={undefined}>
        <ModalHeader>
          <ModalTitle>Add skin</ModalTitle>
        </ModalHeader>
        {stagedUpload && stagedVariant && (
          <div class="cp-skinedit">
            <div class="cp-skinedit__preview">
              <SkinThreePreview
                src={stagedSkinPreviewSrc(stagedUpload)}
                capeSrc={stagedCapeSrc}
                name={stagedName}
                variant={stagedVariant}
                side="front"
                showOuterLayers
              />
            </div>
            <div class="cp-skinedit__form">
              <label class="cp-skinedit__field">
                <span>Name</span>
                <Input
                  value={skinName}
                  onChange={(value) => onSkinNameChange(value.slice(0, 64))}
                  placeholder={uploadSkinName(stagedUpload.file) || 'Skin name'}
                />
              </label>
              <div class="cp-skinedit__field">
                <span>Model</span>
                <Segmented<UploadSkinVariant>
                  options={[
                    { value: 'auto', label: 'Auto' },
                    { value: 'classic', label: 'Classic' },
                    { value: 'slim', label: 'Slim' },
                  ]}
                  value={uploadVariant}
                  onChange={onUploadVariantChange}
                />
              </div>
              {availableCapes.length > 0 && (
                <div class="cp-skinedit__field">
                  <span>Cape</span>
                  <CapePicker
                    capes={availableCapes}
                    value={stagedCapeId}
                    onChange={onStagedCapeChange}
                  />
                </div>
              )}
              {stagedUpload.normalizeStatus === 'error' ? (
                <div class="cp-skin-inline-err">
                  {stagedUpload.normalizeError || 'This file is not a valid Minecraft skin.'}
                </div>
              ) : stagedUpload.normalizeStatus === 'checking' ? (
                <div class="cp-skinedit__meta">Checking skin file...</div>
              ) : null}
            </div>
            <div class="cp-skinedit__footer">
              <Button variant="ghost" disabled={busy} onClick={onClose}>
                Cancel
              </Button>
              <Button
                variant="secondary"
                icon={busy ? 'refresh' : 'download'}
                disabled={!stagedCanSave}
                onClick={() => onSave(false)}
              >
                Save
              </Button>
              <Button
                variant="primary"
                icon={busy ? 'refresh' : 'check'}
                disabled={!stagedCanSave || !onlineReady}
                onClick={() => onSave(true)}
                title={onlineReady ? 'Save locally, then apply to the active Minecraft account' : 'Online Minecraft account required'}
                sound="affirm"
              >
                Save & apply
              </Button>
            </div>
          </div>
        )}
      </ModalContent>
    </Modal>
  );
}

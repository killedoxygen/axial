import type { JSX } from 'preact';
import { Button, Input, Segmented } from '../../ui/Atoms';
import { Modal, ModalContent, ModalHeader, ModalTitle } from '../../ui/Modal';
import { CapePicker } from './CapePicker';
import { savedSkinFileUrl, stagedSkinPreviewSrc } from './api';
import { LazySkinThreePreview as SkinThreePreview } from './LazySkinThreePreview';
import type { MinecraftCape, SavedSkinRecord, SkinVariant, StagedSkinUpload } from './types';

export function SkinEditDialog({
  editingSkin,
  editReplacement,
  editReplacementDragActive,
  editPreviewCapeSrc,
  editName,
  trimmedEditName,
  editVariant,
  editCapeId,
  availableCapes,
  editBusyKey,
  editDetectBusyKey,
  editDetectError,
  editReplacementReady,
  editHasChanges,
  onlineReady,
  deleteKey,
  onClose,
  onEditReplacementDragEnter,
  onEditReplacementDragOver,
  onEditReplacementDragLeave,
  onEditReplacementDrop,
  onEditNameChange,
  onEditVariantChange,
  onEditCapeChange,
  onDetectModel,
  onOpenTexturePicker,
  onClearEditReplacement,
  onDelete,
  onSave,
}: {
  editingSkin: SavedSkinRecord | null;
  editReplacement: StagedSkinUpload | null;
  editReplacementDragActive: boolean;
  editPreviewCapeSrc?: string;
  editName: string;
  trimmedEditName: string;
  editVariant: SkinVariant;
  editCapeId: string;
  availableCapes: MinecraftCape[];
  editBusyKey: string | null;
  editDetectBusyKey: string | null;
  editDetectError: string | null;
  editReplacementReady: boolean;
  editHasChanges: boolean;
  onlineReady: boolean;
  deleteKey: string | null;
  onClose: () => void;
  onEditReplacementDragEnter: (event: DragEvent) => void;
  onEditReplacementDragOver: (event: DragEvent) => void;
  onEditReplacementDragLeave: (event: DragEvent) => void;
  onEditReplacementDrop: (event: DragEvent) => void;
  onEditNameChange: (value: string) => void;
  onEditVariantChange: (value: SkinVariant) => void;
  onEditCapeChange: (value: string) => void;
  onDetectModel: (skin: SavedSkinRecord) => void;
  onOpenTexturePicker: () => void;
  onClearEditReplacement: () => void;
  onDelete: (skin: SavedSkinRecord) => void;
  onSave: (textureKey: string, applyAfterSave?: boolean) => void;
}): JSX.Element {
  const saving = editingSkin ? editBusyKey === editingSkin.texture_key : false;
  const canSave = Boolean(
    editingSkin &&
    editBusyKey === null &&
    editDetectBusyKey === null &&
    editReplacementReady &&
    editHasChanges &&
    trimmedEditName.length > 0,
  );

  return (
    <Modal
      open={Boolean(editingSkin)}
      onOpenChange={(next) => {
        if (!next && editBusyKey === null) onClose();
      }}
    >
      <ModalContent className="cp-skinedit-modal" aria-label="Edit skin" aria-describedby={undefined}>
        <ModalHeader>
          <ModalTitle>Edit skin</ModalTitle>
        </ModalHeader>
        {editingSkin && (
          <div
            class="cp-skinedit"
            data-saved-skin-edit-drop-surface={editingSkin.texture_key}
            data-saved-skin-edit-drop-state={editReplacementDragActive ? 'active' : 'idle'}
            onDragEnter={onEditReplacementDragEnter}
            onDragOver={onEditReplacementDragOver}
            onDragLeave={onEditReplacementDragLeave}
            onDrop={onEditReplacementDrop}
          >
            <div class="cp-skinedit__preview">
              <SkinThreePreview
                src={editReplacement ? stagedSkinPreviewSrc(editReplacement) : savedSkinFileUrl(editingSkin)}
                capeSrc={editPreviewCapeSrc}
                name={trimmedEditName || editingSkin.name}
                variant={editVariant}
                side="front"
                showOuterLayers
              />
            </div>
            <div class="cp-skinedit__form">
              <label class="cp-skinedit__field">
                <span>Name</span>
                <Input
                  value={editName}
                  onChange={(value) => onEditNameChange(value.slice(0, 64))}
                  placeholder="Skin name"
                />
              </label>
              <div class="cp-skinedit__field">
                <span>Model</span>
                <div class="cp-skinedit__inline">
                  <Segmented<SkinVariant>
                    options={[
                      { value: 'classic', label: 'Classic' },
                      { value: 'slim', label: 'Slim' },
                    ]}
                    value={editVariant}
                    onChange={onEditVariantChange}
                  />
                  <Button
                    variant="ghost"
                    size="sm"
                    icon={editDetectBusyKey === editingSkin.texture_key ? 'refresh' : 'search'}
                    disabled={editBusyKey !== null || editDetectBusyKey !== null}
                    onClick={() => onDetectModel(editingSkin)}
                    title="Detect the model from the skin texture"
                  >
                    Detect
                  </Button>
                </div>
              </div>
              {availableCapes.length > 0 && (
                <div class="cp-skinedit__field">
                  <span>Cape</span>
                  <CapePicker capes={availableCapes} value={editCapeId} onChange={onEditCapeChange} />
                </div>
              )}
              <div class="cp-skinedit__field">
                <span>Texture</span>
                <div class="cp-skinedit__inline">
                  <Button
                    variant="secondary"
                    size="sm"
                    icon="image"
                    disabled={editBusyKey !== null || editDetectBusyKey !== null}
                    onClick={onOpenTexturePicker}
                    title="Replace this skin PNG, or drop a file on this panel"
                  >
                    Replace PNG
                  </Button>
                  {editReplacement && (
                    <>
                      <span class="cp-skinedit__meta">
                        {editReplacement.normalizeStatus === 'error'
                          ? editReplacement.normalizeError || 'Skin validation failed.'
                          : editReplacement.normalizeStatus === 'checking'
                            ? 'Validating...'
                            : 'Replacement ready'}
                      </span>
                      <Button
                        variant="ghost"
                        size="sm"
                        icon="x"
                        disabled={editBusyKey !== null}
                        onClick={onClearEditReplacement}
                      >
                        Remove
                      </Button>
                    </>
                  )}
                </div>
              </div>
              {editDetectError && <div class="cp-skin-inline-err">{editDetectError}</div>}
            </div>
            <div class="cp-skinedit__footer">
              {!editingSkin.applied_at && (
                <Button
                  variant="ghost"
                  icon="trash"
                  disabled={editBusyKey !== null || deleteKey === editingSkin.texture_key}
                  onClick={() => onDelete(editingSkin)}
                  style={{ marginRight: 'auto' }}
                >
                  Delete
                </Button>
              )}
              <Button variant="ghost" disabled={editBusyKey !== null} onClick={onClose}>
                Cancel
              </Button>
              <Button
                variant="secondary"
                icon={saving ? 'refresh' : 'download'}
                disabled={!canSave}
                onClick={() => onSave(editingSkin.texture_key)}
              >
                Save
              </Button>
              <Button
                variant="primary"
                icon={saving ? 'refresh' : 'check'}
                disabled={!canSave || !onlineReady}
                onClick={() => onSave(editingSkin.texture_key, true)}
                title={
                  onlineReady
                    ? 'Save changes, then apply to the active Minecraft account'
                    : 'Online Minecraft account required'
                }
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

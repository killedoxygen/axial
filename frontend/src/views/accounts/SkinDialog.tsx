import type { ComponentChildren, JSX } from 'preact';
import { Button } from '../../ui/Atoms';
import { Modal, ModalContent, ModalTitle } from '../../ui/Modal';
import { CapePicker } from './CapePicker';
import { LazySkinThreePreview } from './LazySkinThreePreview';
import type { MinecraftCape, SkinVariant } from './types';

/** A labelled control in a skin dialog. */
export function SkinField({ label, children }: { label: string; children: ComponentChildren }): JSX.Element {
  return (
    <div class="cp-skin-field" role="group" aria-label={label}>
      <span>{label}</span>
      {children}
    </div>
  );
}

/**
 * Adding or editing a skin: the figure in a holder at the left, its fields beside it, and the same three ways out
 * (cancel, save, save and wear).
 */
export function SkinDialog({
  title,
  open,
  busy,
  preview,
  capes,
  capeId,
  onCapeChange,
  canSave,
  canWear,
  disabledReason,
  extraAction,
  dropKey,
  onClose,
  onSave,
  children,
  ...surface
}: {
  title: string;
  open: boolean;
  busy: boolean;
  preview: { src: string; name: string; variant: SkinVariant; capeSrc?: string } | null;
  capes: MinecraftCape[];
  capeId: string;
  onCapeChange: (value: string) => void;
  canSave: boolean;
  canWear: boolean;
  /** Why the skin cannot be worn from here. */
  disabledReason: string;
  extraAction?: ComponentChildren;
  /** The saved skin a PNG dropped on this dialog replaces; the native drop finds the dialog by it. */
  dropKey?: string;
  onClose: () => void;
  onSave: (wear: boolean) => void;
  children: ComponentChildren;
} & Pick<JSX.HTMLAttributes<HTMLDivElement>, 'onDragEnter' | 'onDragOver' | 'onDragLeave' | 'onDrop'> & {
    'data-drop'?: boolean;
  }): JSX.Element {
  return (
    <Modal open={open} onOpenChange={(next) => !next && !busy && onClose()}>
      <ModalContent
        className="cp-dialog cp-pop cp-skin-dialog"
        aria-label={title}
        data-saved-skin-edit-drop-surface={dropKey}
        {...surface}
      >
        <ModalTitle>{title}</ModalTitle>
        {preview && (
          <div class="cp-skin-dialog-body">
            <LazySkinThreePreview {...preview} side="front" showOuterLayers />
            <div class="cp-skin-dialog-form">
              {children}
              {capes.length > 0 && (
                <SkinField label="Cape">
                  <CapePicker capes={capes} value={capeId} onChange={onCapeChange} />
                </SkinField>
              )}
            </div>
          </div>
        )}
        <div class="cp-dialog-actions">
          {extraAction}
          <span class="cp-head-fill" />
          <Button variant="ghost" disabled={busy} onClick={onClose}>
            Cancel
          </Button>
          <Button variant="ghost" icon="download" disabled={!canSave} onClick={() => onSave(false)}>
            Save
          </Button>
          <Button
            icon="check"
            busy={busy}
            disabled={!canSave || !canWear}
            title={canWear ? 'Save, then wear on the active Minecraft account' : disabledReason}
            sound="affirm"
            onClick={() => onSave(true)}
          >
            Save and wear
          </Button>
        </div>
      </ModalContent>
    </Modal>
  );
}

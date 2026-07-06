import type { JSX } from 'preact';
import { useEffect } from 'preact/hooks';
import { Modal, ModalContent } from '../../../ui/Modal';
import { IconButton } from '../../../ui/Atoms';
import type { EnrichedInstance, InstanceScreenshot } from '../../../types-instance';
import { fmtBytes, fmtRelative } from '../format';
import { openInstanceFolder } from '../instance-actions';
import { deleteScreenshots, renameScreenshot, screenshotFileUrl } from '../screenshot-actions';

export function ScreenshotLightbox({
  inst,
  shots,
  name,
  onSelect,
  onClose,
  onRename,
  onRefresh,
}: {
  inst: EnrichedInstance;
  shots: InstanceScreenshot[];
  name: string;
  onSelect: (name: string) => void;
  onClose: () => void;
  onRename: (shot: InstanceScreenshot, newName: string) => void;
  onRefresh: () => void;
}): JSX.Element | null {
  const index = shots.findIndex((shot) => shot.name === name);
  const shot = index >= 0 ? shots[index] : undefined;
  const prev = index > 0 ? shots[index - 1] : undefined;
  const next = index >= 0 && index < shots.length - 1 ? shots[index + 1] : undefined;

  useEffect(() => {
    for (const neighbor of [prev, next]) {
      if (!neighbor) continue;
      new Image().src = screenshotFileUrl(inst, neighbor.name);
    }
  }, [inst, prev, next]);

  if (!shot) return null;

  const rename = (): void => {
    void renameScreenshot(inst, shot.name, (newName) => {
      onRename(shot, newName);
    });
  };
  const remove = (): void => {
    const fallback = next?.name ?? prev?.name ?? '';
    void deleteScreenshots(inst, [shot], () => {
      onRefresh();
      if (fallback) onSelect(fallback);
      else onClose();
    });
  };

  return (
    <Modal open onOpenChange={(open) => (open ? undefined : onClose())}>
      <ModalContent
        className="cp-shot-lightbox"
        showCloseButton={false}
        aria-label={shot.name}
        onKeyDown={(e: KeyboardEvent) => {
          if (e.key === 'ArrowLeft' && prev) onSelect(prev.name);
          if (e.key === 'ArrowRight' && next) onSelect(next.name);
        }}
      >
        <div class="cp-shot-lightbox-bar">
          <div class="cp-shot-lightbox-title">
            <strong title={shot.name}>{shot.name}</strong>
            <div class="cp-shot-lightbox-meta">
              <span>{fmtBytes(shot.size)}</span>
              <span>{fmtRelative(shot.modified_at)}</span>
            </div>
          </div>
          <div class="cp-shot-lightbox-actions">
            <span class="cp-shot-lightbox-count">
              {index + 1} of {shots.length}
            </span>
            <IconButton icon="edit" size={30} tooltip="Rename" onClick={rename} />
            <IconButton
              icon="folder"
              size={30}
              tooltip="Open screenshots folder"
              onClick={() => void openInstanceFolder(inst.id, 'screenshots')}
            />
            <IconButton icon="trash" size={30} danger tooltip="Delete" onClick={remove} />
            <span class="cp-shot-lightbox-sep" aria-hidden="true" />
            <IconButton icon="x" size={30} tooltip="Close" onClick={onClose} />
          </div>
        </div>
        <div class="cp-shot-lightbox-stage">
          <img src={screenshotFileUrl(inst, shot.name)} alt={shot.name} />
          {prev ? (
            <span class="cp-shot-lightbox-nav cp-shot-lightbox-nav--prev">
              <IconButton
                icon="chevron-left"
                variant="overlay"
                size={38}
                tooltip="Previous"
                onClick={() => onSelect(prev.name)}
              />
            </span>
          ) : null}
          {next ? (
            <span class="cp-shot-lightbox-nav cp-shot-lightbox-nav--next">
              <IconButton
                icon="chevron-right"
                variant="overlay"
                size={38}
                tooltip="Next"
                onClick={() => onSelect(next.name)}
              />
            </span>
          ) : null}
        </div>
      </ModalContent>
    </Modal>
  );
}

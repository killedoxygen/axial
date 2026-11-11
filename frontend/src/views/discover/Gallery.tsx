import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { Modal, ModalContent } from '../../ui/Modal';
import type { GalleryImage } from '../../types-content';

/** The project's screenshots; one opens at full size. */
export function Gallery({ images }: { images: GalleryImage[] }): JSX.Element {
  const [open, setOpen] = useState<GalleryImage | null>(null);
  return (
    <section class="cp-content-section">
      <div class="cp-head">
        <h2>Gallery</h2>
        <span class="cp-tab-count">{images.length}</span>
      </div>
      <div class="cp-content-gallery">
        {images.map((image, index) => (
          <button
            key={image.url}
            type="button"
            aria-label={image.title ?? `Screenshot ${index + 1}`}
            title={image.title}
            onClick={() => setOpen(image)}
          >
            <img src={image.url} alt="" loading="lazy" decoding="async" />
          </button>
        ))}
      </div>
      {open && (
        <Modal open onOpenChange={(next) => !next && setOpen(null)}>
          <ModalContent className="cp-content-lightbox" aria-label={open.title ?? 'Screenshot'}>
            <img src={open.url} alt={open.title ?? ''} />
            {open.title && <p>{open.title}</p>}
          </ModalContent>
        </Modal>
      )}
    </section>
  );
}

import type { JSX, Ref, RefObject } from 'preact';

export function SavedSkinFileInputs({
  editTextureInputRef,
  fileInputRef,
  onEditTextureFile,
  onUploadFile,
}: {
  editTextureInputRef: RefObject<HTMLInputElement | null>;
  fileInputRef: RefObject<HTMLInputElement | null>;
  onEditTextureFile: (file: File) => void;
  onUploadFile: (file: File) => void;
}): JSX.Element {
  return (
    <>
      <input
        ref={editTextureInputRef as Ref<HTMLInputElement>}
        type="file"
        accept="image/png"
        style={{ display: 'none' }}
        onChange={(event) => {
          const file = event.currentTarget.files?.[0];
          if (file) onEditTextureFile(file);
        }}
      />
      <input
        ref={fileInputRef as Ref<HTMLInputElement>}
        type="file"
        accept="image/png"
        style={{ display: 'none' }}
        onChange={(event) => {
          const file = event.currentTarget.files?.[0];
          if (file) onUploadFile(file);
        }}
      />
    </>
  );
}

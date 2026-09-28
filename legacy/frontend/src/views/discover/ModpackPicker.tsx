import type { JSX } from 'preact';
import { useEffect, useMemo, useState } from 'preact/hooks';
import { getModpackFiles, installModpack } from '../../content';
import { formatBytes, plural } from '../../format';
import { applyInstallQueueResponse } from '../../machines/downloads';
import { Button } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { Modal, ModalContent } from '../../ui/Modal';
import { errMessage } from '../../utils';
import type { ModpackFilesPlan } from '../../types-content';

export function ModpackPicker({
  open,
  instanceId,
  canonicalId,
  versionId,
  onClose,
}: {
  open: boolean;
  instanceId: string;
  canonicalId: string;
  versionId?: string;
  onClose: () => void;
}): JSX.Element | null {
  const [plan, setPlan] = useState<ModpackFilesPlan | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    setPlan(null);
    setError('');
    void getModpackFiles(instanceId, canonicalId, versionId)
      .then((next) => {
        if (cancelled) return;
        setPlan(next);
        setSelected(
          new Set(next.files.filter((file) => file.compatible && !file.installed).map((file) => file.selection_id)),
        );
      })
      .catch((reason: unknown) => {
        if (!cancelled) setError(errMessage(reason));
      });
    return () => {
      cancelled = true;
    };
  }, [open, instanceId, canonicalId, versionId]);

  const files = useMemo(() => plan?.files.filter((file) => file.compatible && !file.installed) ?? [], [plan]);
  const selectedBytes = files.reduce(
    (total, file) => total + (selected.has(file.selection_id) ? (file.size ?? 0) : 0),
    0,
  );

  if (!open) return null;
  const submit = async (): Promise<void> => {
    if (!plan || selected.size === 0 || busy) return;
    setBusy(true);
    setError('');
    try {
      const queue = await installModpack(instanceId, canonicalId, plan.version_id, {
        selectedFileIds: [...selected],
        includeOverrides: false,
      });
      await applyInstallQueueResponse(queue, { showNotice: true, connectActive: true });
      onClose();
    } catch (reason) {
      setError(errMessage(reason));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal open onOpenChange={(next) => !next && onClose()}>
      <ModalContent className="cp-pack-picker" aria-label="Choose modpack files">
        <div class="cp-pack-picker-head">
          <div>
            <h2>{plan?.name ?? 'Choose pack files'}</h2>
            <p>Only files compatible with this instance are shown. Pack configuration is never copied.</p>
          </div>
          {files.length > 0 && (
            <Button
              variant="ghost"
              size="sm"
              onClick={() =>
                setSelected(
                  selected.size === files.length ? new Set() : new Set(files.map((file) => file.selection_id)),
                )
              }
            >
              {selected.size === files.length ? 'Clear' : 'Select all'}
            </Button>
          )}
        </div>

        <div class="cp-pack-picker-list">
          {!plan && !error && <div class="cp-resource-note">Reading pack contents…</div>}
          {plan && files.length === 0 && (
            <div class="cp-resource-note">This pack has no compatible files that are not already installed.</div>
          )}
          {files.map((file) => (
            <label class="cp-pack-picker-row" key={file.selection_id}>
              <input
                type="checkbox"
                checked={selected.has(file.selection_id)}
                onChange={() => {
                  const next = new Set(selected);
                  if (next.has(file.selection_id)) next.delete(file.selection_id);
                  else next.add(file.selection_id);
                  setSelected(next);
                }}
              />
              <span class="cp-pack-picker-kind" aria-hidden="true">
                <Icon
                  name={file.kind === 'mod' ? 'puzzle' : file.kind === 'shader_pack' ? 'palette' : 'image'}
                  size={15}
                />
              </span>
              <span class="cp-pack-picker-copy">
                <strong>{file.title}</strong>
                <small>{file.identified ? file.filename : `${file.filename}, not recognized by the provider`}</small>
              </span>
              {file.size != null && <span class="cp-pack-picker-size">{formatBytes(file.size)}</span>}
            </label>
          ))}
        </div>

        {error && (
          <div class="cp-discover-conflict">
            <Icon name="alert" size={13} /> {error}
          </div>
        )}
        <div class="cp-discover-dialog-actions">
          <span class="cp-pack-picker-summary">
            {plural(selected.size, 'file', 'files')}
            {selectedBytes > 0 ? `, ${formatBytes(selectedBytes)}` : ''}
          </span>
          <Button variant="ghost" onClick={onClose} disabled={busy}>
            Cancel
          </Button>
          <Button onClick={() => void submit()} disabled={!plan || selected.size === 0 || busy}>
            {busy ? 'Queueing…' : 'Add selected'}
          </Button>
        </div>
      </ModalContent>
    </Modal>
  );
}

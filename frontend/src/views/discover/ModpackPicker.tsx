import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { getModpackFiles, installModpack } from '../../content';
import { contentRevision } from '../../content-activity';
import { formatBytes, plural } from '../../format';
import { applyInstallQueueResponse } from '../../machines/downloads';
import { Button } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { Modal, ModalContent } from '../../ui/Modal';
import { errMessage } from '../../utils';
import type { ModpackFilesPlan } from '../../types-content';

type PickerContext = { open: boolean; instanceId: string; canonicalId: string; versionId?: string };

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
  const context = useRef<PickerContext>({ open, instanceId, canonicalId, versionId });
  if (
    context.current.open !== open ||
    context.current.instanceId !== instanceId ||
    context.current.canonicalId !== canonicalId ||
    context.current.versionId !== versionId
  ) {
    context.current = { open, instanceId, canonicalId, versionId };
  }
  const currentContext = context.current;
  const revision = contentRevision.value;
  const refresh = useRef(0);
  const generation = refresh.current;
  const [loadedPlan, setPlan] = useState<{
    context: PickerContext;
    revision: number;
    generation: number;
    plan: ModpackFilesPlan;
  } | null>(null);
  const plan =
    loadedPlan?.context === currentContext && loadedPlan.revision === revision && loadedPlan.generation === generation
      ? loadedPlan.plan
      : null;
  const selectedContext = useRef<PickerContext | null>(null);
  const pending = useRef<object | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [submitError, setError] = useState('');
  const [planError, setPlanError] = useState('');
  const error = submitError || planError;
  const [busy, setBusy] = useState(false);

  useEffect(
    () => () => {
      pending.current = null;
    },
    [],
  );
  useEffect(() => {
    if (!currentContext.open) return;
    let cancelled = false;
    const initial = selectedContext.current !== currentContext;
    setPlan(null);
    setPlanError('');
    if (initial) {
      setError('');
      setSelected(new Set());
      setBusy(false);
      pending.current = null;
    }
    const current = (): boolean =>
      !cancelled &&
      context.current === currentContext &&
      contentRevision.value === revision &&
      refresh.current === generation;
    void getModpackFiles(currentContext.instanceId, currentContext.canonicalId, currentContext.versionId)
      .then((next) => {
        if (!current()) return;
        setPlan({ context: currentContext, revision, generation, plan: next });
        setSelected(
          (previous) =>
            new Set(
              next.files
                .filter((file) => file.compatible && !file.installed && (initial || previous.has(file.selection_id)))
                .map((file) => file.selection_id),
            ),
        );
        selectedContext.current = currentContext;
      })
      .catch((reason: unknown) => {
        if (current()) setPlanError(errMessage(reason));
      });
    return () => {
      cancelled = true;
    };
  }, [currentContext, revision, generation]);

  const files = useMemo(() => plan?.files.filter((file) => file.compatible && !file.installed) ?? [], [plan]);
  const selectedBytes = files.reduce(
    (total, file) => total + (selected.has(file.selection_id) ? (file.size ?? 0) : 0),
    0,
  );

  if (!open) return null;
  const submit = async (): Promise<void> => {
    if (
      !plan ||
      selected.size === 0 ||
      busy ||
      pending.current ||
      context.current !== currentContext ||
      contentRevision.value !== revision ||
      refresh.current !== generation
    )
      return;
    const submission = {};
    pending.current = submission;
    const current = (): boolean => context.current === currentContext && pending.current === submission;
    setBusy(true);
    setError('');
    try {
      const queue = await installModpack(instanceId, canonicalId, plan.version_id, {
        selectedFileIds: [...selected],
        includeOverrides: false,
      });
      await applyInstallQueueResponse(queue, { showNotice: true, connectActive: true });
      if (current()) onClose();
    } catch (reason) {
      if (current()) {
        refresh.current += 1;
        setPlan(null);
        setError(errMessage(reason));
      }
    } finally {
      if (current()) {
        pending.current = null;
        setBusy(false);
      }
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

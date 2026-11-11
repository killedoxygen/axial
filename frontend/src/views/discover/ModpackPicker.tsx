import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { getModpackFiles, installModpack } from '../../content';
import { contentRevision } from '../../content-activity';
import { formatBytes, plural } from '../../format';
import { applyInstallQueueResponse } from '../../machines/downloads';
import { Button, Chip, Empty, Notice, Status } from '../../ui/Atoms';
import { Art } from '../../ui/Art';
import { Modal, ModalContent, ModalHeader, ModalTitle } from '../../ui/Modal';
import { SelectionCheckbox } from '../../ui/SelectionActionTray';
import { errMessage } from '../../utils';
import type { ModpackFilesPlan } from '../../types-content';
import { KIND_ICON } from './shared';

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
                .filter(
                  (file) =>
                    file.compatible &&
                    !file.installed &&
                    // Updating a project at another version is a deliberate choice.
                    (initial ? !file.installed_version_id : previous.has(file.selection_id)),
                )
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
  const selectedUpdates = files.filter((file) => file.installed_version_id && selected.has(file.selection_id)).length;

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

  const allSelected = selected.size === files.length;
  const toggle = (id: string): void => {
    const next = new Set(selected);
    if (!next.delete(id)) next.add(id);
    setSelected(next);
  };

  return (
    <Modal open onOpenChange={(next) => !next && onClose()}>
      <ModalContent className="cp-dialog cp-pop cp-pack-picker" aria-label="Choose modpack files">
        <ModalHeader>
          <ModalTitle>{plan?.name ?? 'Choose pack files'}</ModalTitle>
          <p class="cp-dialog-body">
            Only files compatible with this instance are shown. Pack configuration is never copied.
          </p>
        </ModalHeader>

        <div class="cp-pack-files">
          {!plan && !error && <Status busy>Reading pack contents</Status>}
          {plan && files.length === 0 && (
            <Empty title="Nothing to add">This pack has no compatible files that are not already installed.</Empty>
          )}
          {files.map((file) => (
            <div class="cp-row" key={file.selection_id} onClick={() => toggle(file.selection_id)}>
              <SelectionCheckbox
                selected={selected.has(file.selection_id)}
                label={`Include ${file.title}`}
                onToggle={(event) => {
                  event.stopPropagation();
                  toggle(file.selection_id);
                }}
              />
              <Art icon={KIND_ICON[file.kind]} size={28} />
              <span class="cp-pack-file">
                <strong>{file.title}</strong>
                <small>{file.filename}</small>
              </span>
              {!file.identified && <Chip title="Not recognized by the provider">Unrecognized</Chip>}
              {file.installed_version_id && <Chip>Other version installed</Chip>}
              {file.size != null && <Chip tone="bare">{formatBytes(file.size)}</Chip>}
            </div>
          ))}
        </div>

        {error && (
          <Notice tone="error" role="alert">
            <p>{error}</p>
          </Notice>
        )}
        <div class="cp-dialog-actions">
          {files.length > 0 && (
            <Button
              variant="ghost"
              onClick={() => setSelected(allSelected ? new Set() : new Set(files.map((file) => file.selection_id)))}
            >
              {allSelected ? 'Clear' : 'Select all'}
            </Button>
          )}
          <span class="cp-chips cp-head-fill">
            <Chip tone="bare">{plural(selected.size, 'file', 'files')}</Chip>
            {selectedUpdates > 0 && <Chip tone="bare">{plural(selectedUpdates, 'update', 'updates')}</Chip>}
            {selectedBytes > 0 && <Chip tone="bare">{formatBytes(selectedBytes)}</Chip>}
          </span>
          <Button variant="ghost" onClick={onClose} disabled={busy}>
            Cancel
          </Button>
          <Button busy={busy} onClick={() => void submit()} disabled={!plan || selected.size === 0 || busy}>
            Add selected
          </Button>
        </div>
      </ModalContent>
    </Modal>
  );
}

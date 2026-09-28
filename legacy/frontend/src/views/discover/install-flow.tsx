import type { JSX } from 'preact';
import { useRef, useState } from 'preact/hooks';
import { Button } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { Modal, ModalContent } from '../../ui/Modal';
import { formatBytes, plural } from '../../format';
import type { ContentSelection, ResolutionPlan } from '../../types-content';
import { addToInstance, commitInstall, type AddOutcome } from './actions';

export interface InstallFlow {
  busy: boolean;
  plan: ResolutionPlan | null;
  add: (selections: ContentSelection[], label: string) => Promise<AddOutcome>;
  confirm: () => Promise<AddOutcome>;
  cancel: () => void;
}

/** The one way content gets added to an instance from the UI: plan first, and
 * when the plan has conflicts hold them (render them with InstallConflictSheet)
 * until the person decides. Every add button shares this so none of them can
 * silently drop a conflict outcome. */
export function useInstallFlow(instanceId: string | undefined): InstallFlow {
  const [busy, setBusy] = useState(false);
  const [plan, setPlan] = useState<ResolutionPlan | null>(null);
  const pending = useRef<{ selections: ContentSelection[]; label: string } | null>(null);

  const add = async (selections: ContentSelection[], label: string): Promise<AddOutcome> => {
    if (!instanceId || busy) return { status: 'failed' };
    setBusy(true);
    try {
      const outcome = await addToInstance(instanceId, selections, label);
      if (outcome.status === 'needs-confirmation' && outcome.plan) {
        pending.current = { selections, label };
        setPlan(outcome.plan);
      }
      return outcome;
    } finally {
      setBusy(false);
    }
  };

  const confirm = async (): Promise<AddOutcome> => {
    const staged = pending.current;
    if (!instanceId || !staged) return { status: 'failed' };
    setBusy(true);
    try {
      const outcome = await commitInstall(instanceId, staged.selections, staged.label, plan ?? undefined, true);
      pending.current = null;
      setPlan(null);
      return outcome;
    } finally {
      setBusy(false);
    }
  };

  const cancel = (): void => {
    pending.current = null;
    setPlan(null);
  };

  return { busy, plan, add, confirm, cancel };
}

export function InstallConflictSheet({
  flow,
  onInstalled,
}: {
  flow: InstallFlow;
  onInstalled?: () => void;
}): JSX.Element | null {
  if (!flow.plan) return null;
  return (
    <ConflictSheet
      plan={flow.plan}
      busy={flow.busy}
      onCancel={flow.cancel}
      onConfirm={() =>
        void flow.confirm().then((outcome) => {
          if (outcome.status === 'installed') onInstalled?.();
        })
      }
    />
  );
}

function ConflictSheet({
  plan,
  busy,
  onCancel,
  onConfirm,
}: {
  plan: ResolutionPlan;
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}): JSX.Element {
  const toInstall = plan.items.filter((item) => !item.already_installed || item.update);
  const overridable = toInstall.length > 0 && plan.conflicts.every((conflict) => conflict.kind === 'incompatible');
  return (
    <Modal open onOpenChange={(next) => !next && onCancel()}>
      <ModalContent className="cp-discover-dialog" aria-label="Resolve conflicts">
        <h2 class="cp-discover-dialog-title">{overridable ? 'This needs a decision' : 'This cannot be added here'}</h2>
        {plan.conflicts.map((conflict, index) => (
          <div key={index} class="cp-discover-conflict">
            <Icon name="alert" size={13} /> {conflict.detail}
          </div>
        ))}
        <p class="cp-discover-dialog-sub">
          {overridable
            ? `Installing anyway may crash the game. ${plural(toInstall.length, 'file', 'files')} would be installed${
                plan.total_download_bytes > 0 ? ` (${formatBytes(plan.total_download_bytes)})` : ''
              }.`
            : 'Nothing can be installed until this is resolved.'}
        </p>
        <div class="cp-discover-dialog-actions">
          <Button variant={overridable ? 'ghost' : 'primary'} onClick={onCancel} disabled={busy}>
            {overridable ? 'Cancel' : 'Close'}
          </Button>
          {overridable && (
            <Button onClick={onConfirm} disabled={busy}>
              {busy ? 'Installing…' : 'Install anyway'}
            </Button>
          )}
        </div>
      </ModalContent>
    </Modal>
  );
}

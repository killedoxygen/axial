import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { Button } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { Modal, ModalContent } from '../../ui/Modal';
import { formatBytes, plural } from '../../format';
import type { ContentSelection, ResolutionPlan } from '../../types-content';
import { addToInstance, commitInstall, type AddOutcome } from './actions';
import { createInstallWorkflow } from './install-workflow';

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
  const [, redraw] = useState(0);
  const workflow = useRef<ReturnType<typeof createInstallWorkflow> | null>(null);
  workflow.current ??= createInstallWorkflow({ addToInstance, commitInstall }, () => redraw((value) => value + 1));
  const active = workflow.current;
  active.setTarget(instanceId);
  useEffect(() => () => active.dispose(), [active]);

  return { ...active.snapshot(), add: active.add, confirm: active.confirm, cancel: active.cancel };
}

export function InstallConflictSheet({
  flow,
  onQueued,
}: {
  flow: InstallFlow;
  onQueued?: () => void;
}): JSX.Element | null {
  if (!flow.plan) return null;
  return (
    <ConflictSheet
      plan={flow.plan}
      busy={flow.busy}
      onCancel={flow.cancel}
      onConfirm={() =>
        void flow.confirm().then((outcome) => {
          if (outcome.status === 'queued') onQueued?.();
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

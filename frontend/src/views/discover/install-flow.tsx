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
  installable: ContentSelection[];
  staged: number;
  add: (selections: ContentSelection[], label: string) => Promise<AddOutcome>;
  confirm: () => Promise<AddOutcome>;
  installRest: () => Promise<AddOutcome>;
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

  return {
    ...active.snapshot(),
    add: active.add,
    confirm: active.confirm,
    installRest: active.installRest,
    cancel: active.cancel,
  };
}

export function InstallConflictSheet({
  flow,
  onQueued,
}: {
  flow: InstallFlow;
  onQueued?: () => void;
}): JSX.Element | null {
  if (!flow.plan) return null;
  const queued = (outcome: AddOutcome): void => {
    if (outcome.status === 'queued') onQueued?.();
  };
  return (
    <ConflictSheet
      plan={flow.plan}
      busy={flow.busy}
      skipped={flow.installable.length > 0 ? flow.staged - flow.installable.length : 0}
      staged={flow.staged}
      onCancel={flow.cancel}
      onConfirm={() => void flow.confirm().then(queued)}
      onInstallRest={() => void flow.installRest().then(queued)}
    />
  );
}

function ConflictSheet({
  plan,
  busy,
  skipped,
  staged,
  onCancel,
  onConfirm,
  onInstallRest,
}: {
  plan: ResolutionPlan;
  busy: boolean;
  skipped: number;
  staged: number;
  onCancel: () => void;
  onConfirm: () => void;
  onInstallRest: () => void;
}): JSX.Element {
  const toInstall = plan.items.filter((item) => !item.already_installed || item.update);
  const overridable =
    toInstall.length > 0 &&
    plan.conflicts.every((conflict) => conflict.kind === 'incompatible' || conflict.kind === 'existing_file');
  const replacing = plan.conflicts.filter((conflict) => conflict.kind === 'existing_file').length;
  const onlyReplacing = replacing > 0 && replacing === plan.conflicts.length;
  const partial = skipped > 0;
  const files = `${plural(toInstall.length, 'file', 'files')} would be installed${
    plan.total_download_bytes > 0 ? ` (${formatBytes(plan.total_download_bytes)})` : ''
  }.`;
  return (
    <Modal open onOpenChange={(next) => !next && onCancel()}>
      <ModalContent className="cp-discover-dialog" aria-label="Resolve conflicts">
        <h2 class="cp-discover-dialog-title">
          {overridable ? 'This needs a decision' : partial ? 'Some of this cannot be added here' : 'This cannot be added here'}
        </h2>
        {plan.conflicts.map((conflict, index) => (
          <div key={index} class="cp-discover-conflict">
            <Icon name="alert" size={13} />{' '}
            {conflict.title ? (
              <span>
                <strong>{conflict.title}</strong>: {conflict.detail}
              </span>
            ) : (
              conflict.detail
            )}
          </div>
        ))}
        <p class="cp-discover-dialog-sub">
          {overridable
            ? onlyReplacing
              ? `Replacing overwrites ${plural(replacing, 'file', 'files')} in this instance.`
              : `Installing anyway may crash the game${
                  replacing > 0 ? ` and replaces ${plural(replacing, 'existing file', 'existing files')}` : ''
                }. ${files}`
            : partial
              ? `${skipped} of ${staged} will be skipped. The rest can still be installed.`
              : 'Nothing can be installed until this is resolved.'}
        </p>
        <div class="cp-discover-dialog-actions">
          <Button variant={overridable || partial ? 'ghost' : 'primary'} onClick={onCancel} disabled={busy}>
            {overridable || partial ? 'Cancel' : 'Close'}
          </Button>
          {partial && (
            <Button onClick={onInstallRest} disabled={busy}>
              Install the rest
            </Button>
          )}
          {overridable && (
            <Button variant={onlyReplacing ? 'danger' : 'primary'} onClick={onConfirm} disabled={busy}>
              {busy
                ? 'Installing…'
                : onlyReplacing
                  ? replacing === 1
                    ? 'Replace file'
                    : 'Replace files'
                  : 'Install anyway'}
            </Button>
          )}
        </div>
      </ModalContent>
    </Modal>
  );
}

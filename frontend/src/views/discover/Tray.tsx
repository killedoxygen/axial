import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Button } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { Modal, ModalContent } from '../../ui/Modal';
import { SelectField } from '../../ui/Select';
import { navigate } from '../../ui-state';
import { errMessage } from '../../utils';
import type { CompatCandidate, ResolutionPlan } from '../../types-content';
import { addToInstance, candidatesFor, commitInstall, createFromDraft } from './actions';
import { clearTray, contentTargets, targetInstance, tray, traySelections, unstage } from './state';
import { ContentIcon, formatBytes, plural, Spinner } from './shared';

/**
 * The staging dock. It carries the whole "I want several things" case: pick a
 * few, then commit once. When an instance is targeted it commits into that
 * instance; when nothing is targeted the set itself decides which instance to
 * build, which is the same flow with the target filled in at the end instead of
 * the start.
 */
export function Tray(): JSX.Element | null {
  const items = tray.value;
  const instance = targetInstance.value;
  const [busy, setBusy] = useState(false);
  const [conflictPlan, setConflictPlan] = useState<ResolutionPlan | null>(null);
  const [picking, setPicking] = useState(false);

  if (items.length === 0) return null;

  const addAll = async (): Promise<void> => {
    if (!instance || busy) return;
    setBusy(true);
    const label = plural(items.length, 'item', 'items');
    const outcome = await addToInstance(instance.id, traySelections(), label);
    setBusy(false);
    if (outcome.status === 'needs-confirmation' && outcome.plan) {
      setConflictPlan(outcome.plan);
      return;
    }
    if (outcome.status === 'installed') clearTray();
  };

  const confirmDespiteConflicts = async (): Promise<void> => {
    if (!instance) return;
    setBusy(true);
    const outcome = await commitInstall(
      instance.id,
      traySelections(),
      plural(items.length, 'item', 'items'),
      conflictPlan ?? undefined,
    );
    setBusy(false);
    setConflictPlan(null);
    if (outcome.status === 'installed') clearTray();
  };

  return (
    <>
      <div class="cp-discover-tray" role="region" aria-label="Staged content">
        <div class="cp-discover-tray-items">
          {items.slice(0, 6).map((item) => (
            <button
              key={item.canonical_id}
              class="cp-discover-tray-chip"
              onClick={() => unstage(item.canonical_id)}
              title={`Remove ${item.title}`}
            >
              <span class="cp-discover-tray-chip-icon">
                <ContentIcon url={item.icon_url} kind={item.kind} size={13} />
              </span>
              <span class="cp-discover-tray-chip-name">{item.title}</span>
              <Icon name="x" size={11} />
            </button>
          ))}
          {items.length > 6 && <span class="cp-discover-tray-more">+{items.length - 6}</span>}
        </div>

        <div class="cp-discover-tray-actions">
          <button class="cp-discover-tray-clear" onClick={clearTray}>
            Clear
          </button>
          {instance ? (
            <Button icon="download" onClick={addAll} disabled={busy}>
              {busy ? 'Adding…' : `Add ${items.length} to ${instance.name}`}
            </Button>
          ) : (
            <>
              <ExistingInstancePicker />
              <Button icon="sparkles" onClick={() => setPicking(true)} disabled={busy}>
                Set up an instance
              </Button>
            </>
          )}
        </div>
      </div>

      {picking && <CompatSheet onClose={() => setPicking(false)} />}

      {conflictPlan && (
        <ConflictSheet
          plan={conflictPlan}
          busy={busy}
          onCancel={() => setConflictPlan(null)}
          onConfirm={confirmDespiteConflicts}
        />
      )}
    </>
  );
}

/**
 * Sending a staged set to an instance the user already has. Choosing one does
 * not fork into a separate install path — it just sets the target, and the
 * normal targeted tray takes over from there.
 */
function ExistingInstancePicker(): JSX.Element | null {
  const options = contentTargets.value.map((instance) => ({
    value: instance.id,
    label: `${instance.name} · ${instance.version_display.summary_label}`,
  }));
  if (options.length === 0) return null;

  return (
    <SelectField
      value=""
      onChange={(id) => navigate({ name: 'discover', target: id })}
      options={[{ value: '', label: 'Add to existing…' }, ...options]}
      ariaLabel="Add to an existing instance"
      width={190}
    />
  );
}

/**
 * A conflict is the one thing worth interrupting for: the user has to decide
 * whether to install anyway.
 */
export function ConflictSheet({
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
  return (
    <Modal open onOpenChange={(next) => !next && onCancel()}>
      <ModalContent className="cp-discover-dialog" aria-label="Resolve conflicts">
        <h2 class="cp-discover-dialog-title">
          <Icon name="alert" size={16} /> This needs a decision
        </h2>
        {plan.conflicts.map((conflict, index) => (
          <div key={index} class="cp-discover-conflict">
            <Icon name="alert" size={13} /> {conflict.detail}
          </div>
        ))}
        <div class="cp-discover-plan-note">
          {toInstall.length > 0
            ? `${plural(toInstall.length, 'file', 'files')} would be installed${
                plan.total_download_bytes > 0 ? ` · ${formatBytes(plan.total_download_bytes)}` : ''
              }.`
            : 'Nothing new would be installed.'}
        </div>
        <div class="cp-discover-dialog-actions">
          <Button variant="ghost" onClick={onCancel} disabled={busy}>
            Cancel
          </Button>
          <Button onClick={onConfirm} disabled={busy}>
            {busy ? 'Installing…' : 'Install anyway'}
          </Button>
        </div>
      </ModalContent>
    </Modal>
  );
}

/**
 * Where a staged set becomes an instance. The backend ranks which
 * loader/Minecraft pairs the picks support, so this only has to render them and
 * say plainly what each one leaves behind.
 */
function CompatSheet({ onClose }: { onClose: () => void }): JSX.Element {
  const items = tray.value;
  const [candidates, setCandidates] = useState<CompatCandidate[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState('');

  useEffect(() => {
    let active = true;
    candidatesFor(traySelections())
      .then((resolved) => {
        if (active) setCandidates(resolved);
      })
      .catch((err) => {
        if (active) setError(errMessage(err));
      });
    return () => {
      active = false;
    };
  }, []);

  const create = async (candidate: CompatCandidate): Promise<void> => {
    if (creating) return;
    setCreating(candidate.selection_id);
    const name = `${candidate.loader_label} ${candidate.game_version}`;
    const ok = await createFromDraft(candidate, traySelections(), name);
    setCreating('');
    if (ok) onClose();
  };

  return (
    <Modal open onOpenChange={(next) => !next && onClose()}>
      <ModalContent className="cp-discover-dialog" aria-label="Set up an instance">
        <h2 class="cp-discover-dialog-title">Set up an instance</h2>
        <p class="cp-discover-dialog-sub">
          {plural(items.length, 'item', 'items')} staged. These are the versions they work on — the best fit is first.
        </p>

        {error && <div class="cp-discover-conflict">{error}</div>}

        {!candidates && !error && (
          <div class="cp-discover-plan-note">
            <Spinner size={12} /> Checking what these work with…
          </div>
        )}

        {candidates?.length === 0 && (
          <div class="cp-discover-conflict">
            <Icon name="alert" size={13} /> These picks share no common version. Try removing one.
          </div>
        )}

        <div class="cp-discover-candidates">
          {candidates?.map((candidate) => (
            <button
              key={candidate.selection_id}
              class="cp-discover-candidate"
              data-complete={candidate.complete}
              onClick={() => create(candidate)}
              disabled={!!creating}
            >
              <div class="cp-discover-candidate-main">
                <span class="cp-discover-candidate-name">
                  {candidate.loader_label} {candidate.game_version}
                </span>
                <span class="cp-discover-candidate-summary">{candidate.summary}</span>
              </div>
              {candidate.drops.length > 0 && (
                <span class="cp-discover-candidate-drops" title={candidate.drops.map((d) => d.title).join(', ')}>
                  drops {candidate.drops.map((drop) => drop.title).join(', ')}
                </span>
              )}
              {creating === candidate.selection_id ? <Spinner size={13} /> : <Icon name="chevron-right" size={14} />}
            </button>
          ))}
        </div>

        <div class="cp-discover-dialog-actions">
          <Button variant="ghost" onClick={onClose} disabled={!!creating}>
            Cancel
          </Button>
        </div>
      </ModalContent>
    </Modal>
  );
}

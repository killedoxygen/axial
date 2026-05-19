import type { JSX } from 'preact';
import { Button } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { navigate } from '../../ui-state';

export type Step = 'pick' | 'name';

export const STEP_ORDER: Step[] = ['pick', 'name'];

const STEP_LABELS: Record<Step, string> = {
  pick: 'Pick',
  name: 'Name',
};

export function Stepper({
  current,
  maxReached,
  onJump,
}: {
  current: number;
  maxReached: number;
  onJump: (index: number) => void;
}): JSX.Element {
  const nodes: JSX.Element[] = [];
  STEP_ORDER.forEach((step, index) => {
    if (index > 0) {
      nodes.push(<span key={`sep-${index}`} class="cp-cr-stepper-sep" aria-hidden="true">/</span>);
    }
    const state = index < current ? 'past' : index === current ? 'active' : 'future';
    const clickable = index !== current && index <= maxReached;
    const label = STEP_LABELS[step];
    const number = String(index + 1).padStart(2, '0');
    const inner = (
      <>
        <span class="cp-cr-stepper-num">{number}</span>
        <span class="cp-cr-stepper-label">{label}</span>
      </>
    );
    if (clickable) {
      nodes.push(
        <button
          key={step}
          type="button"
          class="cp-cr-stepper-item"
          data-state={state}
          onClick={() => onJump(index)}
          aria-label={`Go to ${label}`}
        >
          {inner}
        </button>,
      );
      return;
    }
    nodes.push(
      <div
        key={step}
        class="cp-cr-stepper-item"
        data-state={state}
        aria-current={state === 'active' ? 'step' : undefined}
      >
        {inner}
      </div>,
    );
  });
  return <nav class="cp-cr-stepper" aria-label="Create instance progress">{nodes}</nav>;
}

export function LibraryBlocker(): JSX.Element {
  return (
    <div class="cp-cr-blocker">
      <Icon name="folder" size={32} />
      <h2>Set up your library first</h2>
      <p>Croopor needs a place to keep game files before you can make an instance.</p>
      <Button icon="settings" onClick={() => navigate({ name: 'settings' })}>
        Open setup
      </Button>
    </div>
  );
}

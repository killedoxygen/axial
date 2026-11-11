import type { ComponentChildren, JSX } from 'preact';
import { Button } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { PlayerHeadPreview } from '../../ui/PlayerHeadPreview';
import type { Player } from './use-identity';

export const STEP_COUNT = 2;

/** One onboarding screen: progress, heading, content and the footer actions. */
export function Step({
  index,
  view,
  title,
  lead,
  player,
  children,
  aside,
  next,
  error,
}: {
  index: number;
  /** Changes when the content is replaced, which replays the entrance. */
  view: string;
  title: string;
  lead: string;
  player: Player | null;
  children: ComponentChildren;
  /** Secondary footer content, left of the primary action. */
  aside?: ComponentChildren;
  /** Label of the primary action, which submits the step. */
  next?: { label: string; disabled?: boolean };
  error?: string | null;
}): JSX.Element {
  return (
    <>
      <div class="cp-ob-dots" role="img" aria-label={`Step ${index + 1} of ${STEP_COUNT}`}>
        {Array.from({ length: STEP_COUNT }, (_, dot) => (
          <span key={dot} data-state={dot === index ? 'current' : dot < index ? 'done' : 'ahead'} />
        ))}
      </div>
      <div class="cp-ob-body" key={view}>
        <header class="cp-ob-heading">
          {/* Stands in for the stage when the window is too small to show it. */}
          {player && <PlayerHeadPreview class="cp-ob-head" textureSrc={player.src} size={56} radius={12} />}
          <h1>{title}</h1>
        </header>
        <p class="cp-ob-lead">{lead}</p>
        {children}
      </div>
      <footer class="cp-ob-footer">
        {aside}
        {error && (
          <p class="cp-ob-error" role="alert">
            {error}
          </p>
        )}
        {next && (
          <Button
            type="submit"
            size="lg"
            disabled={next.disabled}
            trailing={<Icon name="arrow-right" size={16} stroke={2.2} />}
          >
            {next.label}
          </Button>
        )}
      </footer>
    </>
  );
}

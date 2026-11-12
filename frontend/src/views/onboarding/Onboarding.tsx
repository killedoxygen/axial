import type { JSX } from 'preact';
import { useRef, useState } from 'preact/hooks';
import { WindowStrip } from '../../shell/WindowControls';
import { Sound } from '../../sound';
import { config } from '../../store';
import { showOnboardingOverlay } from '../../ui-state';
import { errMessage } from '../../utils';
import { finishOnboarding, type Preferences } from './finish';
import { Identity } from './Identity';
import { useIdentity } from './use-identity';
import { Stage } from './Stage';
import { Style } from './Style';

/** Matches the `cp-ob-leave` animation. */
const LEAVE_MS = 560;

export function Onboarding(): JSX.Element {
  const identity = useIdentity();
  const [styling, setStyling] = useState(false);
  const [preferences, setPreferences] = useState<Preferences>(() => ({
    music: false,
    discord: config.value?.discord_rpc_enabled !== false,
    // Anonymous stats stay off until the player turns them on.
    telemetry: config.value?.telemetry_enabled === true,
  }));
  const [phase, setPhase] = useState<'editing' | 'saving' | 'leaving'>('editing');
  const [error, setError] = useState<string | null>(null);
  const finishing = useRef(false);
  const { choice } = identity;

  const finish = async (): Promise<void> => {
    if (!choice || finishing.current) return;
    finishing.current = true;
    setPhase('saving');
    setError(null);
    try {
      await finishOnboarding(choice, preferences);
      setPhase('leaving');
      window.setTimeout(() => {
        showOnboardingOverlay.value = false;
      }, LEAVE_MS);
    } catch (err: unknown) {
      setError(errMessage(err));
      setPhase('editing');
      finishing.current = false;
    }
  };

  // Enter in a field submits the step; focused buttons keep their own Enter.
  const submit = (event: Event): void => {
    event.preventDefault();
    if (!choice) return;
    if (styling) void finish();
    else {
      setStyling(true);
      Sound.ui('click');
    }
  };

  return (
    <div class="cp-ob" data-phase={phase}>
      <WindowStrip />
      <Stage player={identity.player} />
      <form class="cp-ob-pane" onSubmit={submit}>
        {styling && choice ? (
          <Style
            player={identity.player}
            preferences={preferences}
            onChange={setPreferences}
            onBack={() => setStyling(false)}
            saving={phase !== 'editing'}
            error={error}
          />
        ) : (
          <Identity identity={identity} />
        )}
      </form>
    </div>
  );
}

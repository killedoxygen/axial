import type { JSX } from 'preact';
import { useEffect, useRef } from 'preact/hooks';
import { Button, Input, Chip } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { MicrosoftMark } from '../../ui/MicrosoftMark';
import { PlayerHeadPreview } from '../../ui/PlayerHeadPreview';
import { hideTip, showTip } from '../../ui/Tip';
import type { Identity as IdentityState, IdentityView } from './use-identity';
import { Step } from './Step';

const HEADINGS: Record<IdentityView, { title: string; lead: string }> = {
  microsoft: { title: "Who's playing?", lead: 'Sign in to bring your Minecraft profile, skin and capes.' },
  waiting: {
    title: 'Finish in your browser.',
    lead: 'Axial opened the Microsoft sign-in page. Come back here when you are done.',
  },
  'signed-in': { title: 'Welcome in.', lead: 'Your Minecraft profile, skin and cape came with you.' },
  offline: { title: "Who's playing?", lead: 'Pick a name and a look. You can sign in any time from Accounts.' },
};

function MicrosoftSignIn({ identity }: { identity: IdentityState }): JSX.Element {
  return (
    <>
      <button
        type="button"
        class="cp-btn cp-btn--primary cp-btn--lg cp-btn--full cp-ob-msa"
        disabled={identity.signInUnavailable !== null}
        onClick={() => void identity.signIn()}
      >
        <span class="cp-ob-msa-mark">
          <MicrosoftMark />
        </span>
        Sign in with Microsoft
      </button>
      {identity.signInError ? (
        <p class="cp-ob-error" role="alert">
          {identity.signInError}
        </p>
      ) : (
        identity.signInUnavailable && <p class="cp-ob-note">{identity.signInUnavailable}</p>
      )}
      <Button variant="ghost" size="lg" onClick={identity.playOffline}>
        Play offline instead
      </Button>
    </>
  );
}

function Waiting(): JSX.Element {
  return (
    <div class="cp-ob-card" role="status">
      <span class="cp-ob-spinner" aria-hidden="true" />
      <span class="cp-ob-card-title">Waiting for Microsoft</span>
    </div>
  );
}

function SignedIn({ identity }: { identity: IdentityState }): JSX.Element {
  return (
    <>
      <div class="cp-ob-card">
        <PlayerHeadPreview textureSrc={identity.player?.src} size={44} radius={10} />
        <div class="cp-ob-card-text">
          <span class="cp-ob-card-title">{identity.player?.name}</span>
          <span class="cp-ob-chips">
            <Chip>Microsoft account</Chip>
            <Chip>Java Edition</Chip>
          </span>
        </div>
        <Icon name="check-circle" size={22} color="var(--accent)" />
      </div>
      <Button variant="ghost" size="lg" onClick={() => void identity.signIn()}>
        Use a different account
      </Button>
    </>
  );
}

let introduced = false;

/** Points out the typed name's own skin the first time one is found: its tile rings and a tooltip names it. */
function useOwnLookIntro(found: boolean): { current: HTMLLabelElement | null } {
  const ref = useRef<HTMLLabelElement>(null);

  useEffect(() => {
    const tile = ref.current;
    if (!found || introduced || !tile) return undefined;
    tile.dataset.fresh = '';
    showTip("You can wear this name's Minecraft skin", tile);
    const done = (): void => {
      delete tile.dataset.fresh;
      hideTip();
    };
    const timer = window.setTimeout(() => {
      introduced = true;
      done();
    }, 4200);
    return () => {
      window.clearTimeout(timer);
      done();
    };
  }, [found]);

  return ref;
}

function Offline({ identity }: { identity: IdentityState }): JSX.Element {
  const own = useOwnLookIntro(identity.looks[0].lookup !== undefined);
  return (
    <>
      <label class="cp-ob-field">
        <span class="cp-ob-label">Player name</span>
        <Input value={identity.name} onChange={identity.setName} placeholder="Steve" autoFocus />
        <span class="cp-ob-error" role="alert">
          {identity.nameError}
        </span>
      </label>
      <fieldset class="cp-ob-looks">
        <legend class="cp-ob-label">Your look</legend>
        {identity.looks.map((look) => (
          <label key={look.id} ref={look.lookup ? own : undefined} class="cp-ob-look" title={look.name}>
            <input
              type="radio"
              name="look"
              aria-label={look.name}
              checked={look.id === identity.look.id}
              onChange={() => identity.setLook(look.id)}
            />
            <PlayerHeadPreview textureSrc={look.src} size={36} radius={7} />
          </label>
        ))}
      </fieldset>
    </>
  );
}

export function Identity({ identity }: { identity: IdentityState }): JSX.Element {
  const { view } = identity;
  return (
    <Step
      index={0}
      view={view}
      {...HEADINGS[view]}
      player={identity.player}
      aside={
        view === 'offline' && !identity.signInUnavailable ? (
          <Button variant="ghost" size="lg" onClick={identity.useMicrosoft}>
            Sign in with Microsoft instead
          </Button>
        ) : view === 'microsoft' ? (
          <p class="cp-ob-note">Everything here can be changed later in Settings.</p>
        ) : null
      }
      next={view === 'offline' || view === 'signed-in' ? { label: 'Continue', disabled: !identity.choice } : undefined}
    >
      {view === 'microsoft' && <MicrosoftSignIn identity={identity} />}
      {view === 'waiting' && <Waiting />}
      {view === 'signed-in' && <SignedIn identity={identity} />}
      {view === 'offline' && <Offline identity={identity} />}
    </Step>
  );
}

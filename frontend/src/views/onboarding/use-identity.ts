import { useEffect, useState } from 'preact/hooks';
import { DEFAULT_SKINS } from '../../default-skins';
import { accountsSnapshot, activeAccount, refreshAccountsData } from '../../machines/accounts';
import { hasNativeDesktopRuntime } from '../../native';
import { clampPlayerNameInput } from '../../player-name';
import { profileCape, profileSkinTextureSrc } from '../../player-skin';
import { validateUsername } from '../../utils';
import {
  capeFileUrl,
  lookupCapeFileUrl,
  lookupMinecraftSkin,
  lookupSkinFileUrl,
} from '../accounts/api';
import type { MinecraftSkinLookup, SkinVariant } from '../accounts/types';
import { SIGN_IN_UNAVAILABLE } from '../accounts/AccountSwitcher';
import { useMicrosoftSignIn } from '../accounts/useMicrosoftSignIn';

const LOOKUP_DELAY_MS = 450;
const OWN_LOOK = 'own';

export interface Look {
  id: string;
  name: string;
  src: string;
  variant: SkinVariant;
  capeSrc?: string;
  /** Set when the look is the skin of the Minecraft player with the typed name. */
  lookup?: MinecraftSkinLookup;
}

/** The character on the stage. */
export interface Player extends Omit<Look, 'id' | 'lookup'> {}

export type Choice =
  | { kind: 'microsoft' }
  /** `lookChosen` is false while the look is only the automatic suggestion. */
  | { kind: 'offline'; name: string; look: Look; lookChosen: boolean };

export type IdentityView = 'microsoft' | 'waiting' | 'signed-in' | 'offline';

/** The typed name's Minecraft skin: undefined while looking, null when there is none. */
function usePlayerLookup(name: string | null): MinecraftSkinLookup | null | undefined {
  const [found, setFound] = useState<MinecraftSkinLookup | null | undefined>(undefined);

  useEffect(() => {
    setFound(undefined);
    if (!name) return undefined;
    let current = true;
    const settle = (result: MinecraftSkinLookup | null): void => {
      if (current) setFound(result);
    };
    const timer = window.setTimeout(() => {
      lookupMinecraftSkin(name).then(settle, () => settle(null));
    }, LOOKUP_DELAY_MS);
    return () => {
      current = false;
      window.clearTimeout(timer);
    };
  }, [name]);

  return found;
}

export function useIdentity() {
  const { state, signInAvailable } = accountsSnapshot.value;
  const signInUnavailable =
    state === 'unavailable'
      ? 'Microsoft sign-in is unavailable right now.'
      : state === 'ready' && !signInAvailable && !hasNativeDesktopRuntime()
        ? SIGN_IN_UNAVAILABLE
        : null;
  const signIn = useMicrosoftSignIn({ canStart: state === 'ready' && !signInUnavailable });
  const [signingIn, setSigningIn] = useState(false);
  const [offline, setOffline] = useState(false);
  const [name, setName] = useState('');
  const [lookId, setLookId] = useState<string | null>(null);

  useEffect(() => {
    void refreshAccountsData();
  }, []);

  const nameError = validateUsername(name);
  const found = usePlayerLookup(offline && !nameError ? name.trim() : null);
  const looks: Look[] = [
    ...(found
      ? [
          {
            id: OWN_LOOK,
            name: `${found.username}'s skin`,
            src: lookupSkinFileUrl(found),
            variant: found.variant,
            capeSrc: lookupCapeFileUrl(found),
            lookup: found,
          },
        ]
      : []),
    ...DEFAULT_SKINS,
  ];
  const look = looks.find((candidate) => candidate.id === lookId) ?? looks[0];

  const account = activeAccount();
  const online = account?.online_action.state_id === 'online_ready';
  const view: IdentityView = signingIn ? 'waiting' : online ? 'signed-in' : offline ? 'offline' : 'microsoft';

  let player: Player | null = null;
  let choice: Choice | null = null;
  if (view === 'signed-in' && account) {
    const cape = profileCape(account);
    player = {
      name: account.display_name,
      src: profileSkinTextureSrc(account) ?? DEFAULT_SKINS[0].src,
      variant: account.profile_skin?.variant ?? 'classic',
      capeSrc: cape ? capeFileUrl(cape) : undefined,
    };
    choice = { kind: 'microsoft' };
  } else if (view === 'offline' && !nameError) {
    // The character waits for the lookup unless the player already picked a look.
    if (lookId !== null || found !== undefined) player = { ...look, name: name.trim() };
    choice = { kind: 'offline', name: name.trim(), look, lookChosen: lookId !== null };
  }

  return {
    view,
    player,
    choice,
    name,
    nameError: name ? nameError : null,
    setName: (value: string) => setName(clampPlayerNameInput(value)),
    looks,
    look,
    setLook: (id: string) => setLookId(id),
    playOffline: () => setOffline(true),
    useMicrosoft: () => setOffline(false),
    signInUnavailable,
    signInError: signIn.message?.tone === 'err' ? signIn.message.text : null,
    signIn: async (): Promise<void> => {
      setSigningIn(true);
      try {
        await signIn.startLogin();
      } finally {
        setSigningIn(false);
      }
    },
  };
}

export type Identity = ReturnType<typeof useIdentity>;

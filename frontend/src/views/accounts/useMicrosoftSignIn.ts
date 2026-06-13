import { useRef, useState } from 'preact/hooks';
import {
  signInWithMicrosoft,
  type NativeMicrosoftSignInResult,
} from '../../native';
import { boundedMessage } from './api';

export type MicrosoftSignInMessage = { tone: 'ok' | 'err'; text: string } | null;

interface MicrosoftSignInOptions {
  canStart?: boolean;
  onAuthenticated?: (result: NativeMicrosoftSignInResult) => Promise<MicrosoftSignInMessage | void> | MicrosoftSignInMessage | void;
}

export function useMicrosoftSignIn(options: MicrosoftSignInOptions = {}): {
  busy: boolean;
  message: MicrosoftSignInMessage;
  setMessage: (message: MicrosoftSignInMessage) => void;
  clearMessage: () => void;
  startLogin: () => Promise<void>;
} {
  const optionsRef = useRef(options);
  optionsRef.current = options;

  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<MicrosoftSignInMessage>(null);

  const startLogin = async (): Promise<void> => {
    if (busy || optionsRef.current.canStart === false) return;
    setBusy(true);
    setMessage(null);

    try {
      const result = await signInWithMicrosoft();
      if (!result) {
        setMessage({ tone: 'err', text: 'Microsoft sign-in is available in the desktop app.' });
        return;
      }
      if (result.status === 'cancelled') return;
      if (result.status !== 'authenticated') {
        setMessage({ tone: 'err', text: 'Microsoft sign-in returned an unexpected response.' });
        return;
      }

      try {
        const nextMessage = await optionsRef.current.onAuthenticated?.(result);
        if (nextMessage) {
          setMessage(nextMessage);
          return;
        }
      } catch (err: unknown) {
        setMessage({
          tone: 'err',
          text: boundedMessage(
            errorText(err),
            'Microsoft sign-in completed, but Croopor could not switch to that account.',
          ),
        });
        return;
      }

      const profileName = typeof result.profile_name === 'string' && result.profile_name.trim()
        ? result.profile_name.trim()
        : 'Minecraft profile';
      setMessage({ tone: 'ok', text: `${profileName} verified. Online launch is ready.` });
    } catch (err: unknown) {
      setMessage({
        tone: 'err',
        text: boundedMessage(errorText(err), 'Microsoft sign-in could not be completed.'),
      });
    } finally {
      setBusy(false);
    }
  };

  return {
    busy,
    message,
    setMessage,
    clearMessage: () => setMessage(null),
    startLogin,
  };
}

function errorText(error: unknown): string | undefined {
  if (typeof error === 'string') return error;
  if (error instanceof Error) return error.message;
  return undefined;
}

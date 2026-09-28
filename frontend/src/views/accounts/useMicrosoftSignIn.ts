import { useRef, useState } from 'preact/hooks';
import { accountsNotice, accountsOp, signInWithMicrosoftAccount } from '../../machines/accounts';
import type { NativeMicrosoftSignInResult } from '../../native';
import { boundedMessage } from './api';

export type MicrosoftSignInMessage = { tone: 'ok' | 'err'; text: string } | null;

interface MicrosoftSignInOptions {
  canStart?: boolean;
  onAuthenticated?: (
    result: NativeMicrosoftSignInResult,
  ) => Promise<MicrosoftSignInMessage | void> | MicrosoftSignInMessage | void;
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

  const [message, setMessage] = useState<MicrosoftSignInMessage>(null);

  const startLogin = async (): Promise<void> => {
    if (accountsOp.value !== null || optionsRef.current.canStart === false) return;
    setMessage(null);

    try {
      const result = await signInWithMicrosoftAccount();
      if (!result) {
        if (accountsNotice.value) setMessage({ tone: 'err', text: accountsNotice.value });
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
            'Microsoft sign-in completed, but Axial could not switch to that account.',
          ),
        });
        return;
      }

      setMessage({ tone: 'ok', text: 'Microsoft sign-in completed.' });
    } catch (err: unknown) {
      setMessage({
        tone: 'err',
        text: boundedMessage(errorText(err), 'Microsoft sign-in could not be completed.'),
      });
    }
  };

  return {
    busy: accountsOp.value !== null,
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

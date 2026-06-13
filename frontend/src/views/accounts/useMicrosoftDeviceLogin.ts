import { useEffect, useRef, useState } from 'preact/hooks';
import { api, isApiError } from '../../api';
import {
  loginErrorMessage,
  loginPendingResponse,
  pollErrorMessage,
  pollResponse,
  pollSuccessMessage,
  pollTerminalMessage,
  type AuthLoginPending,
  type AuthPollAuthenticatedRecord,
} from './auth';

export type MicrosoftDeviceLoginMessage = { tone: 'ok' | 'err'; text: string } | null;

interface MicrosoftDeviceLoginOptions {
  canStart?: boolean;
  onAuthenticated?: (poll: AuthPollAuthenticatedRecord) => Promise<void> | void;
}

export function useMicrosoftDeviceLogin(options: MicrosoftDeviceLoginOptions = {}): {
  login: AuthLoginPending | null;
  busy: boolean;
  pollHint: string | null;
  message: MicrosoftDeviceLoginMessage;
  setMessage: (message: MicrosoftDeviceLoginMessage) => void;
  clearMessage: () => void;
  startLogin: () => Promise<void>;
  cancelLogin: () => void;
} {
  const optionsRef = useRef(options);
  optionsRef.current = options;

  const [login, setLogin] = useState<AuthLoginPending | null>(null);
  const [busy, setBusy] = useState(false);
  const [pollHint, setPollHint] = useState<string | null>(null);
  const [message, setMessage] = useState<MicrosoftDeviceLoginMessage>(null);

  useEffect(() => {
    if (!login) return undefined;
    let active = true;
    const timeout = window.setTimeout(() => {
      void api('POST', `/auth/login/${encodeURIComponent(login.login_id)}/poll`)
        .then(async (response: unknown) => {
          if (!active) return;
          const poll = pollResponse(response);
          if (!poll) {
            setLogin(null);
            setPollHint(null);
            setMessage({ tone: 'err', text: pollTerminalMessage(null) });
            return;
          }
          if (poll.status === 'pending') {
            setPollHint(poll.poll_hint ?? null);
            setLogin((current) => current?.login_id === login.login_id
              ? { ...current, interval: poll.interval }
              : current);
            return;
          }
          if (poll.status === 'msa_authenticated') {
            try {
              await optionsRef.current.onAuthenticated?.(poll);
            } catch {
              // The login itself succeeded. A view-level refresh failure should not
              // turn the device-code flow into a terminal provider error.
            }
            if (!active) return;
            setLogin(null);
            setPollHint(null);
            setMessage({ tone: 'ok', text: pollSuccessMessage(poll) });
            return;
          }
          setLogin(null);
          setPollHint(null);
          setMessage({ tone: 'err', text: pollTerminalMessage(poll) });
        })
        .catch((err: unknown) => {
          if (!active) return;
          setLogin(null);
          setPollHint(null);
          setMessage({
            tone: 'err',
            text: isApiError(err)
              ? pollErrorMessage(err.payload)
              : 'Could not reach the local backend while polling Microsoft sign-in.',
          });
        });
    }, Math.max(1, login.interval) * 1000);

    return () => {
      active = false;
      window.clearTimeout(timeout);
    };
  }, [login]);

  const startLogin = async (): Promise<void> => {
    if (busy || login || optionsRef.current.canStart === false) return;
    setBusy(true);
    setLogin(null);
    setPollHint(null);
    setMessage(null);
    try {
      const response = await api('POST', '/auth/login');
      const pending = loginPendingResponse(response);
      if (pending) {
        setLogin(pending);
        return;
      }
      setMessage({ tone: 'err', text: loginErrorMessage(response) });
    } catch (err: unknown) {
      setMessage({
        tone: 'err',
        text: isApiError(err)
          ? loginErrorMessage(err.payload)
          : 'Could not reach the local backend to start Microsoft sign-in.',
      });
    } finally {
      setBusy(false);
    }
  };

  const cancelLogin = (): void => {
    setLogin(null);
    setPollHint(null);
  };

  return {
    login,
    busy,
    pollHint,
    message,
    setMessage,
    clearMessage: () => setMessage(null),
    startLogin,
    cancelLogin,
  };
}

import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Button, Input } from '../../ui/Atoms';
import { api } from '../../api';
import { browseDirectory } from '../../native';
import { errMessage } from '../../utils';
import { showSetupOverlay } from '../../ui-state';
import './setup.css';

// Library setup overlay
// Creates a managed library at the recommended path or points at an existing one
export function SetupOverlay(): JSX.Element {
  const [mode, setMode] = useState<'managed' | 'existing'>('managed');
  const [managedPath, setManagedPath] = useState<string>('Preparing default library path…');
  const [existingPath, setExistingPath] = useState<string>('');
  const [status, setStatus] = useState<'pending' | 'running' | 'error' | 'ready'>('pending');
  const [error, setError] = useState<string | null>(null);

  // Fetch defaults on mount, then kick off a managed setup automatically
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const defaults: any = await api('GET', '/setup/defaults');
        if (cancelled) return;
        setManagedPath(defaults.managed_default_path || 'Could not determine a default library path.');
        if (defaults.existing_default_path) setExistingPath(defaults.existing_default_path);
        if (defaults.managed_default_path) {
          setStatus('running');
          const res: any = await api('POST', '/setup/init', { path: defaults.managed_default_path });
          if (cancelled) return;
          if (res.error) { setError(res.error); setStatus('error'); return; }
          setStatus('ready');
          showSetupOverlay.value = false;
        } else {
          setStatus('error');
          setError('No default library path available. Use an existing folder or retry.');
        }
      } catch (err) {
        if (cancelled) return;
        setError(errMessage(err) || 'Setup failed');
        setStatus('error');
      }
    })();
    return () => { cancelled = true; };
  }, []);

  const retryManaged = async (): Promise<void> => {
    setError(null);
    setStatus('running');
    try {
      const res: any = await api('POST', '/setup/init', { path: managedPath });
      if (res.error) { setError(res.error); setStatus('error'); return; }
      showSetupOverlay.value = false;
    } catch (err) {
      setError(errMessage(err) || 'Setup failed');
      setStatus('error');
    }
  };

  const useExisting = async (): Promise<void> => {
    setError(null);
    if (!existingPath.trim()) { setError('Pick a folder first.'); return; }
    setStatus('running');
    try {
      const res: any = await api('POST', '/setup/set-dir', { path: existingPath.trim() });
      if (res.error) { setError(res.error); setStatus('error'); return; }
      showSetupOverlay.value = false;
    } catch (err) {
      setError(errMessage(err) || 'Could not use that path');
      setStatus('error');
    }
  };

  const browseForExisting = async (): Promise<void> => {
    try {
      const picked = await browseDirectory(existingPath);
      if (picked) setExistingPath(picked);
      else if (picked === null) {
        const res: any = await api('POST', '/setup/browse');
        if (res?.path) setExistingPath(res.path);
      }
    } catch {
      /* user cancelled */
    }
  };

  return (
    <div class="cp-setup-overlay">
      <div class="cp-setup-card">
        <img src="logo.svg" alt="" class="cp-logo" width="48" height="48" />
        <h1 class="cp-setup-title">Set up your library</h1>
        <p class="cp-setup-sub">
          Croopor needs a folder for instances, installed versions, and assets. It can manage one
          for you (recommended) or use an existing Minecraft folder.
        </p>

        {mode === 'managed' ? (
          <>
            <div class="cp-setup-path">{managedPath}</div>
            {status === 'running' && <div class="cp-setup-progress" />}
            {error && <div class="cp-setup-error">{error}</div>}
            <div style={{ display: 'flex', gap: 8, justifyContent: 'flex-end' }}>
              <Button variant="ghost" onClick={() => setMode('existing')}>Use existing folder</Button>
              <Button onClick={retryManaged} disabled={status === 'running' || !managedPath}>
                {status === 'running' ? 'Setting up…' : status === 'error' ? 'Retry' : 'Create library'}
              </Button>
            </div>
          </>
        ) : (
          <>
            <div style={{ display: 'flex', gap: 8 }}>
              <Input value={existingPath} onChange={setExistingPath} placeholder="/path/to/.minecraft" style={{ flex: 1 }} />
              <Button variant="secondary" icon="folder" onClick={browseForExisting}>Browse</Button>
            </div>
            {error && <div class="cp-setup-error">{error}</div>}
            <div style={{ display: 'flex', gap: 8, justifyContent: 'flex-end' }}>
              <Button variant="ghost" onClick={() => setMode('managed')}>Use managed library</Button>
              <Button onClick={useExisting} disabled={status === 'running' || !existingPath.trim()}>
                Use this folder
              </Button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

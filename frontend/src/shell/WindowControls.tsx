import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Icon } from '../ui/Icons';
import {
  hasNativeDesktopRuntime,
  windowClose,
  windowMinimize,
  windowToggleMaximize,
  windowIsMaximized,
} from '../native';

// Min, max, close for the custom frame
// Returns null outside Tauri so we don't double up on the OS chrome
export function WindowControls(): JSX.Element | null {
  const isNative = hasNativeDesktopRuntime();
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    if (!isNative) return;
    void windowIsMaximized().then(setMaximized);
  }, [isNative]);

  if (!isNative) return null;

  const onMin = (): void => { void windowMinimize(); };
  const onMax = async (): Promise<void> => {
    const next = await windowToggleMaximize();
    if (next != null) setMaximized(next);
  };
  const onClose = (): void => { void windowClose(); };

  return (
    <div class="cp-winctrls cp-nodrag">
      <button class="cp-winctrl" aria-label="Minimize" onClick={onMin}>
        <Icon name="minus" size={14} stroke={1.8} />
      </button>
      <button class="cp-winctrl" aria-label={maximized ? 'Restore' : 'Maximize'} onClick={onMax}>
        <Icon name="rectangle" size={12} stroke={1.8} />
      </button>
      <button class="cp-winctrl cp-winctrl--close" aria-label="Close" onClick={onClose}>
        <Icon name="x" size={14} stroke={1.8} />
      </button>
    </div>
  );
}

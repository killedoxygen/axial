import { desktopChromeMode, desktopPlatform, loadDesktopChrome, nativeCommands } from '../native';

/**
 * The window's chrome, in one place. The desktop shell says which it draws:
 * - `mac-overlay`: the system's traffic lights float over the top of the rail.
 * - `custom-frameless` (Windows, Linux): no system frame, so `WindowControls` draws the buttons.
 * - `native-decorated` and `browser`: the system frames the window and nothing is drawn here.
 * `html[data-desktop-chrome]` and `html[data-desktop-platform]` carry this to `chrome.css`.
 */

/** Whether the launcher draws the window's buttons. */
export function drawsWindowControls(): boolean {
  return desktopChromeMode() === 'custom-frameless';
}

/** Whether the launcher, not a system title bar, is what the window is dragged by. */
export function ownsWindowFrame(): boolean {
  return drawsWindowControls() || desktopChromeMode() === 'mac-overlay';
}

async function command(name: string): Promise<unknown> {
  return nativeCommands()?.invoke(name);
}

export async function minimizeWindow(): Promise<void> {
  await command('window_minimize');
}

export async function closeWindow(): Promise<void> {
  await command('window_close');
}

export async function isWindowMaximized(): Promise<boolean> {
  return (await command('window_is_maximized')) === true;
}

/** Maximises or restores, and says which it now is. */
export async function toggleWindowMaximized(): Promise<boolean> {
  return (await command('window_toggle_maximize')) === true;
}

// WKWebView and WebKitGTK ignore CSS app-region, so one delegated listener moves the window.
// `.cp-drag` drags across its whole area, controls excepted; an overlay's backdrop (`[data-window-drag-strip]`)
// drags only within the chrome's height, so the rest of it still dismisses the overlay.
const NOT_A_HANDLE = '.cp-nodrag, button, input, select, textarea, a, [role="button"]';

function chromeHeight(): number {
  return parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--chrome-h')) || 0;
}

function dragsWindow(event: MouseEvent): boolean {
  const target = event.target;
  if (event.button !== 0 || !(target instanceof Element) || target.closest(NOT_A_HANDLE)) return false;
  if (target.closest('.cp-drag')) return true;
  return target.matches('[data-window-drag-strip]') && event.clientY < chromeHeight();
}

function installWindowDrag(): void {
  document.addEventListener('mousedown', (event) => {
    if (!dragsWindow(event)) return;
    event.preventDefault();
    void command(event.detail === 2 ? 'window_toggle_maximize' : 'window_start_dragging');
  });
  // A press on a backdrop's strip moves the window; it must not also dismiss the overlay.
  document.addEventListener(
    'click',
    (event) => {
      if (event.target instanceof Element && event.target.matches('[data-window-drag-strip]') && dragsWindow(event)) {
        event.stopPropagation();
      }
    },
    true,
  );
}

/** Learns the chrome from the desktop shell, tells the stylesheet, and makes the frame draggable. */
export async function installWindowChrome(): Promise<void> {
  await loadDesktopChrome();
  const root = document.documentElement;
  root.dataset.desktopPlatform = desktopPlatform();
  root.dataset.desktopChrome = desktopChromeMode();
  if (ownsWindowFrame()) installWindowDrag();
}

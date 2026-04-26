import { byId } from './dom';

const SCRAMBLE_CHARS = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._-';
const scrambleTimers = new Map<HTMLElement, ReturnType<typeof setInterval>>();

export function scrambleText(el: HTMLElement | null, text: string, duration?: number): void {
  if (!el) return;
  if (scrambleTimers.has(el)) clearInterval(scrambleTimers.get(el));
  // Set final text first to measure layout, then animate
  el.textContent = text;
  const finalHeight = el.offsetHeight;
  el.style.minHeight = finalHeight + 'px';
  const steps = 7;
  const interval = (duration || 280) / steps;
  let step = 0;
  const id = setInterval(() => {
    step++;
    const reveal = Math.floor((step / steps) * text.length);
    let out = '';
    for (let i = 0; i < text.length; i++) {
      if (text[i] === ' ') out += ' ';
      else if (i < reveal) out += text[i];
      else out += SCRAMBLE_CHARS[Math.floor(Math.random() * SCRAMBLE_CHARS.length)];
    }
    el.textContent = out;
    if (step >= steps) { clearInterval(id); scrambleTimers.delete(el); el.textContent = text; el.style.minHeight = ''; }
  }, interval);
  scrambleTimers.set(el, id);
}

const LAUNCH_FRAMES: string[][] = [
  ['   ╭──────────────╮   ', '   │  ▓▓▓▓▓▓▓▓▓▓  │   ', '   │ ▓▓  ◉  ◉  ▓▓ │   ', '   │ ▓▓   ▔▔   ▓▓ │   ', '   │ ▓▓  ╲__/  ▓▓ │   ', '   │  ▓▓▓▓▓▓▓▓▓▓  │   ', '   ╰──────────────╯   '],
  ['   ╭──────────────╮   ', '   │  ░▓▓▓▓▓▓▓▓░  │   ', '   │ ▓░  ◌  ◌  ░▓ │   ', '   │ ▓░   ▔▔   ░▓ │   ', '   │ ▓░  ╲__/  ░▓ │   ', '   │  ░▓▓▓▓▓▓▓▓░  │   ', '   ╰──────────────╯   '],
  ['   ╔══════════════╗   ', '   ║  ██████████  ║   ', '   ║ ██  ◆  ◆  ██ ║   ', '   ║ ██   ▔▔   ██ ║   ', '   ║ ██  ╱__╲  ██ ║   ', '   ║  ██████████  ║   ', '   ╚══════════════╝   '],
  ['   ╔══════════════╗   ', '   ║  ███▓▓▓▓███  ║   ', '   ║ ██  ◈  ◈  ██ ║   ', '   ║ ██   ▂▂   ██ ║   ', '   ║ ██  ╲──╱  ██ ║   ', '   ║  ███▓▓▓▓███  ║   ', '   ╚══════════════╝   '],
];

let launchSeqInterval: ReturnType<typeof setInterval> | null = null;

export function startLaunchSequence(): void {
  endLaunchSequence();
  let f = 0;
  const launchAscii = byId<HTMLElement>('launch-ascii');
  if (launchAscii) launchAscii.textContent = LAUNCH_FRAMES[0].join('\n');
  launchSeqInterval = setInterval(() => {
    f = (f + 1) % LAUNCH_FRAMES.length;
    const nextLaunchAscii = byId<HTMLElement>('launch-ascii');
    if (nextLaunchAscii) nextLaunchAscii.textContent = LAUNCH_FRAMES[f].join('\n');
  }, 320);
}

export function endLaunchSequence(): void {
  if (launchSeqInterval) {
    clearInterval(launchSeqInterval);
    launchSeqInterval = null;
  }
}

const RUNNING_FRAMES: string[][] = [
  [' /\\_/\\ ', '( o.o )', ' > ^ < '],
  [' /\\_/\\ ', '( o.o )', ' > ^ < '],
  [' /\\_/\\ ', '( -.- )', ' > ^ < '],
  [' /\\_/\\ ', '( o.o )', ' > ^ < '],
  [' /\\_/\\ ', '( ^.^ )', ' > ^ < '],
  [' /\\_/\\ ', '( ^.^ )', ' > ^ < '],
  [' /\\_/\\ ', '( o.o )', ' > ^ < '],
  [' /\\_/\\ ', '( -.o )', ' > ^ < '],
];

let runningAnimInterval: ReturnType<typeof setInterval> | null = null;

export function startRunningAnimation(): void {
  stopRunningAnimation();
  const runningAscii = byId<HTMLElement>('running-ascii');
  if (!runningAscii) return;
  let f = 0;
  runningAscii.textContent = RUNNING_FRAMES[0].join('\n');
  runningAnimInterval = setInterval(() => {
    f = (f + 1) % RUNNING_FRAMES.length;
    const nextRunningAscii = byId<HTMLElement>('running-ascii');
    if (nextRunningAscii) nextRunningAscii.textContent = RUNNING_FRAMES[f].join('\n');
  }, 900);
}

export function stopRunningAnimation(): void {
  if (runningAnimInterval) {
    clearInterval(runningAnimInterval);
    runningAnimInterval = null;
  }
}

let uptimeInterval: ReturnType<typeof setInterval> | null = null;
let uptimeStart = 0;

export function startUptime(launchedAt: string | number | null): void {
  stopUptime();
  uptimeStart = launchedAt ? new Date(launchedAt).getTime() : Date.now();
  const update = (): void => {
    const elapsed = Math.floor((Date.now() - uptimeStart) / 1000);
    const runningUptime = byId<HTMLElement>('running-uptime');
    if (runningUptime) runningUptime.textContent = `${Math.floor(elapsed / 60)}:${(elapsed % 60).toString().padStart(2, '0')}`;
  };
  update();
  uptimeInterval = setInterval(update, 1000);
}

export function stopUptime(): void {
  if (uptimeInterval) {
    clearInterval(uptimeInterval);
    uptimeInterval = null;
  }
}

export function clearLaunchVisualState(): void {
  endLaunchSequence();
  stopRunningAnimation();
  stopUptime();
  const runningUptime = byId<HTMLElement>('running-uptime');
  const runningPid = byId<HTMLElement>('running-pid');
  const runningVersion = byId<HTMLElement>('running-version');
  if (runningUptime) runningUptime.textContent = '0:00';
  if (runningPid) runningPid.textContent = '';
  if (runningVersion) runningVersion.textContent = '';
}


import { dom } from './state.js';

const SCRAMBLE_CHARS = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._-';
const scrambleTimers = new Map();

export function scrambleText(el, text, duration) {
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

const LAUNCH_FRAMES = [
  ['   ╭──────────────╮   ', '   │  ▓▓▓▓▓▓▓▓▓▓  │   ', '   │ ▓▓  ◉  ◉  ▓▓ │   ', '   │ ▓▓   ▔▔   ▓▓ │   ', '   │ ▓▓  ╲__/  ▓▓ │   ', '   │  ▓▓▓▓▓▓▓▓▓▓  │   ', '   ╰──────────────╯   '],
  ['   ╭──────────────╮   ', '   │  ░▓▓▓▓▓▓▓▓░  │   ', '   │ ▓░  ◌  ◌  ░▓ │   ', '   │ ▓░   ▔▔   ░▓ │   ', '   │ ▓░  ╲__/  ░▓ │   ', '   │  ░▓▓▓▓▓▓▓▓░  │   ', '   ╰──────────────╯   '],
  ['   ╔══════════════╗   ', '   ║  ██████████  ║   ', '   ║ ██  ◆  ◆  ██ ║   ', '   ║ ██   ▔▔   ██ ║   ', '   ║ ██  ╱__╲  ██ ║   ', '   ║  ██████████  ║   ', '   ╚══════════════╝   '],
  ['   ╔══════════════╗   ', '   ║  ███▓▓▓▓███  ║   ', '   ║ ██  ◈  ◈  ██ ║   ', '   ║ ██   ▂▂   ██ ║   ', '   ║ ██  ╲──╱  ██ ║   ', '   ║  ███▓▓▓▓███  ║   ', '   ╚══════════════╝   '],
];

let launchSeqInterval = null;

export function startLaunchSequence() {
  endLaunchSequence();
  let f = 0;
  if (dom.launchAscii) dom.launchAscii.textContent = LAUNCH_FRAMES[0].join('\n');
  launchSeqInterval = setInterval(() => {
    f = (f + 1) % LAUNCH_FRAMES.length;
    if (dom.launchAscii) dom.launchAscii.textContent = LAUNCH_FRAMES[f].join('\n');
  }, 320);
}

export function endLaunchSequence() {
  if (launchSeqInterval) {
    clearInterval(launchSeqInterval);
    launchSeqInterval = null;
  }
}

const RUNNING_FRAMES = [
  [' /\\_/\\ ', '( o.o )', ' > ^ < '],
  [' /\\_/\\ ', '( o.o )', ' > ^ < '],
  [' /\\_/\\ ', '( -.- )', ' > ^ < '],
  [' /\\_/\\ ', '( o.o )', ' > ^ < '],
  [' /\\_/\\ ', '( ^.^ )', ' > ^ < '],
  [' /\\_/\\ ', '( ^.^ )', ' > ^ < '],
  [' /\\_/\\ ', '( o.o )', ' > ^ < '],
  [' /\\_/\\ ', '( -.o )', ' > ^ < '],
];

let runningAnimInterval = null;

export function startRunningAnimation() {
  stopRunningAnimation();
  if (!dom.runningAscii) return;
  let f = 0;
  dom.runningAscii.textContent = RUNNING_FRAMES[0].join('\n');
  runningAnimInterval = setInterval(() => {
    f = (f + 1) % RUNNING_FRAMES.length;
    dom.runningAscii.textContent = RUNNING_FRAMES[f].join('\n');
  }, 900);
}

export function stopRunningAnimation() {
  if (runningAnimInterval) {
    clearInterval(runningAnimInterval);
    runningAnimInterval = null;
  }
}

let uptimeInterval = null;
let uptimeStart = 0;

export function startUptime(launchedAt) {
  stopUptime();
  uptimeStart = launchedAt ? new Date(launchedAt).getTime() : Date.now();
  const update = () => {
    const elapsed = Math.floor((Date.now() - uptimeStart) / 1000);
    if (dom.runningUptime) dom.runningUptime.textContent = `${Math.floor(elapsed / 60)}:${(elapsed % 60).toString().padStart(2, '0')}`;
  };
  update();
  uptimeInterval = setInterval(update, 1000);
}

export function stopUptime() {
  if (uptimeInterval) {
    clearInterval(uptimeInterval);
    uptimeInterval = null;
  }
}

export function clearLaunchVisualState() {
  endLaunchSequence();
  stopRunningAnimation();
  stopUptime();
  if (dom.runningUptime) dom.runningUptime.textContent = '0:00';
  if (dom.runningPid) dom.runningPid.textContent = '';
  if (dom.runningVersion) dom.runningVersion.textContent = '';
}

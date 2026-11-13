import type { ReleaseChannel } from '../../types-content';

/** A version as a row shows it: the project's own number, and which kind of prerelease it is. */
export interface VersionLabel {
  text: string;
  pre?: string;
}

/** Words that name what a file was built for, not its version. */
const TARGETS = new Set([
  'fabric',
  'quilt',
  'forge',
  'neoforge',
  'neo',
  'mc',
  'minecraft',
  'universal',
  'common',
  'multi',
  'multiloader',
]);
const PRERELEASE = /^(alpha|beta|rc|pre|snapshot)\.?(\d*)$/i;

/** A bare number that is the instance's game version, the line it belongs to, or another patch of that line. */
function namesGame(part: string, game: string): boolean {
  const line = part.replace(/\.x$/i, '');
  if (!/^\d+(\.\d+){0,2}$/.test(line)) return false;
  if (line === game || game.startsWith(`${line}.`)) return true;
  const minor = (version: string): string => version.split('.').slice(0, 2).join('.');
  return line.startsWith('1.') && minor(line) === minor(game);
}

/**
 * Files carry what they were built for in their version ("7.2.7-fabric-26.2", "0.6.13+mc1.21.5"). The instance
 * already says which game and loader it runs, so a row shows the project's own number, and names a prerelease
 * beside it. The provider's channel names one the number does not.
 */
export function versionLabel(raw: string, game: string, channel: ReleaseChannel = 'release'): VersionLabel {
  const full = raw.trim();
  let pre: string | undefined;
  const kept: string[] = [];
  for (const piece of full
    .replace(/^v(?=\d)/i, '')
    .split('+')[0]!
    .split(/[-_\s]+/)) {
    const part = piece.replace(/^\.+|\.+$/g, '');
    const word = PRERELEASE.exec(part);
    if (word) pre ??= word[2] ? `${word[1]!.toLowerCase()} ${word[2]}` : word[1]!.toLowerCase();
    else if (part && !TARGETS.has(part.toLowerCase()) && !/^(mc|minecraft)\.?\d/i.test(part)) kept.push(part);
  }
  // A project may share its number with the game, so the game's goes only when another number stays.
  const own = kept.filter((part) => !namesGame(part, game));
  const text = (own.some((part) => /\d/.test(part)) ? own : kept).join('-') || full;
  return { text, pre: pre ?? (channel === 'release' ? undefined : channel) };
}

export function versionText(label: VersionLabel): string {
  return label.pre ? `${label.text} ${label.pre}` : label.text;
}

/** An update's label. When it would read the same as the installed one, the provider's own wording tells them apart. */
export function updateLabel(
  raw: string,
  game: string,
  channel: ReleaseChannel,
  installed?: VersionLabel,
): VersionLabel {
  const label = versionLabel(raw, game, channel);
  return installed && versionText(installed) === versionText(label) ? { text: raw.trim() } : label;
}

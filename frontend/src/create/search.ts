import type { Channel, LoaderKey } from './defaults';
import { CHANNEL_ORDER, type VersionRowModel } from './versions';

export interface VersionQuery {
  /** The loader the text names; null when it names none. */
  loader: LoaderKey | null;
  version: string;
}

/* Letters always mean the same loader: where two begin alike, the earlier one here owns the shorter spelling. */
const SHORTHAND_ORDER: readonly LoaderKey[] = ['forge', 'fabric', 'neoforge', 'quilt', 'vanilla'];

/**
 * `1.13.1`, `forge 1.13.1`, `1.13.1 forge` and `f1131` all name a version. Letters that begin a loader's name, leading
 * the text or as its last word, name that loader (`f` is Forge, `fa` Fabric); anything else, such as `b1.7.3`, is
 * the version.
 */
export function parseVersionQuery(input: string, offered: readonly LoaderKey[]): VersionQuery {
  const text = input.trim().toLowerCase();
  const named = (letters: string | undefined): LoaderKey | null =>
    (letters && SHORTHAND_ORDER.find((loader) => offered.includes(loader) && loader.startsWith(letters))) || null;
  const leading = /^[a-z]+/.exec(text)?.[0];
  const first = named(leading);
  if (leading && first) return { loader: first, version: text.slice(leading.length).trim() };
  const trailing = /\s([a-z]+)$/.exec(text)?.[1];
  const last = named(trailing);
  if (trailing && last) return { loader: last, version: text.slice(0, -trailing.length).trim() };
  return { loader: null, version: text };
}

const undotted = (value: string): string => value.replace(/[.\s]/g, '');

/** How closely a row answers the typed version: 0 is the version itself; null is no match. */
function closeness(row: VersionRowModel, version: string): number | null {
  if (!version) return 0;
  const id = row.id.toLowerCase();
  const name = row.displayName.toLowerCase();
  if (id === version || name === version) return 0;
  // Dots left out stand for any dots; dots typed mean the ones typed.
  const typed = version.includes('.') ? null : undotted(version);
  if (typed !== null && undotted(id) === typed) return 1;
  if (id.startsWith(version) || name.startsWith(version)) return 2;
  if (typed !== null && undotted(id).startsWith(typed)) return 3;
  const rest = (row.hint ?? '').toLowerCase();
  return id.includes(version) || name.includes(version) || rest.includes(version) ? 4 : null;
}

/** The rows a typed version matches, closest first; ties keep releases ahead and then the order given. */
export function rankVersions(rows: readonly VersionRowModel[], version: string): VersionRowModel[] {
  const channelRank = (channel: Channel): number => CHANNEL_ORDER.indexOf(channel);
  return rows
    .map((row, index) => ({ row, index, rank: closeness(row, version) }))
    .filter((entry): entry is { row: VersionRowModel; index: number; rank: number } => entry.rank !== null)
    .sort((a, b) => a.rank - b.rank || channelRank(a.row.channel) - channelRank(b.row.channel) || a.index - b.index)
    .map((entry) => entry.row);
}

import { computed, effect, signal } from '@preact/signals';
import { contentCompatibility, getModpackTarget, planContent } from '../../content';
import { toast } from '../../toast';
import type { CompatCandidate, ModpackTarget } from '../../types-content';
import { errMessage } from '../../utils';
import { installableSelections } from './install-workflow';
import { gameVersion, loader, selectionOf, tray, unstage, type TrayItem } from './state';

const SETTLE_MS = 120;
const NONE: ReadonlySet<string> = new Set();

/** What picks run on: a loader key (`vanilla` for none) and a Minecraft version. */
export interface Fit {
  loader: string;
  loaderLabel: string;
  gameVersion: string;
  /** The picks with no build for them; `null` until that is checked, and when the check failed. */
  drops: ReadonlySet<string> | null;
}

/** What a check found, or why it found nothing. A failed check is not an empty answer. */
type Answer<T> = { value: T } | { error: string };
const errorOf = (answer: Answer<unknown> | null | undefined): string | null =>
  answer && 'error' in answer ? answer.error : null;

const isPack = (item: TrayItem): boolean => item.kind === 'modpack';
const packKey = (item: TrayItem): string => `${item.canonical_id}\n${item.version_id ?? ''}`;
const extrasKey = (items: TrayItem[]): string =>
  items.map((item) => `${item.canonical_id}\n${item.kind}\n${item.version_id ?? ''}`).join('\n\n');
const question = (target: ModpackTarget, extras: TrayItem[]): string =>
  `${target.loader ?? ''}\n${target.minecraft}\n\n\n${extrasKey(extras)}`;

/** What each staged pack needs, by pack and pinned version. */
const packTargets = signal<ReadonlyMap<string, ModpackTarget>>(new Map());
/** Where picks with no pack can live, with the picks it answers. */
const candidates = signal<{ key: string; answer: Answer<CompatCandidate[]> } | null>(null);
/** The picks a pack's loader and version leave out, by the question asked. */
const leftOut = signal<ReadonlyMap<string, Answer<ReadonlySet<string>>>>(new Map());
/** Bumped to ask the failed checks again. */
const retries = signal(0);

/** Each staged pack's loader and version, asked once with the other picks. */
const questions = computed(() => {
  const items = tray.value;
  const extras = items.filter((item) => !isPack(item));
  const asked = new Map<string, { target: ModpackTarget; extras: TrayItem[] }>();
  if (extras.length === 0) return asked;
  for (const pack of items.filter(isPack)) {
    const target = packTargets.value.get(packKey(pack));
    if (target) asked.set(question(target, extras), { target, extras });
  }
  return asked;
});

/**
 * The staged picks as what they make: each pack an instance of its own with the other picks added where they fit,
 * or with no pack one new instance on the loader and version that keeps the most of them.
 */
export const staging = computed(() => {
  const items = tray.value;
  const extras = items.filter((item) => !isPack(item));
  const packs = items.filter(isPack).map((item) => {
    const target = packTargets.value.get(packKey(item));
    if (!target) return { item, target, fit: null, pending: true, error: null };
    const answer = extras.length === 0 ? { value: NONE } : leftOut.value.get(question(target, extras));
    const fit: Fit = {
      loader: target.loader ?? 'vanilla',
      loaderLabel: target.loader_label,
      gameVersion: target.minecraft,
      drops: answer && 'value' in answer ? answer.value : null,
    };
    return { item, target, fit, pending: !answer, error: errorOf(answer) };
  });
  // Picks with no pack: `null` when that is not what is staged, nothing yet while it is being asked.
  const held = candidates.value;
  const shared =
    packs.length > 0 || extras.length === 0 ? null : held?.key === extrasKey(extras) ? held.answer : undefined;
  const list = shared && 'value' in shared ? shared.value : null;
  const best = list?.[0];
  const fit: Fit | null =
    packs.length === 1
      ? packs[0]!.fit
      : best
        ? {
            loader: best.loader,
            loaderLabel: best.loader_label,
            gameVersion: best.game_version,
            drops: new Set(best.drops.map((drop) => drop.canonical_id)),
          }
        : null;
  return {
    packs,
    extras,
    /** One loader and version for everything staged; none while unknown, or when several packs each bring theirs. */
    fit,
    pending: shared === undefined || packs.some((pack) => pack.pending),
    /** The picks share no loader and version at all. */
    apart: list?.length === 0,
    /** Why what fits is not known: a check failed, and `recheck` asks again. */
    error: errorOf(shared) ?? packs.find((pack) => pack.error)?.error ?? null,
  };
});

/** Ask again for the checks that failed. */
export function recheck(): void {
  if (errorOf(candidates.peek()?.answer)) candidates.value = null;
  leftOut.value = new Map([...leftOut.peek()].filter(([, answer]) => !errorOf(answer)));
  retries.value += 1;
}

/** The picks with no build for a pack's loader and version, or that need one of those. */
async function dropsAt(target: ModpackTarget, extras: TrayItem[]): Promise<ReadonlySet<string>> {
  // A pack with no loader takes no mods; planned together, they would refuse the rest with them.
  const loaderless = target.loader ? [] : extras.filter((item) => item.kind === 'mod');
  const asked = extras.filter((item) => !loaderless.includes(item)).map(selectionOf);
  const drops = new Set(loaderless.map((item) => item.canonical_id));
  if (asked.length === 0) return drops;
  const plan = await planContent(
    { kind: 'draft', ...(target.loader ? { loader: target.loader } : {}), game_version: target.minecraft },
    asked,
  );
  if (!plan.conflicts.some((conflict) => conflict.kind === 'unavailable')) return drops;
  const kept = new Set(installableSelections(asked, plan).map((selection) => selection.canonical_id));
  for (const selection of asked) if (!kept.has(selection.canonical_id)) drops.add(selection.canonical_id);
  return drops;
}

const reading = new Set<string>();

// A staged pack is read once for what it needs; one that cannot be read is not a pick.
effect(() => {
  const packs = tray.value.filter(isPack);
  const staged = new Set(packs.map(packKey));
  const known = packTargets.peek();
  if ([...known.keys()].some((held) => !staged.has(held))) {
    packTargets.value = new Map([...known].filter(([held]) => staged.has(held)));
  }
  for (const pack of packs) {
    const held = packKey(pack);
    if (packTargets.peek().has(held) || reading.has(held)) continue;
    reading.add(held);
    const stillStaged = (): boolean => tray.peek().some((item) => isPack(item) && packKey(item) === held);
    getModpackTarget(pack.canonical_id, pack.version_id, { gameVersion: gameVersion.peek(), loader: loader.peek() })
      .then((target) => {
        if (stillStaged()) packTargets.value = new Map(packTargets.peek()).set(held, target);
      })
      .catch((error: unknown) => {
        if (!stillStaged()) return;
        toast(`${pack.title} could not be read: ${errMessage(error)}`, 'error');
        unstage(pack.canonical_id);
      })
      .finally(() => reading.delete(held));
  }
});

let settle: ReturnType<typeof setTimeout> | undefined;
const asking = new Set<string>();

// Once the picks settle they are checked against where they would go: each staged pack's loader and version, or
// with no pack every pair they could share. An answer is kept only while its question still stands.
effect(() => {
  retries.value;
  const items = tray.value;
  const asked = questions.value;
  const extras = items.filter((item) => !isPack(item));
  const packless = extras.length === items.length;
  const key = extrasKey(extras);
  clearTimeout(settle);
  const known = leftOut.peek();
  if ([...known.keys()].some((held) => !asked.has(held))) {
    leftOut.value = new Map([...known].filter(([held]) => asked.has(held)));
  }
  if (extras.length === 0) return;
  settle = setTimeout(() => {
    if (packless && candidates.peek()?.key !== key) {
      const current = (): boolean => extrasKey(tray.peek()) === key;
      contentCompatibility(extras.map(selectionOf))
        .then((response) => {
          if (current()) candidates.value = { key, answer: { value: response.candidates } };
        })
        .catch((error: unknown) => {
          if (current()) candidates.value = { key, answer: { error: errMessage(error) } };
        });
    }
    for (const [held, { target, extras: picks }] of asked) {
      if (leftOut.peek().has(held) || asking.has(held)) continue;
      asking.add(held);
      const publish = (answer: Answer<ReadonlySet<string>>): void => {
        if (questions.peek().has(held)) leftOut.value = new Map(leftOut.peek()).set(held, answer);
      };
      dropsAt(target, picks)
        .then(
          (value) => publish({ value }),
          (error: unknown) => publish({ error: errMessage(error) }),
        )
        .finally(() => asking.delete(held));
    }
  }, SETTLE_MS);
});

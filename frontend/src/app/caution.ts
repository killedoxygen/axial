import { canEditPreferences, local, saveLocalState } from './local-state';
import { toast } from './toast';

/** The parts of the app a caution can belong to, as the user knows them. */
const AREAS = {
  create: 'New instance',
} as const;
export type CautionArea = keyof typeof AREAS;

/**
 * A moment where what the user did is probably not what they meant. Declared once beside the action it protects;
 * `applies` holds the heuristic and is asked only when the action is taken, so a caution costs nothing until then.
 */
export interface Caution<Context> {
  /** Stable and unique: it is what the user's "don't warn me" is remembered by. */
  id: string;
  area: CautionArea;
  applies: (context: Context) => boolean;
  /** What was done instead. */
  held: (context: Context) => string;
  /** What was held back, and why. */
  why: string;
  /** The action that goes ahead after all. */
  proceed: string;
}

const declared = new Map<CautionArea, Set<string>>();

export function caution<Context = void>(definition: Caution<Context>): Caution<Context> {
  const ids = declared.get(definition.area) ?? new Set<string>();
  if (ids.has(definition.id)) throw new Error(`Caution ${definition.id} is declared twice.`);
  declared.set(definition.area, ids.add(definition.id));
  return definition;
}

/**
 * True when the action should go ahead now. Otherwise it is held back without interrupting: the caller does the
 * safe thing, and a notice says so and offers `proceed`. Quieting an area covers the cautions it has today, by
 * id: one added later is still shown.
 */
export function heed<Context>(item: Caution<Context>, context: Context, proceed: () => void): boolean {
  if (local.quietCautions.includes(item.id) || !item.applies(context)) return true;
  toast(item.held(context), 'info', {
    detail: item.why,
    duration: 12_000,
    actions: [
      { label: item.proceed, run: proceed, primary: true },
      {
        label: "Don't warn again",
        title: `Stops warnings like this in ${AREAS[item.area]}`,
        run: () => {
          if (!canEditPreferences()) return;
          local.quietCautions = [...new Set([...local.quietCautions, ...(declared.get(item.area) ?? [])])].sort();
          saveLocalState();
        },
      },
    ],
  });
  return false;
}

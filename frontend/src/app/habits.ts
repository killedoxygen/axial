import { canEditPreferences, local, saveLocalState } from './local-state';

/**
 * What the user has done, counted. It tells a first-timer from a regular, so a tip is shown to those it helps and
 * a caution to those it protects. A new key needs no registration here; name it `area.what`.
 */
export type Habit = 'create.by-hand' | 'create.quick' | 'tip.quick-create';

export function habit(key: Habit): number {
  return local.habits[key] ?? 0;
}

export function noteHabit(key: Habit): void {
  if (!canEditPreferences()) return;
  local.habits = { ...local.habits, [key]: habit(key) + 1 };
  saveLocalState();
}

/** True the first time only: for what is said once, such as a tip. */
export function firstTime(key: Habit): boolean {
  if (habit(key) > 0 || !canEditPreferences()) return false;
  noteHabit(key);
  return true;
}

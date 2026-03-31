export const $ = <T extends Element = Element>(sel: string): T | null => document.querySelector<T>(sel);
export const $$ = <T extends Element = Element>(sel: string): NodeListOf<T> => document.querySelectorAll<T>(sel);

export function byId<T extends HTMLElement = HTMLElement>(id: string): T | null {
  return document.getElementById(id) as T | null;
}

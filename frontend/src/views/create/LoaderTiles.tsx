import type { JSX } from 'preact';
import { LOADER_LABELS, LOADER_TAGLINES } from './defaults';
import { LoaderLogo } from './loader-logos';
import { loaderKeyFromSourceId, type CreateForm } from './use-create-form';

/** One logo tile per loader, the chosen one named once beside them. A modpack fixes the loader, so its tiles stand down. */
export function LoaderTiles({ form }: { form: CreateForm }): JSX.Element {
  const { pack, source } = form;
  const options =
    source.options.length > 0
      ? source.options
      : [{ id: source.id, label: source.label, enabled: true, disabled_reason: null }];
  return (
    <div class="cp-cr-loaders" role="radiogroup" aria-label="Loader" aria-disabled={pack ? true : undefined}>
      {options.map((option) => {
        const key = loaderKeyFromSourceId(option.id);
        const unsupported = source.unsupported(key);
        const enabled = !pack && option.enabled && !unsupported;
        const checked = pack ? key === form.identityKey : source.id === option.id;
        return (
          <button
            key={option.id}
            type="button"
            class="cp-cr-loader"
            role="radio"
            aria-checked={checked}
            aria-label={option.label || LOADER_LABELS[key]}
            title={
              unsupported
                ? 'Your staged picks have no builds for this loader'
                : (option.disabled_reason ?? LOADER_TAGLINES[key])
            }
            disabled={!enabled}
            onClick={() => source.choose(option.id)}
          >
            <LoaderLogo loader={key} size={18} class="cp-cr-loader-mark" />
          </button>
        );
      })}
      <b>{source.label}</b>
    </div>
  );
}

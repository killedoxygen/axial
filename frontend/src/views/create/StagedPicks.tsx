import type { JSX } from 'preact';
import { Chip } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import type { CreateDraftItem } from '../../ui-state';
import { KIND_ICON } from '../discover/shared';
import type { CreateForm } from './use-create-form';

/** What Discover staged to install after create, and which of it this version skips. */
export function StagedPicks({
  draft,
  staged,
}: {
  draft: CreateDraftItem[];
  staged: CreateForm['staged'];
}): JSX.Element {
  return (
    <div class="cp-chips" role="group" aria-label="Staged content">
      {draft.map((item) => {
        const dropped = staged.dropped.has(item.canonical_id);
        return (
          <span
            key={item.canonical_id}
            class="cp-chip cp-cr-pick"
            data-dropped={dropped || undefined}
            title={dropped ? `${item.title} has no build for this version and will be skipped` : item.title}
          >
            {item.icon_url ? <img src={item.icon_url} alt="" /> : <Icon name={KIND_ICON[item.kind]} size={11} />}
            {item.title}
            {item.version_label && <span class="cp-cr-pick-pin">{item.version_label}</span>}
          </span>
        );
      })}
      {staged.dropped.size > 0 && (
        <Chip tone="warn" icon="alert" title={staged.drops.map((drop) => drop.title).join(', ')}>
          {staged.dropped.size} skipped
        </Chip>
      )}
    </div>
  );
}

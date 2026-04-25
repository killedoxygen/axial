import type { JSX } from 'preact';
import { Input } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { AccentField } from '../settings/AccentEditor';
import type { LoaderBuildRecord } from '../../types';
import {
  INSTANCE_ICON_CHOICES,
  LOADER_LABELS,
  type LoaderKey,
} from './defaults';
import { Words } from './shared';

export function IdentityStage({
  source,
  mcVersionId,
  name,
  suggestedName,
  onNameChange,
  icon,
  onIconPick,
  alreadyInstalled,
  selectedBuild,
}: {
  source: LoaderKey;
  mcVersionId: string;
  name: string;
  suggestedName: string;
  onNameChange: (value: string) => void;
  icon: string;
  onIconPick: (name: string) => void;
  alreadyInstalled: boolean;
  selectedBuild: LoaderBuildRecord | null;
}): JSX.Element {
  const summary = source === 'vanilla'
    ? `Vanilla · ${mcVersionId}`
    : selectedBuild
      ? `${LOADER_LABELS[source]} ${selectedBuild.loader_version} · ${mcVersionId}`
      : `${LOADER_LABELS[source]} · ${mcVersionId}`;

  return (
    <>
      <header class="cp-cr-head">
        <h1 class="cp-cr-headline"><Words text="Name it." /></h1>
        <p class="cp-cr-subline">
          {summary}{alreadyInstalled ? '' : ' · downloads after create'}
        </p>
      </header>

      <div class="cp-cr-id">
        <div class="cp-cr-id-card">
          <div class="cp-cr-id-preview" data-icon={icon}>
            <span class="cp-cr-id-preview-glyph">
              <Icon name={icon} size={28} stroke={1.6} />
            </span>
            <span class="cp-cr-id-preview-name">{name.trim() || suggestedName || 'Untitled'}</span>
          </div>

          <div class="cp-cr-id-row">
            <label class="cp-cr-id-label">Name</label>
            <Input
              value={name}
              onChange={onNameChange}
              placeholder={suggestedName || 'Aurora Adventure'}
              autoFocus
            />
          </div>

          <div class="cp-cr-id-row">
            <label class="cp-cr-id-label">Icon</label>
            <div class="cp-cr-iconrow" role="radiogroup" aria-label="Instance icon">
              {INSTANCE_ICON_CHOICES.map((choice, index) => (
                <button
                  key={choice}
                  type="button"
                  class="cp-cr-iconbtn"
                  data-active={icon === choice}
                  aria-label={choice}
                  aria-checked={icon === choice}
                  role="radio"
                  style={{ ['--i' as any]: String(index) }}
                  onClick={() => onIconPick(choice)}
                >
                  <Icon name={choice} size={16} />
                </button>
              ))}
            </div>
          </div>

          <div class="cp-cr-id-row">
            <label class="cp-cr-id-label">Accent</label>
            <AccentField showPresets={true} />
          </div>
        </div>
      </div>
    </>
  );
}

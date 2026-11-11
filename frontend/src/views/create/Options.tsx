import type { JSX } from 'preact';
import { Chip, Toggle } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { Segmented } from '../../ui/Segmented';
import { SelectField } from '../../ui/Select';
import { Slider } from '../../ui/Slider';
import { fmtMem } from '../../format';
import type { CreateForm } from './use-create-form';

function memoryAdvice(memoryGB: number, totalGB: number, recommended: number): string {
  if (memoryGB < 2) return 'Low. May stutter.';
  if (memoryGB > totalGB * 0.75) return 'High. Leave room for the OS.';
  return `Comfortable start: ${recommended} GB.`;
}

/** Closed, the disclosure reads out the current values; open, each control sits on the form's control line. */
export function Options({
  options,
  open,
  onOpenChange,
}: {
  options: CreateForm['options'];
  open: boolean;
  onOpenChange: (open: boolean) => void;
}): JSX.Element {
  const { windowPreset, presetOption, optimizeOption } = options;
  return (
    <>
      <button type="button" class="cp-cr-options" aria-expanded={open} onClick={() => onOpenChange(!open)}>
        <span class="cp-cr-key">
          Options
          <Icon name="chevron-right" size={13} stroke={2} />
        </span>
        {!open && (
          <span class="cp-chips">
            <Chip tone="bare">{fmtMem(options.memoryGB)}</Chip>
            <Chip tone="bare">{windowPreset.label}</Chip>
            {presetOption && <Chip tone="bare">{presetOption.label}</Chip>}
          </span>
        )}
      </button>
      {open && (
        <>
          <span class="cp-cr-key">Memory</span>
          <div class="cp-cr-memory">
            <Slider
              value={options.memoryGB}
              min={1}
              max={options.totalGB}
              step={0.5}
              recommended={[Math.max(2, options.memoryRec - 2), Math.min(options.totalGB, options.memoryRec + 2)]}
              sound="memory"
              onChange={options.setMemoryGB}
              ariaLabel="Max memory in gigabytes"
            />
            <span
              class="cp-cr-memory-reading"
              aria-live="polite"
              title={memoryAdvice(options.memoryGB, options.totalGB, options.memoryRec)}
            >
              {fmtMem(options.memoryGB)}
            </span>
          </div>

          <span class="cp-cr-key">Window</span>
          <div class="cp-cr-window">
            <Segmented<string>
              ariaLabel="Window size"
              value={windowPreset.id}
              onChange={options.setWindowPresetId}
              options={[...options.windowPresets]
                .sort((a, b) => Number(b.id === 'default') - Number(a.id === 'default'))
                .map((preset) => ({ value: preset.id, label: preset.label }))}
            />
            <Chip mono>{windowPreset.id === 'default' ? 'Game default' : `${windowPreset.w} × ${windowPreset.h}`}</Chip>
          </div>

          <span class="cp-cr-key">Profile</span>
          <div title={presetOption?.disabled_reason ?? presetOption?.detail ?? undefined}>
            <SelectField
              value={options.jvmPreset}
              onChange={options.setJvmPreset}
              options={options.presetOptions.map((option) => ({
                value: option.id,
                label: option.label,
                disabled: Boolean(option.disabled_reason),
              }))}
              ariaLabel="Performance profile"
              placeholder="Loading"
              disabled={options.presetOptions.every((option) => option.disabled_reason)}
            />
          </div>

          {optimizeOption && (
            <>
              <span class="cp-cr-key" title={optimizeOption.detail}>
                {optimizeOption.label}
              </span>
              <div class="cp-cr-switch">
                <Toggle on={options.autoOptimize} onChange={options.toggleOptimize} ariaLabel={optimizeOption.label} />
              </div>
            </>
          )}
        </>
      )}
    </>
  );
}

import type { GuardianMode } from './types-guardian';

interface GuardianModeOption {
  value: GuardianMode;
  label: string;
  note: string;
}

export const GUARDIAN_OPTIONS: GuardianModeOption[] = [
  { value: 'managed', label: 'Managed', note: 'Catches risky launch settings and fixes them automatically.' },
  {
    value: 'custom',
    label: 'Custom',
    note: 'Keeps your choices, warns instead of changing, blocks only fatal setups.',
  },
  {
    value: 'disabled',
    label: 'Disabled',
    note: 'Records launch safety observations without changing your configuration.',
  },
];

export function guardianModeFrom(value: string | undefined): GuardianMode {
  if (value === 'custom' || value === 'disabled') return value;
  return 'managed';
}

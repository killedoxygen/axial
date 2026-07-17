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
];

export function guardianModeFrom(value: string | undefined): GuardianMode {
  return value === 'custom' ? 'custom' : 'managed';
}

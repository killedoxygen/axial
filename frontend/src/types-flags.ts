export type { FlagStage } from './generated/FlagStage';
export type { FlagSource } from './generated/FlagSource';
export type { FlagViewModel as FeatureFlagViewModel } from './generated/FlagViewModel';
export type { FlagsResponse } from './generated/FlagsResponse';

export interface FeatureFlagsLoadState {
  status: 'idle' | 'loading' | 'ready' | 'error';
  error: string | null;
}

export type KnownFlagKey = 'dev.state-inspector';

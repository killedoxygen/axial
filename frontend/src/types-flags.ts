export type FlagStage = 'experimental' | 'beta';
export type FlagSource = 'default' | 'override' | 'remote';

export interface FeatureFlagViewModel {
  key: string;
  title: string;
  description: string;
  stage: FlagStage;
  dev_only: boolean;
  default_enabled: boolean;
  enabled: boolean;
  source: FlagSource;
}

export interface FlagsResponse {
  flags: FeatureFlagViewModel[];
}

export type KnownFlagKey = 'dev.state-inspector';

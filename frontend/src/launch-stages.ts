export type LaunchStage =
  | 'queued'
  | 'planning'
  | 'validating'
  | 'ensuring_runtime'
  | 'downloading_runtime'
  | 'preparing'
  | 'prewarming'
  | 'starting'
  | 'monitoring'
  | 'running'
  | 'degraded'
  | 'failed'
  | 'exited';

export interface LaunchStageView {
  stage: LaunchStage;
  label: string;
  pct: number;
}

const LAUNCH_STAGE_VIEWS: Record<LaunchStage, LaunchStageView> = {
  queued: { stage: 'queued', label: 'Preparing launch', pct: 8 },
  planning: { stage: 'planning', label: 'Resolving launch plan', pct: 18 },
  validating: { stage: 'validating', label: 'Checking compatibility', pct: 24 },
  ensuring_runtime: { stage: 'ensuring_runtime', label: 'Ensuring runtime', pct: 34 },
  downloading_runtime: { stage: 'downloading_runtime', label: 'Downloading runtime', pct: 42 },
  preparing: { stage: 'preparing', label: 'Preparing launch files', pct: 56 },
  prewarming: { stage: 'prewarming', label: 'Prewarming game data', pct: 64 },
  starting: { stage: 'starting', label: 'Starting Minecraft', pct: 72 },
  monitoring: { stage: 'monitoring', label: 'Stabilizing startup', pct: 88 },
  running: { stage: 'running', label: 'Playing', pct: 100 },
  degraded: { stage: 'degraded', label: 'Running with warnings', pct: 100 },
  failed: { stage: 'failed', label: 'Launch failed', pct: 100 },
  exited: { stage: 'exited', label: 'Exited', pct: 100 },
};

export function launchStageFrom(value: string | null | undefined): LaunchStage | null {
  if (!value) return null;
  if (value in LAUNCH_STAGE_VIEWS) return value as LaunchStage;
  return null;
}

export function launchStageView(stage: LaunchStage): LaunchStageView {
  return LAUNCH_STAGE_VIEWS[stage];
}

export function launchStageViewFrom(value: string | null | undefined): LaunchStageView | null {
  const stage = launchStageFrom(value);
  return stage ? launchStageView(stage) : null;
}

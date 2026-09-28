export type UpdateKind = 'none' | 'release-page' | 'release-asset';
export type UpdateInstallMode = 'in-app' | 'external';

export interface UpdateInfo {
  current_version: string;
  latest_version: string;
  available: boolean;
  platform: string;
  arch: string;
  kind: UpdateKind;
  install_mode: UpdateInstallMode;
  notes_url: string;
  action_url: string;
  checksum_url?: string | null;
  action_label: string;
  checked_at: string;
}

export type UpdateFlowPhase = 'idle' | 'downloading' | 'ready' | 'applying' | 'restart-pending' | 'failed';

export interface UpdateFlowState {
  phase: UpdateFlowPhase;
  version: string;
  received_bytes: number;
  total_bytes: number | null;
  percent: number | null;
  message: string;
}

export const idleUpdateFlow: UpdateFlowState = {
  phase: 'idle',
  version: '',
  received_bytes: 0,
  total_bytes: null,
  percent: null,
  message: '',
};

import type { UpdateFlow } from './generated/UpdateFlow';

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

export type UpdateFlowPhase = UpdateFlow['phase'];

export type UpdateFlowState = Pick<
  UpdateFlow,
  | 'revision'
  | 'phase'
  | 'version'
  | 'received_bytes'
  | 'total_bytes'
  | 'percent'
  | 'message'
  | 'can_download'
  | 'can_restart'
>;

export const idleUpdateFlow: UpdateFlowState = {
  revision: 0,
  phase: 'idle',
  version: '',
  received_bytes: 0,
  total_bytes: null,
  percent: null,
  message: '',
  can_download: false,
  can_restart: false,
};

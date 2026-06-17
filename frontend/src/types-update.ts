export type UpdateKind = 'none' | 'release-page' | 'release-asset';

export interface UpdateInfo {
  current_version: string;
  latest_version: string;
  available: boolean;
  platform: string;
  arch: string;
  kind: UpdateKind;
  notes_url: string;
  action_url: string;
  checksum_url?: string | null;
  action_label: string;
  checked_at: string;
}

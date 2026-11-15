import type { ConfigView } from '../src/generated/ConfigView';
import type { EnrichedInstance } from '../src/generated/EnrichedInstance';
import type { InstallQueueStateResponse } from '../src/generated/InstallQueueStateResponse';

/** Complete wire values, as the backend serializes them, for tests to override field by field. */

export function enrichedInstance(overrides: Partial<EnrichedInstance> = {}): EnrichedInstance {
  return {
    id: 'fixture-instance',
    name: 'Fixture',
    version_id: '1.21.4',
    created_at: '2026-09-27T00:00:00Z',
    last_played_at: '',
    art_seed: 1,
    java_selection: { kind: 'inherited' },
    icon: '',
    accent: '',
    loader_key: 'vanilla',
    minecraft_version: '1.21.4',
    revision: 1,
    max_memory_mb: 0,
    min_memory_mb: 0,
    java_path: '',
    window_width: 0,
    window_height: 0,
    jvm_preset: '',
    performance_mode: '',
    extra_jvm_args: '',
    auto_optimize: false,
    version_display: {
      loader_key: 'vanilla',
      loader_label: 'Vanilla',
      minecraft_label: '1.21.4',
      loader_version_label: '',
      loader_detail_label: '',
      summary_label: '1.21.4',
      supports_mods: false,
    },
    launchable: true,
    launch_action: { state_id: 'ready', label: 'Launch', tone: 'ok', launchable: true, primary_action: 'launch' },
    status_detail: '',
    needs_install: '',
    install_target: null,
    java_major: 21,
    saves_count: 0,
    mods_count: 0,
    resource_count: 0,
    shader_count: 0,
    counts_available: false,
    ...overrides,
  };
}

export function queueSnapshot(overrides: Partial<InstallQueueStateResponse> = {}): InstallQueueStateResponse {
  return {
    queue_epoch: 'fixture-queue',
    revision: 1,
    registry_revision: 0,
    items: [],
    view_model: {
      state_id: 'idle',
      status_label: 'Idle',
      title: 'Downloads',
      summary: 'Ready',
      queued_count: 0,
      queued_count_label: 'No queued downloads',
      queued_item_label: 'No items queued',
      section_title: 'Queue',
      empty_title: 'Nothing downloading',
      empty_summary: 'Ready',
    },
    ...overrides,
  };
}

export function configView(overrides: Partial<ConfigView> = {}): ConfigView {
  return {
    revision: 1,
    account_selection_revision: 1,
    username: 'Player',
    launch_auth_mode: 'offline',
    max_memory_mb: 4096,
    min_memory_mb: 512,
    java_path_override: '',
    window_width: 0,
    window_height: 0,
    onboarding_done: true,
    jvm_preset: '',
    performance_mode: 'managed',
    theme: '',
    custom_hue: null,
    custom_vibrancy: null,
    lightness: null,
    telemetry_enabled: false,
    discord_rpc_enabled: false,
    discord_rpc_onboarding_seen: true,
    music_enabled: null,
    music_volume: null,
    music_track: 0,
    ...overrides,
  };
}

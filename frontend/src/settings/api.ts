import { request } from '../platform/transport';
import type { ApplicationStatus } from '../generated/ApplicationStatus';
import type { ConfigPatch } from '../generated/ConfigPatch';
import type { ConfigView } from '../generated/ConfigView';
import type { FlagsResponse } from '../generated/FlagsResponse';
import type { InterfacePreferencesReceipt } from '../generated/InterfacePreferencesReceipt';
import type { InterfacePreferencesSnapshot } from '../generated/InterfacePreferencesSnapshot';
import type { InterfacePreferencesUpdate } from '../generated/InterfacePreferencesUpdate';
import type { SetupStatusResponse } from '../generated/SetupStatusResponse';
import type { StatusResponse } from '../generated/StatusResponse';
import type { SystemResourceResponse } from '../generated/SystemResourceResponse';

export function getConfig(): Promise<ConfigView> {
  return request('GET', '/config');
}

export function updateConfig(patch: ConfigPatch): Promise<ConfigView> {
  return request('PUT', '/config', patch);
}

export function completeOnboarding(expectedRevision: number): Promise<StatusResponse> {
  return request('POST', '/onboarding/complete', { expected_revision: expectedRevision });
}

const preferences = '/config/interface-preferences';

export function getInterfacePreferences(): Promise<InterfacePreferencesSnapshot> {
  return request('GET', preferences);
}

export function updateInterfacePreferences(update: InterfacePreferencesUpdate): Promise<InterfacePreferencesReceipt> {
  return request('PUT', preferences, update);
}

export function getFlags(): Promise<FlagsResponse> {
  return request('GET', '/flags');
}

/** `enabled: null` removes the override. */
export function setFlag(key: string, enabled: boolean | null, expectedRevision: number): Promise<FlagsResponse> {
  return request('PUT', `/flags/${encodeURIComponent(key)}`, { enabled, expected_revision: expectedRevision });
}

export function applicationStatus(): Promise<ApplicationStatus> {
  return request('GET', '/status');
}

/** Creates the managed library on first run. */
export function initializeSetup(): Promise<SetupStatusResponse> {
  return request('POST', '/setup/init');
}

export function systemResources(): Promise<SystemResourceResponse> {
  return request('GET', '/system');
}

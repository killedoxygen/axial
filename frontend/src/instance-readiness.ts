import { api } from './api';
import { enrichedInstanceResponse, instancesResponse } from './dto-core';
import { config, instances, launchSessions } from './store';
import { showError } from './utils';

const readinessReads = new Map<string, symbol>();

export async function refreshInstanceReadiness(instanceId?: string): Promise<void> {
  const token = Symbol();
  const expectedConfig = config.value;
  const targets = new Map(instances.value
    .filter((instance) => instanceId === undefined || instance.id === instanceId)
    .map((instance) => [instance.id, {
      instance,
      sessionId: launchSessions.value[instance.id]?.sessionId,
    }]));
  for (const id of targets.keys()) readinessReads.set(id, token);

  const isCurrent = (id: string): boolean => {
    const target = targets.get(id);
    return target !== undefined && readinessReads.get(id) === token && config.value === expectedConfig &&
      instances.value.find((instance) => instance.id === id) === target.instance &&
      launchSessions.value[id]?.sessionId === target.sessionId;
  };

  try {
    // Terminal publication can briefly precede release of the launch reservation.
    // Re-read once; only the backend decides whether the instance is available.
    for (let attempt = 0; attempt < 2; attempt += 1) {
      if (![...targets.keys()].some(isCurrent)) return;
      try {
        const refreshed = instanceId === undefined
          ? instancesResponse(await api('GET', '/instances')).instances
          : [enrichedInstanceResponse(await api('GET', `/instances/${encodeURIComponent(instanceId)}`))];
        if (instanceId !== undefined && refreshed[0].id !== instanceId) {
          throw new Error('Instance readiness response did not match the requested instance.');
        }
        const updates = new Map(refreshed.filter((instance) => isCurrent(instance.id))
          .map((instance) => [instance.id, instance]));
        if (!updates.size) return;
        instances.value = instances.value.map((instance) => updates.get(instance.id) ?? instance);
        for (const [id, instance] of updates) targets.get(id)!.instance = instance;
        if ([...updates.values()].every((instance) => instance.launch_action.launchable)) return;
      } catch {
        if (![...targets.keys()].some(isCurrent)) return;
        if (attempt === 1) {
          showError('Could not refresh launch availability. Refresh the launcher to check again.');
          return;
        }
      }
      if (attempt === 0) await new Promise<void>((resolve) => window.setTimeout(resolve, 250));
    }
  } finally {
    for (const id of targets.keys()) {
      if (readinessReads.get(id) === token) readinessReads.delete(id);
    }
  }
}

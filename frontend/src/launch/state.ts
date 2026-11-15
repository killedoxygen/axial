import { batch, signal } from '@preact/signals';
import type { SessionNotice } from '../generated/SessionNotice';
import type { SessionOutcome } from '../generated/SessionOutcome';
import type { SessionSnapshot } from '../generated/SessionSnapshot';
import type { SessionViewModel } from '../generated/SessionViewModel';

/** A game this window started or adopted, and the latest status it has applied. */
export interface LaunchSession {
  sessionId: string;
  intentKey?: string;
  stopping?: boolean;
  launchedAt: string;
  viewModel: SessionViewModel;
  statusRevision: number;
}

/** Launch settings edited on the instance page and not yet saved. */
export interface InstanceLaunchDraft {
  javaPath: string;
  jvmPreset: string;
  extraJvmArgs: string;
  dirty: boolean;
}

export interface LaunchStatusUpdate {
  revision: number;
  /** The launched incarnation the snapshot describes. */
  launchedAt: string;
  viewModel: SessionViewModel;
  notice: SessionNotice | null;
  outcome: SessionOutcome | null;
}

/**
 * The status a snapshot carries, when it is this session's own and consistent: a terminal status always has its
 * outcome, and the revision orders exactly in JavaScript.
 */
export function launchStatusUpdate(snapshot: SessionSnapshot, sessionId: string): LaunchStatusUpdate | null {
  if (snapshot.session_id !== sessionId) return null;
  if (snapshot.view_model.terminal !== (snapshot.outcome !== null)) return null;
  if (!Number.isSafeInteger(snapshot.revision) || snapshot.revision < 0) return null;
  return {
    revision: snapshot.revision,
    launchedAt: snapshot.launched_at,
    viewModel: snapshot.view_model,
    notice: snapshot.notice,
    outcome: snapshot.outcome,
  };
}

/** Process controls for every live session, keyed by instance, after a reload. */
export function liveSessions(snapshots: SessionSnapshot[]): Record<string, LaunchSession> {
  const sessions: Record<string, LaunchSession> = {};
  for (const snapshot of snapshots) {
    const status = launchStatusUpdate(snapshot, snapshot.session_id);
    if (!status) throw new Error('Launch session snapshot was invalid.');
    if (status.viewModel.terminal) continue;
    if (Object.prototype.hasOwnProperty.call(sessions, snapshot.instance_id)) {
      throw new Error('Launch session snapshot repeated an active instance.');
    }
    sessions[snapshot.instance_id] = {
      sessionId: snapshot.session_id,
      launchedAt: snapshot.launched_at,
      viewModel: snapshot.view_model,
      statusRevision: snapshot.revision,
    };
  }
  return sessions;
}

export type LaunchState =
  | { status: 'idle' }
  | {
      status: 'preparing';
      instanceId: string;
      pct: number;
      label: string;
      stage?: string;
      determinate?: boolean;
    };

export const launchState = signal<LaunchState>({ status: 'idle' });
export const launchSessions = signal<Record<string, LaunchSession>>({});
export const instanceLaunchDrafts = signal<Record<string, InstanceLaunchDraft>>({});
export const launchNotices = signal<Record<string, SessionNotice>>({});

export function startLaunch(instanceId: string): void {
  launchState.value = { status: 'preparing', instanceId, pct: 0, label: 'Starting launch', determinate: false };
}

export function updateLaunchPrep(instanceId: string, pct: number, label: string, stage?: string): void {
  const current = launchState.value;
  if (current.status !== 'preparing' || current.instanceId !== instanceId) return;
  const nextPct = Math.max(0, Math.min(100, pct));
  launchState.value = {
    status: 'preparing',
    instanceId,
    pct: Math.max(current.pct, nextPct),
    label,
    stage: stage ?? current.stage,
    determinate: current.determinate === true || nextPct > 0,
  };
}

export function updateLaunchPrepView(instanceId: string, viewModel: SessionViewModel): void {
  const current = launchState.value;
  if (current.status !== 'preparing' || current.instanceId !== instanceId) return;
  const pct = Number.isFinite(viewModel.progress_pct) ? viewModel.progress_pct : current.pct;
  const label = viewModel.label.trim() || current.label;
  launchState.value = {
    status: 'preparing',
    instanceId,
    pct: Math.max(current.pct, Math.max(0, Math.min(100, pct))),
    label,
    stage: viewModel.state_id || current.stage,
    determinate: true,
  };
}

export function confirmLaunch(instanceId: string, session: LaunchSession): void {
  batch(() => {
    launchState.value = { status: 'idle' };
    launchSessions.value = { ...launchSessions.value, [instanceId]: session };
  });
}

export function endLaunchPrep(): void {
  launchState.value = { status: 'idle' };
}

export function endSession(instanceId: string): void {
  const next = { ...launchSessions.value };
  delete next[instanceId];
  launchSessions.value = next;
}

export function endSessionIfCurrent(instanceId: string, sessionId: string): boolean {
  if (launchSessions.value[instanceId]?.sessionId !== sessionId) return false;
  endSession(instanceId);
  return true;
}

export function updateLaunchSessionState(instanceId: string, patch: Partial<LaunchSession>): void {
  const current = launchSessions.value[instanceId];
  if (!current) return;
  launchSessions.value = {
    ...launchSessions.value,
    [instanceId]: { ...current, ...patch },
  };
}

function applyLaunchStatusUpdate(instanceId: string, sessionId: string, update: LaunchStatusUpdate): boolean {
  const current = launchSessions.value[instanceId];
  if (
    !current ||
    current.sessionId !== sessionId ||
    current.launchedAt !== update.launchedAt ||
    update.revision <= current.statusRevision
  )
    return false;
  launchSessions.value = {
    ...launchSessions.value,
    [instanceId]: {
      ...current,
      viewModel: update.viewModel,
      statusRevision: update.revision,
    },
  };
  return true;
}

export function convergeLaunchStatus(
  instanceId: string,
  sessionId: string,
  value: SessionSnapshot,
): LaunchStatusUpdate | null {
  const update = launchStatusUpdate(value, sessionId);
  if (!update || !applyLaunchStatusUpdate(instanceId, sessionId, update)) return null;
  return update;
}

export function setLaunchNotice(instanceId: string, notice: SessionNotice): void {
  launchNotices.value = { ...launchNotices.value, [instanceId]: notice };
}

export function clearLaunchNotice(instanceId: string): void {
  if (!launchNotices.value[instanceId]) return;
  const next = { ...launchNotices.value };
  delete next[instanceId];
  launchNotices.value = next;
}

import type { JSX } from 'preact';
import { useEffect, useRef } from 'preact/hooks';
import {
  selectedInstance, selectedVersion,
  launchState, runningSessions, launchNotices,
  installState, installQueue,
} from '../store';
import { launchGame, killGame } from '../launch';
import { handleInstallClick } from '../install';
import { startRunningAnimation, stopRunningAnimation, startUptime, stopUptime } from '../effects';
import { clearLaunchNotice } from '../actions';

function installTarget(inst: { version_id: string }, version: { needs_install?: string; id: string } | null): string {
  return version?.needs_install || version?.id || inst.version_id;
}

export function ActionArea(): JSX.Element | null {
  const inst = selectedInstance.value;
  if (!inst) return null;

  const version = selectedVersion.value;
  const ls = launchState.value;
  const sessions = runningSessions.value;
  const is = installState.value;
  const queue = installQueue.value;
  const notice = launchNotices.value[inst.id];

  const session = sessions[inst.id];
  const noticeDetails = notice?.details?.length ? notice.details : (notice?.detail ? [notice.detail] : []);

  const noticeEl = notice ? (
    <div class={`launch-notice launch-notice-${notice.tone}`}>
      <div class="launch-notice-copy">
        <div class="launch-notice-message">{notice.message}</div>
        {noticeDetails.length > 0 ? (
          <div class="launch-notice-details">
            {noticeDetails.map((detail) => (
              <div class="launch-notice-detail" key={detail}>{detail}</div>
            ))}
          </div>
        ) : null}
      </div>
      <button
        type="button"
        class="launch-notice-dismiss"
        aria-label="Dismiss launch notice"
        onClick={() => clearLaunchNotice(inst.id)}
      >
        ×
      </button>
    </div>
  ) : null;

  // 1. This instance is currently launching
  if (ls.status === 'preparing' && ls.instanceId === inst.id) {
    return (
      <div class="action-area-stack">
        {noticeEl}
        <div class="launching-area" id="launching-area">
          <div class="launch-sequence">
            <div class="launch-seq-head">
              <span class="launch-seq-label">Launch Sequence</span>
              <span class="launch-seq-version" id="launch-seq-version">{inst.name} ({inst.version_id})</span>
            </div>
            <pre class="launch-ascii" id="launch-ascii"></pre>
            <div class="launch-seq-text">Preparing runtime, assets and session...</div>
            <div class="launch-seq-dots"><span></span><span></span><span></span></div>
          </div>
        </div>
      </div>
    );
  }

  // 2. Another instance is launching
  if (ls.status === 'preparing') {
    return (
      <div class="action-area-stack">
        {noticeEl}
        <div class="not-launchable" id="not-launchable">
          <span id="not-launchable-text">Another launch is being prepared.</span>
        </div>
      </div>
    );
  }

  // 3. This instance is running
  if (session) {
    return (
      <div class="action-area-stack">
        {noticeEl}
        <RunningCard
          name={inst.name}
          versionId={inst.version_id}
          pid={session.pid}
          state={session.state}
          stopping={session.stopping}
          launchedAt={session.launchedAt}
        />
      </div>
    );
  }

  const target = installTarget(inst, version);

  // 4. Active install for this instance's version
  if (is.status === 'active' && is.versionId === target) {
    return (
      <div class="action-area-stack">
        {noticeEl}
        <div class="install-area" id="install-area">
          <button class="install-btn" id="install-btn" disabled>
            <span class="install-btn-text">INSTALLING...</span>
          </button>
          <div class="install-progress" id="install-progress">
            <div class="progress-bar">
              <div class="progress-fill" id="progress-fill" style={{ width: `${is.pct}%` }} />
            </div>
            <span class="progress-text" id="progress-text">{is.label}</span>
          </div>
        </div>
      </div>
    );
  }

  // 5. Queued install
  if (queue.some(q => q.versionId === target)) {
    return (
      <div class="action-area-stack">
        {noticeEl}
        <div class="install-area" id="install-area">
          <button class="install-btn" id="install-btn" disabled>
            <span class="install-btn-text">QUEUED</span>
          </button>
        </div>
      </div>
    );
  }

  // 6. Version not found
  if (!version) {
    return (
      <div class="action-area-stack">
        {noticeEl}
        <div class="install-area" id="install-area">
          <p class="install-text" id="install-text">Version {inst.version_id} is not installed</p>
          <button class="install-btn" id="install-btn" onClick={() => handleInstallClick()}>
            <span class="install-btn-text">INSTALL</span>
          </button>
        </div>
      </div>
    );
  }

  // 7. Version found but not launchable
  if (!version.launchable) {
    return (
      <div class="action-area-stack">
        {noticeEl}
        <div class="install-area" id="install-area">
          <p class="install-text" id="install-text">
            {version.status_detail || 'Game files need downloading'}
          </p>
          <button class="install-btn" id="install-btn" onClick={() => handleInstallClick()}>
            <span class="install-btn-text">INSTALL</span>
          </button>
        </div>
      </div>
    );
  }

  // 8. Launchable
  return (
    <div class="action-area-stack">
      {noticeEl}
      <div class="launch-area" id="launch-area">
        <button class="launch-btn" id="launch-btn" data-action="launch" onClick={() => launchGame()}>
          <span class="launch-btn-text">LAUNCH</span>
          <span class="launch-btn-glow"></span>
        </button>
      </div>
    </div>
  );
}

function RunningCard({ name, versionId, pid, launchedAt, state, stopping }: {
  name: string;
  versionId: string;
  pid: number;
  launchedAt: string;
  state?: string;
  stopping?: boolean;
}): JSX.Element {
  useEffect(() => {
    startRunningAnimation();
    startUptime(launchedAt);
    return () => {
      stopRunningAnimation();
      stopUptime();
    };
  }, [launchedAt]);

  return (
    <div class="running-area" id="running-area">
      <div class="running-card">
        <div class="running-card-head">
          <span class="running-card-label">{runningLabel(pid, state)}</span>
        </div>
        <div class="running-top">
          <pre class="running-ascii" id="running-ascii"></pre>
          <div class="running-info">
            <span class="running-version" id="running-version">
              {name} ({versionId})
            </span>
            <span class="running-pid" id="running-pid">{pid > 0 ? `PID ${pid}` : runningDetail(state)}</span>
          </div>
        </div>
        <div class="running-bottom">
          <div class="running-uptime-wrap">
            <span class="running-uptime-label">Session Time</span>
            <div class="running-uptime" id="running-uptime">0:00</div>
          </div>
          <button class="kill-btn" id="kill-btn" disabled={stopping} onClick={() => killGame()}>
            {stopping ? 'STOPPING...' : 'STOP'}
          </button>
        </div>
      </div>
    </div>
  );
}

function runningLabel(pid: number, state?: string): string {
  if (pid > 0) return 'Game Launched';
  switch (state) {
    case 'validating':
      return 'Validating Launch';
    case 'ensuring_runtime':
      return 'Checking Java Runtime';
    case 'planning':
      return 'Planning Launch';
    case 'preparing':
      return 'Preparing Files';
    case 'starting':
      return 'Starting Game';
    case 'exited':
      return 'Stopping Session';
    default:
      return 'Preparing Launch';
  }
}

function runningDetail(state?: string): string {
  switch (state) {
    case 'validating':
      return 'Checking version and overrides...';
    case 'ensuring_runtime':
      return 'Resolving Java runtime...';
    case 'planning':
      return 'Building the launch plan...';
    case 'preparing':
      return 'Preparing natives and launch files...';
    case 'starting':
      return 'Waiting for process...';
    case 'exited':
      return 'Waiting for stop confirmation...';
    default:
      return 'Waiting for process...';
  }
}

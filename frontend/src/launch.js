import { state, dom, API } from './state.js';
import { api } from './api.js';
import { Sound } from './sound.js';
import { fmtMem, showError, appendLog } from './utils.js';
import { renderInstanceList } from './sidebar.js';
import { refreshSelectedInstanceActionState } from './instance.js';
import { clearLaunchVisualState, startLaunchSequence, endLaunchSequence, startRunningAnimation, startUptime } from './effects.js';
import { showConfirm } from './dialogs.js';

export async function launchGame() {
  const inst = state.selectedInstance;
  if (!inst || state.runningSessions[inst.id] || state.launchingInstanceId) return;
  const version = state.versions.find(v => v.id === inst.version_id);
  if (!version?.launchable) return;

  const username = dom.usernameInput?.value.trim() || 'Player';
  const maxMemMB = Math.round(parseFloat(dom.memorySlider?.value || 4) * 1024);

  // Resource warning when other instances are already running
  const runningCount = Object.keys(state.runningSessions).length;
  if (runningCount > 0) {
    const totalMB = state.systemInfo?.total_memory_mb || 0;
    const allocatedMB = Object.values(state.runningSessions).reduce((sum, s) => sum + (s.allocatedMB || 0), 0);
    if (totalMB > 0 && allocatedMB + maxMemMB > totalMB - 2048) {
      const ok = await showConfirm(
        `You have ${runningCount} instance${runningCount > 1 ? 's' : ''} running, using ~${fmtMem(allocatedMB / 1024)} of ${fmtMem(totalMB / 1024)} system RAM.\n\nLaunching with ${fmtMem(maxMemMB / 1024)} allocated may cause performance issues.`,
        { confirmText: 'Launch Anyway' }
      );
      if (!ok) return;
    }
  }

  Sound.init();
  clearLaunchVisualState();
  state.launchingInstanceId = inst.id;
  if (dom.launchSeqVersion) dom.launchSeqVersion.textContent = `${inst.name} (${inst.version_id})`;
  refreshSelectedInstanceActionState();
  startLaunchSequence();
  renderInstanceList();

  try {
    const res = await api('POST', '/launch', { instance_id: inst.id, username, max_memory_mb: maxMemMB });
    if (res.error) {
      showError(res.error);
      clearLaunchVisualState();
      state.launchingInstanceId = null;
      refreshSelectedInstanceActionState();
      renderInstanceList();
      return;
    }

    const launchedAt = res.launched_at || new Date().toISOString();
    state.launchingInstanceId = null;
    state.runningSessions[inst.id] = {
      sessionId: res.session_id,
      versionId: inst.version_id,
      pid: res.pid,
      launchedAt,
      allocatedMB: maxMemMB,
    };

    endLaunchSequence();
    Sound.ui('launchSuccess');
    if (dom.runningVersion) dom.runningVersion.textContent = `${inst.name} (${inst.version_id})`;
    if (dom.runningPid) dom.runningPid.textContent = `PID ${res.pid}`;
    startRunningAnimation();
    startUptime(launchedAt);
    refreshSelectedInstanceActionState();
    renderInstanceList();
    dom.logPanel?.classList.add('expanded');
    connectLaunchSSE(res.session_id, inst);

    inst.last_played_at = launchedAt;
    if (state.config) {
      state.config.username = username;
      state.config.max_memory_mb = maxMemMB;
    }
  } catch (err) {
    showError(err.message);
    clearLaunchVisualState();
    state.launchingInstanceId = null;
    refreshSelectedInstanceActionState();
    renderInstanceList();
  }
}

function connectLaunchSSE(sessionId, inst) {
  const es = new EventSource(`${API}/launch/${sessionId}/events`);
  const session = state.runningSessions[inst.id];
  if (session) session.eventSource = es;

  es.addEventListener('status', (e) => {
    if (state.runningSessions[inst.id]?.sessionId !== sessionId) return;
    const d = JSON.parse(e.data);
    if (d.state === 'exited') onGameExited(d.exit_code, inst.id, sessionId);
  });
  es.addEventListener('log', (e) => {
    if (state.runningSessions[inst.id]?.sessionId !== sessionId) return;
    const d = JSON.parse(e.data);
    appendLog(d.source, d.text, inst.id, inst.name);
  });
  es.onerror = () => {
    if (state.runningSessions[inst.id]?.sessionId === sessionId) {
      onGameExited(-1, inst.id, sessionId);
    }
  };
}

function onGameExited(exitCode, instanceId, sessionId) {
  const session = state.runningSessions[instanceId];
  if (!session || (sessionId && session.sessionId !== sessionId)) return;

  if (session.eventSource) session.eventSource.close();
  delete state.runningSessions[instanceId];

  if (state.selectedInstance?.id === instanceId) {
    clearLaunchVisualState();
  }
  refreshSelectedInstanceActionState();

  const instObj = state.instances.find(i => i.id === instanceId);
  appendLog('system', `${instObj?.name || instanceId} exited with code ${exitCode}`, instanceId, instObj?.name);
  renderInstanceList();
}

export async function killGame() {
  const inst = state.selectedInstance;
  if (!inst) return;
  const session = state.runningSessions[inst.id];
  if (!session) return;
  try {
    await api('POST', `/launch/${session.sessionId}/kill`);
  } catch (err) {
    showError('Failed to kill: ' + err.message);
  }
}

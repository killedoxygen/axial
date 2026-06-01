import type { JSX, ComponentChildren } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Button, Card, Input, Pill } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { Slider } from '../../ui/Slider';
import { AccentField, AccentModeToggle } from './AccentEditor';
import { local, saveLocalState, STORAGE_KEY } from '../../state';
import { Sound } from '../../sound';
import { Music, musicStateVersion } from '../../music';
import {
  config,
  systemInfo,
  devMode,
  appVersion,
  instances,
  lastInstanceId,
  selectedInstanceId,
  updateCheckState,
  updateInfo,
} from '../../store';
import { navigate, ROUTE_STORAGE_KEY } from '../../ui-state';
import { api } from '../../api';
import { toast } from '../../toast';
import { clampPlayerNameInput } from '../../player-name';
import { errMessage, fmtMem, getMemoryRecommendation, validateUsername } from '../../utils';
import {
  checkForUpdates,
  dismissAvailableUpdate,
  formatUpdateCheckTime,
  hasVisibleUpdate,
  openUpdateAction,
  openUpdateNotes,
} from '../../updater';
import type {
  BenchmarkMatrixResponse,
  BenchmarkQualificationPreviewResponse,
  BenchmarkQualificationResponse,
  BenchmarkQualificationTargetEvidencePreview,
  BenchmarkSuiteDriverResponse,
  BenchmarkSuiteDriverStatus,
  BenchmarkSuiteDriverSuiteStatus,
  BenchmarkSuiteDriversResponse,
  GuardianMode,
  LaunchProofComparison,
  LaunchProofRecord,
  LaunchReportsResponse,
  PerformanceMode,
  PerformanceRulesStatus,
} from '../../types';
import './settings.css';

type SectionId = 'appearance' | 'gameplay' | 'performance' | 'audio' | 'shortcuts' | 'advanced' | 'about';

const SECTIONS: Array<{ id: SectionId; label: string; icon: string }> = [
  { id: 'appearance', label: 'Appearance', icon: 'palette' },
  { id: 'gameplay',   label: 'Gameplay',   icon: 'cube' },
  { id: 'performance', label: 'Performance', icon: 'shield-check' },
  { id: 'audio',      label: 'Audio',      icon: 'headphones' },
  { id: 'shortcuts',  label: 'Shortcuts',  icon: 'keyboard' },
  { id: 'advanced',   label: 'Advanced',   icon: 'terminal' },
  { id: 'about',      label: 'About',      icon: 'info' },
];

function SettingsCard({
  title, desc, control, stack, children,
}: {
  title: string;
  desc?: string;
  control?: ComponentChildren;
  stack?: boolean;
  children?: ComponentChildren;
}): JSX.Element {
  return (
    <Card class={`cp-settings-card${stack ? ' cp-settings-card--stack' : ''}`}>
      <div>
        <div class="cp-settings-card-title">{title}</div>
        {desc && <div class="cp-settings-card-desc">{desc}</div>}
        {stack && children}
      </div>
      {(control || (!stack && children)) && (
        <div class="cp-settings-card-control">{control || children}</div>
      )}
    </Card>
  );
}

type ModeOption<T extends string> = {
  value: T;
  label: string;
  note: string;
};

function ModeChoice<T extends string>({
  label,
  value,
  options,
  disabled,
  onChange,
}: {
  label: string;
  value: T;
  options: Array<ModeOption<T>>;
  disabled?: boolean;
  onChange: (value: T) => void;
}): JSX.Element {
  return (
    <div class="cp-settings-mode-choice">
      <div class="cp-settings-mode-choice-label">{label}</div>
      <div class="cp-settings-mode-seg" role="radiogroup" aria-label={label}>
        {options.map((option) => (
          <button
            key={option.value}
            type="button"
            role="radio"
            aria-checked={option.value === value}
            data-active={option.value === value}
            disabled={disabled}
            onClick={() => onChange(option.value)}
          >
            <span>{option.label}</span>
            <small>{option.note}</small>
          </button>
        ))}
      </div>
    </div>
  );
}

function performanceModeFrom(value: string | undefined): PerformanceMode {
  if (value === 'vanilla' || value === 'custom') return value;
  return 'managed';
}

function guardianModeFrom(value: string | undefined): GuardianMode {
  return value === 'custom' ? 'custom' : 'managed';
}

function Toggle({ on, onChange }: { on: boolean; onChange: () => void }): JSX.Element {
  return (
    <button
      type="button"
      class="cp-toggle"
      data-on={on}
      role="switch"
      aria-checked={on}
      onClick={onChange}
    />
  );
}

// ── Appearance ─────────────────────────────────────────────────────────

function AppearanceSection(): JSX.Element {
  return (
    <>
      <SettingsCard
        title="Mode"
        desc="Light or dark canvas. Accent colors re-derive automatically so contrast stays safe."
        control={<AccentModeToggle />}
      />
      <SettingsCard
        title="Accent"
        desc="Drag inside the field to pick any hue and chroma, or tap a preset. Every tint, ring, and on-accent contrast is derived from this single point."
        stack
      >
        <AccentField />
      </SettingsCard>
    </>
  );
}

// ── Gameplay ────────────────────────────────────────────────────────────

function GameplaySection(): JSX.Element {
  const cfg = config.value;
  const sys = systemInfo.value;
  const savedUsername = cfg?.username || 'Player';
  const savedMemGB = (cfg?.max_memory_mb ?? 4096) / 1024;
  const [username, setUsername] = useState(cfg?.username || 'Player');
  const [memGB, setMemGB] = useState<number>(savedMemGB);
  const lastSaveRequest = useRef(0);
  const totalGB = sys?.total_memory_mb ? Math.floor(sys.total_memory_mb / 1024) : 16;
  const maxGB = Math.max(1, totalGB);
  const rec = getMemoryRecommendation(totalGB);
  const recHigh = Math.min(maxGB, rec.rec + 2);
  const recLow = Math.min(Math.max(2, rec.rec - 2), recHigh);
  const recZone: [number, number] = [recLow, recHigh];
  const memoryTicks = [1, Math.round(maxGB / 4), Math.round(maxGB / 2), Math.round(maxGB * 0.75), maxGB]
    .filter((value, index, values) => value >= 1 && value <= maxGB && values.indexOf(value) === index);

  useEffect(() => {
    setUsername(savedUsername);
    setMemGB(savedMemGB);
  }, [savedMemGB, savedUsername]);

  const recText = useMemo(() => {
    if (memGB < 2) return 'Low, may stutter';
    if (memGB > totalGB * 0.75) return 'Leave room for the OS';
    return rec.text;
  }, [memGB, totalGB, rec.text]);

  const nameError = validateUsername(username);
  const nameValid = nameError === null;
  const showNameError = username.length > 0 && !nameValid;
  const dirty = username !== savedUsername || memGB !== savedMemGB;

  const save = async (): Promise<void> => {
    if (!dirty || !nameValid) return;
    const requestId = lastSaveRequest.current + 1;
    lastSaveRequest.current = requestId;
    try {
      const res: any = await api('PUT', '/config', {
        username: username.trim(),
        max_memory_mb: Math.round(memGB * 1024),
      });
      if (res.error) throw new Error(res.error);
      if (requestId !== lastSaveRequest.current) return;
      config.value = res;
      toast('Saved');
    } catch (err) {
      if (requestId !== lastSaveRequest.current) return;
      toast(`Could not save settings: ${errMessage(err)}`);
    }
  };

  return (
    <>
      <SettingsCard
        title="Player name"
        desc="Shown to Minecraft at launch. Letters, numbers, or underscores (3–16)."
        stack
      >
        <div class="cp-settings-name">
          <Input
            value={username}
            onChange={(v) => setUsername(clampPlayerNameInput(v))}
            placeholder="Player"
            style={{ width: 240 }}
          />
          {dirty && <Button size="sm" onClick={save} disabled={!nameValid} sound="affirm">Save</Button>}
          {showNameError && <span class="cp-settings-name-err">{nameError}</span>}
        </div>
      </SettingsCard>
      <SettingsCard
        title="Memory"
        desc={`Maximum RAM given to the JVM when launching. ${recText} (system has ${totalGB} GB).`}
        stack
      >
        <div style={{ marginTop: 14 }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: 12, marginBottom: 6 }}>
            <span style={{ color: 'var(--text-mute)' }}>Allocation</span>
            <span style={{ color: 'var(--text)', fontWeight: 700 }}>{fmtMem(memGB)}</span>
          </div>
          <Slider
            value={memGB}
            min={1} max={maxGB} step={0.5}
            recommended={recZone}
            ticks={memoryTicks}
            sound="memory"
            onChange={setMemGB}
            onCommit={() => { if (dirty) void save(); }}
            ariaLabel="Max memory in gigabytes"
          />
        </div>
      </SettingsCard>
    </>
  );
}

// ── Performance ─────────────────────────────────────────────────────────

const PERFORMANCE_OPTIONS: Array<ModeOption<PerformanceMode>> = [
  { value: 'managed', label: 'Managed', note: 'Croopor plans safe defaults' },
  { value: 'vanilla', label: 'Vanilla', note: 'No managed add-ons' },
  { value: 'custom', label: 'Custom', note: 'Keep manual tuning' },
];

const GUARDIAN_OPTIONS: Array<ModeOption<GuardianMode>> = [
  { value: 'managed', label: 'Managed', note: 'Warns and can intervene' },
  { value: 'custom', label: 'Custom', note: 'Preserves choices; blocks fatal setups' },
];

type RulesStatusState =
  | { status: 'loading'; data: null; error?: undefined }
  | { status: 'ready'; data: PerformanceRulesStatus; error?: undefined }
  | { status: 'error'; data: null; error: string };

type LaunchReportsState =
  | { status: 'loading'; data: LaunchProofRecord[]; error?: undefined }
  | { status: 'ready'; data: LaunchProofRecord[]; error?: undefined }
  | { status: 'error'; data: LaunchProofRecord[]; error: string };

type BenchmarkMatrixState =
  | { status: 'loading'; data: BenchmarkMatrixResponse | null; error?: undefined }
  | { status: 'ready'; data: BenchmarkMatrixResponse; error?: undefined }
  | { status: 'error'; data: BenchmarkMatrixResponse | null; error: string };

type BenchmarkQualificationPreviewState =
  | { status: 'loading'; data: BenchmarkQualificationPreviewResponse | null; error?: undefined }
  | { status: 'ready'; data: BenchmarkQualificationPreviewResponse; error?: undefined }
  | { status: 'error'; data: BenchmarkQualificationPreviewResponse | null; error: string };

type BenchmarkDriversState =
  | { status: 'loading'; data: BenchmarkSuiteDriverResponse[]; error?: undefined }
  | { status: 'ready'; data: BenchmarkSuiteDriverResponse[]; error?: undefined }
  | { status: 'error'; data: BenchmarkSuiteDriverResponse[]; error: string };

type BenchmarkQualificationRowCheckState =
  | { status: 'loading'; data: BenchmarkQualificationResponse | null; error?: undefined }
  | { status: 'ready'; data: BenchmarkQualificationResponse; error?: undefined }
  | { status: 'error'; data: BenchmarkQualificationResponse | null; error: string };

type BenchmarkQualificationRowChecks = Record<string, BenchmarkQualificationRowCheckState>;

const BENCHMARK_SUITE_DEFAULT_MODE = 'development';
const BENCHMARK_SUITE_DRIVER_DEFAULT_INTERVAL_SECONDS = 30;
const BENCHMARK_SUITE_DRIVER_MIN_INTERVAL_SECONDS = 5;
const BENCHMARK_SUITE_DRIVER_MAX_INTERVAL_SECONDS = 3600;

function clampBenchmarkSuiteDriverIntervalSeconds(value: number): number {
  return Math.min(
    BENCHMARK_SUITE_DRIVER_MAX_INTERVAL_SECONDS,
    Math.max(BENCHMARK_SUITE_DRIVER_MIN_INTERVAL_SECONDS, value),
  );
}

function parseBenchmarkSuiteDriverIntervalSeconds(value: string): number | null {
  const parsed = Number(value.trim());
  if (!Number.isFinite(parsed)) return null;
  return parsed;
}

function healthStateLabel(value: string): string {
  return value
    .split('_')
    .map((part) => part[0]?.toUpperCase() + part.slice(1))
    .join(' ');
}

function ownershipLabel(value: string): string {
  if (value === 'composition_managed') return 'Croopor-managed';
  if (value === 'user_managed') return 'User-managed';
  return healthStateLabel(value);
}

function formatRuleDate(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return new Intl.DateTimeFormat(undefined, { year: 'numeric', month: 'short', day: 'numeric' }).format(date);
}

function emergencyDisableSummary(status: PerformanceRulesStatus): string {
  const count = status.emergency_disable_count ?? status.emergency_disables?.length ?? 0;
  if (count === 0) return 'None active';
  const firstReason = status.emergency_disables?.[0]?.reason?.trim();
  const prefix = `${count} active`;
  return firstReason ? `${prefix}: ${firstReason}` : prefix;
}

function rulesCacheSummary(status: PerformanceRulesStatus): string {
  const cache = status.rules_cache;
  if (cache?.state === 'invalid') return 'Invalid local cache';
  if (!cache?.recorded) return 'Unavailable';
  return 'Recorded locally';
}

function formatProofDate(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value || 'Unknown time';
  return new Intl.DateTimeFormat(undefined, {
    month: 'short',
    day: 'numeric',
    hour: 'numeric',
    minute: '2-digit',
  }).format(date);
}

function formatDurationMs(value: number): string {
  const abs = Math.abs(value);
  if (abs >= 1000) return `${(abs / 1000).toFixed(abs >= 10000 ? 0 : 1)}s`;
  return `${Math.round(abs)}ms`;
}

function formatMemoryMb(value: number): string {
  const sign = value < 0 ? '-' : '';
  const abs = Math.abs(value);
  if (abs >= 1024) {
    const gb = abs / 1024;
    const rounded = gb === Math.floor(gb) ? String(gb) : gb.toFixed(1).replace(/\.0$/, '');
    return `${sign}${rounded} GB`;
  }
  return `${sign}${Math.round(abs)} MB`;
}

function formatLoadAverageX100(value: number): string {
  return (value / 100).toFixed(2);
}

function labelFromToken(value: string | undefined, fallback: string): string {
  const raw = value?.trim();
  if (!raw) return fallback;
  return raw
    .split(/[_\s-]+/)
    .filter(Boolean)
    .map((part) => part[0]?.toUpperCase() + part.slice(1))
    .join(' ');
}

type ProofEvidenceTone = 'neutral' | 'ok' | 'warn' | 'err' | 'info';

type ProofEvidenceSummary = {
  tone: ProofEvidenceTone;
  label: string;
  detail?: string;
};

function proofDetailLooksSensitive(value: string): boolean {
  return [
    /(^|[\s"'`([{])(?:[A-Za-z]:[\\/]|~[\\/]|[\\/](?:Users|home|var|tmp|opt|usr|etc|Library|Applications|mnt|Volumes)\b)/,
    /[\\/][^\s"'`)}\]]+[\\/][^\s"'`)}\]]+/,
    /(^|\s)(?:\.{1,2}[\\/]|[A-Za-z0-9._-]+[\\/](?:bin|lib|jre|jdk|java|\.minecraft)\b)/i,
    /\.(?:jar|exe|dll|dylib|so)\b/i,
    /\b(?:java(?:\.exe)?|cmd(?:\.exe)?|powershell|bash|sh)\s+[-/\\\w"']/i,
    /(^|\s)-{1,2}(?:Xmx\S*|Xms\S*|XX:\S*|D[a-zA-Z0-9_.-]+=\S*|jar\b|cp\b|classpath\b|add-opens\b|add-modules\b)/,
    /\b(?:token|access_token|refresh_token|password|secret|username)\s*[=:]/i,
    /\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b/,
  ].some((pattern) => pattern.test(value));
}

function boundedProofDetail(text: string | undefined): string {
  const normalized = text?.replace(/\s+/g, ' ').trim() ?? '';
  if (!normalized || proofDetailLooksSensitive(normalized)) return '';
  return normalized.length > 150 ? `${normalized.slice(0, 147).trimEnd()}...` : normalized;
}

function firstBoundedProofDetail(values: Array<string | undefined>): string {
  for (const value of values) {
    const detail = boundedProofDetail(value);
    if (detail) return detail;
  }
  return '';
}

function guardianDecisionLabel(decision: string | undefined): string {
  if (decision === 'blocked') return 'Guardian blocked';
  if (decision === 'warned') return 'Guardian warned';
  if (decision === 'intervened') return 'Guardian intervened';
  return 'Guardian note';
}

function guardianDecisionTone(decision: string | undefined): ProofEvidenceTone {
  if (decision === 'blocked') return 'err';
  if (decision === 'warned') return 'warn';
  if (decision === 'intervened') return 'info';
  return 'info';
}

function familyLabel(value: string | undefined): string {
  const raw = value?.trim();
  if (!raw) return 'Unknown family';
  return /^[A-Z](?:-[A-Z])?$/.test(raw) ? `Family ${raw}` : labelFromToken(raw, raw);
}

function qualificationTargetLabel(target: BenchmarkQualificationPreviewResponse['target']): string {
  const family = familyLabel(target.family);
  const loader = labelFromToken(target.loader, 'Unknown loader');
  const version = target.version || 'Unknown version';
  const mode = labelFromToken(target.mode, 'Unknown mode');
  return `${family}, ${loader}, ${version}, ${mode}`;
}

function normalizeBenchmarkQualification(response: BenchmarkQualificationResponse): BenchmarkQualificationResponse {
  return {
    ...response,
    targets: Array.isArray(response.targets) ? response.targets : [],
  };
}

function outcomeTone(outcome: string): 'neutral' | 'ok' | 'warn' | 'err' {
  const normalized = outcome.toLowerCase();
  if (normalized === 'running' || normalized === 'completed' || normalized === 'exited') return 'ok';
  if (normalized === 'stopped' || normalized === 'cancelled' || normalized === 'canceled') return 'warn';
  if (normalized.includes('fail') || normalized.includes('crash') || normalized === 'error') return 'err';
  return 'neutral';
}

function driverStateTone(state: string): 'neutral' | 'ok' | 'warn' | 'err' | 'info' | 'accent' {
  const normalized = state.toLowerCase();
  if (normalized === 'complete') return 'ok';
  if (normalized === 'failed') return 'err';
  if (normalized === 'stopped' || normalized === 'interrupted') return 'warn';
  if (normalized === 'active') return 'accent';
  if (normalized === 'scheduled' || normalized === 'launched_next') return 'info';
  return 'neutral';
}

function isTerminalDriverState(state: string): boolean {
  return ['complete', 'failed', 'stopped', 'interrupted'].includes(state.toLowerCase());
}

function isRestartableDriverState(state: string): boolean {
  return ['failed', 'stopped', 'interrupted'].includes(state.toLowerCase());
}

function compactId(value: string): string {
  if (value.length <= 22) return value;
  return `${value.slice(0, 12)}...${value.slice(-6)}`;
}

function suiteProgressLabel(suite: BenchmarkSuiteDriverSuiteStatus): string {
  if (typeof suite.launched_run_count !== 'number' || typeof suite.run_count !== 'number') return 'Progress unknown';
  return `${suite.launched_run_count}/${suite.run_count} launched`;
}

function pendingRunLabel(suite: BenchmarkSuiteDriverSuiteStatus): string {
  if (typeof suite.pending_run_index !== 'number') return 'Pending none';
  return `Pending #${suite.pending_run_index + 1}`;
}

function driverUpdatedLabel(driver: BenchmarkSuiteDriverStatus): string {
  return driver.updated_at ? `Updated ${formatProofDate(driver.updated_at)}` : 'Updated unknown';
}

function qualificationRequiredLabel(target: BenchmarkQualificationTargetEvidencePreview): string {
  const required = target.required;
  return [
    required.profile,
    required.run_type,
    required.mode,
    required.performance_mode,
  ]
    .filter(Boolean)
    .map((part) => labelFromToken(part, part))
    .join(' · ');
}

function qualificationSuiteLabel(target: BenchmarkQualificationTargetEvidencePreview): string {
  const run = target.suite_run;
  if (!run?.present) return 'Suite missing';
  const state = run.state ? labelFromToken(run.state, run.state) : 'Suite present';
  if (typeof run.run_index === 'number') return `${state}, run #${run.run_index + 1}`;
  return state;
}

function qualificationSuitePresent(suite: BenchmarkQualificationResponse['suite'] | undefined): boolean {
  if (!suite || suite.present === false) return false;
  if (suite.present === true) return true;
  return Boolean(suite.suite_id || suite.mode || typeof suite.run_count === 'number');
}

function qualificationProofLabel(target: BenchmarkQualificationTargetEvidencePreview): string {
  const proof = target.proof;
  if (!proof?.present) return 'Proof missing';
  const outcome = proof.outcome ? labelFromToken(proof.outcome, proof.outcome) : 'Proof present';
  const matched = proof.comparison?.present && typeof proof.comparison.matched_sample_count === 'number'
    ? `, ${proof.comparison.matched_sample_count} matched`
    : '';
  return `${outcome}${matched}`;
}

function qualificationMissingLabel(target: BenchmarkQualificationTargetEvidencePreview): string {
  const count = Array.isArray(target.missing) ? target.missing.length : 0;
  if (count === 0) return 'Complete';
  return `${count} missing`;
}

function qualificationStatusLabel(status: BenchmarkQualificationResponse['status']): string {
  return status === 'ready' ? 'Ready' : 'Incomplete';
}

function qualificationStatusTone(status: BenchmarkQualificationResponse['status']): 'ok' | 'warn' {
  return status === 'ready' ? 'ok' : 'warn';
}

function isReleaseValidationMode(value: string | undefined): boolean {
  return value?.trim().toLowerCase() === 'release_validation';
}

function safeQualificationErrorMessage(err: unknown): string {
  const message = errMessage(err).toLowerCase();
  if (message.includes('404') || message.includes('not found') || message.includes('missing')) {
    return 'Suite evidence was not found.';
  }
  if (message.includes('network') || message.includes('fetch') || message.includes('failed to fetch')) {
    return 'Qualification service is unreachable.';
  }
  if (message.includes('timeout') || message.includes('timed out')) {
    return 'Qualification check timed out.';
  }
  return 'Qualification check failed.';
}

function qualificationMissingTokenLabel(value: string): string {
  const cleaned = value.replace(/[^a-zA-Z0-9 _-]/g, ' ').replace(/\s+/g, ' ').trim();
  if (!cleaned) return 'Evidence';
  return labelFromToken(cleaned.slice(0, 40), 'Evidence');
}

function qualificationMissingSummary(qualification: BenchmarkQualificationResponse): string {
  const missing = qualification.targets.flatMap((target) => Array.isArray(target.missing) ? target.missing : []);
  if (missing.length === 0) return 'No missing evidence';
  const labels = Array.from(new Set(missing.map(qualificationMissingTokenLabel))).slice(0, 2);
  const suffix = missing.length > labels.length ? `, +${missing.length - labels.length}` : '';
  return `${missing.length} missing: ${labels.join(', ')}${suffix}`;
}

function qualificationSuiteSummary(qualification: BenchmarkQualificationResponse): string {
  const suite = qualification.suite;
  if (!qualificationSuitePresent(suite)) return 'Suite missing';
  const mode = suite.mode ? labelFromToken(suite.mode, 'Suite') : 'Suite present';
  if (typeof suite.run_count === 'number') return `${mode}, ${suite.run_count} runs`;
  return mode;
}

function qualificationEvidenceSummary(qualification: BenchmarkQualificationResponse): string {
  const rows = qualification.targets;
  if (rows.length === 0) return 'No target evidence';
  const roles = ['baseline', 'managed'];
  const selected = roles
    .map((role) => rows.find((target) => target.role.toLowerCase() === role))
    .filter((target): target is BenchmarkQualificationTargetEvidencePreview => Boolean(target));
  const fallback = selected.length > 0 ? selected : rows.slice(0, 2);
  return fallback
    .slice(0, 2)
    .map((target) => `${labelFromToken(target.role, 'Target')}: ${qualificationSuiteLabel(target)}, ${qualificationProofLabel(target)}`)
    .join(' · ');
}

function preferredBenchmarkInstanceId(
  list: Array<{ id: string }>,
  selectedId: string | null,
  lastId: string | null,
): string {
  if (selectedId && list.some((instance) => instance.id === selectedId)) return selectedId;
  if (lastId && list.some((instance) => instance.id === lastId)) return lastId;
  return list[0]?.id ?? '';
}

type ComparisonMetricCopy = {
  fasterBy: string;
  slowerBy: string;
  matchesBaseline: string;
};

function comparisonMetricCopy(metricName: string): ComparisonMetricCopy {
  if (metricName === 'boot_duration_ms') {
    return {
      fasterBy: 'Boot faster by',
      slowerBy: 'Boot slower by',
      matchesBaseline: 'Boot matches baseline',
    };
  }
  if (metricName === 'total_completed_stage_duration_ms') {
    return {
      fasterBy: 'Launch stages faster by',
      slowerBy: 'Launch stages slower by',
      matchesBaseline: 'Launch stages match baseline',
    };
  }
  return {
    fasterBy: 'Faster by',
    slowerBy: 'Slower by',
    matchesBaseline: 'Matches baseline',
  };
}

function comparisonSummary(comparison: LaunchProofComparison | null | undefined): {
  label: string;
  detail: string;
  tone: 'neutral' | 'ok' | 'warn';
} {
  if (!comparison) {
    return { label: 'No baseline', detail: 'No comparable local proof yet', tone: 'neutral' };
  }

  const percent = Math.abs(comparison.delta_percent).toFixed(1).replace(/\.0$/, '');
  const current = formatDurationMs(comparison.current_value_ms);
  const baseline = formatDurationMs(comparison.baseline_value_ms);
  const samples = `${comparison.matched_sample_count} matched ${comparison.matched_sample_count === 1 ? 'proof' : 'proofs'}`;
  const metricCopy = comparisonMetricCopy(comparison.metric_name);
  if (comparison.delta_ms < 0) {
    return {
      label: `${metricCopy.fasterBy} ${formatDurationMs(comparison.delta_ms)} (${percent}%)`,
      detail: `${current} now, ${baseline} baseline, ${samples}`,
      tone: 'ok',
    };
  }
  if (comparison.delta_ms > 0) {
    return {
      label: `${metricCopy.slowerBy} ${formatDurationMs(comparison.delta_ms)} (${percent}%)`,
      detail: `${current} now, ${baseline} baseline, ${samples}`,
      tone: 'warn',
    };
  }
  return {
    label: metricCopy.matchesBaseline,
    detail: `${current} now, ${baseline} baseline, ${samples}`,
    tone: 'neutral',
  };
}

function guardianProofSummary(record: LaunchProofRecord): ProofEvidenceSummary | null {
  const guardian = record.guardian;
  if (!guardian) return null;

  const detail = firstBoundedProofDetail([
    guardian.message,
    ...(guardian.details || []),
    ...(guardian.guidance || []),
    ...(guardian.interventions || []).map((intervention) => intervention.detail),
  ]);
  const hasGuardianAction = guardian.decision === 'blocked'
    || guardian.decision === 'warned'
    || guardian.decision === 'intervened';
  if (!hasGuardianAction && !detail) return null;

  return {
    tone: guardianDecisionTone(guardian.decision),
    label: guardianDecisionLabel(guardian.decision),
    detail,
  };
}

function healingProofSummary(record: LaunchProofRecord): ProofEvidenceSummary | null {
  const healing = record.healing;
  if (!healing) return null;

  const retryCount = healing.retry_count && healing.retry_count > 0 ? healing.retry_count : 0;
  const hasEvidence = retryCount > 0
    || Boolean(healing.fallback_applied)
    || Boolean(healing.failure_class)
    || Boolean(healing.warnings && healing.warnings.length > 0);
  if (!hasEvidence) return null;

  const detail = firstBoundedProofDetail([
    healing.fallback_applied,
    ...(healing.warnings || []),
    ...(healing.events || []).map((event) => event.detail),
    healing.failure_class ? `Reason: ${labelFromToken(healing.failure_class, 'launch failure')}` : undefined,
  ]);
  const label = retryCount > 0
    ? `Healing retried ${retryCount} ${retryCount === 1 ? 'time' : 'times'}`
    : healing.failure_class
      ? 'Healing failure'
      : 'Healing applied';

  return {
    tone: healing.failure_class ? 'err' : retryCount > 0 ? 'ok' : 'info',
    label,
    detail,
  };
}

function launchProofEvidenceSummary(record: LaunchProofRecord): ProofEvidenceSummary | null {
  const guardianSummary = guardianProofSummary(record);
  if (guardianSummary) return guardianSummary;
  return healingProofSummary(record);
}

function resourceBudgetSummary(record: LaunchProofRecord): {
  pressureLabel: string;
  details: string[];
  pressure: boolean;
} | null {
  const budget = record.resource_budget;
  if (!budget) return null;

  const pressures: string[] = [];
  if (budget.memory_pressure) pressures.push('memory');
  if (budget.cpu_pressure) pressures.push('CPU');
  if (budget.install_pressure) pressures.push('installs');
  if (budget.disk_pressure) pressures.push('disk');

  const details: string[] = [];
  if (budget.estimated_remaining_memory_mb !== undefined) {
    details.push(`${formatMemoryMb(budget.estimated_remaining_memory_mb)} remaining`);
  }
  if (budget.host_available_memory_mb !== undefined) {
    details.push(`${formatMemoryMb(budget.host_available_memory_mb)} available`);
  } else if (budget.host_used_memory_mb !== undefined) {
    details.push(`${formatMemoryMb(budget.host_used_memory_mb)} used`);
  } else if (budget.launcher_process_memory_mb !== undefined) {
    details.push(`${formatMemoryMb(budget.launcher_process_memory_mb)} launcher RSS`);
  }
  if (budget.host_cpu_load_1m_x100 !== undefined) {
    const threads = budget.host_cpu_threads && budget.host_cpu_threads > 0
      ? `/${budget.host_cpu_threads} threads`
      : '';
    details.push(`load ${formatLoadAverageX100(budget.host_cpu_load_1m_x100)}${threads}`);
  }
  if (budget.active_session_count > 0) {
    const allocation = budget.active_memory_allocation_mb > 0
      ? `, ${formatMemoryMb(budget.active_memory_allocation_mb)} allocated`
      : '';
    details.push(`${budget.active_session_count} active ${budget.active_session_count === 1 ? 'session' : 'sessions'}${allocation}`);
  }
  if (budget.active_install_count > 0) {
    details.push(`${budget.active_install_count} active ${budget.active_install_count === 1 ? 'install' : 'installs'}`);
  }
  if (budget.launch_disk_available_mb !== undefined) {
    details.push(`${formatMemoryMb(budget.launch_disk_available_mb)} disk free`);
  }

  return {
    pressureLabel: pressures.length > 0 ? `Pressure: ${pressures.join(', ')}` : 'Pressure clear',
    details,
    pressure: pressures.length > 0,
  };
}

function stableJsonValue(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(stableJsonValue);
  if (!value || typeof value !== 'object') return value;

  const source = value as Record<string, unknown>;
  return Object.keys(source).sort().reduce<Record<string, unknown>>((acc, key) => {
    acc[key] = stableJsonValue(source[key]);
    return acc;
  }, {});
}

function stablePrettyJson(value: unknown): string {
  return `${JSON.stringify(stableJsonValue(value), null, 2)}\n`;
}

function proofCopyFailureMessage(err: unknown): string {
  const message = errMessage(err).trim().toLowerCase();
  if (message.includes('clipboard')) return 'Clipboard is unavailable.';
  if (message.includes('not found') || message.includes('404')) return 'Launch proof was not found.';
  if (message.includes('network') || message.includes('fetch')) return 'Launch proof service is unreachable.';
  return 'Launch proof could not be copied.';
}

function PerformanceRulesStatusBlock({ state }: { state: RulesStatusState }): JSX.Element {
  if (state.status === 'loading') {
    return (
      <div class="cp-settings-rule-status" aria-live="polite">
        <div class="cp-settings-rule-status-copy">
          <strong>Loading rule status</strong>
          <span>Checking the active performance rule source.</span>
        </div>
      </div>
    );
  }

  if (state.status === 'error') {
    return (
      <div class="cp-settings-rule-status cp-settings-rule-status--warn" aria-live="polite">
        <div class="cp-settings-rule-status-copy">
          <strong>Rule status unavailable</strong>
          <span>{state.error || 'Performance controls still use saved settings.'}</span>
        </div>
      </div>
    );
  }

  const status = state.data;
  const source = status.rule_source === 'built_in' ? 'Built-in rules' : healthStateLabel(status.rule_source);
  const channel = status.rule_channel === 'bundled' ? 'bundled manifest' : healthStateLabel(status.rule_channel);
  const refresh = status.remote_refresh
    ? status.last_refresh_at
      ? `Last refreshed ${formatRuleDate(status.last_refresh_at)}`
      : 'Remote refresh configured, not refreshed yet'
    : 'Remote refresh off';

  return (
    <div class="cp-settings-rule-status" aria-live="polite">
      <div class="cp-settings-rule-status-head">
        <div class="cp-settings-rule-status-copy">
          <strong>{source} active</strong>
          <span>
            {status.composition_count} compositions, schema v{status.schema_version}, generated {formatRuleDate(status.generated_at)}.
          </span>
        </div>
        <Pill tone={status.validation === 'valid' ? 'ok' : 'err'} icon={status.validation === 'valid' ? 'check' : 'alert'}>
          {status.validation === 'valid' ? 'Valid' : 'Invalid'}
        </Pill>
      </div>
      <div class="cp-settings-rule-status-grid">
        <span>Source</span>
        <strong>{channel}</strong>
        <span>Refresh</span>
        <strong>{refresh}</strong>
        <span>Rules cache</span>
        <strong>{rulesCacheSummary(status)}</strong>
        <span>Emergency disables</span>
        <strong>{emergencyDisableSummary(status)}</strong>
        <span>Bundle health</span>
        <strong>{status.health_states.map(healthStateLabel).join(', ')}</strong>
        <span>Ownership</span>
        <strong>{status.ownership_classes.map(ownershipLabel).join(', ')}</strong>
      </div>
      {status.warnings.length > 0 && (
        <div class="cp-settings-rule-status-warnings">
          {status.warnings.map((warning) => <span key={warning}>{warning}</span>)}
        </div>
      )}
    </div>
  );
}

function LaunchProofHistoryBlock({ state }: { state: LaunchReportsState }): JSX.Element {
  const records = state.data.slice(0, 6);
  const [copyingSessionId, setCopyingSessionId] = useState<string | null>(null);

  const copyProof = async (sessionId: string): Promise<void> => {
    if (!navigator.clipboard?.writeText) {
      toast(`Copy failed: ${proofCopyFailureMessage(new Error('clipboard API unavailable'))}`, 'error');
      return;
    }

    setCopyingSessionId(sessionId);
    try {
      const proof = await api('GET', `/launch/reports/${encodeURIComponent(sessionId)}`);
      if (proof?.error) throw new Error(proof.error);
      await navigator.clipboard.writeText(stablePrettyJson(proof));
      toast('Sanitized launch proof copied');
    } catch (err) {
      toast(`Copy failed: ${proofCopyFailureMessage(err)}`, 'error');
    } finally {
      setCopyingSessionId((current) => current === sessionId ? null : current);
    }
  };

  return (
    <div class="cp-settings-proof-history" aria-live="polite">
      <div class="cp-settings-proof-history-head">
        <div class="cp-settings-rule-status-copy">
          <strong>Launch proof history</strong>
          <span>Recent local proofs recorded after launches.</span>
        </div>
        {state.status === 'loading' && <span class="cp-settings-proof-muted">Loading</span>}
        {state.status === 'error' && <span class="cp-settings-proof-error">Unavailable</span>}
      </div>

      {state.status === 'error' && records.length === 0 && (
        <div class="cp-settings-proof-empty">
          Launch proof history is unavailable. {state.error}
        </div>
      )}

      {state.status !== 'error' && records.length === 0 && (
        <div class="cp-settings-proof-empty">
          {state.status === 'loading' ? 'Checking local launch proofs.' : 'No local launch proofs yet.'}
        </div>
      )}

      {records.length > 0 && (
        <div class="cp-settings-proof-list">
          {records.map((record) => {
            const scenario = record.scenario ?? {
              scenario_id: 'unknown_launch',
              performance_mode: 'unknown',
            };
            const comparison = comparisonSummary(record.comparison);
            const budgetSummary = resourceBudgetSummary(record);
            const evidenceSummary = launchProofEvidenceSummary(record);
            const memory = scenario.requested_memory_mb
              ? fmtMem(scenario.requested_memory_mb / 1024)
              : null;
            const bootDuration = Number.isFinite(record.boot_duration_ms)
              ? `Boot ${formatDurationMs(record.boot_duration_ms as number)}`
              : null;
            const benchmarkParts = [
              scenario.benchmark_mode ? `Mode ${labelFromToken(scenario.benchmark_mode, scenario.benchmark_mode)}` : null,
              scenario.benchmark_profile?.trim(),
              scenario.benchmark_run_type?.trim(),
            ].filter(Boolean);
            const version = scenario.version_id || record.version_id || 'Unknown version';

            return (
              <div class="cp-settings-proof-row" key={record.session_id}>
                <div class="cp-settings-proof-main">
                  <Pill tone={outcomeTone(record.outcome)}>{labelFromToken(record.outcome, 'Unknown')}</Pill>
                  <div class="cp-settings-proof-title">
                    <strong>{version}</strong>
                    <span>{labelFromToken(scenario.performance_mode, 'Unknown mode')}</span>
                  </div>
                </div>
                <div class="cp-settings-proof-evidence">
                  <div class="cp-settings-proof-meta">
                    <span>Launched {formatProofDate(record.launched_at)}</span>
                    <span>Recorded {formatProofDate(record.recorded_at)}</span>
                    {bootDuration && <span>{bootDuration}</span>}
                    {memory && <span>{memory} requested</span>}
                    {benchmarkParts.length > 0 && <span>{benchmarkParts.join(' · ')}</span>}
                  </div>
                  {budgetSummary && (
                    <div class="cp-settings-proof-budget" data-pressure={budgetSummary.pressure ? 'true' : 'false'}>
                      <strong>{budgetSummary.pressureLabel}</strong>
                      {budgetSummary.details.length > 0 && <span>{budgetSummary.details.join(' · ')}</span>}
                    </div>
                  )}
                  {evidenceSummary && (
                    <div class="cp-settings-proof-guardian">
                      <Pill tone={evidenceSummary.tone}>{evidenceSummary.label}</Pill>
                      {evidenceSummary.detail && <span>{evidenceSummary.detail}</span>}
                    </div>
                  )}
                </div>
                <div class="cp-settings-proof-compare" data-tone={comparison.tone}>
                  <strong>{comparison.label}</strong>
                  <span>{comparison.detail}</span>
                </div>
                <div class="cp-settings-proof-action">
                  <Button
                    variant="ghost"
                    size="sm"
                    icon="copy"
                    disabled={copyingSessionId === record.session_id}
                    title="Copy sanitized launch proof JSON"
                    onClick={() => void copyProof(record.session_id)}
                  >
                    {copyingSessionId === record.session_id ? 'Copying' : 'Copy proof'}
                  </Button>
                </div>
              </div>
            );
          })}
        </div>
      )}

      {state.status === 'error' && records.length > 0 && (
        <div class="cp-settings-proof-note">Could not refresh proof history. Showing the last loaded records.</div>
      )}
    </div>
  );
}

function BenchmarkMatrixBlock({ state: matrixState }: { state: BenchmarkMatrixState }): JSX.Element {
  const matrix = matrixState.data;
  const modes = matrix?.modes ?? [];
  const profiles = matrix?.profiles ?? [];
  const runTypes = matrix?.run_types ?? [];
  const targets = matrix?.representative_targets ?? [];

  return (
    <div class="cp-settings-benchmark-matrix" aria-live="polite">
      <div class="cp-settings-benchmark-matrix-head">
        <div class="cp-settings-rule-status-copy">
          <strong>Benchmark matrix</strong>
          <span>Descriptor reference for advanced local benchmark naming and suite driver modes.</span>
        </div>
        {matrixState.status === 'loading' && <span class="cp-settings-proof-muted">Loading</span>}
        {matrixState.status === 'error' && <span class="cp-settings-proof-error">Unavailable</span>}
      </div>

      {!matrix && (
        <div class="cp-settings-proof-empty">
          {matrixState.status === 'loading'
            ? 'Checking benchmark descriptors.'
            : `Benchmark matrix is unavailable. ${matrixState.error}`}
        </div>
      )}

      {matrix && (
        <>
          <div class="cp-settings-benchmark-counts">
            <span><strong>{modes.length}</strong> modes</span>
            <span><strong>{profiles.length}</strong> profiles</span>
            <span><strong>{runTypes.length}</strong> run types</span>
            <span><strong>{targets.length}</strong> targets</span>
            <span><strong>v{matrix.schema_version}</strong> schema</span>
          </div>
          <div class="cp-settings-benchmark-lists">
            <div>
              <span>Modes</span>
              <strong>{modes.map((mode) => labelFromToken(mode.id, mode.id)).join(', ') || 'None'}</strong>
            </div>
            <div>
              <span>Profiles</span>
              <strong>{profiles.slice(0, 4).map((profile) => profile.scenario || labelFromToken(profile.id, profile.id)).join(', ') || 'None'}</strong>
            </div>
            <div>
              <span>Run types</span>
              <strong>{runTypes.map((runType) => labelFromToken(runType.id, runType.id)).join(', ') || 'None'}</strong>
            </div>
            <div>
              <span>Targets</span>
              <strong>
                {targets.slice(0, 5).map((target) => {
                  const family = /^[A-Z](?:-[A-Z])?$/.test(target.family)
                    ? `Family ${target.family}`
                    : labelFromToken(target.family, 'Target');
                  const loader = target.loader || labelFromToken(target.id, target.id);
                  const version = target.version ? ` ${target.version}` : '';
                  return `${family} ${loader}${version}`;
                }).join(', ') || 'None'}
              </strong>
            </div>
          </div>
          {matrixState.status === 'error' && (
            <div class="cp-settings-proof-note">Could not refresh benchmark descriptors. Showing the last loaded matrix.</div>
          )}
        </>
      )}
    </div>
  );
}

function BenchmarkQualificationPreviewBlock({ state }: { state: BenchmarkQualificationPreviewState }): JSX.Element {
  const preview = state.data;
  const rows = preview?.targets ?? [];

  return (
    <div class="cp-settings-qualification-preview" aria-live="polite">
      <div class="cp-settings-qualification-head">
        <div class="cp-settings-rule-status-copy">
          <strong>Family C qualification</strong>
          <span>No-launch evidence preview. Incomplete is expected until suite and proof evidence exists.</span>
        </div>
        <div class="cp-settings-qualification-status">
          {state.status === 'loading' && <span class="cp-settings-proof-muted">Loading</span>}
          {state.status === 'error' && <span class="cp-settings-proof-error">Unavailable</span>}
          {preview && (
            <Pill tone={preview.status === 'ready' ? 'ok' : 'warn'}>
              {preview.status === 'ready' ? 'Ready' : 'Incomplete'}
            </Pill>
          )}
        </div>
      </div>

      {!preview && (
        <div class="cp-settings-proof-empty">
          {state.status === 'loading'
            ? 'Checking Family C qualification evidence.'
            : `Family C qualification preview is unavailable. ${state.error}`}
        </div>
      )}

      {preview && (
        <>
          <div class="cp-settings-qualification-summary">
            <div>
              <span>Target</span>
              <strong>{qualificationTargetLabel(preview.target)}</strong>
            </div>
            <div>
              <span>Suite</span>
              <strong>
                {qualificationSuitePresent(preview.suite)
                  ? `${labelFromToken(preview.suite.mode, 'Suite present')}, ${preview.suite.run_count ?? 0} runs`
                  : 'Suite evidence missing'}
              </strong>
            </div>
            <div>
              <span>Schema</span>
              <strong>v{preview.schema_version}</strong>
            </div>
          </div>

          {rows.length === 0 && (
            <div class="cp-settings-proof-empty">No qualification targets are described yet.</div>
          )}

          {rows.length > 0 && (
            <div class="cp-settings-qualification-table">
              <div class="cp-settings-qualification-table-head">
                <span>Role</span>
                <span>Target ID</span>
                <span>Required evidence</span>
                <span>Suite</span>
                <span>Proof</span>
                <span>Missing</span>
              </div>
              {rows.map((row) => (
                <div class="cp-settings-qualification-row" key={`${row.role}:${row.target_id}`}>
                  <span data-label="Role">{labelFromToken(row.role, row.role || 'Target')}</span>
                  <strong data-label="Target ID" title={row.target_id}>{compactId(row.target_id || 'Unknown target')}</strong>
                  <span data-label="Required evidence">{qualificationRequiredLabel(row) || 'Requirement unknown'}</span>
                  <span data-label="Suite" data-present={row.suite_run?.present ? 'true' : 'false'}>{qualificationSuiteLabel(row)}</span>
                  <span data-label="Proof" data-present={row.proof?.present ? 'true' : 'false'}>{qualificationProofLabel(row)}</span>
                  <span data-label="Missing" data-missing={row.missing?.length ? 'true' : 'false'}>{qualificationMissingLabel(row)}</span>
                </div>
              ))}
            </div>
          )}

          {state.status === 'error' && (
            <div class="cp-settings-proof-note">Could not refresh qualification preview. Showing the last loaded evidence state.</div>
          )}
        </>
      )}
    </div>
  );
}

function BenchmarkSuiteDriversBlock({ matrixState }: { matrixState: BenchmarkMatrixState }): JSX.Element {
  const [driversState, setDriversState] = useState<BenchmarkDriversState>({ status: 'loading', data: [] });
  const [stoppingIds, setStoppingIds] = useState<Set<string>>(() => new Set());
  const [resumingIds, setResumingIds] = useState<Set<string>>(() => new Set());
  const [qualificationChecks, setQualificationChecks] = useState<BenchmarkQualificationRowChecks>({});
  const instanceRows = instances.value;
  const preferredInstanceId = preferredBenchmarkInstanceId(instanceRows, selectedInstanceId.value, lastInstanceId.value);
  const suiteModes = useMemo(() => {
    const ids = matrixState.data?.modes.map((mode) => mode.id).filter(Boolean) ?? [];
    return ids.length > 0 ? ids : [BENCHMARK_SUITE_DEFAULT_MODE];
  }, [matrixState.data]);
  const [startInstanceId, setStartInstanceId] = useState(preferredInstanceId);
  const [startSuiteMode, setStartSuiteMode] = useState(BENCHMARK_SUITE_DEFAULT_MODE);
  const [intervalSeconds, setIntervalSeconds] = useState(String(BENCHMARK_SUITE_DRIVER_DEFAULT_INTERVAL_SECONDS));
  const [starting, setStarting] = useState(false);
  const requestRef = useRef(0);
  const qualificationRequestRef = useRef<Record<string, number>>({});
  const aliveRef = useRef(true);

  useEffect(() => {
    setStartInstanceId((current) => (
      current && instanceRows.some((instance) => instance.id === current)
        ? current
        : preferredInstanceId
    ));
  }, [instanceRows, preferredInstanceId]);

  useEffect(() => {
    setStartSuiteMode((current) => {
      if (suiteModes.includes(current)) return current;
      return suiteModes.includes(BENCHMARK_SUITE_DEFAULT_MODE) ? BENCHMARK_SUITE_DEFAULT_MODE : suiteModes[0];
    });
  }, [suiteModes]);

  const loadDrivers = async (): Promise<void> => {
    const requestId = requestRef.current + 1;
    requestRef.current = requestId;
    setDriversState((prev) => ({ status: 'loading', data: prev.data }));
    try {
      const res = await api('GET', '/launch/benchmark/suite/drivers');
      if (res?.error) throw new Error(res.error);
      if (!aliveRef.current || requestId !== requestRef.current) return;
      const drivers = (res as BenchmarkSuiteDriversResponse).drivers;
      setDriversState({ status: 'ready', data: Array.isArray(drivers) ? drivers : [] });
    } catch (err) {
      if (!aliveRef.current || requestId !== requestRef.current) return;
      setDriversState((prev) => ({ status: 'error', data: prev.data, error: errMessage(err) }));
    }
  };

  useEffect(() => {
    aliveRef.current = true;
    void loadDrivers();
    return () => { aliveRef.current = false; };
  }, []);

  const stopDriver = async (id: string): Promise<void> => {
    setStoppingIds((prev) => {
      const next = new Set(prev);
      next.add(id);
      return next;
    });
    try {
      const res = await api('POST', `/launch/benchmark/suite/drivers/${encodeURIComponent(id)}/stop`);
      if (res?.error) throw new Error(res.error);
      const nextDriver = res as BenchmarkSuiteDriverResponse;
      if (!aliveRef.current) return;
      setDriversState((prev) => ({
        status: prev.status === 'error' ? 'ready' : prev.status,
        data: prev.data.map((driver) => (driver.driver.id === id ? nextDriver : driver)),
      }));
      toast('Driver stopped');
    } catch (err) {
      if (aliveRef.current) toast(`Stop failed: ${errMessage(err)}`, 'error');
    } finally {
      if (!aliveRef.current) return;
      setStoppingIds((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    }
  };

  const resumeDriver = async (id: string): Promise<void> => {
    setResumingIds((prev) => {
      const next = new Set(prev);
      next.add(id);
      return next;
    });
    try {
      const res = await api('POST', `/launch/benchmark/suite/drivers/${encodeURIComponent(id)}/resume`);
      if (res?.error) throw new Error(res.error);
      const nextDriver = res as BenchmarkSuiteDriverResponse;
      if (!aliveRef.current) return;
      setDriversState((prev) => ({
        status: 'ready',
        data: [
          nextDriver,
          ...prev.data.filter((driver) => driver.driver.id !== nextDriver.driver.id),
        ],
      }));
      toast('Driver resumed');
    } catch (err) {
      if (aliveRef.current) toast(`Resume failed: ${errMessage(err)}`, 'error');
    } finally {
      if (!aliveRef.current) return;
      setResumingIds((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    }
  };

  const checkFamilyCQualification = async (driverId: string, suiteId: string): Promise<void> => {
    const requestId = (qualificationRequestRef.current[driverId] ?? 0) + 1;
    qualificationRequestRef.current[driverId] = requestId;
    setQualificationChecks((prev) => ({
      ...prev,
      [driverId]: { status: 'loading', data: prev[driverId]?.data ?? null },
    }));
    try {
      const res = await api('GET', `/launch/benchmark/qualification/family-c-1-12-2/${encodeURIComponent(suiteId)}`);
      if (res?.error) throw new Error(res.error);
      if (!aliveRef.current || qualificationRequestRef.current[driverId] !== requestId) return;
      setQualificationChecks((prev) => ({
        ...prev,
        [driverId]: { status: 'ready', data: normalizeBenchmarkQualification(res as BenchmarkQualificationResponse) },
      }));
    } catch (err) {
      if (!aliveRef.current || qualificationRequestRef.current[driverId] !== requestId) return;
      setQualificationChecks((prev) => ({
        ...prev,
        [driverId]: {
          status: 'error',
          data: prev[driverId]?.data ?? null,
          error: safeQualificationErrorMessage(err),
        },
      }));
    }
  };

  const selectedStartInstance = instanceRows.find((instance) => instance.id === startInstanceId) ?? null;
  const parsedIntervalSeconds = parseBenchmarkSuiteDriverIntervalSeconds(intervalSeconds);
  const intervalValid = parsedIntervalSeconds !== null
    && parsedIntervalSeconds >= BENCHMARK_SUITE_DRIVER_MIN_INTERVAL_SECONDS
    && parsedIntervalSeconds <= BENCHMARK_SUITE_DRIVER_MAX_INTERVAL_SECONDS;
  const showIntervalError = intervalSeconds.trim().length > 0 && !intervalValid;
  const canStartDriver = Boolean(selectedStartInstance && startSuiteMode && intervalValid && !starting);

  const normalizeIntervalSeconds = (): void => {
    const parsed = parseBenchmarkSuiteDriverIntervalSeconds(intervalSeconds);
    if (parsed === null) return;
    setIntervalSeconds(String(Math.round(clampBenchmarkSuiteDriverIntervalSeconds(parsed))));
  };

  const startDriver = async (): Promise<void> => {
    if (!selectedStartInstance || !startSuiteMode || !intervalValid || parsedIntervalSeconds === null) return;
    setStarting(true);
    try {
      const intervalMs = Math.round(parsedIntervalSeconds * 1000);
      const res = await api('POST', '/launch/benchmark/suite/driver', {
        instance_id: selectedStartInstance.id,
        suite_mode: startSuiteMode,
        interval_ms: intervalMs,
      });
      if (res?.error) throw new Error(res.error);
      const nextDriver = res as BenchmarkSuiteDriverResponse;
      if (!aliveRef.current) return;
      setDriversState((prev) => ({
        status: 'ready',
        data: [
          nextDriver,
          ...prev.data.filter((driver) => driver.driver.id !== nextDriver.driver.id),
        ],
      }));
      toast('Benchmark driver started');
    } catch (err) {
      if (aliveRef.current) toast(`Start failed: ${errMessage(err)}`, 'error');
    } finally {
      if (aliveRef.current) setStarting(false);
    }
  };

  const rows = driversState.data;

  return (
    <div class="cp-settings-driver-history" aria-live="polite">
      <div class="cp-settings-driver-history-head">
        <div class="cp-settings-rule-status-copy">
          <strong>Benchmark drivers</strong>
          <span>Recent background suite drivers for local benchmark runs.</span>
        </div>
        <div class="cp-settings-driver-actions">
          {driversState.status === 'loading' && <span class="cp-settings-proof-muted">Loading</span>}
          {driversState.status === 'error' && <span class="cp-settings-proof-error">Unavailable</span>}
          <Button
            variant="secondary"
            size="sm"
            icon="refresh"
            disabled={driversState.status === 'loading'}
            onClick={() => void loadDrivers()}
          >
            Refresh
          </Button>
        </div>
      </div>

      <div class="cp-settings-driver-start">
        <label class="cp-settings-driver-start-field cp-settings-driver-start-field--instance">
          <span>Instance</span>
          <select
            value={startInstanceId}
            disabled={starting || instanceRows.length === 0}
            onChange={(event) => setStartInstanceId((event.currentTarget as HTMLSelectElement).value)}
            aria-label="Benchmark driver instance"
          >
            {instanceRows.length === 0 && <option value="">No instances</option>}
            {instanceRows.map((instance) => (
              <option key={instance.id} value={instance.id}>
                {instance.name} ({instance.version_id})
              </option>
            ))}
          </select>
        </label>
        <label class="cp-settings-driver-start-field cp-settings-driver-start-field--mode">
          <span>Suite mode</span>
          <select
            value={startSuiteMode}
            disabled={starting}
            onChange={(event) => setStartSuiteMode((event.currentTarget as HTMLSelectElement).value)}
            aria-label="Benchmark suite mode"
          >
            {suiteModes.map((mode) => (
              <option key={mode} value={mode}>{labelFromToken(mode, mode)}</option>
            ))}
          </select>
        </label>
        <label class="cp-settings-driver-start-field cp-settings-driver-start-field--interval" data-invalid={showIntervalError ? 'true' : 'false'}>
          <span>Interval</span>
          <div class="cp-settings-driver-interval-input">
            <input
              type="number"
              min={BENCHMARK_SUITE_DRIVER_MIN_INTERVAL_SECONDS}
              max={BENCHMARK_SUITE_DRIVER_MAX_INTERVAL_SECONDS}
              step={1}
              value={intervalSeconds}
              autocomplete="off"
              aria-label="Benchmark driver interval in seconds"
              aria-invalid={showIntervalError}
              disabled={starting}
              onInput={(event) => setIntervalSeconds((event.currentTarget as HTMLInputElement).value)}
              onBlur={normalizeIntervalSeconds}
            />
            <span>s</span>
          </div>
        </label>
        <Button
          size="sm"
          icon="play"
          sound="affirm"
          disabled={!canStartDriver}
          onClick={() => void startDriver()}
        >
          {starting ? 'Starting' : 'Start'}
        </Button>
      </div>
      {showIntervalError && (
        <div class="cp-settings-driver-start-error">
          Interval must be {BENCHMARK_SUITE_DRIVER_MIN_INTERVAL_SECONDS}-{BENCHMARK_SUITE_DRIVER_MAX_INTERVAL_SECONDS} seconds.
        </div>
      )}

      {driversState.status === 'error' && rows.length === 0 && (
        <div class="cp-settings-proof-empty">
          Benchmark driver status is unavailable. {driversState.error}
        </div>
      )}

      {driversState.status !== 'error' && rows.length === 0 && (
        <div class="cp-settings-proof-empty">
          {driversState.status === 'loading' ? 'Checking recent benchmark drivers.' : 'No recent benchmark drivers.'}
        </div>
      )}

      {rows.length > 0 && (
        <div class="cp-settings-driver-list">
          {rows.map((row) => {
            const driver = row.driver;
            const suite = row.suite;
            const state = driver.state || row.status || 'unknown';
            const rawSuiteId = suite.suite_id || driver.suite_id;
            const suiteId = rawSuiteId || 'Unknown suite';
            const mode = suite.mode || driver.mode;
            const checkState = qualificationChecks[driver.id];
            const canCheckQualification = Boolean(driver.id && rawSuiteId && isReleaseValidationMode(mode));
            const checkingQualification = checkState?.status === 'loading';
            const canStop = Boolean(driver.id) && !isTerminalDriverState(state);
            const canResume = Boolean(driver.id) && isRestartableDriverState(state);
            const stopping = stoppingIds.has(driver.id);
            const resuming = resumingIds.has(driver.id);

            return (
              <div class="cp-settings-driver-row" key={driver.id}>
                <div class="cp-settings-driver-main">
                  <Pill tone={driverStateTone(state)}>{labelFromToken(state, 'Unknown')}</Pill>
                  <div class="cp-settings-driver-title">
                    <strong title={suiteId}>{suiteId}</strong>
                    <span>{labelFromToken(mode, 'Unknown mode')}</span>
                  </div>
                </div>
                <div class="cp-settings-driver-meta">
                  <span>{suiteProgressLabel(suite)}</span>
                  <span>{pendingRunLabel(suite)}</span>
                  <span>{driverUpdatedLabel(driver)}</span>
                </div>
                <div class="cp-settings-driver-sessions">
                  {driver.active_session_id && (
                    <span title={driver.active_session_id}>Active {compactId(driver.active_session_id)}</span>
                  )}
                  {driver.last_session_id && (
                    <span title={driver.last_session_id}>Last {compactId(driver.last_session_id)}</span>
                  )}
                  {!driver.active_session_id && !driver.last_session_id && <span>No session yet</span>}
                </div>
                <div class="cp-settings-driver-control">
                  {canCheckQualification && (
                    <Button
                      variant="secondary"
                      size="sm"
                      icon="shield-check"
                      disabled={checkingQualification}
                      onClick={() => void checkFamilyCQualification(driver.id, rawSuiteId as string)}
                    >
                      {checkingQualification ? 'Checking' : 'Check'}
                    </Button>
                  )}
                  {canResume && (
                    <Button
                      variant="secondary"
                      size="sm"
                      icon="play"
                      disabled={resuming}
                      onClick={() => void resumeDriver(driver.id)}
                    >
                      {resuming ? 'Resuming' : 'Resume'}
                    </Button>
                  )}
                  {canStop && (
                    <Button
                      variant="secondary"
                      size="sm"
                      icon="stop"
                      disabled={stopping}
                      onClick={() => void stopDriver(driver.id)}
                    >
                      {stopping ? 'Stopping' : 'Stop'}
                    </Button>
                  )}
                </div>
                {checkState && (
                  <div class="cp-settings-driver-qualification">
                    {checkState.status === 'loading' && (
                      <>
                        <Pill tone="info">Checking</Pill>
                        <span>Reading Family C suite evidence.</span>
                      </>
                    )}
                    {checkState.status === 'error' && (
                      <>
                        <Pill tone="err">Unavailable</Pill>
                        <span>{checkState.error}</span>
                      </>
                    )}
                    {checkState.status === 'ready' && (
                      <>
                        <Pill tone={qualificationStatusTone(checkState.data.status)}>
                          {qualificationStatusLabel(checkState.data.status)}
                        </Pill>
                        <span>{qualificationMissingSummary(checkState.data)}</span>
                        <span>{qualificationSuiteSummary(checkState.data)}</span>
                        <span>{qualificationEvidenceSummary(checkState.data)}</span>
                      </>
                    )}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}

      {driversState.status === 'error' && rows.length > 0 && (
        <div class="cp-settings-proof-note">Could not refresh benchmark drivers. Showing the last loaded rows.</div>
      )}
    </div>
  );
}

function PerformanceSection(): JSX.Element {
  const cfg = config.value;
  const isDev = devMode.value;
  const savedPerformance = performanceModeFrom(cfg?.performance_mode);
  const savedGuardian = guardianModeFrom(cfg?.guardian_mode);
  const [performanceMode, setPerformanceMode] = useState<PerformanceMode>(savedPerformance);
  const [guardianMode, setGuardianMode] = useState<GuardianMode>(savedGuardian);
  const [rulesStatus, setRulesStatus] = useState<RulesStatusState>({ status: 'loading', data: null });
  const [launchReports, setLaunchReports] = useState<LaunchReportsState>({ status: 'loading', data: [] });
  const [benchmarkMatrix, setBenchmarkMatrix] = useState<BenchmarkMatrixState>({ status: 'loading', data: null });
  const [qualificationPreview, setQualificationPreview] = useState<BenchmarkQualificationPreviewState>({ status: 'loading', data: null });
  const [saving, setSaving] = useState<'performance' | 'guardian' | null>(null);
  const requestRef = useRef(0);

  useEffect(() => {
    setPerformanceMode(savedPerformance);
    setGuardianMode(savedGuardian);
  }, [savedPerformance, savedGuardian]);

  useEffect(() => {
    let alive = true;
    setRulesStatus({ status: 'loading', data: null });
    api('GET', '/performance/status')
      .then((res) => {
        if (!alive) return;
        if (res?.error) throw new Error(res.error);
        setRulesStatus({ status: 'ready', data: res as PerformanceRulesStatus });
      })
      .catch((err) => {
        if (!alive) return;
        setRulesStatus({ status: 'error', data: null, error: errMessage(err) });
      });
    return () => { alive = false; };
  }, []);

  useEffect(() => {
    if (!isDev) {
      setLaunchReports({ status: 'ready', data: [] });
      return;
    }
    let alive = true;
    setLaunchReports({ status: 'loading', data: [] });
    api('GET', '/launch/reports')
      .then((res) => {
        if (!alive) return;
        if (res?.error) throw new Error(res.error);
        const reports = (res as LaunchReportsResponse).reports;
        setLaunchReports({ status: 'ready', data: Array.isArray(reports) ? reports : [] });
      })
      .catch((err) => {
        if (!alive) return;
        setLaunchReports((prev) => ({ status: 'error', data: prev.data, error: errMessage(err) }));
      });
    return () => { alive = false; };
  }, [isDev]);

  useEffect(() => {
    if (!isDev) return;
    let alive = true;
    setBenchmarkMatrix((prev) => ({ status: 'loading', data: prev.data }));
    api('GET', '/launch/benchmark/matrix')
      .then((res) => {
        if (!alive) return;
        if (res?.error) throw new Error(res.error);
        setBenchmarkMatrix({ status: 'ready', data: res as BenchmarkMatrixResponse });
      })
      .catch((err) => {
        if (!alive) return;
        setBenchmarkMatrix((prev) => ({ status: 'error', data: prev.data, error: errMessage(err) }));
      });
    return () => { alive = false; };
  }, [isDev]);

  useEffect(() => {
    if (!isDev) return;
    let alive = true;
    setQualificationPreview((prev) => ({ status: 'loading', data: prev.data }));
    api('GET', '/launch/benchmark/qualification/family-c-1-12-2/preview')
      .then((res) => {
        if (!alive) return;
        if (res?.error) throw new Error(res.error);
        const preview = res as BenchmarkQualificationPreviewResponse;
        setQualificationPreview({
          status: 'ready',
          data: normalizeBenchmarkQualification(preview),
        });
      })
      .catch((err) => {
        if (!alive) return;
        setQualificationPreview((prev) => ({ status: 'error', data: prev.data, error: errMessage(err) }));
      });
    return () => { alive = false; };
  }, [isDev]);

  const savePatch = async (
    key: 'performance_mode' | 'guardian_mode',
    value: PerformanceMode | GuardianMode,
  ): Promise<void> => {
    const savingKey = key === 'performance_mode' ? 'performance' : 'guardian';
    const requestId = requestRef.current + 1;
    requestRef.current = requestId;
    setSaving(savingKey);
    try {
      const res: any = await api('PUT', '/config', { [key]: value });
      if (res?.error) throw new Error(res.error);
      if (requestId !== requestRef.current) return;
      config.value = res;
      toast('Saved');
    } catch (err) {
      if (requestId !== requestRef.current) return;
      setPerformanceMode(savedPerformance);
      setGuardianMode(savedGuardian);
      const settingLabel = key === 'performance_mode' ? 'performance settings' : 'Guardian settings';
      toast(`Could not save ${settingLabel}: ${errMessage(err)}`, 'error');
    } finally {
      if (requestId === requestRef.current) setSaving(null);
    }
  };

  const changePerformance = (next: PerformanceMode): void => {
    if (next === performanceMode) return;
    setPerformanceMode(next);
    void savePatch('performance_mode', next);
  };

  const changeGuardian = (next: GuardianMode): void => {
    if (next === guardianMode) return;
    setGuardianMode(next);
    void savePatch('guardian_mode', next);
  };

  return (
    <>
      <SettingsCard
        title="Performance program"
        desc="Global default for new and inherited instances. Instance settings can still opt out when needed."
        stack
      >
        <ModeChoice
          label="Default mode"
          value={performanceMode}
          options={PERFORMANCE_OPTIONS}
          disabled={saving !== null}
          onChange={changePerformance}
        />
        <PerformanceRulesStatusBlock state={rulesStatus} />
      </SettingsCard>
      {isDev && (
        <SettingsCard
          title="Performance lab"
          desc="Developer-only benchmark descriptors, qualification evidence, and suite drivers."
          stack
        >
          <LaunchProofHistoryBlock state={launchReports} />
          <BenchmarkMatrixBlock state={benchmarkMatrix} />
          <BenchmarkQualificationPreviewBlock state={qualificationPreview} />
          <BenchmarkSuiteDriversBlock matrixState={benchmarkMatrix} />
        </SettingsCard>
      )}
      <SettingsCard
        title="Guardian"
        desc="Launch safety policy for Java, JVM arguments, and risky runtime changes."
        stack
      >
        <ModeChoice
          label="Support mode"
          value={guardianMode}
          options={GUARDIAN_OPTIONS}
          disabled={saving !== null}
          onChange={changeGuardian}
        />
      </SettingsCard>
    </>
  );
}

// ── Audio ────────────────────────────────────────────────────────────

function AudioSection(): JSX.Element {
  // Reactive subscription to Music state
  musicStateVersion.value;
  const [soundsOn, setSoundsOn] = useState<boolean>(local.sounds);
  const [musicOn, setMusicOn] = useState<boolean>(Music.enabled);
  const [volume, setVolume] = useState<number>(Music.volume);

  useEffect(() => { setMusicOn(Music.enabled); setVolume(Music.volume); }, [musicStateVersion.value]);

  const toggleSounds = (): void => {
    const next = !soundsOn;
    setSoundsOn(next);
    local.sounds = next;
    Sound.enabled = next;
    saveLocalState();
    if (next) Sound.ui('affirm');
  };

  const toggleMusic = (): void => {
    Music.toggle();
    setMusicOn(Music.enabled);
  };

  return (
    <>
      <SettingsCard
        title="UI sounds"
        desc="Soft audio feedback for buttons, sliders, and theme changes."
        control={<Toggle on={soundsOn} onChange={toggleSounds} />}
      />
      <SettingsCard
        title="Background music"
        desc="Ambient track while you're in the launcher. Pauses automatically during gameplay."
        control={<Toggle on={musicOn} onChange={toggleMusic} />}
      />
      {musicOn && (
        <SettingsCard title="Music volume" desc="Set the ambient level without muting." stack>
          <div style={{ marginTop: 14 }}>
            <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: 12, marginBottom: 6 }}>
              <span style={{ color: 'var(--text-mute)' }}>Volume</span>
              <span style={{ color: 'var(--text)', fontWeight: 700 }}>{volume}%</span>
            </div>
            <Slider
              value={volume} min={0} max={100} step={1}
              sound="volume"
              onChange={(v) => {
                setVolume(v);
                Music.setVolume(v);
              }}
              ariaLabel="Music volume"
            />
          </div>
        </SettingsCard>
      )}
    </>
  );
}

// ── Shortcuts ────────────────────────────────────────────────────────────

function ShortcutsSection(): JSX.Element {
  const rows: Array<[string, string]> = [
    ['Open settings', 'Ctrl + ,'],
    ['Focus search', 'Ctrl + F'],
    ['New instance', 'Ctrl + N'],
    ['Launch selected', 'Ctrl + Enter'],
    ['Close dialogs', 'Esc'],
  ];
  return (
    <SettingsCard title="Keyboard shortcuts" desc="Global shortcuts built into the launcher. Custom rebinding is coming." stack>
      <div style={{ marginTop: 14, display: 'flex', flexDirection: 'column', gap: 2 }}>
        {rows.map(([label, combo]) => (
          <div key={label} style={{
            display: 'flex', justifyContent: 'space-between', alignItems: 'center',
            padding: '8px 4px', borderBottom: '1px dashed var(--line)',
          }}>
            <span style={{ fontSize: 13, color: 'var(--text)' }}>{label}</span>
            <kbd class="cp-kbd">{combo}</kbd>
          </div>
        ))}
      </div>
    </SettingsCard>
  );
}

// ── Advanced ────────────────────────────────────────────────────────────

function AdvancedSection(): JSX.Element {
  const cfg = config.value;
  const isDev = devMode.value;
  const savedTelemetry = cfg?.telemetry_enabled === true;
  const [telemetryEnabled, setTelemetryEnabled] = useState(savedTelemetry);
  const [savingTelemetry, setSavingTelemetry] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setTelemetryEnabled(savedTelemetry);
  }, [savedTelemetry]);

  const toggleTelemetry = async (): Promise<void> => {
    if (savingTelemetry) return;
    const next = !telemetryEnabled;
    setTelemetryEnabled(next);
    setSavingTelemetry(true);
    try {
      const res: any = await api('PUT', '/config', { telemetry_enabled: next });
      if (res?.error) throw new Error(res.error);
      config.value = res;
      toast('Saved');
    } catch (err) {
      setTelemetryEnabled(savedTelemetry);
      toast(`Could not save diagnostics setting: ${errMessage(err)}`, 'error');
    } finally {
      setSavingTelemetry(false);
    }
  };

  const flush = async (): Promise<void> => {
    const { showConfirm } = await import('../../ui/Dialog');
    const ok = await showConfirm('Delete all Croopor-owned data and reset the launcher to first run?', { destructive: true, confirmText: 'Reset' });
    if (!ok) return;
    setBusy(true);
    try {
      await api('POST', '/dev/flush');
      for (const key of [STORAGE_KEY, ROUTE_STORAGE_KEY]) {
        localStorage.removeItem(key);
      }
      location.reload();
    } catch (err) {
      toast(`Failed: ${errMessage(err)}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <SettingsCard
        title="Optional diagnostics"
        desc="Stores consent for future lightweight diagnostics. Current builds do not upload telemetry or open a remote diagnostics channel."
        control={<Toggle on={telemetryEnabled} onChange={() => void toggleTelemetry()} />}
      />
      <SettingsCard
        title="Reload launcher"
        desc="Useful if the launcher gets out of sync with the backend."
        control={<Button variant="secondary" icon="refresh" onClick={() => location.reload()}>Reload</Button>}
      />
      {__CROOPOR_ENABLE_DEV_LAB__ && isDev && (
        <SettingsCard
          title="Dev lab"
          desc="Developer-only workbench for procedural art and future internal experiments."
          control={<Button variant="secondary" icon="palette" onClick={() => navigate({ name: 'dev-lab' })}>Open lab</Button>}
        />
      )}
      {isDev && (
        <SettingsCard
          title="Flush all data"
          desc="Deletes every Croopor-managed file and restarts from first run. Existing libraries selected through 'Use existing' are preserved."
          control={<Button variant="danger" icon="trash" disabled={busy} onClick={flush}>{busy ? 'Flushing…' : 'Flush'}</Button>}
        />
      )}
    </>
  );
}

// ── About ──────────────────────────────────────────────────────────────

function displayReleaseVersion(version: string): string {
  return version.startsWith('v') || version.startsWith('V') ? version : `v${version}`;
}

function AboutSection(): JSX.Element {
  const info = updateInfo.value;
  const checkState = updateCheckState.value;
  const [, setDismissedAt] = useState(0);
  const checking = checkState === 'checking';
  const latestVersion = info?.latest_version || appVersion.value;
  const status = checking
    ? 'Checking for updates...'
    : info
      ? info.available
        ? `Latest release: ${displayReleaseVersion(latestVersion)}`
        : `Current release: ${displayReleaseVersion(info.current_version)}`
      : 'Updates have not been checked yet.';
  const visibleUpdate = hasVisibleUpdate();
  const checkedAt = info ? formatUpdateCheckTime(info.checked_at) : 'Not checked yet';

  const dismiss = (): void => {
    dismissAvailableUpdate();
    setDismissedAt(Date.now());
  };

  return (
    <SettingsCard title="Croopor" desc={`Version ${appVersion.value}. A focused Minecraft launcher.`} stack>
      <div style={{ marginTop: 12, display: 'flex', gap: 8, flexWrap: 'wrap' }}>
        <Button variant="secondary" icon="globe" onClick={() => window.open('https://github.com/mateoltd/croopor', '_blank', 'noopener')}>Homepage</Button>
        <Button variant="secondary" icon="refresh" disabled={checking} onClick={() => void checkForUpdates({ force: true })}>
          {checking ? 'Checking...' : 'Check'}
        </Button>
      </div>
      <div style={{ marginTop: 12, color: 'var(--text)', fontSize: 13, fontWeight: 700 }}>{status}</div>
      <div style={{ marginTop: 4, color: 'var(--text-mute)', fontSize: 12 }}>Last checked: {checkedAt}</div>
      {checkState === 'error' && (
        <div style={{ marginTop: 8, color: 'var(--danger)', fontSize: 12 }}>Could not check for updates.</div>
      )}
      {visibleUpdate && (
        <div style={{ marginTop: 12, display: 'flex', gap: 8, flexWrap: 'wrap' }}>
          <Button variant="primary" icon="globe" onClick={() => void openUpdateAction()}>
            {info?.action_label || 'Open release'}
          </Button>
          <Button variant="secondary" icon="tag" onClick={() => void openUpdateNotes()}>Notes</Button>
          <Button variant="secondary" icon="x" onClick={dismiss}>Dismiss</Button>
        </div>
      )}
    </SettingsCard>
  );
}

export function SettingsView(): JSX.Element {
  const [section, setSection] = useState<SectionId>('appearance');

  return (
    <div class="cp-settings">
      <aside class="cp-settings-rail">
        <h1>Settings</h1>
        <div class="cp-settings-rail-list">
          {SECTIONS.map(s => (
            <button
              key={s.id}
              class="cp-settings-rail-btn"
              data-active={section === s.id}
              onClick={() => setSection(s.id)}
            >
              <Icon name={s.icon} size={16} stroke={1.8} />
              {s.label}
            </button>
          ))}
        </div>
      </aside>
      <div class="cp-settings-body">
        {section === 'appearance' && <AppearanceSection />}
        {section === 'gameplay' && <GameplaySection />}
        {section === 'performance' && <PerformanceSection />}
        {section === 'audio' && <AudioSection />}
        {section === 'shortcuts' && <ShortcutsSection />}
        {section === 'advanced' && <AdvancedSection />}
        {section === 'about' && <AboutSection />}
      </div>
    </div>
  );
}

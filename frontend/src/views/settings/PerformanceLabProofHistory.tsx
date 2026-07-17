import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { api } from '../../api';
import { Button, Pill } from '../../ui/Atoms';
import { toast } from '../../toast';
import { versionById } from '../../store';
import { fmtMem } from '../../format';
import { errMessage } from '../../utils';
import { minecraftVersionLabel } from '../../version-display';
import type { LaunchReportsState } from './PerformanceLabTypes';
import type { LaunchProofRecord } from '../../types-launch';
import { launchProofGuardianEvidence } from '../../launch-proof-presenters';
import { formatDurationMs, formatProofDate, labelFromToken } from './PerformanceLabFormat';

function stableJsonValue(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(stableJsonValue);
  if (!value || typeof value !== 'object') return value;

  const source = value as Record<string, unknown>;
  return Object.keys(source)
    .sort()
    .reduce<Record<string, unknown>>((acc, key) => {
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

export function LaunchProofHistoryBlock({ state }: { state: LaunchReportsState }): JSX.Element {
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
      setCopyingSessionId((current) => (current === sessionId ? null : current));
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
        <div class="cp-settings-proof-empty">Launch proof history is unavailable. {state.error}</div>
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
            const viewModel = record.view_model;
            const comparison = viewModel.comparison;
            const budgetSummary = viewModel.resource_budget;
            const evidenceSummary = launchProofGuardianEvidence(record);
            const memory = scenario.requested_memory_mb ? fmtMem(scenario.requested_memory_mb / 1024) : null;
            const bootDuration = Number.isFinite(record.boot_duration_ms)
              ? `Boot ${formatDurationMs(record.boot_duration_ms as number)}`
              : null;
            const benchmarkParts = [
              scenario.benchmark_mode
                ? `Mode ${labelFromToken(scenario.benchmark_mode, scenario.benchmark_mode)}`
                : null,
              scenario.benchmark_profile?.trim(),
              scenario.benchmark_run_type?.trim(),
            ].filter(Boolean);
            const versionId = scenario.version_id || record.version_id || '';
            const versionRecord = versionById(versionId);
            const version = minecraftVersionLabel(versionRecord, versionId || 'Unknown version');

            return (
              <div class="cp-settings-proof-row" key={record.session_id}>
                <div class="cp-settings-proof-main">
                  <Pill tone={viewModel.outcome_tone}>{viewModel.outcome_label}</Pill>
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
                    {benchmarkParts.length > 0 && <span>{benchmarkParts.join(', ')}</span>}
                  </div>
                  {budgetSummary && (
                    <div class="cp-settings-proof-budget" data-pressure={budgetSummary.pressure ? 'true' : 'false'}>
                      <strong>{budgetSummary.pressure_label}</strong>
                      {budgetSummary.details.length > 0 && <span>{budgetSummary.details.join(', ')}</span>}
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

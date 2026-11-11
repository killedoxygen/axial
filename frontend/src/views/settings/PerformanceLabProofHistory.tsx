import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { api } from '../../api';
import { Button, Chip } from '../../ui/Atoms';
import { toast } from '../../toast';
import { versionById } from '../../store';
import { fmtMem } from '../../format';
import { errMessage } from '../../utils';
import { minecraftVersionLabel } from '../../version-display';
import type { LaunchReportsState } from './PerformanceLabTypes';
import { formatDurationMs, formatProofDate, labelFromToken } from './PerformanceLabFormat';
import { dtoError } from '../../dto-contract';

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
      const error = dtoError(proof);
      if (error) throw new Error(error);
      await navigator.clipboard.writeText(stablePrettyJson(proof));
      toast('Sanitized launch proof copied');
    } catch (err) {
      toast(`Copy failed: ${proofCopyFailureMessage(err)}`, 'error');
    } finally {
      setCopyingSessionId((current) => (current === sessionId ? null : current));
    }
  };

  return (
    <div class="cp-lab-block" aria-live="polite">
      <div class="cp-lab-head">
        <div class="cp-lab-title">
          <strong>Launch proof history</strong>
          <span>Recent local proofs recorded after launches.</span>
        </div>
        {state.status === 'loading' && <Chip>Loading</Chip>}
        {state.status === 'error' && <Chip tone="err">Unavailable</Chip>}
      </div>

      {state.status === 'error' && records.length === 0 && (
        <div class="cp-lab-note">Launch proof history is unavailable. {state.error}</div>
      )}

      {state.status !== 'error' && records.length === 0 && (
        <div class="cp-lab-note">
          {state.status === 'loading' ? 'Checking local launch proofs.' : 'No local launch proofs yet.'}
        </div>
      )}

      {records.length > 0 && (
        <div class="cp-lab-list">
          {records.map((record) => {
            const scenario = record.scenario ?? {
              scenario_id: 'unknown_launch',
              performance_mode: 'unknown',
            };
            const viewModel = record.view_model;
            const comparison = viewModel.comparison;
            const budgetSummary = viewModel.resource_budget;
            const evidenceSummary = viewModel.evidence;
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
              <div class="cp-lab-proof-row" key={record.session_id}>
                <div class="cp-lab-main">
                  <Chip tone={viewModel.outcome_tone}>{viewModel.outcome_label}</Chip>
                  <div class="cp-lab-name">
                    <strong>{version}</strong>
                    <span>{labelFromToken(scenario.performance_mode, 'Unknown mode')}</span>
                  </div>
                </div>
                <div class="cp-lab-proof-evidence">
                  <div class="cp-lab-meta">
                    <span>Launched {formatProofDate(record.launched_at)}</span>
                    <span>Recorded {formatProofDate(record.recorded_at)}</span>
                    {bootDuration && <span>{bootDuration}</span>}
                    {memory && <span>{memory} requested</span>}
                    {benchmarkParts.length > 0 && <span>{benchmarkParts.join(', ')}</span>}
                  </div>
                  {budgetSummary && (
                    <div class="cp-lab-proof-budget" data-pressure={budgetSummary.pressure ? 'true' : 'false'}>
                      <strong>{budgetSummary.pressure_label}</strong>
                      {budgetSummary.details.length > 0 && <span>{budgetSummary.details.join(', ')}</span>}
                    </div>
                  )}
                  {evidenceSummary && (
                    <div class="cp-lab-proof-guardian">
                      <Chip tone={evidenceSummary.tone}>{evidenceSummary.label}</Chip>
                      {evidenceSummary.detail && <span>{evidenceSummary.detail}</span>}
                    </div>
                  )}
                </div>
                <div class="cp-lab-proof-compare" data-tone={comparison.tone}>
                  <strong>{comparison.label}</strong>
                  <span>{comparison.detail}</span>
                </div>
                <div class="cp-lab-proof-action">
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
        <div class="cp-lab-note">Could not refresh proof history. Showing the last loaded records.</div>
      )}
    </div>
  );
}

import type { JSX } from 'preact';
import { Pill } from '../../ui/Atoms';
import { errMessage } from '../../utils';
import type { BenchmarkQualificationResponse } from '../../types-performance';
import { compactId } from './PerformanceLabFormat';
import type { BenchmarkQualificationPreviewState } from './PerformanceLabTypes';

export function normalizeBenchmarkQualification(
  response: BenchmarkQualificationResponse,
): BenchmarkQualificationResponse {
  return {
    ...response,
    targets: Array.isArray(response.targets) ? response.targets : [],
  };
}

export function safeQualificationErrorMessage(err: unknown): string {
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

export function BenchmarkQualificationPreviewBlock({
  state,
}: {
  state: BenchmarkQualificationPreviewState;
}): JSX.Element {
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
          {preview && <Pill tone={preview.view_model.status_tone}>{preview.view_model.status_label}</Pill>}
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
              <strong>{preview.view_model.target_label}</strong>
            </div>
            <div>
              <span>Suite</span>
              <strong>{preview.view_model.suite_label}</strong>
            </div>
            <div>
              <span>Schema</span>
              <strong>{preview.view_model.schema_label}</strong>
            </div>
          </div>

          {rows.length === 0 && <div class="cp-settings-proof-empty">No qualification targets are described yet.</div>}

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
                  <span data-label="Role">{row.view_model.role_label}</span>
                  <strong data-label="Target ID" title={row.target_id}>
                    {compactId(row.target_id || 'Unknown target')}
                  </strong>
                  <span data-label="Required evidence">{row.view_model.required_label || 'Requirement unknown'}</span>
                  <span data-label="Suite" data-present={row.view_model.suite_present ? 'true' : 'false'}>
                    {row.view_model.suite_label}
                  </span>
                  <span data-label="Proof" data-present={row.view_model.proof_present ? 'true' : 'false'}>
                    {row.view_model.proof_label}
                  </span>
                  <span data-label="Missing" data-missing={row.view_model.missing_tone === 'warn' ? 'true' : 'false'}>
                    {row.view_model.missing_label}
                  </span>
                </div>
              ))}
            </div>
          )}

          {state.status === 'error' && (
            <div class="cp-settings-proof-note">
              Could not refresh qualification preview. Showing the last loaded evidence state.
            </div>
          )}
        </>
      )}
    </div>
  );
}

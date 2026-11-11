import type { JSX } from 'preact';
import { Chip } from '../../ui/Atoms';
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
    <div class="cp-lab-block" aria-live="polite">
      <div class="cp-lab-head">
        <div class="cp-lab-title">
          <strong>Family C qualification</strong>
          <span>No-launch evidence preview. Incomplete is expected until suite and proof evidence exists.</span>
        </div>
        <div class="cp-lab-status">
          {state.status === 'loading' && <Chip>Loading</Chip>}
          {state.status === 'error' && <Chip tone="err">Unavailable</Chip>}
          {preview && <Chip tone={preview.view_model.status_tone}>{preview.view_model.status_label}</Chip>}
        </div>
      </div>

      {!preview && (
        <div class="cp-lab-note">
          {state.status === 'loading'
            ? 'Checking Family C qualification evidence.'
            : `Family C qualification preview is unavailable. ${state.error}`}
        </div>
      )}

      {preview && (
        <>
          <div class="cp-lab-qualification-summary">
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

          {rows.length === 0 && <div class="cp-lab-note">No qualification targets are described yet.</div>}

          {rows.length > 0 && (
            <div class="cp-lab-qualification-table">
              <div class="cp-lab-qualification-table-head">
                <span>Role</span>
                <span>Target ID</span>
                <span>Required evidence</span>
                <span>Suite</span>
                <span>Proof</span>
                <span>Missing</span>
              </div>
              {rows.map((row) => (
                <div class="cp-lab-qualification-row" key={`${row.role}:${row.target_id}`}>
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
            <div class="cp-lab-note">
              Could not refresh qualification preview. Showing the last loaded evidence state.
            </div>
          )}
        </>
      )}
    </div>
  );
}

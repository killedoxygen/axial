import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Pill } from '../../ui/Atoms';
import { api } from '../../api';
import { errMessage } from '../../utils';
import type { PerformanceRulesStatus } from '../../types-performance';

export type RulesStatusState =
  | { status: 'loading'; data: null; error?: undefined }
  | { status: 'ready'; data: PerformanceRulesStatus; error?: undefined }
  | { status: 'error'; data: null; error: string };

export function usePerformanceRulesStatus(): RulesStatusState {
  const [state, setState] = useState<RulesStatusState>({ status: 'loading', data: null });

  useEffect(() => {
    let alive = true;
    setState({ status: 'loading', data: null });
    api('GET', '/performance/status')
      .then((res) => {
        if (!alive) return;
        if (res?.error) throw new Error(res.error);
        setState({ status: 'ready', data: res as PerformanceRulesStatus });
      })
      .catch((err) => {
        if (!alive) return;
        setState({ status: 'error', data: null, error: errMessage(err) });
      });
    return () => {
      alive = false;
    };
  }, []);

  return state;
}

export function PerformanceRulesStatusBlock({ state }: { state: RulesStatusState }): JSX.Element {
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
  const viewModel = status.view_model;

  return (
    <div class="cp-settings-rule-status" aria-live="polite">
      <div class="cp-settings-rule-status-head">
        <div class="cp-settings-rule-status-copy">
          <strong>{viewModel.source_label} active</strong>
          <span>{viewModel.summary}</span>
        </div>
        <Pill tone={viewModel.validation_tone} icon={viewModel.validation_icon}>
          {viewModel.validation_label}
        </Pill>
      </div>
      <div class="cp-settings-rule-status-meta">
        <span>Source</span>
        <strong>{viewModel.channel_label}</strong>
        <span>Refresh</span>
        <strong>{viewModel.refresh_label}</strong>
        <span>Compositions</span>
        <strong>{status.composition_count}</strong>
      </div>
      <details class="cp-settings-rule-details">
        <summary>{viewModel.details_label}</summary>
        {viewModel.warnings.length > 0 && (
          <div class="cp-settings-rule-status-warnings">
            {viewModel.warnings.map((warning) => (
              <span key={warning}>{warning}</span>
            ))}
          </div>
        )}
        <div class="cp-settings-rule-status-grid">
          <span>Schema</span>
          <strong>v{status.schema_version}</strong>
          <span>Generated</span>
          <strong>{viewModel.generated_label}</strong>
          <span>Rules cache</span>
          <strong>{viewModel.cache_label}</strong>
          <span>Emergency disables</span>
          <strong>{viewModel.emergency_disable_label}</strong>
          <span>Bundle health</span>
          <strong>{viewModel.health_states_label}</strong>
          <span>Ownership</span>
          <strong>{viewModel.ownership_label}</strong>
        </div>
      </details>
    </div>
  );
}

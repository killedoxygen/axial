import type { JSX } from 'preact';
import { Pill } from '../../ui/Atoms';
import type { BenchmarkMatrixState } from './PerformanceLabTypes';
import { familyLabel, labelFromToken } from './PerformanceLabFormat';

export function BenchmarkMatrixBlock({ state: matrixState }: { state: BenchmarkMatrixState }): JSX.Element {
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
            <Pill tone="neutral">
              <strong>{modes.length}</strong> modes
            </Pill>
            <Pill tone="neutral">
              <strong>{profiles.length}</strong> profiles
            </Pill>
            <Pill tone="neutral">
              <strong>{runTypes.length}</strong> run types
            </Pill>
            <Pill tone="neutral">
              <strong>{targets.length}</strong> targets
            </Pill>
            <Pill tone="neutral">
              <strong>v{matrix.schema_version}</strong> schema
            </Pill>
          </div>
          <div class="cp-settings-benchmark-lists">
            <div>
              <span>Modes</span>
              <strong>{modes.map((mode) => labelFromToken(mode.id, mode.id)).join(', ') || 'None'}</strong>
            </div>
            <div>
              <span>Profiles</span>
              <strong>
                {profiles
                  .slice(0, 4)
                  .map((profile) => profile.scenario || labelFromToken(profile.id, profile.id))
                  .join(', ') || 'None'}
              </strong>
            </div>
            <div>
              <span>Run types</span>
              <strong>{runTypes.map((runType) => labelFromToken(runType.id, runType.id)).join(', ') || 'None'}</strong>
            </div>
            <div>
              <span>Targets</span>
              <strong>
                {targets
                  .slice(0, 5)
                  .map((target) => {
                    const loader = target.loader || labelFromToken(target.id, target.id);
                    const version = target.version ? ` ${target.version}` : '';
                    return `${familyLabel(target.family)} ${loader}${version}`;
                  })
                  .join(', ') || 'None'}
              </strong>
            </div>
          </div>
          {matrixState.status === 'error' && (
            <div class="cp-settings-proof-note">
              Could not refresh benchmark descriptors. Showing the last loaded matrix.
            </div>
          )}
        </>
      )}
    </div>
  );
}

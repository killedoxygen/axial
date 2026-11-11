import type { JSX } from 'preact';
import { Chip } from '../../ui/Atoms';
import type { BenchmarkMatrixState } from './PerformanceLabTypes';
import { familyLabel, labelFromToken } from './PerformanceLabFormat';

export function BenchmarkMatrixBlock({ state: matrixState }: { state: BenchmarkMatrixState }): JSX.Element {
  const matrix = matrixState.data;
  const modes = matrix?.modes ?? [];
  const profiles = matrix?.profiles ?? [];
  const runTypes = matrix?.run_types ?? [];
  const targets = matrix?.representative_targets ?? [];

  return (
    <div class="cp-lab-block" aria-live="polite">
      <div class="cp-lab-head">
        <div class="cp-lab-title">
          <strong>Benchmark matrix</strong>
          <span>Descriptor reference for advanced local benchmark naming and suite driver modes.</span>
        </div>
        {matrixState.status === 'loading' && <Chip>Loading</Chip>}
        {matrixState.status === 'error' && <Chip tone="err">Unavailable</Chip>}
      </div>

      {!matrix && (
        <div class="cp-lab-note">
          {matrixState.status === 'loading'
            ? 'Checking benchmark descriptors.'
            : `Benchmark matrix is unavailable. ${matrixState.error}`}
        </div>
      )}

      {matrix && (
        <>
          <div class="cp-lab-benchmark-counts">
            <Chip tone="neutral">
              <strong>{modes.length}</strong> modes
            </Chip>
            <Chip tone="neutral">
              <strong>{profiles.length}</strong> profiles
            </Chip>
            <Chip tone="neutral">
              <strong>{runTypes.length}</strong> run types
            </Chip>
            <Chip tone="neutral">
              <strong>{targets.length}</strong> targets
            </Chip>
            <Chip tone="neutral">
              <strong>v{matrix.schema_version}</strong> schema
            </Chip>
          </div>
          <div class="cp-lab-benchmark-lists">
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
            <div class="cp-lab-note">Could not refresh benchmark descriptors. Showing the last loaded matrix.</div>
          )}
        </>
      )}
    </div>
  );
}

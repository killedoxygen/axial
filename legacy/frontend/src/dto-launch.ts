import { dtoArray, dtoBoolean, dtoNumber, dtoRecord, dtoString, isDtoRecord } from './dto-contract';
import type { InstanceLogTail } from './types-instance';
import type { LaunchProofRecord, LaunchReportsResponse } from './types-launch';

export function launchReportsResponse(value: unknown): LaunchReportsResponse {
  const record = dtoRecord(value, 'Launch reports');
  return {
    reports: dtoArray(record.reports, 'Launch reports list').map((report) => {
      if (!isLaunchProofRecord(report)) throw new Error('Launch report response was invalid.');
      return report;
    }),
  };
}

function isLaunchProofRecord(value: unknown): value is LaunchProofRecord {
  if (!isDtoRecord(value)) return false;
  const scenario = value.scenario;
  const device = value.device;
  const view = value.view_model;
  return (
    typeof value.schema === 'string' &&
    typeof value.schema_version === 'number' &&
    typeof value.session_id === 'string' &&
    typeof value.instance_id === 'string' &&
    typeof value.version_id === 'string' &&
    typeof value.launched_at === 'string' &&
    typeof value.recorded_at === 'string' &&
    typeof value.outcome === 'string' &&
    isDtoRecord(scenario) &&
    typeof scenario.scenario_id === 'string' &&
    typeof scenario.performance_mode === 'string' &&
    isDtoRecord(device) &&
    typeof device.tier === 'string' &&
    isDtoRecord(view) &&
    typeof view.outcome_label === 'string' &&
    typeof view.outcome_tone === 'string' &&
    isDtoRecord(view.comparison)
  );
}

export function instanceLogTailResponse(value: unknown): InstanceLogTail {
  const record = dtoRecord(value, 'Instance log');
  return {
    name: dtoString(record.name, 'Instance log name'),
    size: dtoNumber(record.size, 'Instance log size'),
    truncated: dtoBoolean(record.truncated, 'Instance log truncation'),
    text: dtoString(record.text, 'Instance log text'),
  };
}

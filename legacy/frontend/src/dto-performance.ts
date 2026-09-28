import {
  dtoArray,
  dtoBoolean,
  dtoEnum,
  dtoNumber,
  dtoOptionalNumber,
  dtoOptionalString,
  dtoRecord,
  dtoString,
} from './dto-contract';
import type {
  BenchmarkMatrixResponse,
  BenchmarkQualificationResponse,
  BenchmarkSuiteDriverResponse,
  BenchmarkSuiteDriversResponse,
  PerformanceHealthResponse,
} from './types-performance';

export function performanceHealthResponse(value: unknown): PerformanceHealthResponse {
  const record = dtoRecord(value, 'Performance health');
  const view = dtoRecord(record.view_model, 'Performance health view');
  return {
    health: dtoEnum(record.health, 'Performance health state', ['healthy', 'disabled', 'invalid'] as const),
    view_model: {
      tone: dtoEnum(view.tone, 'Performance health tone', ['ok', 'warn', 'err', 'mute'] as const),
      title: dtoString(view.title, 'Performance health title'),
      detail: dtoString(view.detail, 'Performance health detail'),
    },
  };
}

export function benchmarkMatrixResponse(value: unknown): BenchmarkMatrixResponse {
  const record = dtoRecord(value, 'Benchmark matrix');
  const descriptor = (item: unknown, label: string): { id: string; description: string } => {
    const entry = dtoRecord(item, label);
    return {
      id: dtoString(entry.id, `${label} id`),
      description: dtoString(entry.description, `${label} description`),
    };
  };
  return {
    schema: dtoString(record.schema, 'Benchmark matrix schema'),
    schema_version: dtoNumber(record.schema_version, 'Benchmark matrix schema version'),
    modes: dtoArray(record.modes, 'Benchmark modes').map((item) => {
      const entry = dtoRecord(item, 'Benchmark mode');
      return {
        ...descriptor(entry, 'Benchmark mode'),
        intended_use: dtoString(entry.intended_use, 'Benchmark mode intended use'),
      };
    }),
    run_types: dtoArray(record.run_types, 'Benchmark run types').map((item) => descriptor(item, 'Benchmark run type')),
    profiles: dtoArray(record.profiles, 'Benchmark profiles').map((item) => {
      const entry = dtoRecord(item, 'Benchmark profile');
      return {
        ...descriptor(entry, 'Benchmark profile'),
        scenario: dtoString(entry.scenario, 'Benchmark profile scenario'),
        intended_use: dtoString(entry.intended_use, 'Benchmark profile intended use'),
      };
    }),
    representative_targets: dtoArray(record.representative_targets, 'Benchmark targets').map((item) => {
      const entry = dtoRecord(item, 'Benchmark target');
      return {
        ...descriptor(entry, 'Benchmark target'),
        family: dtoString(entry.family, 'Benchmark target family'),
        version: dtoString(entry.version, 'Benchmark target version'),
        loader: dtoString(entry.loader, 'Benchmark target loader'),
        profile: dtoString(entry.profile, 'Benchmark target profile'),
        run_type: dtoString(entry.run_type, 'Benchmark target run type'),
        intended_use: dtoString(entry.intended_use, 'Benchmark target intended use'),
      };
    }),
    limits: (() => {
      const limits = dtoRecord(record.limits, 'Benchmark limits');
      return {
        max_payload_bytes: dtoNumber(limits.max_payload_bytes, 'Benchmark payload limit'),
        custom_post_values_allowed: dtoBoolean(limits.custom_post_values_allowed, 'Benchmark custom values'),
      };
    })(),
  };
}

export function benchmarkSuiteDriverResponse(value: unknown): BenchmarkSuiteDriverResponse {
  const record = dtoRecord(value, 'Benchmark driver');
  const driver = dtoRecord(record.driver, 'Benchmark driver status');
  const suite = dtoRecord(record.suite, 'Benchmark driver suite');
  const view = dtoRecord(record.view_model, 'Benchmark driver view');
  return {
    status: dtoString(record.status, 'Benchmark driver response status'),
    driver: {
      id: dtoString(driver.id, 'Benchmark driver id'),
      state: dtoString(driver.state, 'Benchmark driver state'),
      suite_id: dtoOptionalString(driver.suite_id, 'Benchmark driver suite'),
      mode: dtoOptionalString(driver.mode, 'Benchmark driver mode'),
      interval_ms: dtoOptionalNumber(driver.interval_ms, 'Benchmark driver interval'),
      created_at: dtoOptionalString(driver.created_at, 'Benchmark driver creation time'),
      updated_at: dtoOptionalString(driver.updated_at, 'Benchmark driver update time'),
      active_session_id: dtoOptionalString(driver.active_session_id, 'Benchmark driver active session'),
      last_run_index: dtoOptionalNumber(driver.last_run_index, 'Benchmark driver last run'),
      last_session_id: dtoOptionalString(driver.last_session_id, 'Benchmark driver last session'),
      error: dtoOptionalString(driver.error, 'Benchmark driver error'),
    },
    suite: {
      suite_id: dtoOptionalString(suite.suite_id, 'Benchmark suite id'),
      mode: dtoOptionalString(suite.mode, 'Benchmark suite mode'),
      run_count: dtoOptionalNumber(suite.run_count, 'Benchmark suite run count'),
      launched_run_count: dtoOptionalNumber(suite.launched_run_count, 'Benchmark suite launched count'),
      pending_run_index:
        suite.pending_run_index == null ? null : dtoNumber(suite.pending_run_index, 'Benchmark pending run'),
    },
    view_model: {
      state_label: dtoString(view.state_label, 'Benchmark driver state label'),
      state_tone: dtoEnum(view.state_tone, 'Benchmark driver state tone', [
        'neutral',
        'accent',
        'ok',
        'warn',
        'err',
        'info',
      ] as const),
      can_stop: dtoBoolean(view.can_stop, 'Benchmark driver can stop'),
      can_resume: dtoBoolean(view.can_resume, 'Benchmark driver can resume'),
      can_check_family_c_qualification: dtoBoolean(
        view.can_check_family_c_qualification,
        'Benchmark driver qualification availability',
      ),
    },
    resumed_from: dtoOptionalString(record.resumed_from, 'Benchmark driver resumed source'),
  };
}

export function benchmarkSuiteDriversResponse(value: unknown): BenchmarkSuiteDriversResponse {
  const record = dtoRecord(value, 'Benchmark drivers');
  return {
    status: dtoString(record.status, 'Benchmark drivers status'),
    drivers: dtoArray(record.drivers, 'Benchmark drivers list').map(benchmarkSuiteDriverResponse),
  };
}

export function benchmarkQualificationResponse(value: unknown): BenchmarkQualificationResponse {
  const record = dtoRecord(value, 'Benchmark qualification');
  const view = dtoRecord(record.view_model, 'Benchmark qualification view');
  const suite = dtoRecord(record.suite, 'Benchmark qualification suite');
  const target = dtoRecord(record.target, 'Benchmark qualification target');
  return {
    schema: dtoString(record.schema, 'Benchmark qualification schema'),
    schema_version: dtoNumber(record.schema_version, 'Benchmark qualification schema version'),
    status: dtoEnum(record.status, 'Benchmark qualification status', ['ready', 'incomplete'] as const),
    view_model: {
      status_label: dtoString(view.status_label, 'Qualification status label'),
      status_tone: dtoEnum(view.status_tone, 'Qualification status tone', [
        'neutral',
        'accent',
        'ok',
        'warn',
        'err',
        'info',
      ] as const),
      target_label: dtoString(view.target_label, 'Qualification target label'),
      suite_label: dtoString(view.suite_label, 'Qualification suite label'),
      schema_label: dtoString(view.schema_label, 'Qualification schema label'),
      missing_summary: dtoString(view.missing_summary, 'Qualification missing summary'),
      suite_summary: dtoString(view.suite_summary, 'Qualification suite summary'),
      evidence_summary: dtoString(view.evidence_summary, 'Qualification evidence summary'),
    },
    suite: {
      present: suite.present == null ? undefined : dtoBoolean(suite.present, 'Qualification suite presence'),
      suite_id: dtoOptionalString(suite.suite_id, 'Qualification suite id'),
      mode: dtoOptionalString(suite.mode, 'Qualification suite mode'),
      run_count: dtoOptionalNumber(suite.run_count, 'Qualification suite run count'),
    },
    target: {
      family: dtoString(target.family, 'Qualification target family'),
      loader: dtoString(target.loader, 'Qualification target loader'),
      version: dtoString(target.version, 'Qualification target version'),
      mode: dtoString(target.mode, 'Qualification target mode'),
    },
    targets: dtoArray(record.targets, 'Qualification targets').map(qualificationTargetResponse),
  };
}

function qualificationTargetResponse(value: unknown): BenchmarkQualificationResponse['targets'][number] {
  const record = dtoRecord(value, 'Qualification target evidence');
  const required = dtoRecord(record.required, 'Qualification required evidence');
  const suite = dtoRecord(record.suite_run, 'Qualification suite run');
  const proof = dtoRecord(record.proof, 'Qualification proof');
  const comparison = proof.comparison == null ? null : dtoRecord(proof.comparison, 'Qualification comparison');
  const view = dtoRecord(record.view_model, 'Qualification target view');
  return {
    role: dtoString(record.role, 'Qualification target role'),
    target_id: dtoString(record.target_id, 'Qualification target id'),
    family: dtoString(record.family, 'Qualification target family'),
    loader: dtoString(record.loader, 'Qualification target loader'),
    version: dtoString(record.version, 'Qualification target version'),
    required: {
      profile: dtoString(required.profile, 'Qualification required profile'),
      run_type: dtoString(required.run_type, 'Qualification required run type'),
      mode: dtoString(required.mode, 'Qualification required mode'),
      performance_mode: dtoString(required.performance_mode, 'Qualification required performance mode'),
    },
    suite_run: {
      present: dtoBoolean(suite.present, 'Qualification suite run presence'),
      run_index: dtoOptionalNumber(suite.run_index, 'Qualification suite run index'),
      profile: dtoOptionalString(suite.profile, 'Qualification suite profile'),
      run_type: dtoOptionalString(suite.run_type, 'Qualification suite run type'),
      target_id: dtoOptionalString(suite.target_id, 'Qualification suite target'),
      benchmark_id: dtoOptionalString(suite.benchmark_id, 'Qualification suite benchmark'),
      session_id: dtoOptionalString(suite.session_id, 'Qualification suite session'),
      state: dtoOptionalString(suite.state, 'Qualification suite state'),
    },
    proof: {
      present: dtoBoolean(proof.present, 'Qualification proof presence'),
      session_id: dtoOptionalString(proof.session_id, 'Qualification proof session'),
      benchmark_id: dtoOptionalString(proof.benchmark_id, 'Qualification proof benchmark'),
      profile: dtoOptionalString(proof.profile, 'Qualification proof profile'),
      run_type: dtoOptionalString(proof.run_type, 'Qualification proof run type'),
      mode: dtoOptionalString(proof.mode, 'Qualification proof mode'),
      performance_mode: dtoOptionalString(proof.performance_mode, 'Qualification proof performance mode'),
      version: dtoOptionalString(proof.version, 'Qualification proof version'),
      outcome: dtoOptionalString(proof.outcome, 'Qualification proof outcome'),
      comparison: comparison
        ? {
            present: dtoBoolean(comparison.present, 'Qualification comparison presence'),
            baseline_session_id: dtoOptionalString(comparison.baseline_session_id, 'Qualification baseline session'),
            metric_name: dtoOptionalString(comparison.metric_name, 'Qualification metric'),
            matched_sample_count: dtoOptionalNumber(comparison.matched_sample_count, 'Qualification matched samples'),
          }
        : undefined,
    },
    missing: dtoArray(record.missing, 'Qualification missing evidence').map((item) =>
      dtoString(item, 'Qualification missing evidence'),
    ),
    view_model: {
      role_label: dtoString(view.role_label, 'Qualification role label'),
      target_label: dtoString(view.target_label, 'Qualification target label'),
      required_label: dtoString(view.required_label, 'Qualification required label'),
      suite_label: dtoString(view.suite_label, 'Qualification suite label'),
      suite_present: dtoBoolean(view.suite_present, 'Qualification suite presence'),
      proof_label: dtoString(view.proof_label, 'Qualification proof label'),
      proof_present: dtoBoolean(view.proof_present, 'Qualification proof presence'),
      missing_label: dtoString(view.missing_label, 'Qualification missing label'),
      missing_tone: dtoEnum(view.missing_tone, 'Qualification missing tone', [
        'neutral',
        'accent',
        'ok',
        'warn',
        'err',
        'info',
      ] as const),
    },
  };
}

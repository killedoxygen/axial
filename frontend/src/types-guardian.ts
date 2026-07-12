export type GuardianMode = 'managed' | 'custom';

export type GuardianDecision = 'allowed' | 'warned' | 'blocked' | 'intervened';

export interface GuardianSummary {
  mode: GuardianMode;
  decision: GuardianDecision;
  message?: string;
  details?: string[];
  guidance?: string[];
}

export interface GuardianEvidenceField {
  key: string;
  value: string;
  sensitivity: string;
}

export interface GuardianTargetDescriptor {
  system: string;
  kind: string;
  id: string;
  ownership: string;
}

export interface GuardianFact {
  operation_id?: string | null;
  id: string;
  domain: string;
  phase: string;
  reliability: string;
  severity?: string | null;
  confidence?: string | null;
  ownership: string;
  target?: GuardianTargetDescriptor | null;
  fields: GuardianEvidenceField[];
}

export interface OperationProofRecord {
  operation_id: string;
  command: string;
  status: string;
  outcome?: string | null;
  targets: GuardianTargetDescriptor[];
  failure_point?: string | null;
  guardian_diagnosis_ids: string[];
  rollback: string;
  fields: GuardianEvidenceField[];
  retention: string;
}

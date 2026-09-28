import type { LaunchProofEvidenceViewModel } from './types-launch';

interface LaunchProofGuardianEvidenceSource {
  view_model: {
    evidence?: LaunchProofEvidenceViewModel | null;
  };
}

export function launchProofGuardianEvidence(
  record: LaunchProofGuardianEvidenceSource,
): LaunchProofEvidenceViewModel | null {
  const evidence = record.view_model.evidence;
  if (!evidence) return null;
  return {
    tone: evidence.tone,
    label: evidence.label,
    detail: evidence.detail ?? null,
  };
}

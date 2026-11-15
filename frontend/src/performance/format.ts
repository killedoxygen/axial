export function formatProofDate(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value || 'Unknown time';
  return new Intl.DateTimeFormat(undefined, {
    month: 'short',
    day: 'numeric',
    hour: 'numeric',
    minute: '2-digit',
  }).format(date);
}

export function formatDurationMs(value: number): string {
  const abs = Math.abs(value);
  if (abs >= 1000) return `${(abs / 1000).toFixed(abs >= 10000 ? 0 : 1)}s`;
  return `${Math.round(abs)}ms`;
}

export function labelFromToken(value: string | undefined, fallback: string): string {
  const raw = value?.trim();
  if (!raw) return fallback;
  return raw
    .split(/[_\s-]+/)
    .filter(Boolean)
    .map((part) => part[0]?.toUpperCase() + part.slice(1))
    .join(' ');
}

export function familyLabel(value: string | undefined): string {
  const raw = value?.trim();
  if (!raw) return 'Unknown family';
  return /^[A-Z](?:-[A-Z])?$/.test(raw) ? `Family ${raw}` : labelFromToken(raw, raw);
}

export function compactId(value: string): string {
  if (value.length <= 22) return value;
  return `${value.slice(0, 12)}...${value.slice(-6)}`;
}

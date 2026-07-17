export function startupWarningMessages(value: unknown): string[] {
  if (!Array.isArray(value)) return [];

  const messages: string[] = [];
  const seen = new Set<string>();
  for (const candidate of value) {
    if (typeof candidate !== 'string') continue;
    const message = candidate.trim();
    if (!message || seen.has(message)) continue;
    seen.add(message);
    messages.push(message);
  }
  return messages;
}

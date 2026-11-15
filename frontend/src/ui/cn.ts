export function cn(...inputs: unknown[]): string {
  return inputs.filter((v): v is string => typeof v === 'string' && v.length > 0).join(' ');
}

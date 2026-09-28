export type DtoRecord = Record<string, unknown>;

export function dtoRecord(value: unknown, label: string): DtoRecord {
  if (!isDtoRecord(value)) throw new Error(`${label} response was invalid.`);
  return value;
}

export function isDtoRecord(value: unknown): value is DtoRecord {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

export function dtoArray(value: unknown, label: string): unknown[] {
  if (!Array.isArray(value)) throw new Error(`${label} response was invalid.`);
  return value;
}

export function dtoString(value: unknown, label: string): string {
  if (typeof value !== 'string') throw new Error(`${label} response was invalid.`);
  return value;
}

export function dtoNumber(value: unknown, label: string): number {
  if (typeof value !== 'number' || !Number.isFinite(value)) throw new Error(`${label} response was invalid.`);
  return value;
}

export function dtoBoolean(value: unknown, label: string): boolean {
  if (typeof value !== 'boolean') throw new Error(`${label} response was invalid.`);
  return value;
}

export function dtoEnum<const T extends string>(value: unknown, label: string, values: readonly T[]): T {
  if (typeof value !== 'string' || !values.includes(value as T)) throw new Error(`${label} response was invalid.`);
  return value as T;
}

export function dtoOptionalString(value: unknown, label: string): string | undefined {
  if (value == null) return undefined;
  return dtoString(value, label);
}

export function dtoOptionalNumber(value: unknown, label: string): number | undefined {
  if (value == null) return undefined;
  return dtoNumber(value, label);
}

export function dtoError(value: unknown): string | null {
  if (!isDtoRecord(value) || typeof value.error !== 'string' || !value.error.trim()) return null;
  return value.error;
}

export function dtoWithoutError(value: unknown, label: string): DtoRecord {
  const record = dtoRecord(value, label);
  const error = dtoError(record);
  if (error) throw new Error(error);
  return record;
}

import { signal } from '@preact/signals';

export const devMode = signal(false);
export const bootstrapState = signal<'loading' | 'ready' | 'error'>('loading');
export const bootstrapError = signal<string | null>(null);
/** Why startup has not finished: the API could not be reached, an owner is busy with other work, or it refused. */
export const bootstrapFailure = signal<'transport' | 'busy' | 'refused' | null>(null);
export const appVersion = signal('1.1.0');

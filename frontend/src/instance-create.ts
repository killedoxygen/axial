// Orchestrates instance creation: POST /instances, handle name collisions,
// optionally queue a version install, toast, and navigate to the new instance.
// Lives at the top level (next to install.ts) so it can import from both
// install.ts and actions.ts without creating a cycle with actions.ts.
import { api, isApiError } from './api';
import { toast } from './toast';
import { errMessage } from './utils';
import { navigate } from './ui-state';
import { addInstance } from './actions';
import { installVersion, installLoaderVersion } from './install';
import type { Instance, LoaderBuildRecord } from './types';

const MAX_NAME_COLLISION_RETRIES = 9;

type InstallIntent =
  | { kind: 'none' }
  | { kind: 'vanilla'; versionId: string }
  | { kind: 'loader'; build: LoaderBuildRecord };

export interface InitialInstanceSettings {
  max_memory_mb?: number;
  art_seed?: number;
  window_width?: number;
  window_height?: number;
  jvm_preset?: string;
}

export interface CreateInstanceArgs {
  name: string;
  versionId: string;
  icon: string;
  accent: string;
  install: InstallIntent;
  initialSettings?: InitialInstanceSettings;
}

export interface CreateInstanceResult {
  ok: boolean;
  instance?: Instance;
  error?: string;
}

interface CreateResponse {
  id?: string;
  error?: string;
}

function isInstance(value: CreateResponse & Partial<Instance>): value is Instance {
  return typeof value.id === 'string'
    && value.id.trim().length > 0
    && typeof value.name === 'string'
    && value.name.trim().length > 0
    && typeof value.version_id === 'string'
    && value.version_id.trim().length > 0
    && typeof value.created_at === 'string'
    && value.created_at.trim().length > 0;
}

function isNameCollision(error: string): boolean {
  return /already exists/i.test(error);
}

function nextCandidateName(base: string, attempt: number): string {
  return `${base} (${attempt + 1})`;
}

async function attemptCreate(
  name: string,
  versionId: string,
  icon: string,
  accent: string,
): Promise<CreateResponse & Partial<Instance>> {
  return api('POST', '/instances', { name, version_id: versionId, icon, accent }) as Promise<
    CreateResponse & Partial<Instance>
  >;
}

export async function createInstance(args: CreateInstanceArgs): Promise<CreateInstanceResult> {
  const { versionId, icon, accent, install } = args;
  const baseName = args.name.trim();
  if (!baseName) return { ok: false, error: 'Name is required' };
  if (!versionId) return { ok: false, error: 'Version is required' };

  let name = baseName;
  let created: Instance | null = null;
  let lastError = '';

  for (let attempt = 0; attempt <= MAX_NAME_COLLISION_RETRIES; attempt++) {
    try {
      const res = await attemptCreate(name, versionId, icon, accent);
      if (res.error) {
        if (isNameCollision(res.error) && attempt < MAX_NAME_COLLISION_RETRIES) {
          name = nextCandidateName(baseName, attempt);
          continue;
        }
        lastError = res.error;
        break;
      }
      if (isInstance(res)) {
        created = res;
        break;
      }
      lastError = 'server returned an incomplete instance';
      console.error('Create instance returned invalid payload');
      break;
    } catch (err: unknown) {
      const message = errMessage(err);
      if (isApiError(err) && isNameCollision(message) && attempt < MAX_NAME_COLLISION_RETRIES) {
        name = nextCandidateName(baseName, attempt);
        continue;
      }
      lastError = message;
      break;
    }
  }

  if (!created) {
    toast(`Failed to create instance: ${lastError || 'unknown error'}`, 'error');
    return { ok: false, error: lastError };
  }

  // Apply user-tuned defaults from the create flow (memory, art seed, window
  // size). Failures here don't kill the create; the instance is fine, the
  // user can re-tune in Settings. The server returns the updated record so
  // the UI signal carries the right values.
  const initial = args.initialSettings;
  if (initial && Object.keys(initial).length > 0) {
    try {
      const res = await api('PUT', `/instances/${encodeURIComponent(created.id)}`, initial) as
        CreateResponse & Partial<Instance>;
      if (!res.error && isInstance(res)) {
        created = res;
      }
    } catch {
      /* non-fatal */
    }
  }

  addInstance(created);
  let queuedInstall = false;
  if (install.kind === 'vanilla') {
    installVersion(install.versionId);
    queuedInstall = true;
  } else if (install.kind === 'loader') {
    installLoaderVersion(install.build);
    queuedInstall = true;
  }

  toast(queuedInstall ? `Created ${created.name}; download queued` : `Created ${created.name}`);
  navigate({ name: 'instance', id: created.id });

  return { ok: true, instance: created };
}

// Orchestrates instance creation: POST /instances, handle name collisions,
// optionally queue a version install, toast, and navigate to the new instance.
// Lives at the top level (next to install.ts) so it can import from both
// install.ts and actions.ts without creating a cycle with actions.ts.
import { api } from './api';
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

export interface CreateInstanceArgs {
  name: string;
  versionId: string;
  icon: string;
  accent: string;
  install: InstallIntent;
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
      if (res.id) {
        created = res as Instance;
        break;
      }
      lastError = 'server returned no instance';
      break;
    } catch (err: unknown) {
      lastError = errMessage(err);
      break;
    }
  }

  if (!created) {
    toast(`Failed to create instance: ${lastError || 'unknown error'}`, 'error');
    return { ok: false, error: lastError };
  }

  addInstance(created);
  toast(`Created ${created.name}`);
  navigate({ name: 'instance', id: created.id });

  if (install.kind === 'vanilla') {
    installVersion(install.versionId);
  } else if (install.kind === 'loader') {
    installLoaderVersion(install.build);
  }

  return { ok: true, instance: created };
}

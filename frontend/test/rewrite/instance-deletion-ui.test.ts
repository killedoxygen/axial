import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import type { DeletionSnapshot } from '../../src/generated/DeletionSnapshot';
import type { EnrichedInstance } from '../../src/types-instance';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const ts: typeof import('typescript') = createRequire(resolve(frontend, 'package.json'))('typescript');
function source<T>(path: string, imports: Record<string, unknown>, globals: Record<string, unknown> = {}): T {
  const filename = resolve(frontend, 'src', path);
  const { outputText } = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: {
      module: ts.ModuleKind.CommonJS,
      target: ts.ScriptTarget.ES2022,
      jsx: ts.JsxEmit.ReactJSX,
      jsxImportSource: 'preact',
    },
  });
  const exports = {};
  vm.runInNewContext(
    outputText,
    {
      exports,
      Error,
      URLSearchParams,
      require(id: string): unknown {
        if (id === '@preact/signals') return { signal: <V>(value: V) => ({ value }) };
        if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
        throw new Error(`Unreviewed deletion test dependency: ${id}`);
      },
      ...globals,
    },
    { filename },
  );
  return exports as T;
}

const contract = source<typeof import('../../src/dto-contract')>('dto-contract.ts', {});
const installDto = source<typeof import('../../src/dto-install')>('dto-install.ts', { './dto-contract': contract });
const coreDto = source<typeof import('../../src/dto-core')>('dto-core.ts', {
  './dto-contract': contract,
  './dto-install': installDto,
});
const instanceId = '94d4ec3a-4a90-4d70-9e97-a5774f1c0a8a';
const oldOperation = '1577a9ba-9f20-40f1-b70c-b6c80956317d';
const freshOperation = 'dbd4a061-dae5-4bce-9f83-8e59f1d963d0';
function instance(id = instanceId): EnrichedInstance {
  return {
    id,
    name: 'Test world',
    version_id: '1.21.1',
    created_at: '2026-09-08T12:00:00Z',
    java_selection: { kind: 'inherited' },
    revision: 1,
    version_display: {
      loader_key: 'vanilla',
      loader_label: 'Vanilla',
      minecraft_label: '1.21.1',
      loader_version_label: '',
      loader_detail_label: '',
      summary_label: 'Minecraft 1.21.1',
      supports_mods: false,
    },
    launchable: false,
    launch_action: {
      state_id: 'install_required',
      label: 'Install',
      tone: 'warn',
      launchable: false,
      primary_action: 'install',
    },
    saves_count: 1,
    mods_count: 0,
    resource_count: 0,
    shader_count: 0,
  };
}
function deletion(status: DeletionSnapshot['status'], overrides: Partial<DeletionSnapshot> = {}): DeletionSnapshot {
  return { operation_id: oldOperation, instance_id: instanceId, intent: 'delete_files', status, ...overrides };
}
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
const settle = () => new Promise<void>((resolve) => setImmediate(resolve));
type Node = { type: unknown; props: Record<string, unknown> };
function nodes(value: unknown): Node[] {
  if (Array.isArray(value)) return value.flatMap(nodes);
  if (!value || typeof value !== 'object' || !('props' in value)) return [];
  const node = value as Node;
  return [node, ...nodes(node.props.children)];
}
function textOf(value: unknown): string {
  if (Array.isArray(value)) return value.map(textOf).join('');
  if (typeof value === 'string') return value;
  if (value && typeof value === 'object' && 'props' in value) return textOf((value as Node).props.children);
  return '';
}

function harness() {
  const calls: Array<{ method: string; path: string }> = [];
  const notices: string[] = [];
  const removed: string[] = [];
  const cleared: string[] = [];
  const choices: Array<{ message: string; options: Array<{ value: string; label: string }> }> = [];
  const store = { instances: { value: [instance()] } };
  let choice: string | null = 'delete-files';
  let pending: unknown[] = [];
  let pendingRead: (() => Promise<unknown>) | null = null;
  let statusRead: ((operation: string) => Promise<unknown>) | null = null;
  let deleteRequest: ((requested: DeletionSnapshot) => Promise<unknown>) | null = null;
  let sequence = 0;
  const statusById = new Map<string, DeletionSnapshot>();
  const apiModule = {
    isApiError: (error: unknown) => error instanceof Error && error.name === 'ApiError',
    async api(method: string, path: string) {
      calls.push({ method, path });
      if (method === 'GET' && path === '/instances/pending')
        return pendingRead ? pendingRead() : { creations: [], deletions: pending };
      if (method === 'GET' && path.startsWith('/instances/deletions/')) {
        const operation = path.slice('/instances/deletions/'.length);
        if (statusRead) return statusRead(operation);
        const result =
          statusById.get(operation) ?? pending.find((row) => (row as DeletionSnapshot).operation_id === operation);
        if (!result) throw new Error('Status unavailable');
        return result;
      }
      if (method === 'GET' && path.startsWith('/instances/')) return instance(path.slice('/instances/'.length));
      if (method === 'DELETE') {
        const [target, query] = path.split('?');
        const params = new URLSearchParams(query);
        const requested = deletion('removed', {
          instance_id: decodeURIComponent(target!.slice('/instances/'.length)),
          operation_id: params.get('operation_id')!,
          intent: params.get('keep_files') === 'true' ? 'keep_files' : 'delete_files',
        });
        statusById.set(requested.operation_id, requested);
        return deleteRequest ? deleteRequest(requested) : { status: requested.status, deletion: requested };
      }
      throw new Error(`Unexpected deletion request: ${method} ${path}`);
    },
  };
  const uiActions = {
    addInstance: (row: EnrichedInstance) => {
      store.instances.value = [...store.instances.value, row];
    },
    updateInstanceInList: (row: EnrichedInstance) => {
      store.instances.value = store.instances.value.map((item) => (item.id === row.id ? row : item));
    },
    removeInstance: (id: string) => {
      removed.push(id);
      store.instances.value = store.instances.value.filter((row) => row.id !== id);
    },
  };
  const shared = {
    '../../api': apiModule,
    '../../actions': uiActions,
    '../../dto-contract': contract,
    '../../dto-core': coreDto,
    '../../store': store,
    '../../utils': { errMessage: (error: unknown) => (error instanceof Error ? error.message : String(error)) },
    '../../toast': { toast: (message: string) => notices.push(message) },
    '../../ui/Dialog': {
      showChoice: async (message: string, options: Array<{ value: string; label: string }>) => {
        choices.push({ message, options });
        return choice;
      },
    },
    './mod-provenance-cache': { clearModProvenance: (id: string) => cleared.push(id) },
  };
  const helper = source<typeof import('../../src/views/instance/deletions')>(
    'views/instance/deletions.ts',
    shared,
    {
      crypto: {
        randomUUID: () => {
          sequence += 1;
          return sequence === 1 ? freshOperation : `dbd4a061-dae5-4bce-9f83-${String(sequence).padStart(12, '0')}`;
        },
      },
    },
  );
  const bulk = source<typeof import('../../src/views/instance/bulk-actions')>('views/instance/bulk-actions.ts', shared);
  const actions = source<typeof import('../../src/views/instance/instance-actions')>(
    'views/instance/instance-actions.ts',
    { ...shared, './deletions': helper, './bulk-actions': bulk },
  );
  const notice = source<typeof import('../../src/views/instances/PendingRemovalsNotice')>(
    'views/instances/PendingRemovalsNotice.tsx',
    {
      'preact/jsx-runtime': {
        jsx: (type: unknown, props: Record<string, unknown>) => ({ type, props }),
        jsxs: (type: unknown, props: Record<string, unknown>) => ({ type, props }),
      },
      'preact/hooks': { useEffect: () => undefined },
      '../../ui/Atoms': { Button: 'Button' },
      '../../store': store,
      '../instance/instance-actions': actions,
      '../instance/deletions': helper,
    },
  );
  return {
    helper,
    actions,
    store,
    calls,
    notices,
    removed,
    cleared,
    choices,
    notice: notice.PendingRemovalsNotice,
    setChoice: (value: string | null) => {
      choice = value;
    },
    setPending: (value: unknown[]) => {
      pending = value;
    },
    setPendingRead: (value: () => Promise<unknown>) => {
      pendingRead = value;
    },
    setStatus: (value: (operation: string) => Promise<unknown>) => {
      statusRead = value;
    },
    setDelete: (value: (requested: DeletionSnapshot) => Promise<unknown>) => {
      deleteRequest = value;
    },
  };
}

test('normal removal sends a fresh operation and explicit file choice; only Removed completes the UI', async () => {
  const h = harness();
  h.setChoice('keep-files');
  let done = 0;
  await h.actions.deleteInstanceFlow(instance(), () => {
    done += 1;
  });
  const request = h.calls.find((call) => call.method === 'DELETE')!;
  assert.match(request.path, new RegExp(`operation_id=${freshOperation}`));
  assert.match(request.path, /keep_files=true/);
  assert.deepEqual(h.removed, [instanceId]);
  assert.deepEqual(h.cleared, [instanceId]);
  assert.equal(done, 1);
});

test('Aborted is preserved, not counted as deleted or navigated away from', async () => {
  const h = harness();
  h.setDelete(async (row) => ({ status: 'aborted', deletion: { ...row, status: 'aborted' } }));
  let done = 0;
  await h.actions.deleteInstanceFlow(instance(), () => {
    done += 1;
  });
  assert.equal(done, 0);
  assert.equal(h.store.instances.value.length, 1);
  assert.deepEqual(h.removed, []);
  assert.deepEqual(h.cleared, []);
  assert.match(h.notices[h.notices.length - 1]!, /aborted.*preserved/i);
});

test('lost DELETE response reconciles the exact operation through one read, never a second mutation', async () => {
  const h = harness();
  h.setDelete(async () => {
    throw new Error('Connection lost');
  });
  let done = 0;
  await h.actions.deleteInstanceFlow(instance(), () => {
    done += 1;
  });
  assert.equal(h.calls.filter((call) => call.method === 'DELETE').length, 1);
  assert.equal(h.calls.filter((call) => call.path === `/instances/deletions/${freshOperation}`).length, 1);
  assert.equal(done, 1);
  assert.deepEqual(h.removed, [instanceId]);
});

test('unconfirmed outcomes retain the row and expose only status checking, including another delete click', async () => {
  const h = harness();
  h.setDelete(async () => {
    throw new Error('Connection lost');
  });
  h.setStatus(async () => {
    throw new Error('No status yet');
  });
  await h.actions.deleteInstanceFlow(instance());
  await h.actions.deleteInstanceFlow(instance());
  assert.equal(h.calls.filter((call) => call.method === 'DELETE').length, 1);
  assert.equal(h.helper.instanceDeletions.value[0]!.status, 'unknown');
  assert.equal(h.store.instances.value.length, 1);
  const view = h.notice();
  assert.match(textOf(view), /result is unconfirmed/);
  const buttons = nodes(view).filter((node) => node.type === 'Button');
  assert.deepEqual(
    buttons.map((button) => textOf(button)),
    ['Check status'],
  );
  await (buttons[0]!.props.onClick as () => void)();
  await settle();
  assert.equal(h.calls.filter((call) => call.method === 'DELETE').length, 1);
});

test('a later status check confirming Removed finishes the original detail navigation without replay', async () => {
  const h = harness();
  h.setDelete(async () => {
    throw new Error('Connection lost');
  });
  h.setStatus(async () => {
    throw new Error('No status yet');
  });
  let done = 0;
  await h.actions.deleteInstanceFlow(instance(), () => {
    done += 1;
  });
  assert.equal(done, 0);
  h.setStatus(async () => deletion('removed', { operation_id: freshOperation }));
  await h.actions.deleteInstanceFlow(instance(), () => {
    done += 1;
  });
  assert.equal(done, 1);
  assert.deepEqual(h.removed, [instanceId]);
  assert.equal(h.calls.filter((call) => call.method === 'DELETE').length, 1);
});

test('mismatched identities, intents, and unknown statuses cannot clear the instance', async () => {
  for (const change of [
    { instance_id: 'another-instance' },
    { intent: 'keep_files' },
    { operation_id: oldOperation },
    { status: 'successful' },
  ]) {
    const h = harness();
    h.setDelete(async (row) => ({ status: 'removed', deletion: { ...row, ...change } }));
    h.setStatus(async () => ({ ...deletion('removed', { operation_id: freshOperation }), ...change }));
    await h.actions.deleteInstanceFlow(instance());
    assert.deepEqual(h.removed, []);
    assert.equal(h.helper.instanceDeletions.value[0]!.status, 'unknown');
  }
});

test('a fresh library can discover a hidden prepared removal and explicitly restore its exact operation', async () => {
  const h = harness();
  h.store.instances.value = [];
  h.setPending([deletion('pending_restore')]);
  h.setChoice('continue');
  h.setDelete(async (row) => ({ status: 'aborted', deletion: { ...row, status: 'aborted' } }));
  await h.helper.refreshPendingInstanceDeletions();
  const view = h.notice();
  assert.match(textOf(view), /Restore instance/);
  assert.match(textOf(view), new RegExp(instanceId));
  const restore = nodes(view).find((node) => node.type === 'Button' && textOf(node) === 'Restore instance')!;
  await (restore.props.onClick as () => void)();
  await settle();
  const request = h.calls.find((call) => call.method === 'DELETE')!;
  assert.match(request.path, new RegExp(`operation_id=${oldOperation}`));
  assert.match(request.path, /keep_files=false/);
  assert.equal(h.store.instances.value[0]?.id, instanceId);
  assert.deepEqual(h.removed, []);
  assert.equal(h.helper.instanceDeletions.value.length, 0);
  assert.match(h.choices[0]!.message, /does not start another deletion/);
});

test('committed recovery retains the exact delete-files operation and only removes on confirmed completion', async () => {
  const h = harness();
  h.setPending([deletion('cleanup_pending')]);
  h.setChoice('continue');
  await h.helper.refreshPendingInstanceDeletions();
  await h.actions.recoverInstanceDeletionFlow(h.helper.instanceDeletions.value[0]!);
  assert.match(
    h.calls.find((call) => call.method === 'DELETE')!.path,
    new RegExp(`operation_id=${oldOperation}&keep_files=false`),
  );
  assert.match(h.choices[0]!.message, /already committed.*remaining files/);
  assert.deepEqual(h.removed, [instanceId]);
});

test('canceling a recovery confirmation performs no deletion', async () => {
  const h = harness();
  h.setPending([deletion('pending_restore')]);
  h.setChoice(null);
  await h.helper.refreshPendingInstanceDeletions();
  await h.actions.recoverInstanceDeletionFlow(h.helper.instanceDeletions.value[0]!);
  assert.equal(h.calls.filter((call) => call.method === 'DELETE').length, 0);
  assert.equal(h.helper.instanceDeletions.value[0]!.status, 'pending_restore');
});

test('bulk deletion stops at Aborted and keeps the remaining selection callback untouched', async () => {
  const h = harness();
  const second = instance('b');
  const third = instance('c');
  h.store.instances.value = [instance(), second, third];
  h.setDelete(async (row) => {
    const status = row.instance_id === 'b' ? 'aborted' : 'removed';
    return { status, deletion: { ...row, status } };
  });
  let done = 0;
  await h.actions.deleteInstancesFlow([instance(), second, third], () => {
    done += 1;
  });
  assert.equal(done, 0);
  assert.deepEqual(h.removed, [instanceId]);
  assert.deepEqual(
    h.store.instances.value.map((row) => row.id),
    ['b', 'c'],
  );
  assert.equal(h.calls.filter((call) => call.method === 'DELETE').length, 2);
  assert.match(h.notices[h.notices.length - 1]!, /Removal confirmed for 1 of 3.*aborted/);
});

test('concurrent new requests cannot mint two destructive operations for one instance', async () => {
  const h = harness();
  const pending = deferred<unknown>();
  h.setDelete(() => pending.promise);
  const first = h.helper.requestInstanceDeletion(instanceId, 'delete_files');
  await settle();
  await assert.rejects(h.helper.requestInstanceDeletion(instanceId, 'keep_files'), /unfinished removal/);
  pending.resolve({ status: 'removed', deletion: deletion('removed', { operation_id: freshOperation }) });
  await first;
  assert.equal(h.calls.filter((call) => call.method === 'DELETE').length, 1);
});

test('a stale pending-list read cannot resurrect a completed operation', async () => {
  const h = harness();
  h.setPending([deletion('cleanup_pending')]);
  await h.helper.refreshPendingInstanceDeletions();
  const delayed = deferred<unknown>();
  h.setPendingRead(() => delayed.promise);
  const refresh = h.helper.refreshPendingInstanceDeletions();
  await h.helper.continueInstanceDeletion(deletion('cleanup_pending'));
  delayed.resolve({ deletions: [deletion('cleanup_pending')] });
  await refresh;
  assert.equal(h.helper.instanceDeletions.value.length, 0);
});

test('a confirmed rejected request with no journal record does not strand an invented operation', async () => {
  const h = harness();
  const error = (status: number) => Object.assign(new Error('Busy'), { name: 'ApiError', status });
  h.setDelete(async () => {
    throw error(409);
  });
  h.setStatus(async () => {
    throw error(404);
  });
  await h.actions.deleteInstanceFlow(instance());
  assert.equal(h.helper.instanceDeletions.value.length, 0);
  assert.equal(h.store.instances.value.length, 1);
  assert.deepEqual(h.removed, []);
  assert.equal(h.calls.filter((call) => call.method === 'DELETE').length, 1);
});

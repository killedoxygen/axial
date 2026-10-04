import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import type { PendingSkinStatus } from '../../src/generated/PendingSkinStatus';
import type { WardrobeContext } from '../../src/machines/skin-wardrobe';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const requireDependency = createRequire(resolve(frontend, 'package.json'));
const ts: typeof import('typescript') = requireDependency('typescript');

function source<T>(path: string, imports: Record<string, unknown>, globals: Record<string, unknown> = {}): T {
  const filename = resolve(frontend, 'src', path);
  const output = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: {
      module: ts.ModuleKind.CommonJS,
      target: ts.ScriptTarget.ES2020,
      jsx: ts.JsxEmit.ReactJSX,
      jsxImportSource: 'preact',
    },
  });
  const exports = {};
  vm.runInNewContext(
    output.outputText,
    {
      exports,
      Error,
      URLSearchParams,
      require(id: string): unknown {
        if (id === '@preact/signals') return { signal: <V>(value: V) => ({ value }) };
        if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
        throw new Error(`Unreviewed skin test dependency: ${id}`);
      },
      ...globals,
    },
    { filename },
  );
  return exports as T;
}

function deferred<T>() {
  let resolveValue!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolveValue = done;
  });
  return { promise, resolve: resolveValue };
}

const settle = (): Promise<void> => new Promise((done) => setImmediate(done));
const accountA = '94d4ec3a-4a90-4d70-9e97-a5774f1c0a8a';
const accountB = '732945c2-979f-41be-8f29-ad91fc43bf60';
const failure = 'Minecraft rejected the skin change.';
const skin = {
  texture_key: 'texture-a',
  name: 'Saved skin',
  variant: 'classic',
  source: 'local_upload',
  cape_id: null,
  created_at: '2026-09-08T12:00:00Z',
  updated_at: '2026-09-08T12:00:00Z',
  applied_at: null,
  byte_size: 180,
};

function pending(overrides: Partial<PendingSkinStatus> = {}): PendingSkinStatus {
  return {
    account_id: accountA,
    generation: 1,
    phase: 'queued',
    texture_key: skin.texture_key,
    error: null,
    ...overrides,
  };
}

function context(accountId = accountA, selectionRevision: number | null = 7): WardrobeContext {
  return { accountKey: `account:${accountId}`, selectionRevision, skinActionsEnabled: true, profile: null };
}

type ViewNode = { type: unknown; props: Record<string, unknown> };
function nodes(tree: unknown): ViewNode[] {
  if (Array.isArray(tree)) return tree.flatMap(nodes);
  if (!tree || typeof tree !== 'object' || !('props' in tree)) return [];
  const node = tree as ViewNode;
  return [node, ...nodes(node.props.children)];
}

function harness() {
  let nextTimer = 0;
  const timers = new Map<number, () => void>();
  const calls: [string, string][] = [];
  const notices: string[] = [];
  const preferences = new Map<string, string>();
  let accountRefreshes = 0;
  let status: PendingSkinStatus | null = null;
  let skinsRead: (() => Promise<unknown>) | null = null;
  let statusRead: (() => Promise<unknown>) | null = null;
  let cancelRead: (() => Promise<unknown>) | null = null;
  const transport = {
    async api(method: string, path: string) {
      calls.push([method, path]);
      const url = new URL(path, 'http://skin-fixture.invalid');
      if (method === 'GET' && url.pathname === '/skins')
        return skinsRead ? skinsRead() : { skins: [skin], pending_apply_texture_key: status?.texture_key ?? null };
      if (method === 'GET' && url.pathname === '/skins/pending') return statusRead ? statusRead() : status;
      if (method !== 'GET') {
        assert.ok(url.searchParams.get('expected_account_id'));
        assert.ok(url.searchParams.get('expected_selection_revision'));
      }
      if (method === 'POST' && url.pathname.endsWith('/apply')) {
        assert.equal(url.searchParams.get('defer'), 'true');
        status = pending();
        return { status: 'queued', pending: status, view_model: { summary: 'Skin queued.' } };
      }
      if (method === 'POST' && url.pathname === '/skins/flush') {
        assert.ok(url.searchParams.get('expected_generation'));
        status = pending({ phase: 'idle', texture_key: null });
        return { status: 'flushed', applied: 1, view_model: { summary: 'Skin applied.' } };
      }
      if (method === 'DELETE' && url.pathname === '/skins/pending') {
        assert.ok(url.searchParams.get('expected_generation'));
        if (cancelRead) return cancelRead();
        status = pending({ phase: 'idle', texture_key: null });
        return { status: 'cleared', cleared: true, view_model: { summary: 'Skin change canceled.' } };
      }
      throw new Error(`Unexpected skin request: ${method} ${path}`);
    },
    isApiError: () => false,
    apiResourceUrl: (path: string) => path,
  };
  const defaults = { DEFAULT_SKINS: [{ id: 'steve', name: 'Steve', variant: 'classic', src: 'fixture.png' }] };
  const api = source<typeof import('../../src/views/accounts/api')>('views/accounts/api.ts', {
    '../../api': transport,
    '../../default-skins': defaults,
  });
  const machine = source<typeof import('../../src/machines/skin-wardrobe')>(
    'machines/skin-wardrobe.ts',
    {
      '../api': transport,
      '../default-skins': defaults,
      '../views/accounts/api': api,
      '../toast': { toast: (message: string) => notices.push(message) },
      '../ui/Dialog': { showConfirm: async () => true },
      './accounts': {
        refreshAccountsData: async () => {
          accountRefreshes += 1;
        },
      },
      '../player-skin': {
        FALLBACK_SKIN_ACCOUNT_KEY: 'account:fallback',
        launcherSkinAccountKey: (id: string) => `account:${id.trim()}`,
        hasSelectedSkinForAccount: (key: string) => preferences.has(key),
        selectedSkinForAccount: (key: string) => preferences.get(key) ?? 'default:steve',
        setSelectedSkin: (value: string, key: string) => preferences.set(key, value),
        resetSelectedSkin: (key: string) => preferences.delete(key),
        refreshAccountSkin() {},
      },
    },
    {
      window: {
        setTimeout(callback: () => void, delay: number) {
          assert.equal(delay, 11_500);
          const id = ++nextTimer;
          timers.set(id, callback);
          return id;
        },
        clearTimeout(id: number) {
          timers.delete(id);
        },
      },
    },
  );
  const primitives = Object.fromEntries(
    [
      'SavedSkinCapeSection',
      'SavedSkinDefaultStrip',
      'SavedSkinFileInputs',
      'SavedSkinLibraryGrid',
      'SkinEditDialog',
      'SkinFinder',
      'SkinStage',
      'SkinUploadDialog',
    ].map((name) => [`./${name}`, { [name]: name }]),
  );
  const { SavedSkinLibrary } = source<typeof import('../../src/views/accounts/SavedSkinLibrary')>(
    'views/accounts/SavedSkinLibrary.tsx',
    {
      ...primitives,
      '../../api': transport,
      '../../default-skins': defaults,
      '../../machines/skin-wardrobe': machine,
      '../../state': { local: { hideSkinNametag: false } },
      './api': api,
      './types': { NO_CAPE_VALUE: '__none' },
      './AccountSwitcher': { AccountSwitcherChip: 'AccountSwitcherChip' },
      './saved-skin-menu': { menuItemsForSavedSkin: () => [] },
      './use-saved-skin-edit-workflow': { useSavedSkinEditWorkflow: () => ({ editReplacementDrop: {} }) },
      './use-saved-skin-lookup-workflow': { useSavedSkinLookupWorkflow: () => ({}) },
      './use-saved-skin-upload-workflow': { useSavedSkinUploadWorkflow: () => ({ uploadDrop: {} }) },
      './use-saved-skin-native-drag-drop': { useSavedSkinNativeDragDrop() {} },
      'preact/hooks': { useRef: <V>(value: V) => ({ current: value }), useMemo: <V>(read: () => V) => read() },
      'preact/jsx-runtime': {
        jsx: (type: unknown, props: Record<string, unknown>) => ({ type, props }),
        jsxs: (type: unknown, props: Record<string, unknown>) => ({ type, props }),
        Fragment: 'Fragment',
      },
    },
  );
  return {
    machine,
    api,
    timers,
    calls,
    notices,
    setStatus(value: PendingSkinStatus | null) {
      status = value;
    },
    setStatusRead(read: (() => Promise<unknown>) | null) {
      statusRead = read;
    },
    setSkinsRead(read: () => Promise<unknown>) {
      skinsRead = read;
    },
    setCancelRead(read: () => Promise<unknown>) {
      cancelRead = read;
    },
    get accountRefreshes() {
      return accountRefreshes;
    },
    view() {
      return nodes(SavedSkinLibrary({ skinActionDisabledReason: '', playerName: 'Player' }));
    },
    async recheck() {
      const timer = timers.entries().next().value;
      assert.ok(timer, 'A queued skin change has a scheduled recheck');
      timers.delete(timer[0]);
      timer[1]();
      await settle();
    },
  };
}

function inlineErrors(h: ReturnType<typeof harness>): unknown[] {
  return h
    .view()
    .filter((node) => node.props.class === 'cp-skin-inline-err')
    .map((node) => node.props.children);
}

function nativeSkinBoundary(invoke: (command: string, args?: Record<string, unknown>) => Promise<unknown>) {
  return source<typeof import('../../src/native')>(
    'native.ts',
    { './dto-contract': source<typeof import('../../src/dto-contract')>('dto-contract.ts', {}) },
    { window: { __TAURI__: { core: { invoke } } }, File },
  );
}

test('native picker validation failures reach the upload hook and wardrobe notice unchanged', async () => {
  for (const message of ['Choose a valid PNG skin file.', 'Skin file is too large; choose a PNG under 256 KiB.']) {
    const h = harness();
    const commands: string[] = [];
    const native = nativeSkinBoundary(async (command) => {
      commands.push(command);
      throw message;
    });
    const { useSavedSkinUploadWorkflow } = source<
      typeof import('../../src/views/accounts/use-saved-skin-upload-workflow')
    >('views/accounts/use-saved-skin-upload-workflow.ts', {
      '../../native': native,
      '../../machines/skin-wardrobe': h.machine,
      './api': h.api,
      './types': { NO_CAPE_VALUE: '__none' },
      './use-saved-skin-upload-drop': { useSavedSkinUploadDrop: () => ({}) },
      'preact/hooks': {
        useEffect() {},
        useRef: <V>(value: V) => ({ current: value }),
        useState: <V>(value: V) => [value, () => {}],
      },
    });
    let browserPickerOpened = false;
    const workflow = useSavedSkinUploadWorkflow();
    workflow.fileInputRef.current = {
      value: '',
      click() {
        browserPickerOpened = true;
      },
    } as HTMLInputElement;
    workflow.openUploadPicker();
    await settle();
    assert.deepEqual(inlineErrors(h), [message]);
    assert.deepEqual(commands, ['pick_skin_file']);
    assert.equal(browserPickerOpened, false);
    assert.deepEqual(h.calls, []);
  }
});

test('native drop refusal becomes an Error usable by the wardrobe read-error consumer', async () => {
  const message = 'Dropped skin file is no longer available. Drop it again.';
  const token = 'a'.repeat(64);
  const native = nativeSkinBoundary(async (command, args) => {
    assert.equal(command, 'consume_skin_drop');
    assert.equal(args?.token, token);
    throw message;
  });
  const h = harness();
  await assert.rejects(native.consumeNativeSkinDrop(token), (error: unknown) => {
    assert.ok(error instanceof Error);
    assert.equal(h.machine.wardrobeErrorMessage(error, 'Could not read dropped skin file.'), message);
    return true;
  });
});

test('unexpected native skin rejection shapes use safe copy and later file reads still succeed', async () => {
  let reject = true;
  let reason: unknown;
  const native = nativeSkinBoundary(async () => {
    if (reject) throw reason;
    return { name: 'admitted.png', bytes: [137, 80, 78, 71] };
  });
  for (reason of [
    '',
    '  ',
    new Error('private transport detail'),
    {
      message: 'private transport detail',
      toString() {
        throw new Error('unexpected coercion');
      },
    },
    null,
  ]) {
    for (const read of [() => native.pickNativeSkinFile(), () => native.consumeNativeSkinDrop('a'.repeat(64))]) {
      await assert.rejects(read(), (error: unknown) => {
        assert.ok(error instanceof Error);
        assert.equal(error.message, 'Could not read skin file.');
        return true;
      });
    }
  }
  reject = false;
  for (const file of [await native.pickNativeSkinFile(), await native.consumeNativeSkinDrop('a'.repeat(64))]) {
    assert.ok(file instanceof File);
    assert.equal(file.name, 'admitted.png');
    assert.deepEqual([...new Uint8Array(await file.arrayBuffer())], [137, 80, 78, 71]);
  }
  assert.equal(await nativeSkinBoundary(async () => null).pickNativeSkinFile(), null);
});

test('a post-import wardrobe refresh rejects an older in-flight skin list without changing account selection', async () => {
  const h = harness();
  h.machine.setWardrobeContext(context());
  await settle();
  const oldRead = deferred<unknown>();
  h.setSkinsRead(() => oldRead.promise);
  const beforeImport = h.machine.refreshWardrobe();
  const imported = { ...skin, name: 'Imported name', texture_key: 'e'.repeat(64) };
  h.setSkinsRead(async () => ({ skins: [imported], pending_apply_texture_key: null }));
  await h.machine.refreshWardrobe();
  oldRead.resolve({ skins: [skin], pending_apply_texture_key: null });
  await beforeImport;
  assert.equal(h.machine.wardrobeData.value.skins[0]?.name, 'Imported name');
  assert.equal(h.machine.wardrobeContext.value.accountKey, `account:${accountA}`);
  assert.equal(h.calls.filter(([method]) => method !== 'GET').length, 0);
  assert.equal(h.timers.size, 0);
});

test('accepted deferred apply exposes its later backend failure in the retained wardrobe', async () => {
  const h = harness();
  h.machine.setWardrobeContext(context());
  await settle();
  await h.machine.applySkin(skin.texture_key);
  assert.deepEqual(h.notices, ['Skin queued.']);
  assert.equal(h.timers.size, 1);
  h.setStatus(pending({ phase: 'failed', texture_key: null, error: failure }));
  await h.recheck();
  assert.deepEqual(inlineErrors(h), [failure]);
  assert.equal(h.view().find((node) => node.type === 'SavedSkinLibraryGrid')?.props.pendingApplyKey, null);
  assert.equal(h.machine.wardrobeData.value.skins[0].applied_at, null);
  assert.equal(h.timers.size, 0);
  assert.equal(h.calls.filter(([method, path]) => method === 'POST' && path.includes('/apply')).length, 1);
});

test('retrying background failure remains visible until cancel clears its status and recheck', async () => {
  const h = harness();
  h.setStatus(pending({ error: 'Minecraft is temporarily unavailable.' }));
  h.machine.setWardrobeContext(context());
  await settle();
  assert.deepEqual(inlineErrors(h), ['Minecraft is temporarily unavailable.']);
  assert.equal(h.timers.size, 1);
  const stale = deferred<unknown>();
  h.setStatusRead(() => stale.promise);
  const oldRefresh = h.machine.refreshWardrobe();
  h.setStatusRead(null);
  await h.machine.cancelPendingApply();
  stale.resolve(pending({ error: failure }));
  await oldRefresh;
  assert.deepEqual(inlineErrors(h), []);
  assert.equal(h.machine.wardrobeData.value.pendingApply?.phase, 'idle');
  assert.equal(h.timers.size, 0);
});

test('account changes discard queued rechecks and in-flight failures, including an A-B-A switch', async () => {
  const h = harness();
  h.setStatus(pending());
  h.machine.setWardrobeContext(context());
  await settle();
  assert.equal(h.timers.size, 1);
  const stale = deferred<unknown>();
  h.setStatusRead(() => stale.promise);
  const oldRefresh = h.machine.refreshWardrobe();
  h.setStatusRead(null);
  h.setStatus(null);
  h.machine.setWardrobeContext(context(accountB));
  await settle();
  assert.equal(h.timers.size, 0);
  h.machine.setWardrobeContext(context());
  await settle();
  stale.resolve(pending({ phase: 'failed', texture_key: null, error: failure }));
  await oldRefresh;
  assert.deepEqual(inlineErrors(h), []);
  assert.equal(h.machine.wardrobeData.value.pendingApply, null);
});

test('a status for another exact account cannot publish its failure or pending marker', async () => {
  const h = harness();
  h.machine.setWardrobeContext(context());
  await settle();
  const before = h.machine.wardrobeData.value;
  h.setStatus(pending({ account_id: accountB, error: failure }));
  await h.machine.refreshWardrobe();
  assert.equal(h.machine.wardrobeData.value, before);
  assert.deepEqual(inlineErrors(h), []);
  assert.equal(h.timers.size, 0);
  assert.equal(h.accountRefreshes, 1);
});

test('late cancellation from the previous account cannot cancel the new account recheck', async () => {
  const h = harness();
  h.setStatus(pending());
  h.machine.setWardrobeContext(context());
  await settle();
  const cancelled = deferred<unknown>();
  h.setCancelRead(() => cancelled.promise);
  const operation = h.machine.cancelPendingApply();
  h.setStatus(pending({ account_id: accountB, generation: 2 }));
  h.machine.setWardrobeContext(context(accountB));
  await settle();
  assert.equal(h.timers.size, 1);
  const reads = h.calls.length;
  cancelled.resolve({ status: 'cleared', view_model: { summary: 'Skin change canceled.' } });
  await operation;
  assert.equal(h.timers.size, 1);
  assert.equal(h.calls.length, reads);
  assert.equal(h.machine.wardrobeData.value.pendingApply?.account_id, accountB);
  assert.deepEqual(h.notices, []);
});

test('malformed or unavailable pending status remains visibly unavailable', async () => {
  const h = harness();
  assert.equal(h.api.pendingSkinStatus(null), null);
  for (const bad of [
    undefined,
    {},
    pending({ phase: 'success' }),
    pending({ generation: -1 }),
    pending({ generation: Number.MAX_SAFE_INTEGER + 1 }),
    pending({ texture_key: null }),
    pending({ phase: 'failed', texture_key: null }),
    { ...pending(), error: undefined },
  ]) {
    assert.equal(h.api.pendingSkinStatus(bad), undefined);
  }
  h.setStatusRead(async () => ({ phase: 'done' }));
  h.machine.setWardrobeContext(context());
  await settle();
  assert.equal(h.machine.wardrobeData.value.state, 'unavailable');
  assert.deepEqual(inlineErrors(h), ['Skin change status returned an invalid response.']);
  h.setStatusRead(async () => {
    throw new Error('Skin status could not be read.');
  });
  await h.machine.refreshWardrobe();
  assert.deepEqual(inlineErrors(h), ['Skin status could not be read.']);
  assert.equal(h.timers.size, 0);
});

test('a failed background status read preserves the queue and recovers its terminal failure', async () => {
  const h = harness();
  h.setStatus(pending());
  h.machine.setWardrobeContext(context());
  await settle();
  h.setStatusRead(async () => {
    throw new Error('Skin status could not be read.');
  });
  await h.recheck();
  assert.equal(h.machine.wardrobeData.value.state, 'ready');
  assert.equal(h.machine.wardrobeData.value.skins[0].texture_key, skin.texture_key);
  assert.equal(h.machine.wardrobeData.value.pendingApply?.texture_key, skin.texture_key);
  assert.deepEqual(inlineErrors(h), ['Skin status could not be read.']);
  assert.equal(h.timers.size, 1);
  h.setStatusRead(null);
  h.setStatus(pending({ phase: 'failed', texture_key: null, error: failure }));
  await h.recheck();
  assert.deepEqual(inlineErrors(h), [failure]);
  assert.equal(h.timers.size, 0);
});

test('repeated status read failures stop after three retries with a visible unresolved queue', async () => {
  const h = harness();
  h.setStatus(pending());
  h.machine.setWardrobeContext(context());
  await settle();
  h.setStatusRead(async () => {
    throw new Error('Skin status could not be read.');
  });
  for (let failure = 0; failure < 4; failure += 1) await h.recheck();
  assert.equal(h.timers.size, 0);
  assert.deepEqual(inlineErrors(h), ['Skin status could not be read.']);
  assert.equal(h.machine.wardrobeData.value.pendingApply?.phase, 'queued');
  assert.equal(h.calls.filter(([method]) => method !== 'GET').length, 0);
});

test('the accepted generation survives failure of the first status read after applying', async () => {
  const h = harness();
  h.machine.setWardrobeContext(context());
  await settle();
  h.setStatusRead(async () => {
    throw new Error('Skin status could not be read.');
  });
  await h.machine.applySkin(skin.texture_key);
  assert.equal(h.machine.wardrobeData.value.pendingApply?.generation, 1);
  assert.equal(h.timers.size, 1);
  assert.deepEqual(inlineErrors(h), ['Skin status could not be read.']);
  h.setStatusRead(null);
  h.setStatus(pending({ phase: 'failed', texture_key: null, error: failure }));
  await h.recheck();
  assert.deepEqual(inlineErrors(h), [failure]);
  assert.equal(h.timers.size, 0);
});

test('terminal status stops rechecking and removes the queued marker even when it retains a texture key', async () => {
  const h = harness();
  h.setStatus(pending());
  h.machine.setWardrobeContext(context());
  await settle();
  h.setStatus(pending({ phase: 'failed', error: failure }));
  await h.recheck();
  assert.deepEqual(inlineErrors(h), [failure]);
  assert.equal(h.view().find((node) => node.type === 'SavedSkinLibraryGrid')?.props.pendingApplyKey, null);
  assert.equal(h.timers.size, 0);
});

test('skin commands bind account and selection, and queue commands bind the displayed generation', async () => {
  const h = harness();
  h.machine.setWardrobeContext(context());
  await settle();
  await h.machine.applySkin(skin.texture_key);
  await h.machine.flushPendingApply();
  h.setStatus(pending({ generation: 8 }));
  await h.machine.refreshWardrobe();
  await h.machine.cancelPendingApply();
  const commands = h.calls.filter(([method]) => method !== 'GET');
  assert.equal(commands.length, 3);
  for (const [, path] of commands) {
    const params = new URL(path, 'http://skin-fixture.invalid').searchParams;
    assert.equal(params.get('expected_account_id'), accountA);
    assert.equal(params.get('expected_selection_revision'), '7');
  }
  assert.equal(new URL(commands[1][1], 'http://skin-fixture.invalid').searchParams.get('expected_generation'), '1');
  assert.equal(new URL(commands[2][1], 'http://skin-fixture.invalid').searchParams.get('expected_generation'), '8');
});

test('a changed selection revision invalidates delayed apply even when the selected account returns to A', async () => {
  const h = harness();
  h.machine.setWardrobeContext(context());
  await settle();
  const capture = h.machine.captureWardrobeContext();
  h.machine.setWardrobeContext(context(accountA, 9));
  await settle();
  await assert.rejects(h.machine.applySavedSkin(skin.texture_key, { capture }), /The account changed/);
  assert.equal(h.calls.filter(([method]) => method !== 'GET').length, 0);
});

test('missing selection or pending generation refuses commands with a visible actionable error', async () => {
  const h = harness();
  h.machine.setWardrobeContext(context(accountA, null));
  await settle();
  await h.machine.applySkin(skin.texture_key);
  assert.deepEqual(inlineErrors(h), ['Refresh the selected account before changing its skin.']);
  h.machine.setWardrobeContext(context());
  await settle();
  await h.machine.cancelPendingApply();
  assert.deepEqual(inlineErrors(h), ['Refresh the queued skin change before trying again.']);
  assert.equal(h.calls.filter(([method]) => method !== 'GET').length, 0);
});

test('an acknowledged cancellation never rearms the old queue after a status read failure', async () => {
  const h = harness();
  h.setStatus(pending());
  h.machine.setWardrobeContext(context());
  await settle();
  h.setStatusRead(async () => {
    throw new Error('Skin status could not be read.');
  });
  await h.machine.cancelPendingApply();
  assert.deepEqual(inlineErrors(h), ['Skin status could not be read.']);
  assert.equal(h.machine.wardrobeData.value.pendingApply, null);
  assert.equal(h.timers.size, 0);
});

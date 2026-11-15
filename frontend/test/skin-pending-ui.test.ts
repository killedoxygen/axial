import assert from 'node:assert/strict';
import test from 'node:test';
import { apiModule, loadSource } from './source';
import type { PendingSkinStatus } from '../src/generated/PendingSkinStatus';
import type { WardrobeContext } from '../src/accounts/wardrobe-state';

const signals = { signal: <V>(value: V) => ({ value }) };

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
  variant: 'classic' as const,
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
    due_in_ms: 10_000,
    error: null,
    ...overrides,
  };
}

function context(accountId = accountA, selectionRevision: number | null = 7): WardrobeContext {
  return { accountKey: `account:${accountId}`, selectionRevision, skinActionsEnabled: true, account: null };
}

function profileContext(selectionRevision = 7): WardrobeContext {
  return {
    ...context(accountA, selectionRevision),
    account: {
      account_id: accountA,
      display_name: 'Player',
      profile_skin: { id: 'profile-skin', variant: 'classic', url: 'https://textures.minecraft.net/texture/abc' },
      profile_cape_id: 'profile-cape',
      minecraft_profile: {
        id: accountA,
        name: 'Player',
        skins: [],
        capes: [{ id: 'profile-cape', state: 'ACTIVE', url: 'https://textures.minecraft.net/texture/def' }],
      },
    } as unknown as WardrobeContext['account'],
  };
}

type ViewNode = { type: unknown; props: Record<string, unknown> };

function nodes(tree: unknown): ViewNode[] {
  if (Array.isArray(tree)) return tree.flatMap(nodes);
  if (!tree || typeof tree !== 'object' || !('props' in tree)) return [];
  const node = tree as ViewNode;
  return [node, ...nodes(node.props.children)];
}

function harness(
  native = {
    exportNativeSavedSkinPng: async (_skin: typeof skin): Promise<'published' | 'cancelled' | undefined> => undefined,
  },
) {
  let nextTimer = 0;
  const timers = new Map<number, () => void>();
  const calls: [string, string][] = [];
  const profileSaves: { path: string; body: string }[] = [];
  const notices: string[] = [];
  const preferences = new Map<string, string>();
  let accountRefreshes = 0;
  let status: PendingSkinStatus | null = null;
  let skinsRead: (() => Promise<unknown>) | null = null;
  let statusRead: (() => Promise<unknown>) | null = null;
  let cancelRead: (() => Promise<unknown>) | null = null;
  let normalize: (body: unknown) => Promise<unknown> = async () => {
    throw new Error('Unexpected skin normalization');
  };
  let confirm: () => Promise<boolean> = async () => true;
  const transport = {
    async request(method: string, path: string, body?: unknown) {
      calls.push([method, path]);
      const url = new URL(path, 'http://skin-fixture.invalid');
      if (method === 'GET' && url.pathname === '/skins')
        return skinsRead ? skinsRead() : { skins: [skin], pending: statusRead ? await statusRead() : status };
      if (method === 'POST' && url.pathname === '/skins/from-profile') {
        profileSaves.push({ path, body: JSON.stringify(body) });
        return skin;
      }
      if (method === 'POST' && url.pathname === '/skins/normalize') return normalize(body);
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
      if (method === 'POST' && (url.pathname === '/skin/profile/reset' || url.pathname === '/skin/cape/reset')) {
        return { status: 'reset', profile_updated: true, view_model: { summary: 'Profile reset.' } };
      }
      throw new Error(`Unexpected skin request: ${method} ${path}`);
    },
    isApiError: () => false,
    apiResourceUrl: (path: string) => path,
  };
  const defaults = { DEFAULT_SKINS: [{ id: 'steve', name: 'Steve', variant: 'classic', src: 'fixture.png' }] };
  const api = apiModule<typeof import('../src/accounts/api')>('accounts/api.ts', transport.request);
  const machine = loadSource<typeof import('../src/accounts/wardrobe-state')>(
    'accounts/wardrobe-state.ts',
    {
      '@preact/signals': signals,
      'accounts/default-skins': defaults,
      'accounts/api': { ...api, exportNativeSavedSkinPng: native.exportNativeSavedSkinPng },
      'app/toast': { toast: (message: string) => notices.push(message) },
      'ui/Dialog': { showConfirm: () => confirm() },
      'accounts/state': {
        refreshAccountsData: async () => {
          accountRefreshes += 1;
        },
      },
      'accounts/player-skin': {
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
    ['CapePicker', 'SkinEditDialog', 'SkinFinder', 'SkinSnapshotImg', 'SkinStage', 'SkinUploadDialog', 'WearTile'].map(
      (name) => [`accounts/${name}`, { [name]: name }],
    ),
  );
  primitives['accounts/WearTile'] = { WearTile: 'WearTile', WearGrid: 'WearGrid' };
  const { Wardrobe } = loadSource<typeof import('../src/accounts/Wardrobe')>('accounts/Wardrobe.tsx', {
    ...primitives,
    'accounts/default-skins': defaults,
    'accounts/wardrobe-state': machine,
    'accounts/player-skin': { profileCape: () => null },
    'ui/Atoms': { Notice: 'Notice', Status: 'Status' },
    'ui/Icons': { Icon: 'Icon' },
    'accounts/api': api,
    'accounts/types': { NO_CAPE_VALUE: '__none' },
    'accounts/skin-menu': { savedSkinMenuItems: () => [] },
    'accounts/use-palette-fit': { usePaletteFit: () => [Infinity, Infinity] },
    'accounts/use-saved-skin-edit-workflow': { useSavedSkinEditWorkflow: () => ({ editReplacementDrop: {} }) },
    'accounts/use-saved-skin-lookup-workflow': { useSavedSkinLookupWorkflow: () => ({}) },
    'accounts/use-saved-skin-upload-workflow': { useSavedSkinUploadWorkflow: () => ({ uploadDrop: {} }) },
    'accounts/use-saved-skin-native-drag-drop': { useSavedSkinNativeDragDrop() {} },
    'preact/hooks': { useRef: <V>(value: V) => ({ current: value }), useMemo: <V>(read: () => V) => read() },
    'preact/jsx-runtime': {
      jsx: (type: unknown, props: Record<string, unknown>) => ({ type, props }),
      jsxs: (type: unknown, props: Record<string, unknown>) => ({ type, props }),
      Fragment: 'Fragment',
    },
  });
  return {
    machine,
    api,
    timers,
    calls,
    profileSaves,
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
    setNormalize(read: (body: unknown) => Promise<unknown>) {
      normalize = read;
    },
    setConfirm(read: () => Promise<boolean>) {
      confirm = read;
    },
    get accountRefreshes() {
      return accountRefreshes;
    },
    view() {
      return nodes(Wardrobe({ skinActionDisabledReason: '' }));
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
    .filter((node) => node.type === 'Notice' && node.props.tone === 'err')
    .map((node) => (node.props.children as ViewNode).props.children);
}

function nativeSkinBoundary(invoke: (command: string, args?: Record<string, unknown>) => Promise<unknown>) {
  const globals = { window: { __TAURI__: { core: { invoke } } }, File };
  const native = loadSource<typeof import('../src/platform/native')>('platform/native.ts', {}, globals);
  const api = loadSource<typeof import('../src/accounts/api')>(
    'accounts/api.ts',
    { 'platform/transport': {} },
    globals,
  );
  return { ...native, exportNativeSavedSkinPng: api.exportNativeSavedSkinPng };
}

test('native picker validation failures reach the upload hook and wardrobe notice unchanged', async () => {
  for (const message of ['Choose a valid PNG skin file.', 'Skin file is too large; choose a PNG under 256 KiB.']) {
    const h = harness();
    const commands: string[] = [];
    const native = nativeSkinBoundary(async (command) => {
      commands.push(command);
      throw message;
    });
    const hooks = {
      useEffect() {},
      useRef: <V>(value: V) => ({ current: value }),
      useState: <V>(value: V) => [value, () => {}],
    };
    const { useSavedSkinUploadWorkflow } = loadSource<typeof import('../src/accounts/use-saved-skin-upload-workflow')>(
      'accounts/use-saved-skin-upload-workflow.ts',
      {
        'accounts/wardrobe-state': h.machine,
        'accounts/api': h.api,
        'accounts/types': { NO_CAPE_VALUE: '__none' },
        'accounts/use-png-drop': { usePngDrop: () => ({}) },
        'accounts/use-staged-skin-png': loadSource('accounts/use-staged-skin-png.ts', {
          'platform/native': native,
          'accounts/wardrobe-state': h.machine,
          'accounts/api': h.api,
          'preact/hooks': hooks,
        }),
        'preact/hooks': hooks,
      },
    );
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

/** Keeps hook state across renders so a workflow observes its own earlier updates; effects mount once. */
function hookRuntime() {
  const slots: unknown[] = [];
  const mountEffects: (() => void | (() => void))[] = [];
  const cleanups: (() => void)[] = [];
  let cursor = 0;
  let mounted = false;
  let writes = 0;
  const hooks = {
    useRef<V>(initial: V) {
      const index = cursor++;
      if (!(index in slots)) slots[index] = { current: initial };
      return slots[index] as { current: V };
    },
    useState<V>(initial: V): [V, (next: V | ((current: V) => V)) => void] {
      const index = cursor++;
      if (!(index in slots)) slots[index] = initial;
      return [
        slots[index] as V,
        (next) => {
          writes += 1;
          slots[index] = typeof next === 'function' ? (next as (current: V) => V)(slots[index] as V) : next;
        },
      ];
    },
    useEffect(run: () => void | (() => void), dependencies: unknown[]) {
      cursor++;
      assert.equal(dependencies.length, 0, 'Staged skin PNG effects run once per mount');
      if (!mounted) mountEffects.push(run);
    },
  };
  return {
    hooks,
    render<T>(hook: () => T): T {
      cursor = 0;
      const result = hook();
      if (!mounted) {
        mounted = true;
        for (const run of mountEffects.splice(0)) {
          const cleanup = run();
          if (cleanup) cleanups.push(cleanup);
        }
      }
      return result;
    },
    unmount() {
      for (const cleanup of cleanups.splice(0)) cleanup();
    },
    get writes() {
      return writes;
    },
  };
}

/** The production staging owner over the harness transport, with each normalization answered by the test. */
function stagedSkinPngFixture(h: ReturnType<typeof harness>) {
  const runtime = hookRuntime();
  const revoked: string[] = [];
  const normalizations = new Map<string, ReturnType<typeof deferred<unknown>>>();
  h.setNormalize((body) => {
    const request = deferred<unknown>();
    normalizations.set((body as File).name, request);
    return request.promise;
  });
  const staged = loadSource<typeof import('../src/accounts/use-staged-skin-png')>(
    'accounts/use-staged-skin-png.ts',
    {
      'platform/native': { pickNativeSkinFile: async () => null },
      'accounts/wardrobe-state': h.machine,
      'accounts/api': h.api,
      'preact/hooks': runtime.hooks,
    },
    {
      URL: {
        createObjectURL: (file: File) => `blob:${file.name}`,
        revokeObjectURL: (url: string) => revoked.push(url),
      },
    },
  );
  const stubs = {
    'accounts/wardrobe-state': h.machine,
    'accounts/api': h.api,
    'accounts/types': { NO_CAPE_VALUE: '__none' },
    'accounts/use-png-drop': { usePngDrop: () => ({}) },
    'accounts/use-staged-skin-png': staged,
    'preact/hooks': runtime.hooks,
  };
  return {
    runtime,
    revoked,
    stubs,
    png: (name: string) => new File([new Uint8Array([137, 80, 78, 71])], name, { type: 'image/png' }),
    async answer(name: string, outcome: { variant: 'classic' | 'slim' } | { error: string }) {
      const request = normalizations.get(name);
      assert.ok(request, `${name} was sent for normalization`);
      request.resolve(
        'error' in outcome
          ? Promise.reject(new Error(outcome.error))
          : {
              texture_key: 'f'.repeat(64),
              variant_suggestion: outcome.variant,
              normalized_data_url: `data:image/png;base64,${name}`,
            },
      );
      await settle();
    },
  };
}

test('a staged upload keeps only its latest PNG, revokes each preview once and ignores late validation', async () => {
  const h = harness();
  h.machine.setWardrobeContext(context());
  await settle();
  const f = stagedSkinPngFixture(h);
  const { useSavedSkinUploadWorkflow } = loadSource<typeof import('../src/accounts/use-saved-skin-upload-workflow')>(
    'accounts/use-saved-skin-upload-workflow.ts',
    f.stubs,
  );
  const render = () => f.runtime.render(useSavedSkinUploadWorkflow);
  const input = { value: 'chosen.png' } as HTMLInputElement;
  render().fileInputRef.current = input;

  render().stageUploadFile(new File(['text'], 'notes.txt', { type: 'text/plain' }));
  assert.equal(render().stagedUpload, null);
  assert.equal(input.value, '');
  assert.deepEqual(inlineErrors(h), ['Upload a PNG skin file.']);

  render().stageUploadFile(f.png('first.png'));
  render().setStagedCapeId('cape-a');
  render().stageUploadFile(f.png('second.png'));
  assert.deepEqual(inlineErrors(h), []);
  assert.deepEqual(f.revoked, ['blob:first.png']);
  assert.equal(render().stagedCapeId, '__none');
  await f.answer('first.png', { variant: 'slim' });
  assert.equal(render().stagedUpload?.objectUrl, 'blob:second.png');
  assert.equal(render().stagedUpload?.normalizeStatus, 'checking');
  assert.equal(render().stagedCanSave, false);

  await f.answer('second.png', { variant: 'slim' });
  const ready = render();
  assert.equal(ready.stagedUpload?.normalizeStatus, 'ready');
  assert.equal(ready.stagedUpload?.normalizedDataUrl, 'data:image/png;base64,second.png');
  assert.equal(ready.stagedVariant, 'slim');
  assert.equal(ready.stagedCanSave, true);
  ready.setUploadVariant('classic');
  assert.equal(render().stagedVariant, 'classic');

  render().stageUploadFile(f.png('third.png'));
  input.value = 'third.png';
  render().clearStagedUpload();
  assert.equal(input.value, '');
  await f.answer('third.png', { error: 'Skin PNG must be 64x64.' });
  assert.equal(render().stagedUpload, null);

  render().stageUploadFile(f.png('fourth.png'));
  await f.answer('fourth.png', { error: 'Skin PNG must be 64x64.' });
  assert.equal(render().stagedUpload?.normalizeError, 'Skin PNG must be 64x64.');
  assert.equal(render().stagedCanSave, false);

  render().stageUploadFile(f.png('fifth.png'));
  f.runtime.unmount();
  const writes = f.runtime.writes;
  await f.answer('fifth.png', { error: 'Skin PNG must be 64x64.' });
  assert.equal(f.runtime.writes, writes);
  assert.deepEqual(f.revoked, [
    'blob:first.png',
    'blob:second.png',
    'blob:third.png',
    'blob:fourth.png',
    'blob:fifth.png',
  ]);
  assert.deepEqual(
    h.calls.filter(([method]) => method !== 'GET'),
    ['first', 'second', 'third', 'fourth', 'fifth'].map(() => ['POST', '/skins/normalize']),
  );
});

test('an edit replacement adopts only its current detected model and a closed edit ignores late validation', async () => {
  const h = harness();
  h.machine.setWardrobeContext(context());
  await settle();
  const f = stagedSkinPngFixture(h);
  const { useSavedSkinEditWorkflow } = loadSource<typeof import('../src/accounts/use-saved-skin-edit-workflow')>(
    'accounts/use-saved-skin-edit-workflow.ts',
    {
      ...f.stubs,
      'app/toast': { toast() {} },
      'ui/Dialog': { showConfirm: async () => true },
    },
  );
  const render = () => f.runtime.render(useSavedSkinEditWorkflow);

  render().stageEditReplacementFile(f.png('unopened.png'));
  assert.equal(render().editReplacement, null);
  assert.deepEqual(f.revoked, []);

  assert.equal(await render().startEdit(skin), true);
  render().stageEditReplacementFile(f.png('first.png'));
  render().stageEditReplacementFile(f.png('second.png'));
  await f.answer('first.png', { variant: 'slim' });
  assert.equal(render().editVariant, 'classic');
  assert.equal(render().editReplacementReady, false);

  await f.answer('second.png', { variant: 'slim' });
  const ready = render();
  assert.equal(ready.editVariant, 'slim');
  assert.equal(ready.editReplacementReady, true);
  assert.equal(ready.editReplacement?.normalizedDataUrl, 'data:image/png;base64,second.png');
  ready.setEditVariant('classic');
  assert.equal(render().editVariant, 'classic');
  assert.equal(render().savedSkinEditHasChanges(skin), true);

  render().clearEditReplacement();
  assert.equal(render().savedSkinEditHasChanges(skin), false);

  render().stageEditReplacementFile(f.png('third.png'));
  render().cancelEdit();
  await f.answer('third.png', { variant: 'slim' });
  const closed = render();
  assert.equal(closed.editKey, null);
  assert.equal(closed.editReplacement, null);
  assert.equal(closed.editVariant, 'classic');
  assert.deepEqual(f.revoked, ['blob:first.png', 'blob:second.png', 'blob:third.png']);
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
  assert.equal(
    h.view().some((node) => node.type === 'WearTile' && node.props.queued === true),
    false,
  );
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

test('an unreadable wardrobe remains visibly unavailable', async () => {
  const h = harness();
  h.setSkinsRead(async () => {
    throw new Error('Skin status could not be read.');
  });
  h.machine.setWardrobeContext(context());
  await settle();
  assert.equal(h.machine.wardrobeData.value.state, 'unavailable');
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
  assert.equal(
    h.view().some((node) => node.type === 'WearTile' && node.props.queued === true),
    false,
  );
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

test('profile reset commands bind the confirmed account and selection', async () => {
  for (const [action, route] of [
    ['resetProfileSkin', '/skin/profile/reset'],
    ['resetProfileCape', '/skin/cape/reset'],
  ] as const) {
    const h = harness();
    h.machine.setWardrobeContext(profileContext());
    await settle();
    h.calls.length = 0;
    await h.machine[action]();
    const commands = h.calls.filter(([method]) => method !== 'GET');
    assert.equal(commands.length, 1);
    const url = new URL(commands[0][1], 'http://skin-fixture.invalid');
    assert.equal(url.pathname, route);
    assert.equal(url.searchParams.get('expected_account_id'), accountA);
    assert.equal(url.searchParams.get('expected_selection_revision'), '7');
    assert.equal(h.machine.wardrobeNotice.value, null);
  }
});

test('automatic seeding and manual profile save serialize the displayed account and selection with the profile options', async () => {
  const h = harness();
  h.machine.setWardrobeContext(profileContext());
  await settle();
  const assertSaved = (label: string): void => {
    assert.equal(h.profileSaves.length, 1, label);
    const { path, body } = h.profileSaves[0];
    const url = new URL(path, 'http://skin-fixture.invalid');
    assert.equal(url.pathname, '/skins/from-profile', label);
    assert.equal(url.searchParams.get('expected_account_id'), accountA, label);
    assert.equal(url.searchParams.get('expected_selection_revision'), '7', label);
    assert.deepEqual(JSON.parse(body), { mark_current: true }, label);
  };
  assertSaved('automatic');
  h.profileSaves.length = 0;
  await h.machine.saveProfileSkinLocally();
  assertSaved('manual');
});

for (const missingTarget of [false, true]) {
  test(`onboarding profile seed ${missingTarget ? 'skips a missing target and completes' : 'keeps its pre-save selection while config is pending'}`, async () => {
    const savedConfig = deferred<void>();
    const calls: { method: string; path: string; body: string }[] = [];
    const accountsSnapshot = {
      value: {
        state: 'ready',
        accounts: missingTarget ? [] : [{ account_id: accountA, kind: 'microsoft', active: true }],
        selection_revision: missingTarget ? null : Number.MAX_SAFE_INTEGER,
      },
    };
    const config = { value: { revision: 7, onboarding_done: false } };
    const call = async (method: string, path: string, body?: unknown) => {
      calls.push({ method, path, body: JSON.stringify(body) });
      if (path.startsWith('/skins/from-profile')) throw new Error('Stale fixture selection');
      if (method === 'POST' && path === '/onboarding/complete') {
        config.value.onboarding_done = true;
        return { status: 'ok' };
      }
      if (method === 'GET' && path === '/config') return config.value;
      throw new Error(`Unexpected onboarding request: ${method} ${path}`);
    };
    const { finishOnboarding } = loadSource<typeof import('../src/onboarding/finish')>(
      'onboarding/finish.ts',
      {
        'accounts/default-skins': { DEFAULT_SKINS: [{ id: 'steve' }] },
        'settings/api': apiModule('settings/api.ts', call),
        'settings/autosave': { saveConfigPatch: () => savedConfig.promise },
        'accounts/state': { accountsSnapshot },
        'music/music': { Music: { applyConfig() {} } },
        'accounts/player-skin': { refreshAccountSkin() {} },
        'settings/state': { config, systemInfo: { value: null } },
        'ui/MemoryField': { getMemoryRecommendation: () => ({ rec: 4 }) },
        'accounts/api': apiModule('accounts/api.ts', call),
      },
      { console: { warn() {} } },
    );
    const finished = finishOnboarding({ kind: 'microsoft' }, { music: false, discord: false, telemetry: false });
    assert.equal(calls.length, 0);
    accountsSnapshot.value = {
      ...accountsSnapshot.value,
      accounts: [{ account_id: accountB, kind: 'microsoft', active: true }],
      selection_revision: 3,
    };
    savedConfig.resolve();
    await finished;
    const seeds = calls.filter((call) => call.path.startsWith('/skins/from-profile'));
    assert.equal(seeds.length, missingTarget ? 0 : 1);
    if (!missingTarget) {
      const url = new URL(seeds[0].path, 'http://skin-fixture.invalid');
      assert.equal(url.searchParams.get('expected_account_id'), accountA);
      assert.equal(url.searchParams.get('expected_selection_revision'), String(Number.MAX_SAFE_INTEGER));
      assert.deepEqual(JSON.parse(seeds[0].body), { mark_current: true });
    }
    assert.equal(calls.filter((call) => call.method === 'POST' && call.path === '/onboarding/complete').length, 1);
  });
}

test('cancelled or stale profile reset confirmations never send a reset', async () => {
  for (const action of ['resetProfileSkin', 'resetProfileCape'] as const) {
    for (const outcome of ['cancel', 'switch', 'reselect'] as const) {
      const h = harness();
      h.machine.setWardrobeContext(profileContext());
      await settle();
      h.calls.length = 0;
      const confirmation = deferred<boolean>();
      h.setConfirm(() => confirmation.promise);
      const resetting = h.machine[action]();
      if (outcome !== 'cancel') h.machine.setWardrobeContext(context(accountB, 8));
      if (outcome === 'reselect') h.machine.setWardrobeContext(profileContext(9));
      confirmation.resolve(outcome !== 'cancel');
      await resetting;
      assert.equal(h.calls.filter(([method, path]) => method === 'POST' && path.startsWith('/skin/')).length, 0);
    }
  }
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

test('native Download PNG waits for settled publication and cannot replay or fetch a blob on an unknown response', async () => {
  const result = deferred<unknown>();
  const commands: string[] = [];
  const native = nativeSkinBoundary(async (command, args) => {
    commands.push(command);
    assert.equal(JSON.stringify(args?.expected), JSON.stringify(skin));
    return result.promise;
  });
  const h = harness(native);
  const download = h.machine.downloadSavedSkin(skin);
  await settle();
  assert.equal(h.machine.wardrobeOp.value?.kind, 'download');
  assert.deepEqual(h.notices, []);
  await h.machine.downloadSavedSkin(skin);
  assert.deepEqual(commands, ['export_saved_skin_png']);
  result.resolve('published');
  await download;
  assert.deepEqual(h.notices, ['Skin PNG downloaded']);
  assert.deepEqual(h.calls, []);

  for (const payload of ['cancelled', 'unexpected']) {
    const unknown = harness(nativeSkinBoundary(async () => payload));
    await unknown.machine.downloadSavedSkin(skin);
    assert.deepEqual(unknown.notices, []);
    assert.deepEqual(unknown.calls, []);
    if (payload === 'cancelled') assert.equal(unknown.machine.wardrobeNotice.value, null);
    else assert.match(unknown.machine.wardrobeNotice.value ?? '', /outcome is unknown/);
  }
  let attempts = 0;
  const lost = harness(
    nativeSkinBoundary(async () => {
      attempts += 1;
      throw new Error('transport disappeared');
    }),
  );
  await lost.machine.downloadSavedSkin(skin);
  assert.equal(attempts, 1);
  assert.deepEqual(lost.calls, []);
  assert.deepEqual(lost.notices, []);
  assert.match(lost.machine.wardrobeNotice.value ?? '', /outcome is unknown/);
  const refused = harness(
    nativeSkinBoundary(async () => {
      throw { status: 'refused', message: 'A file already exists at that destination. Choose a new filename.' };
    }),
  );
  await refused.machine.downloadSavedSkin(skin);
  assert.match(refused.machine.wardrobeNotice.value ?? '', /already exists/);
  const transportString = harness(
    nativeSkinBoundary(async () => {
      throw 'channel closed';
    }),
  );
  await transportString.machine.downloadSavedSkin(skin);
  assert.match(transportString.machine.wardrobeNotice.value ?? '', /outcome is unknown/);
});

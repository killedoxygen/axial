import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import * as core from '../../src/dto-core';
import * as installItems from '../../src/install-item';
import * as launchPresenters from '../../src/launch-presenters';
import { minecraftVersionLabel } from '../../src/version-display';
import type { EnrichedInstance } from '../../src/types-instance';
import type { LaunchSession } from '../../src/types-launch';
import type { LaunchState } from '../../src/store';
import type { ActiveDownload, DownloadFailure } from '../../src/machines/downloads';
import type { InstallQueueInstallItemViewModel, InstallQueuedItemViewModel } from '../../src/types-install';
import type { LoaderComponentId } from '../../src/types-loader';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const requireDependency = createRequire(resolve(frontend, 'package.json'));
const ts: typeof import('typescript') = requireDependency('typescript');
const { enrichedInstanceResponse } = core;

function harness() {
  const downloads = {
    activeDownload: { value: null as ActiveDownload | null },
    downloadFailure: { value: null as DownloadFailure | null },
    downloadQueue: { value: { items: [] as InstallQueuedItemViewModel[] } },
  };
  const filename = resolve(frontend, 'src/instance-install-status.ts');
  const compiled = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.CommonJS },
  });
  const exports = {};
  vm.runInNewContext(
    compiled.outputText,
    {
      exports,
      require(id: string): unknown {
        if (id === './machines/downloads') return downloads;
        if (id === './install-item') return installItems;
        if (id === './version-display') return { minecraftVersionLabel };
        throw new Error(`Unreviewed instance install dependency: ${id}`);
      },
    },
    { filename },
  );
  return { downloads, ...(exports as typeof import('../../src/instance-install-status')) };
}

function target(component: LoaderComponentId): InstallQueueInstallItemViewModel {
  return {
    version_id: `opaque-installed-${component}`,
    loader: {
      component_id: component,
      build_id: `opaque-build-${component}`,
      minecraft_version: '1.21.4',
      loader_version: 'exact-build.7',
    },
  };
}

function instance(installTarget: InstallQueueInstallItemViewModel | null) {
  return {
    id: 'registered-instance',
    name: 'Survival',
    version_id: installTarget?.version_id ?? 'unavailable-loader',
    created_at: '2026-09-27T09:00:00Z',
    java_selection: { kind: 'inherited' as const },
    revision: 1,
    version_display: {
      loader_key: 'fabric',
      loader_label: 'Fabric',
      minecraft_label: '1.21.4',
      loader_version_label: '',
      loader_detail_label: 'Fabric',
      summary_label: '1.21.4, Fabric',
      supports_mods: true,
    },
    launchable: false,
    launch_action: {
      state_id: installTarget ? 'install' : 'blocked',
      label: installTarget ? 'Install' : 'Unavailable',
      tone: 'warn',
      launchable: false,
      primary_action: installTarget ? 'install' : 'blocked',
    },
    needs_install: installTarget?.version_id ?? 'unavailable-loader',
    install_target: installTarget,
    saves_count: 0,
    mods_count: 0,
    resource_count: 0,
    shader_count: 0,
  };
}

function queued(installTarget: InstallQueueInstallItemViewModel): InstallQueuedItemViewModel {
  return {
    queue_id: 'queued-loader',
    state_id: 'queued',
    kind: 'loader',
    title: 'Loader',
    label: 'Loader',
    summary: 'Queued',
    detail: 'Waiting',
    position: 1,
    total: 1,
    install_item: installTarget,
    remove_action: { action: 'remove_from_queue', label: 'Remove', enabled: true },
  };
}

function failed(installTarget: InstallQueueInstallItemViewModel): DownloadFailure {
  return {
    item: installItems.installItemFromQueueInstallItem(installTarget),
    displayName: 'Loader',
    failedAt: 1,
    viewModel: {
      state_id: 'failed',
      title: 'Install failed',
      tone: 'err',
      summary: 'Try again',
      details: [],
      retry_action: { action: 'retry', label: 'Retry', enabled: true },
      dismiss_action: { action: 'dismiss', label: 'Dismiss', enabled: true },
    },
  };
}

for (const component of [
  'net.fabricmc.fabric-loader',
  'org.quiltmc.quilt-loader',
  'net.minecraftforge',
  'net.neoforged',
] as const) {
  for (const outcome of ['failed', 'removed'] as const) {
    test(`${component} ${outcome} install retries its exact build after restart without an installed version`, () => {
      const installTarget = target(component);
      const wire = instance(installTarget);
      const h = harness();
      const parsed = enrichedInstanceResponse(wire);
      if (outcome === 'failed') {
        h.downloads.downloadFailure.value = failed(installTarget);
        assert.equal(h.instanceInstallStatus(parsed, undefined).state, 'failed');
        h.downloads.downloadFailure.value = null;
      } else {
        h.downloads.downloadQueue.value.items = [queued(installTarget)];
        assert.equal(h.instanceInstallStatus(parsed, undefined).state, 'queued');
        h.downloads.downloadQueue.value.items = [];
      }
      // Reloaded registry response and an empty queue are sufficient. The
      // frontend never decodes the opaque installed/build identities.
      const restarted = harness();
      const refreshed = enrichedInstanceResponse(JSON.parse(JSON.stringify(wire)));
      const status = restarted.instanceInstallStatus(refreshed, undefined);
      assert.equal(status.state, 'idle');
      assert.equal(status.installing, false);
      assert.ok(status.item?.loader);
      assert.equal(status.item.loader.minecraftVersion, '1.21.4');
      assert.deepEqual(installItems.installQueueRequestFromItem(status.item), {
        kind: 'loader',
        component_id: component,
        build_id: `opaque-build-${component}`,
      });
    });
  }
}

test('an explicit unavailable target cannot become a vanilla install or inherit a stale failure', () => {
  const h = harness();
  const wire = instance(null);
  const stale = target('net.fabricmc.fabric-loader');
  stale.version_id = wire.version_id;
  h.downloads.downloadFailure.value = failed(stale);
  h.downloads.downloadQueue.value.items = [queued(stale)];
  const parsed = enrichedInstanceResponse(wire);
  assert.equal(parsed.install_target, null);
  assert.equal(parsed.launch_action.primary_action, 'blocked');
  assert.equal(h.installItemForInstance(parsed, undefined), null);
  const status = h.instanceInstallStatus(parsed, undefined);
  assert.equal(status.item, null);
  assert.equal(status.target, '');
  assert.equal(status.failure, null);
  assert.equal(status.state, 'idle');
});

test('an authoritative loader target does not match another build with the same version label', () => {
  const h = harness();
  const expected = target('net.fabricmc.fabric-loader');
  const other = structuredClone(expected);
  other.loader!.build_id = 'different-build';
  h.downloads.downloadFailure.value = failed(other);
  h.downloads.downloadQueue.value.items = [queued(other)];
  const status = h.instanceInstallStatus(enrichedInstanceResponse(instance(expected)), undefined);
  assert.equal(status.state, 'idle');
  assert.ok(status.item);
  assert.deepEqual(installItems.installQueueRequestFromItem(status.item), {
    kind: 'loader',
    component_id: 'net.fabricmc.fabric-loader',
    build_id: expected.loader!.build_id,
  });
});

test('the instance boundary validates install targets and preserves absent compatibility fields', () => {
  const wire = instance(target('net.fabricmc.fabric-loader'));
  assert.throws(() => enrichedInstanceResponse({ ...wire, install_target: { version_id: 42 } }), /Install version/);
  assert.throws(
    () =>
      enrichedInstanceResponse({
        ...wire,
        install_target: { ...wire.install_target, loader: { ...wire.install_target?.loader, build_id: 42 } },
      }),
    /Loader build/,
  );
  const { install_target: _omitted, ...oldWire } = wire;
  const parsed = enrichedInstanceResponse(oldWire);
  assert.equal(parsed.install_target, undefined);
  const h = harness();
  const item = h.installItemForInstance({ version_id: '1.21.4' }, undefined);
  assert.ok(item);
  assert.deepEqual(installItems.installQueueRequestFromItem(item), { kind: 'vanilla', version_id: '1.21.4' });
});

function detailHarness(inst: EnrichedInstance) {
  const h = harness();
  const store = {
    config: { value: null },
    instances: { value: [inst] as EnrichedInstance[] },
    launchSessions: { value: {} as Record<string, LaunchSession> },
    launchNotices: { value: {} },
    launchState: { value: { status: 'idle' } as LaunchState },
    versionById: () => undefined,
  };
  const reads: string[] = [];
  let read = async (): Promise<unknown> => inst;
  const values: unknown[] = [];
  let cursor = 0;
  let effects: Array<() => void | (() => void)> = [];
  const cleanups: Array<() => void> = [];
  const readinessFile = resolve(frontend, 'src/instance-readiness.ts');
  const readinessExports = {};
  vm.runInNewContext(ts.transpileModule(readFileSync(readinessFile, 'utf8'), {
    compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.CommonJS },
  }).outputText, {
    exports: readinessExports,
    require(id: string): unknown {
      if (id === './api') return { api: async (_method: string, path: string) => { reads.push(path); return read(); } };
      if (id === './dto-core') return core;
      if (id === './store') return store;
      if (id === './utils') return { showError: assert.fail };
      throw new Error(`Unreviewed readiness dependency: ${id}`);
    },
  }, { filename: readinessFile });
  const imports: Record<string, unknown> = {
    'preact/hooks': {
      useState: <T>(initial: T | (() => T)) => {
        const index = cursor++;
        if (!(index in values)) values[index] = typeof initial === 'function' ? (initial as () => T)() : initial;
        return [values[index], (next: T | ((current: T) => T)) => {
          values[index] = typeof next === 'function' ? (next as (current: T) => T)(values[index] as T) : next;
        }];
      },
      useRef: <T>(value: T) => { const index = cursor++; return values[index] ??= { current: value }; },
      useEffect(run: () => void | (() => void)) { effects.push(run); },
    },
    'preact/jsx-runtime': requireDependency('preact/jsx-runtime'),
    '../../ui/Icons': { Icon: 'Icon' },
    '../../ui/Atoms': { Button: 'Button', IconButton: 'IconButton', Pill: 'Pill' },
    '../../ui/InstanceVisual': { InstanceTile: 'InstanceTile', guardedInstanceHue: () => 140 },
    '../../ui/ContextMenu': {},
    '../../store': store,
    '../../ui-state': {},
    '../../actions': {},
    '../../launch': {},
    '../../machines/downloads': h.downloads,
    '../../utils': { errMessage: (error: Error) => error.message },
    '../../format': { formatDate: () => 'Today', fmtRelative: () => 'Never' },
    '../../instance-install-status': h,
    '../../instance-setup': {},
    '../../instance-readiness': readinessExports,
    '../../launch-presenters': launchPresenters,
    './resources': { fetchInstanceResources: async () => { throw new Error('Resources unavailable'); } },
    './logs': {},
    './instance-actions': {},
    '../../hooks/use-theme': { useTheme: () => ({}) },
    './components/launch': {
      LaunchSplitButton: 'LaunchSplitButton',
      InstallBarrierPane: 'InstallBarrierPane',
      LaunchOutcomeNotice: 'LaunchOutcomeNotice',
    },
    ...Object.fromEntries(
      ['Mods', 'Worlds', 'Screenshots', 'Logs', 'Settings'].map((name) => [
        `./tabs/${name}Pane`,
        { [`${name}Pane`]: `${name}Pane` },
      ]),
    ),
  };
  const filename = resolve(frontend, 'src/views/instance/InstanceDetailView.tsx');
  const compiled = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: {
      target: ts.ScriptTarget.ES2020,
      module: ts.ModuleKind.CommonJS,
      jsx: ts.JsxEmit.ReactJSX,
      jsxImportSource: 'preact',
    },
  });
  const exports = {};
  vm.runInNewContext(
    compiled.outputText,
    {
      exports,
      require(id: string) {
        if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
        throw new Error(`Unreviewed instance view dependency: ${id}`);
      },
    },
    { filename },
  );
  const { InstanceDetailView } = exports as typeof import('../../src/views/instance/InstanceDetailView');
  type Node = { type: unknown; props: Record<string, unknown> };
  function nodes(value: unknown): Node[] {
    if (Array.isArray(value)) return value.flatMap(nodes);
    if (!value || typeof value !== 'object' || !('props' in value)) return [];
    const node = value as Node;
    return [node, ...nodes(node.props.children)];
  }
  function render(): Node[] {
    cursor = 0;
    effects = [];
    const root = InstanceDetailView({ id: inst.id });
    assert.equal(typeof root.type, 'function');
    const render = root.type as (props: { id: string }) => unknown;
    return nodes(render({ id: inst.id }));
  }
  function status(): unknown {
    const result = render();
    const header = result.find((node) => node.props.class === 'cp-instance-status');
    assert.ok(header);
    return header.props.children;
  }
  return {
    ...h, store, reads, render, status,
    respond(next: typeof read) { read = next; },
    mount() { render(); for (const run of effects) { const cleanup = run(); if (cleanup) cleanups.push(cleanup); } },
    unmount() { for (const cleanup of cleanups) cleanup(); },
  };
}

test('the real instance header follows installation and backend readiness instead of assuming Ready', () => {
  const installTarget = target('net.fabricmc.fabric-loader');
  const inst = enrichedInstanceResponse(instance(installTarget));
  const h = detailHarness(inst);
  const { status, store } = h;
  assert.equal(status(), 'Install');
  h.downloads.activeDownload.value = {
    queueId: 'queue-1',
    kind: 'loader',
    item: installItems.installItemFromQueueInstallItem(installTarget),
    displayName: 'Minecraft',
    pct: 0,
    label: 'Preparing Minecraft',
    phase: 'starting',
    activeStep: null,
    startedAt: 1,
  };
  assert.equal(status(), 'Preparing Minecraft');
  h.downloads.activeDownload.value = null;
  h.downloads.downloadQueue.value.items = [{ ...queued(installTarget), title: 'Queued install' }];
  assert.equal(status(), 'Queued install');
  h.downloads.downloadQueue.value.items = [];
  h.downloads.downloadFailure.value = failed(installTarget);
  assert.equal(status(), 'Install failed');
  store.instances.value = [
    { ...inst, launch_action: { ...inst.launch_action, primary_action: 'blocked', label: 'Unavailable' } },
  ];
  assert.equal(status(), 'Unavailable');
  store.instances.value = [
    {
      ...inst,
      launchable: true,
      launch_action: { state_id: 'ready', primary_action: 'launch', label: 'Launch', launchable: true, tone: 'ok' },
    },
  ];
  assert.equal(status(), 'Ready'); // Historical failure is not current readiness authority.
  store.launchState.value = { status: 'preparing', instanceId: inst.id, pct: 10, label: 'Preparing launch' };
  assert.equal(status(), 'Preparing launch');
  store.launchSessions.value = {
    [inst.id]: {
      sessionId: 'session-1',
      launchedAt: '2026-09-27T09:00:00Z',
      statusRevision: 1,
      viewModel: {
        state_id: 'running',
        label: 'Playing',
        progress_pct: 100,
        terminal: false,
        playing: true,
        process_live: true,
        can_stop: true,
      },
    },
  };
  assert.equal(status(), 'Playing');
});

test('entering instance details and refreshing Mods recover stale availability while retaining resource errors', async () => {
  const busy = enrichedInstanceResponse(instance(null));
  const ready: EnrichedInstance = { ...busy, launchable: true,
    launch_action: { state_id: 'ready', primary_action: 'launch', label: 'Launch', launchable: true, tone: 'ok' } };
  const h = detailHarness(busy);
  h.respond(async () => ready);
  h.mount();
  await new Promise((done) => setImmediate(done));
  assert.equal(h.status(), 'Ready');
  assert.deepEqual(h.reads, [`/instances/${busy.id}`]);
  h.store.instances.value = [busy];
  const mods = h.render().find((node) => node.type === 'ModsPane');
  assert.ok(mods);
  (mods.props.onRefresh as () => void)();
  await new Promise((done) => setImmediate(done));
  assert.equal(h.status(), 'Ready');
  assert.deepEqual(h.reads, [`/instances/${busy.id}`, `/instances/${busy.id}`]);
  const resources = h.render().find((node) => node.type === 'ModsPane')?.props.resources as
    { status: string; data: unknown; error: string };
  assert.equal(resources.status, 'error');
  assert.equal(resources.data, null);
  assert.equal(resources.error, 'Resources unavailable');
  h.unmount();
});

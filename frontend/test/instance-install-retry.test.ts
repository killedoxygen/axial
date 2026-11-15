import assert from 'node:assert/strict';
import test from 'node:test';
import { apiModule, loadSource, requireDependency } from './source';
import { enrichedInstance } from './wire';
import * as installItems from '../src/install/item';
import * as launchPresenters from '../src/launch/presenters';
import type { EnrichedInstance } from '../src/generated/EnrichedInstance';
import type { LaunchSession, LaunchState } from '../src/launch/state';
import type { ActiveDownload, DownloadFailure } from '../src/install/queue';
import type { InstallItem } from '../src/generated/InstallItem';
import type { InstallQueuedItemViewModel } from '../src/generated/InstallQueuedItemViewModel';
import type { LoaderComponentId } from '../src/generated/LoaderComponentId';

function harness() {
  const downloads = {
    activeDownload: { value: null as ActiveDownload | null },
    downloadFailures: { value: [] as DownloadFailure[] },
    downloadQueue: { value: { items: [] as InstallQueuedItemViewModel[] } },
    installedStateSettling: { value: false },
  };
  const status = loadSource<typeof import('../src/install/status')>('install/status.ts', {
    'install/queue': downloads,
    'install/item': installItems,
  });
  return { downloads, ...status };
}

function target(component: LoaderComponentId): InstallItem {
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

function instance(installTarget: InstallItem | null): EnrichedInstance {
  return enrichedInstance({
    id: 'registered-instance',
    name: 'Survival',
    version_id: installTarget?.version_id ?? 'unavailable-loader',
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
  });
}

function queued(installTarget: InstallItem): InstallQueuedItemViewModel {
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

function failed(installTarget: InstallItem): DownloadFailure {
  return {
    operationId: 'failed-operation',
    item: installTarget,
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
      const parsed = wire;
      if (outcome === 'failed') {
        h.downloads.downloadFailures.value = [failed(installTarget)];
        assert.equal(h.instanceInstallStatus(parsed).state, 'failed');
        h.downloads.downloadFailures.value = [];
      } else {
        h.downloads.downloadQueue.value.items = [queued(installTarget)];
        assert.equal(h.instanceInstallStatus(parsed).state, 'queued');
        h.downloads.downloadQueue.value.items = [];
      }
      // Reloaded registry response and an empty queue are sufficient. The
      // frontend never decodes the opaque installed/build identities.
      const restarted = harness();
      const refreshed: EnrichedInstance = JSON.parse(JSON.stringify(wire));
      const status = restarted.instanceInstallStatus(refreshed);
      assert.equal(status.state, 'idle');
      assert.equal(status.installing, false);
      assert.ok(status.item?.loader);
      assert.equal(status.item.loader.minecraft_version, '1.21.4');
      assert.deepEqual(installItems.installQueueRequest(status.item), {
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
  h.downloads.downloadFailures.value = [failed(stale)];
  h.downloads.downloadQueue.value.items = [queued(stale)];
  const parsed = wire;
  assert.equal(parsed.install_target, null);
  assert.equal(parsed.launch_action.primary_action, 'blocked');
  const status = h.instanceInstallStatus(parsed);
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
  h.downloads.downloadFailures.value = [failed(other)];
  h.downloads.downloadQueue.value.items = [queued(other)];
  const status = h.instanceInstallStatus(instance(expected));
  assert.equal(status.state, 'idle');
  assert.ok(status.item);
  assert.deepEqual(installItems.installQueueRequest(status.item), {
    kind: 'loader',
    component_id: 'net.fabricmc.fabric-loader',
    build_id: expected.loader!.build_id,
  });
});

function detailHarness(inst: EnrichedInstance) {
  const h = harness();
  const store = {
    instances: { value: [inst] as EnrichedInstance[] },
    launchSessions: { value: {} as Record<string, LaunchSession> },
    launchNotices: { value: {} },
    launchState: { value: { status: 'idle' } as LaunchState },
  };
  const reads: string[] = [];
  let read = async (): Promise<unknown> => inst;
  const values: unknown[] = [];
  let cursor = 0;
  let effects: Array<{ run: () => void | (() => void); deps?: unknown[] }> = [];
  let mountedDeps: Array<unknown[] | undefined> = [];
  const cleanups: Array<() => void> = [];
  let resourceReads = 0;
  const instanceApi = apiModule<typeof import('../src/instances/api')>('instances/api.ts', async (_method, path) => {
    if (path.endsWith('/resources')) {
      resourceReads += 1;
      throw new Error('Resources unavailable');
    }
    reads.push(path);
    return read();
  });
  const instanceState = { instances: store.instances, selectInstance: () => {} };
  const launchState = { ...store, clearLaunchNotice: () => {} };
  const readiness = loadSource<typeof import('../src/instances/readiness')>(
    'instances/readiness.ts',
    {
      'instances/api': instanceApi,
      'settings/state': { config: { value: null } },
      'instances/state': instanceState,
      'launch/state': launchState,
      'app/toast': { showError: assert.fail },
    },
    { window: { setTimeout } },
  );
  const jsx = requireDependency('preact/jsx-runtime');
  const launchControls = loadSource<typeof import('../src/instances/detail/components/launch')>(
    'instances/detail/components/launch.tsx',
    {
      'preact/jsx-runtime': jsx,
      'ui/Atoms': { Button: 'Button', Meter: 'Meter', Notice: 'Notice', SplitButton: 'SplitButton' },
      'install/DownloadFailureNotice': { DownloadFailureNotice: 'DownloadFailureNotice' },
      'ui/ContextMenu': {},
      'app/route': {},
      'launch/state': launchState,
      'app/toast': {},
      'launch/presenters': launchPresenters,
      'instances/detail/instance-actions': {},
    },
  );
  const view = loadSource<typeof import('../src/instances/detail/InstanceDetailView')>(
    'instances/detail/InstanceDetailView.tsx',
    {
      'preact/hooks': {
        useState: <T>(initial: T | (() => T)) => {
          const index = cursor++;
          if (!(index in values)) values[index] = typeof initial === 'function' ? (initial as () => T)() : initial;
          return [
            values[index],
            (next: T | ((current: T) => T)) => {
              values[index] = typeof next === 'function' ? (next as (current: T) => T)(values[index] as T) : next;
            },
          ];
        },
        useRef: <T>(value: T) => {
          const index = cursor++;
          return (values[index] ??= { current: value });
        },
        useEffect(run: () => void | (() => void), deps?: unknown[]) {
          effects.push({ run, deps });
        },
      },
      'preact/jsx-runtime': jsx,
      'ui/Atoms': {
        Button: 'Button',
        Chip: 'Chip',
        Empty: 'Empty',
        IconButton: 'IconButton',
        Status: 'Status',
        Tabs: 'Tabs',
      },
      'ui/InstanceVisual': { artSeedFor: () => 0 },
      'vista/Vista': { Vista: () => null },
      'ui/ContextMenu': {},
      'content/activity': { contentRevision: { value: 0 } },
      'instances/state': instanceState,
      'launch/state': launchState,
      'app/route': { navigate: () => {}, resetViewScroll: () => {} },
      'launch/controller': {},
      'install/queue': {},
      'app/toast': { errMessage: (error: Error) => error.message },
      'ui/format': { fmtRelative: () => 'Never' },
      'install/status': h,
      'instances/readiness': readiness,
      'instances/setup': {},
      'launch/presenters': launchPresenters,
      'instances/api': instanceApi,
      'instances/detail/listing-cache': { lastListing: () => null, readListing: () => () => undefined },
      'instances/detail/logs': { LOG_RESOURCE_POLL_MS: 2500 },
      'instances/detail/instance-actions': {},
      ...Object.fromEntries(
        ['Mods', 'Packs', 'Worlds', 'Screenshots', 'Logs', 'Settings'].map((name) => [
          `instances/detail/tabs/${name}Pane`,
          { [`${name}Pane`]: `${name}Pane` },
        ]),
      ),
      'instances/detail/components/launch': {
        LaunchSplitButton: 'LaunchSplitButton',
        InstallBarrierPane: 'InstallBarrierPane',
        LaunchOutcomeNotice: 'LaunchOutcomeNotice',
      },
      'instances/detail/components/pack-origin': { PackChips: 'PackChips' },
      'instances/detail/mod-provenance-cache': { cachedModProvenance: () => null },
    },
    { window: { setInterval, clearInterval } },
  );
  const { LaunchSplitButton } = launchControls;
  const { InstanceDetailView } = view;
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
  function primaryLabel(): unknown {
    const control = render().find((node) => node.type === 'LaunchSplitButton');
    assert.ok(control);
    const tree = LaunchSplitButton(control.props as Parameters<typeof LaunchSplitButton>[0]);
    const button = nodes(tree).find((node) => node.type === 'SplitButton');
    assert.ok(button);
    return button.props.children;
  }
  function status(): unknown {
    const result = render();
    const header = result.find((node) => node.type === 'Status');
    const children = header?.props.children;
    return Array.isArray(children) ? children[0] : children;
  }
  return {
    ...h,
    store,
    reads,
    render,
    status,
    primaryLabel,
    respond(next: typeof read) {
      read = next;
    },
    resourceReads: () => resourceReads,
    mount() {
      render();
      mountedDeps = effects.map(({ deps }) => deps);
      for (const { run } of effects) {
        const cleanup = run();
        if (cleanup) cleanups.push(cleanup);
      }
    },
    update() {
      render();
      effects.forEach(({ run, deps }, index) => {
        const previous = mountedDeps[index];
        if (deps && previous && deps.every((value, at) => Object.is(value, previous[at]))) return;
        mountedDeps[index] = deps;
        const cleanup = run();
        if (cleanup) cleanups.push(cleanup);
      });
    },
    unmount() {
      for (const cleanup of cleanups) cleanup();
    },
  };
}

test('the real instance header follows installation and backend readiness instead of assuming Ready', () => {
  const installTarget = target('net.fabricmc.fabric-loader');
  const inst = instance(installTarget);
  const h = detailHarness(inst);
  const { status, store } = h;
  assert.equal(status(), undefined);
  assert.equal(h.primaryLabel(), 'Install');
  h.downloads.activeDownload.value = {
    queueId: 'queue-1',
    kind: 'loader',
    item: installTarget,
    displayName: 'Minecraft',
    pct: 0,
    ringPct: 0,
    label: 'Preparing Minecraft',
    phase: 'starting',
    activeStep: null,
    startedAt: 1,
  };
  assert.equal(status(), undefined);
  assert.equal(h.primaryLabel(), 'Preparing Minecraft');
  h.downloads.activeDownload.value = null;
  h.downloads.downloadQueue.value.items = [{ ...queued(installTarget), title: 'Queued install' }];
  assert.equal(status(), undefined);
  assert.equal(h.primaryLabel(), 'Queued install');
  h.downloads.downloadQueue.value.items = [];
  h.downloads.downloadFailures.value = [failed(installTarget)];
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
  assert.equal(status(), undefined);
  assert.equal(h.primaryLabel(), 'Launch');
  store.launchState.value = { status: 'preparing', instanceId: inst.id, pct: 10, label: 'Preparing launch' };
  assert.equal(status(), undefined);
  assert.equal(h.primaryLabel(), 'Preparing launch');
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
  const busy = instance(null);
  const ready: EnrichedInstance = {
    ...busy,
    launchable: true,
    launch_action: { state_id: 'ready', primary_action: 'launch', label: 'Launch', launchable: true, tone: 'ok' },
  };
  const h = detailHarness(busy);
  h.respond(async () => ready);
  h.mount();
  await new Promise((done) => setImmediate(done));
  assert.equal(h.status(), undefined);
  assert.equal(h.primaryLabel(), 'Launch');
  assert.deepEqual(h.reads, [`/instances/${busy.id}`]);
  h.store.instances.value = [busy];
  const mods = h.render().find((node) => node.type === 'ModsPane');
  assert.ok(mods);
  (mods.props.onRefresh as () => void)();
  await new Promise((done) => setImmediate(done));
  assert.equal(h.status(), undefined);
  assert.equal(h.primaryLabel(), 'Launch');
  assert.deepEqual(h.reads, [`/instances/${busy.id}`, `/instances/${busy.id}`]);
  const resources = h.render().find((node) => node.type === 'ModsPane')?.props.resources as {
    status: string;
    data: unknown;
    error: string;
  };
  assert.equal(resources.status, 'error');
  assert.equal(resources.data, null);
  assert.equal(resources.error, 'Resources unavailable');
  h.unmount();
});

test('instance details reread resources once setup releases the instance', async () => {
  const pending = instance(null);
  const settling: EnrichedInstance = {
    ...pending,
    launch_action: { ...pending.launch_action, state_id: 'setup_queued' },
  };
  const ready: EnrichedInstance = {
    ...pending,
    launchable: true,
    launch_action: { state_id: 'ready', primary_action: 'launch', label: 'Launch', launchable: true, tone: 'ok' },
  };
  const h = detailHarness(settling);
  h.respond(async () => h.store.instances.value[0]);
  h.mount();
  await new Promise((done) => setImmediate(done));
  assert.equal(h.resourceReads(), 1);
  h.update();
  await new Promise((done) => setImmediate(done));
  assert.equal(h.resourceReads(), 1, 'an unchanged projection must not reread resources');
  h.store.instances.value = [ready];
  h.update();
  await new Promise((done) => setImmediate(done));
  assert.equal(h.resourceReads(), 2);
  h.unmount();
});

test('idle instance readiness stays settling until the installed-state refresh completes', () => {
  const h = harness();
  const inst = instance(null);
  assert.equal(h.instanceInstallStatus(inst).settling, false);
  h.downloads.installedStateSettling.value = true;
  const pending = h.instanceInstallStatus(inst);
  assert.equal(pending.settling, true);
  assert.equal(pending.installing, false);
  assert.equal(pending.state, 'idle');
  h.downloads.installedStateSettling.value = false;
  assert.equal(h.instanceInstallStatus(inst).settling, false);
});

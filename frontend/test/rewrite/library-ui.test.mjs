import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import vm from 'node:vm';
import test from 'node:test';

/** @typedef {import('../../src/types-instance').EnrichedInstance} LibraryInstance */
/** @typedef {ReturnType<typeof event>} TestEvent */
/** @template [P = Record<string, unknown>] @typedef {{ type: unknown, props: P }} ViewNode */
/** @typedef {{ onClick: (event?: TestEvent) => void, children?: unknown, icon?: string, title?: string, disabled?: boolean, tooltip?: string }} ActionProps */
/** @typedef {{ inst: LibraryInstance, selected: boolean, onToggleSelect: () => void, onContextMenu: (event: TestEvent) => void }} InstanceCardProps */
/** @typedef {{ role: string, tabIndex: number, 'aria-label': string, children?: unknown, onKeyDown: (event: TestEvent) => void, onContextMenu: (event: TestEvent) => void }} RowProps */
/**
 * @typedef {object} PartProps
 * @property {ActionProps} Button
 * @property {ActionProps} IconButton
 * @property {{ onChange: (value: string) => void }} Input
 * @property {{ icon?: string, tone?: string, children?: unknown }} Pill
 * @property {Record<string, unknown>} Card
 * @property {{ action: { label: string, onClick: () => void } }} SectionHeading
 * @property {Record<string, unknown>} Icon
 * @property {InstanceCardProps} InstanceCard
 * @property {Record<string, unknown>} InstanceTile
 * @property {Record<string, unknown>} InstanceGlyph
 * @property {{ onChange: (value: string) => void }} Segmented
 * @property {{ actions: { label: string, danger?: boolean, onClick: () => void }[] }} SelectionActionTray
 * @property {{ onToggle: (event: TestEvent) => void }} SelectionCheckbox
 */
/** @typedef {{ [K in keyof PartProps]: (props: PartProps[K]) => unknown }} Parts */
/** @typedef {{ FeatureBanner: RowProps, ListRow: RowProps, EmptyHome: Record<string, unknown> }} OwnComponents */
/** @typedef {{ label: string, onSelect?: () => void }} MenuItem */
/** @typedef {{ e: TestEvent, menu: MenuItem[] } | { e: TestEvent, inst: LibraryInstance }} MenuCall */

const frontendPath = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const frontendRoot = pathToFileURL(`${frontendPath}/`);
let dependencyRequire = createRequire(new URL('package.json', frontendRoot));
try {
  dependencyRequire.resolve('typescript');
} catch {
  dependencyRequire = createRequire(new URL('../legacy/frontend/package.json', frontendRoot));
}
const ts = /** @type {typeof import('typescript')} */ (dependencyRequire('typescript'));
const jsxRuntime = /** @type {typeof import('preact/jsx-runtime')} */ (dependencyRequire('preact/jsx-runtime'));

/** @param {string} path @param {Record<string, unknown>} [imports] @returns {Record<string, unknown>} */
function loadSource(path, imports = {}) {
  const filename = fileURLToPath(new URL(path, frontendRoot));
  const { outputText } = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: {
      module: ts.ModuleKind.CommonJS,
      target: ts.ScriptTarget.ES2022,
      jsx: ts.JsxEmit.ReactJSX,
      jsxImportSource: 'preact',
    },
  });
  /** @type {Record<string, unknown>} */
  const exports = {};
  vm.runInNewContext(
    outputText,
    {
      exports,
      /** @param {string} id */
      require(id) {
        if (id === 'preact/jsx-runtime') return jsxRuntime;
        if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
        throw new Error(`Unreviewed library UI dependency: ${id}`);
      },
      Date,
    },
    { filename },
  );
  return exports;
}

const presenters = loadSource('src/launch-presenters.ts');
const contracts = loadSource('src/dto-contract.ts');
const installDtos = loadSource('src/dto-install.ts', { './dto-contract': contracts });
const { enrichedInstanceResponse } = /** @type {typeof import('../../src/dto-core')} */ (
  loadSource('src/dto-core.ts', { './dto-contract': contracts, './dto-install': installDtos })
);
const parts = /** @type {Parts} */ (
  /** @type {unknown} */ (
    Object.fromEntries(
      [
        'Button',
        'IconButton',
        'Input',
        'Pill',
        'Card',
        'SectionHeading',
        'Icon',
        'InstanceCard',
        'InstanceTile',
        'InstanceGlyph',
        'Segmented',
        'SelectionActionTray',
        'SelectionCheckbox',
      ].map((name) => [name, Object.defineProperty(() => undefined, 'name', { value: name })]),
    )
  )
);

/** @param {unknown} tree @param {(node: ViewNode) => boolean} predicate @returns {ViewNode[]} */
function nodes(tree, predicate) {
  if (tree == null || typeof tree !== 'object') return [];
  if (Array.isArray(tree)) return tree.flatMap((child) => nodes(child, predicate));
  const node = viewNode(tree);
  return [...(predicate(node) ? [node] : []), ...nodes(node.props.children, predicate)];
}

/** @param {unknown} value @returns {ViewNode} */
function viewNode(value) {
  assert.ok(value && typeof value === 'object' && 'type' in value && 'props' in value);
  assert.ok(value.props && typeof value.props === 'object');
  return /** @type {ViewNode} */ (value);
}

/** @template P @param {unknown} tree @param {(props: P) => unknown} type @returns {ViewNode<P>[]} */
function ofType(tree, type) {
  return /** @type {ViewNode<P>[]} */ (/** @type {unknown} */ (nodes(tree, (node) => node.type === type)));
}

/** @param {unknown} tree @returns {string} */
function textOf(tree) {
  if (tree == null || typeof tree === 'boolean') return '';
  if (typeof tree !== 'object') return String(tree);
  if (Array.isArray(tree)) return tree.map(textOf).join('');
  return textOf(viewNode(tree).props.children);
}

/** @template {keyof OwnComponents} K @param {unknown} tree @param {K} name @returns {ViewNode<OwnComponents[K]>} */
function ownComponent(tree, name) {
  const node = nodes(tree, (node) => typeof node.type === 'function' && node.type.name === name)[0];
  assert.ok(node, `${name} is present`);
  assert.equal(typeof node.type, 'function');
  const component = /** @type {(props: Record<string, unknown>) => unknown} */ (node.type);
  return /** @type {ViewNode<OwnComponents[K]>} */ (/** @type {unknown} */ (viewNode(component(node.props))));
}

/** @param {string} [key] @param {boolean} [nested] */
function event(key = '', nested = false) {
  const target = {};
  return {
    key,
    target: nested ? {} : target,
    currentTarget: target,
    prevented: 0,
    stopped: 0,
    preventDefault() {
      this.prevented += 1;
    },
    stopPropagation() {
      this.stopped += 1;
    },
  };
}

/** @param {string} id @param {Partial<Omit<LibraryInstance, 'launch_action'>> & { launch_action?: Partial<LibraryInstance['launch_action']> }} [overrides] @returns {LibraryInstance} */
function instance(id, overrides = {}) {
  return {
    id,
    name: `Instance ${id}`,
    version_id: `version-${id}`,
    created_at: '2026-01-01T00:00:00Z',
    java_selection: { kind: 'inherited' },
    revision: 1,
    last_played_at: '2026-01-01T00:00:00Z',
    art_seed: 40,
    mods_count: 3,
    counts_available: true,
    launchable: true,
    saves_count: 0,
    resource_count: 0,
    shader_count: 0,
    version_display: {
      loader_key: 'fixture-loader',
      loader_label: 'Backend loader',
      minecraft_label: 'Backend Minecraft',
      loader_version_label: 'Backend build',
      loader_detail_label: 'Backend build label',
      summary_label: 'Backend version summary',
      supports_mods: true,
    },
    ...overrides,
    launch_action: {
      state_id: 'ready',
      tone: 'ok',
      launchable: true,
      primary_action: 'launch',
      label: 'Launch game',
      ...overrides.launch_action,
    },
  };
}

/** @param {LibraryInstance[]} [initialInstances] */
function libraryHarness(initialInstances = []) {
  const store = {
    instances: { value: initialInstances },
    config: { value: { username: 'Offline player' } },
    launchSessions: { value: /** @type {Record<string, { viewModel: { playing: boolean, label: string } }>} */ ({}) },
    versionById: () => undefined,
  };
  /** @type {unknown[]} */
  const state = [];
  let stateIndex = 0;
  /** @type {Set<string>} */
  const selectedIds = new Set();
  /** @type {{ navigations: { name: string, id?: string }[], create: number, menus: MenuCall[], deletes: { selected: LibraryInstance[], onDone: () => void }[], clears: number, filtered: string[] }} */
  const calls = { navigations: [], create: 0, menus: [], deletes: [], clears: 0, filtered: [] };
  /** @type {Map<string, { state: string, installing: boolean, label?: string, queuedItem?: { title: string } }>} */
  const installs = new Map();
  const clearSelection = () => {
    selectedIds.clear();
    calls.clears += 1;
  };
  const imports = {
    'preact/hooks': {
      /** @template T @param {() => T} fn */
      useMemo: (fn) => fn(),
      /** @template T @param {T} fn */
      useCallback: (fn) => fn,
      /** @param {unknown} initial @returns {[unknown, (value: unknown) => void]} */
      useState(initial) {
        const index = stateIndex++;
        if (!(index in state)) state[index] = initial;
        return [
          state[index],
          (value) => {
            state[index] = value;
          },
        ];
      },
    },
    '../../ui/Atoms': parts,
    '../../ui/Icons': parts,
    '../../ui/InstanceCard': parts,
    '../../ui/InstanceVisual': { ...parts, guardedInstanceHue: () => 40 },
    '../../ui/Segmented': parts,
    '../../ui/SelectionActionTray': parts,
    '../../ui/ContextMenu': {
      /** @param {TestEvent} e @param {MenuItem[]} menu */
      openContextMenu: (e, menu) => calls.menus.push({ e, menu }),
    },
    '../../ui/selection': {
      /** @param {boolean} selected @param {string} name */
      selectionToggleLabel: (selected, name) => `${selected ? 'Deselect' : 'Select'} ${name}`,
      /** @param {{ toggle: (id: string) => unknown }} selection @param {string} id */
      selectionMenuItem: (selection, id) => ({ label: 'Select', onSelect: () => selection.toggle(id) }),
      /** @template T @param {T[]} items @param {(item: T) => string} getId */
      useSelection(items, getId) {
        calls.filtered = items.map(getId);
        return {
          selectedItems: items.filter((item) => selectedIds.has(getId(item))),
          selectedCount: selectedIds.size,
          /** @param {string} id */
          isSelected: (id) => selectedIds.has(id),
          /** @param {string} id */
          toggle: (id) => (selectedIds.has(id) ? selectedIds.delete(id) : selectedIds.add(id)),
          clear: clearSelection,
        };
      },
    },
    '../../hooks/use-theme': { useTheme: () => ({ r: { sm: 12 } }) },
    '../../store': store,
    '../../instance-install-status': {
      /** @param {LibraryInstance} inst */
      instanceInstallStatus: (inst) => installs.get(inst.id) ?? { state: 'idle', installing: false },
    },
    '../../launch-presenters': presenters,
    '../../ui-state': {
      /** @param {{ name: string, id?: string }} route */
      navigate: (route) => calls.navigations.push(route),
      openCreate: () => {
        calls.create += 1;
      },
    },
    '../instance/instance-menu': {
      /** @param {LibraryInstance} inst */
      instanceMenuItems: (inst) => [{ label: `Backend menu for ${inst.id}` }],
      /** @param {TestEvent} e @param {LibraryInstance} inst */
      openInstanceContextMenu: (e, inst) => calls.menus.push({ e, inst }),
    },
    '../instance/instance-actions': {
      /** @param {LibraryInstance[]} selected @param {() => void} onDone */
      deleteInstancesFlow: async (selected, onDone) => {
        calls.deletes.push({ selected, onDone });
      },
    },
    './PendingRemovalsNotice': { PendingRemovalsNotice: () => null },
    '../../format': {
      /** @param {string | undefined} iso */
      fmtRelativeCompact: (iso) => iso ?? 'Never',
    },
  };
  const { HomeView } = /** @type {typeof import('../../src/views/home/HomeView')} */ (
    loadSource('src/views/home/HomeView.tsx', imports)
  );
  const { InstancesView } = /** @type {typeof import('../../src/views/instances/InstancesView')} */ (
    loadSource('src/views/instances/InstancesView.tsx', imports)
  );
  return {
    store,
    calls,
    installs,
    selectedIds,
    home: () => HomeView(),
    library: () => {
      stateIndex = 0;
      return InstancesView();
    },
  };
}

test('home retains newest featured instance, fourteen recent cards and See all without mutating the snapshot', () => {
  const records = Array.from({ length: 18 }, (_, index) =>
    instance(String(index), {
      last_played_at: `2026-01-${String(index + 1).padStart(2, '0')}T00:00:00Z`,
    }),
  );
  const h = libraryHarness(records);
  const home = h.home();
  const featured = nodes(home, (node) => typeof node.type === 'function' && node.type.name === 'FeatureBanner')[0];
  const featuredInstance = /** @type {LibraryInstance} */ (featured.props.inst);
  assert.equal(featuredInstance.id, '17');
  assert.deepEqual(
    ofType(home, parts.InstanceCard).map((node) => node.props.inst.id),
    ['16', '15', '14', '13', '12', '11', '10', '9', '8', '7', '6', '5', '4', '3'],
  );
  assert.equal(records[0].id, '0');
  assert.equal(h.store.instances.value, records);
  assert.match(textOf(home), /18 instances in your library/);
  const action = ofType(home, parts.SectionHeading)[0].props.action;
  assert.equal(action.label, 'See all');
  action.onClick();
  assert.equal(h.calls.navigations[0].name, 'instances');
});

test('unavailable or omitted count availability hides summary counts without suppressing known counts', () => {
  for (const available of [false, undefined]) {
    const unknown = enrichedInstanceResponse(instance('unknown', { counts_available: available }));
    assert.equal(unknown.counts_available, false);
    const h = libraryHarness([unknown]);
    const library = h.library();
    const summary = nodes(library, (node) => node.props.class === 'cp-page-sub')[0];
    assert.equal(textOf(summary), '1 total');
    const banner = ownComponent(h.home(), 'FeatureBanner');
    assert.doesNotMatch(textOf(banner), /\d+ mods/);
    ofType(library, parts.Segmented)[0].props.onChange('list');
    const row = ownComponent(h.library(), 'ListRow');
    assert.doesNotMatch(textOf(row), /\d+ mods/);
    assert.match(textOf(row), /Backend Minecraft/);
  }
  const known = enrichedInstanceResponse(instance('known', { mods_count: 0 }));
  assert.equal(known.counts_available, true);
  const h = libraryHarness([known]);
  const library = h.library();
  const summary = nodes(library, (node) => node.props.class === 'cp-page-sub')[0];
  assert.equal(textOf(summary), '1 total, 0 mods across all');
  assert.match(textOf(ownComponent(h.home(), 'FeatureBanner')), /0 mods/);
  ofType(library, parts.Segmented)[0].props.onChange('list');
  assert.match(textOf(ownComponent(h.library(), 'ListRow')), /0 mods/);
});

test('a partial count snapshot never becomes a total and malformed availability is rejected', () => {
  const records = [
    enrichedInstanceResponse(instance('known')),
    enrichedInstanceResponse(instance('unknown', { counts_available: false, mods_count: 99 })),
  ];
  const h = libraryHarness(records);
  const summary = nodes(h.library(), (node) => node.props.class === 'cp-page-sub')[0];
  assert.equal(textOf(summary), '2 total');
  for (const invalid of [null, 'false', 0, {}]) {
    assert.throws(
      () => enrichedInstanceResponse({ ...instance('invalid'), counts_available: invalid }),
      /Instance count availability/,
    );
  }
});

test('empty home keeps both create entrypoints and no featured instance', () => {
  const h = libraryHarness();
  const home = h.home();
  const empty = ownComponent(home, 'EmptyHome');
  assert.match(textOf(empty), /Create your first instance/);
  assert.equal(nodes(home, (node) => typeof node.type === 'function' && node.type.name === 'FeatureBanner').length, 0);
  ofType(home, parts.Button)[0].props.onClick();
  ofType(empty, parts.Button)[0].props.onClick();
  assert.equal(h.calls.create, 2);
});

test('featured action renders the backend install and blocked labels and reasons', () => {
  for (const [action, label, icon] of /** @type {const} */ ([
    ['install', 'Install required files', 'download'],
    ['blocked', 'Review launch issue', 'alert'],
  ])) {
    const h = libraryHarness([
      instance('opaque-id', {
        launch_action: { primary_action: action, label, disabled_reason: 'Backend refusal reason' },
      }),
    ]);
    const banner = ownComponent(h.home(), 'FeatureBanner');
    const button = ofType(banner, parts.Button)[0];
    assert.equal(textOf(button), label);
    assert.equal(button.props.icon, icon);
    if (action === 'blocked') assert.equal(button.props.title, 'Backend refusal reason');
    const click = event();
    button.props.onClick(click);
    assert.equal(click.stopped, 1);
    assert.equal(h.calls.navigations[0].id, 'opaque-id');
  }
});

test('featured navigation handles Enter and Space once and leaves nested controls independent', () => {
  const h = libraryHarness([instance('keyboard')]);
  const banner = ownComponent(h.home(), 'FeatureBanner');
  assert.equal(banner.props.role, 'button');
  assert.equal(banner.props.tabIndex, 0);
  for (const key of ['Enter', ' ']) {
    const keyEvent = event(key);
    banner.props.onKeyDown(keyEvent);
    assert.equal(keyEvent.prevented, 1);
  }
  banner.props.onKeyDown(event('Enter', true));
  banner.props.onKeyDown(event('Escape'));
  assert.equal(h.calls.navigations.length, 2);
  banner.props.onContextMenu(event());
  const menu = h.calls.menus[0];
  assert.ok('inst' in menu);
  assert.equal(menu.inst.id, 'keyboard');
});

test('home preserves queue title and backend session label without inferring playing from existence', () => {
  const h = libraryHarness([instance('busy')]);
  h.installs.set('busy', {
    state: 'queued',
    installing: true,
    label: 'fallback',
    queuedItem: { title: 'Waiting for Java' },
  });
  h.store.launchSessions.value.busy = { viewModel: { playing: false, label: 'Settling process output' } };
  let banner = ownComponent(h.home(), 'FeatureBanner');
  let button = ofType(banner, parts.Button)[0];
  assert.equal(button.props.disabled, true);
  assert.equal(textOf(button), 'Waiting for Java');
  assert.equal(banner.props['aria-label'], 'Open Instance busy. Waiting for Java');
  const sessionPill = ofType(banner, parts.Pill).find((node) => textOf(node) === 'Settling process output');
  assert.ok(sessionPill);
  assert.equal(sessionPill.props.icon, 'clock');
  assert.equal(sessionPill.props.tone, undefined);
  h.installs.clear();
  banner = ownComponent(h.home(), 'FeatureBanner');
  button = ofType(banner, parts.Button)[0];
  assert.equal(textOf(button), 'Open');
});

test('grid filtering preserves case-insensitive trimmed names, card selection and contextual menus', () => {
  const h = libraryHarness([instance('one', { name: 'Survival' }), instance('two', { name: 'Creative' })]);
  let library = h.library();
  assert.equal(ofType(library, parts.InstanceCard).length, 2);
  ofType(library, parts.Input)[0].props.onChange('  SURV  ');
  library = h.library();
  const cards = ofType(library, parts.InstanceCard);
  assert.equal(cards.length, 1);
  assert.equal(cards[0].props.inst.id, 'one');
  assert.deepEqual(h.calls.filtered, ['one']);
  cards[0].props.onToggleSelect();
  library = h.library();
  assert.equal(ofType(library, parts.InstanceCard)[0].props.selected, true);
  cards[0].props.onContextMenu(event());
  const menu = h.calls.menus[0];
  assert.ok('menu' in menu);
  assert.equal(menu.menu[menu.menu.length - 1].label, 'Backend menu for one');
});

test('list rows preserve backend labels, keyboard access and independent selection/overflow clicks', () => {
  const h = libraryHarness([
    instance('row', {
      launch_action: { primary_action: 'blocked', label: 'Unavailable', disabled_reason: 'Backend reason' },
    }),
  ]);
  ofType(h.library(), parts.Segmented)[0].props.onChange('list');
  const row = ownComponent(h.library(), 'ListRow');
  assert.equal(row.props.role, 'button');
  assert.equal(row.props.tabIndex, 0);
  assert.equal(row.props['aria-label'], 'Open Instance row');
  assert.match(textOf(row), /Backend build label/);
  assert.match(textOf(row), /Backend Minecraft/);
  assert.match(textOf(row), /3 mods/);
  const action = ofType(row, parts.Button)[0];
  assert.equal(textOf(action), 'Unavailable');
  assert.equal(action.props.title, 'Backend reason');
  for (const key of ['Enter', ' ']) {
    const keyEvent = event(key);
    row.props.onKeyDown(keyEvent);
    assert.equal(keyEvent.prevented, 1);
  }
  row.props.onKeyDown(event('Enter', true));
  row.props.onKeyDown(event('ArrowDown'));
  assert.equal(h.calls.navigations.length, 2);
  const selectEvent = event();
  ofType(row, parts.SelectionCheckbox)[0].props.onToggle(selectEvent);
  assert.equal(selectEvent.stopped, 1);
  assert.ok(h.selectedIds.has('row'));
  const menuEvent = event();
  const overflow = ofType(row, parts.IconButton)[0];
  assert.equal(overflow.props.tooltip, 'Actions for Instance row');
  overflow.props.onClick(menuEvent);
  assert.equal(menuEvent.stopped, 1);
  assert.equal(h.calls.menus.length, 1);
  assert.equal(h.calls.navigations.length, 2);
});

test('bulk delete forwards selected instances and lets the shared workflow decide completion', async () => {
  const h = libraryHarness([instance('keep'), instance('remove')]);
  h.selectedIds.add('remove');
  const tray = ofType(h.library(), parts.SelectionActionTray)[0];
  assert.equal(tray.props.actions[0].label, 'Delete');
  assert.equal(tray.props.actions[0].danger, true);
  tray.props.actions[0].onClick();
  await Promise.resolve();
  assert.equal(h.calls.deletes.length, 1);
  assert.deepEqual(
    h.calls.deletes[0].selected.map((inst) => inst.id),
    ['remove'],
  );
  assert.equal(h.calls.clears, 0);
  assert.equal(h.store.instances.value.length, 2);
  h.calls.deletes[0].onDone();
  assert.equal(h.calls.clears, 1);
});

test('library distinguishes an empty library from an unmatched filter', () => {
  const h = libraryHarness();
  let library = h.library();
  assert.match(textOf(library), /No instances yet/);
  assert.equal(ofType(library, parts.Button).length, 2);
  h.store.instances.value = [instance('existing')];
  ofType(library, parts.Input)[0].props.onChange('unmatched');
  library = h.library();
  assert.match(textOf(library), /No matches/);
  assert.equal(ofType(library, parts.Button).length, 1);
});

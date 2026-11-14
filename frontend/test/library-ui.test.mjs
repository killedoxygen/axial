import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import vm from 'node:vm';
import test from 'node:test';

/** @typedef {import('../src/types-instance').EnrichedInstance} LibraryInstance */
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
 * @property {{ icon?: string, tone?: string, children?: unknown }} Chip
 * @property {{ title: string, actions?: unknown, children?: unknown }} Empty
 * @property {Record<string, unknown>} Icon
 * @property {InstanceCardProps} InstanceCard
 * @property {Record<string, unknown>} InstanceTile
 * @property {Record<string, unknown>} InstanceGlyph
 * @property {{ onChange: (value: string) => void }} Segmented
 * @property {Record<string, unknown>} SelectionActionTray
 * @property {{ onToggle: (event: TestEvent) => void }} SelectionCheckbox
 */
/** @typedef {{ [K in keyof PartProps]: (props: PartProps[K]) => unknown }} Parts */
/** @typedef {{ Lead: RowProps, Shelf: RowProps, ListRow: RowProps, SessionChip: PartProps['Chip'] }} OwnComponents */
/** @typedef {{ label: string, onSelect?: () => void }} MenuItem */
/** @typedef {{ e: TestEvent, menu: MenuItem[] } | { e: TestEvent, inst: LibraryInstance }} MenuCall */

const frontendPath = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const frontendRoot = pathToFileURL(`${frontendPath}/`);
const dependencyRequire = createRequire(new URL('package.json', frontendRoot));
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
const { enrichedInstanceResponse } = /** @type {typeof import('../src/dto-core')} */ (
  loadSource('src/dto-core.ts', { './dto-contract': contracts, './dto-install': installDtos })
);
const parts = /** @type {Parts} */ (
  /** @type {unknown} */ (
    Object.fromEntries(
      [
        'Button',
        'IconButton',
        'Input',
        'Chip',
        'Empty',
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

const facts = loadSource('src/ui/InstanceFacts.tsx', { './Atoms': parts, '../launch-presenters': presenters });
const rows = /** @type {{ RowLead: (props: object) => unknown, RowMenu: (props: object) => unknown }} */ (
  loadSource('src/ui/Rows.tsx', {
    './Atoms': parts,
    './SelectionActionTray': parts,
    './selection': {
      /** @param {boolean} selected @param {string} name */
      selectionToggleLabel: (selected, name) => `${selected ? 'Deselect' : 'Select'} ${name}`,
    },
  })
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
    launchState: { value: { status: 'idle' } },
    versionById: () => undefined,
  };
  /** @type {unknown[]} */
  const state = [];
  let stateIndex = 0;
  /** @type {Set<string>} */
  const selectedIds = new Set();
  /** @type {{ navigations: { name: string, id?: string }[], create: number, menus: MenuCall[], filtered: string[] }} */
  const calls = { navigations: [], create: 0, menus: [], filtered: [] };
  const launches = { selected: '', count: 0 };
  /** @type {Map<string, { state: string, installing: boolean, label?: string, queuedItem?: { title: string } }>} */
  const installs = new Map();
  const imports = {
    'preact/hooks': {
      /** @template T @param {() => T} fn */
      useMemo: (fn) => fn(),
      useRef: () => ({ current: null }),
      useLayoutEffect() {},
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
    '../../ui/Face': { Face: 'Face' },
    '../../vista/Vista': { Vista: () => null },
    '../../actions': {
      /** @param {string} id */
      selectInstance(id) {
        launches.selected = id;
      },
    },
    '../../launch': {
      launchGame() {
        launches.count++;
      },
    },
    '../../ui/Icons': parts,
    '../../ui/InstanceCard': {
      ...loadSource('src/ui/InstanceCard.tsx', {
        './InstanceVisual': parts,
        './InstanceFacts': facts,
        './Atoms': parts,
        './Icons': parts,
        './SelectionActionTray': parts,
        './selection': {},
        '../ui-state': {},
        '../store': {},
        '../instance-install-status': {},
        '../launch-presenters': presenters,
      }),
      InstanceCard: parts.InstanceCard,
    },
    '../../ui/InstanceFacts': facts,
    '../../ui/InstanceVisual': { ...parts, guardedInstanceHue: () => 40, artSeedFor: () => 0 },
    '../../ui/Rows': rows,
    '../../ui/Segmented': parts,
    '../../ui/motion': { settleChildren() {} },
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
          clear: () => selectedIds.clear(),
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
      deleteInstancesFlow: async () => {},
    },
    './PendingRemovalsNotice': { PendingRemovalsNotice: () => null },
    '../../format': {
      /** @param {string | undefined} iso */
      fmtRelative: (iso) => iso ?? 'Never',
    },
  };
  const { HomeView } = /** @type {typeof import('../src/views/home/HomeView')} */ (
    loadSource('src/views/home/HomeView.tsx', imports)
  );
  const { InstancesView } = /** @type {typeof import('../src/views/instances/InstancesView')} */ (
    loadSource('src/views/instances/InstancesView.tsx', imports)
  );
  return {
    store,
    calls,
    installs,
    launches,
    selectedIds,
    home: () => HomeView(),
    library: () => {
      stateIndex = 0;
      return InstancesView();
    },
  };
}

test('home retains newest lead, six recent covers and See all without mutating the snapshot', () => {
  const records = Array.from({ length: 18 }, (_, index) =>
    instance(String(index), {
      last_played_at: `2026-01-${String(index + 1).padStart(2, '0')}T00:00:00Z`,
    }),
  );
  const h = libraryHarness(records);
  const home = h.home();
  assert.equal(textOf(nodes(ownComponent(home, 'Lead'), (n) => n.props.class === 'cp-home-name')[0]), 'Instance 17');
  const shelf = ownComponent(home, 'Shelf');
  assert.deepEqual(
    ofType(shelf, parts.InstanceTile).map((n) => /** @type {LibraryInstance} */ (n.props.inst).id),
    ['16', '15', '14', '13', '12', '11'],
  );
  assert.equal(records[0].id, '0');
  assert.equal(h.store.instances.value, records);
  const action = ofType(shelf, parts.Button)[0];
  assert.equal(textOf(action), 'See all 18');
  action.props.onClick();
  assert.equal(h.calls.navigations[0].name, 'instances');
});

test('unavailable or omitted count availability hides row counts without suppressing known zero', () => {
  for (const available of [false, undefined]) {
    const unknown = enrichedInstanceResponse(instance('unknown', { counts_available: available }));
    assert.equal(unknown.counts_available, false);
    const h = libraryHarness([unknown]);
    const library = h.library();

    const banner = ownComponent(h.home(), 'Lead');
    assert.doesNotMatch(textOf(banner), /\d+ mods/);
    ofType(library, parts.Segmented)[0].props.onChange('list');
    const row = ownComponent(h.library(), 'ListRow');
    assert.equal(nodes(row, (n) => n.props.class === 'cp-library-cell')[1].props.children, null);
    assert.match(textOf(row), /Backend Minecraft/);
  }
  const known = enrichedInstanceResponse(instance('known', { mods_count: 0 }));
  assert.equal(known.counts_available, true);
  const h = libraryHarness([known]);
  const library = h.library();
  ofType(library, parts.Segmented)[0].props.onChange('list');
  assert.equal(
    nodes(ownComponent(h.library(), 'ListRow'), (n) => n.props.class === 'cp-library-cell')[1].props.children,
    0,
  );
});

test('partial count snapshots retain each row availability and reject malformed availability', () => {
  const records = [
    enrichedInstanceResponse(instance('known')),
    enrichedInstanceResponse(instance('unknown', { counts_available: false, mods_count: 99 })),
  ];
  const h = libraryHarness(records);
  ofType(h.library(), parts.Segmented)[0].props.onChange('list');
  const rows = nodes(h.library(), (n) => typeof n.type === 'function' && n.type.name === 'ListRow');
  assert.equal(rows.length, 2);
  const counts = rows.map((node) => {
    const component = /** @type {(props: Record<string, unknown>) => unknown} */ (node.type);
    return nodes(component(node.props), (n) => n.props.class === 'cp-library-cell')[1].props.children;
  });
  assert.deepEqual(counts, [3, null]);
  for (const invalid of [null, 'false', 0, {}]) {
    assert.throws(
      () => enrichedInstanceResponse({ ...instance('invalid'), counts_available: invalid }),
      /Instance count availability/,
    );
  }
});

test('empty home keeps its create entrypoint and no lead', () => {
  const h = libraryHarness();
  const home = h.home();
  assert.match(textOf(home), /No instances yet/);
  assert.equal(nodes(home, (n) => typeof n.type === 'function' && n.type.name === 'Lead').length, 0);
  ofType(home, parts.Button)[0].props.onClick();
  assert.equal(h.calls.create, 1);
});

test('featured action renders the backend install and blocked labels and reasons', () => {
  for (const [action, label, icon] of /** @type {const} */ ([
    ['install', 'Install required files', 'download'],
    ['blocked', 'Review launch issue', 'alert'],
  ])) {
    const h = libraryHarness([
      instance('opaque-id', {
        launch_action: { primary_action: action, launchable: false, label, disabled_reason: 'Backend refusal reason' },
      }),
    ]);
    const banner = ownComponent(h.home(), 'Lead');
    const button = ofType(banner, parts.Button)[0];
    assert.equal(textOf(button), label);
    assert.equal(button.props.icon, icon);
    if (action === 'blocked') assert.equal(button.props.title, 'Backend refusal reason');
    const click = event();
    button.props.onClick(click);
    assert.equal(h.calls.navigations[0].id, 'opaque-id');
  }
});

test('home Play launches the captured instance and shelf buttons navigate with contextual menus', () => {
  const h = libraryHarness([instance('keyboard'), instance('shelf')]);
  const lead = ownComponent(h.home(), 'Lead');
  ofType(lead, parts.Button)[0].props.onClick();
  assert.deepEqual(h.launches, { selected: 'keyboard', count: 1 });
  ofType(lead, parts.Button)[1].props.onClick();
  const shelf = ownComponent(h.home(), 'Shelf');
  const cover = nodes(shelf, (n) => n.props.class === 'cp-home-item')[0];
  assert.equal(cover.type, 'button');
  /** @type {() => void} */ (cover.props.onClick)();
  assert.deepEqual(
    h.calls.navigations.map((n) => n.id),
    ['keyboard', 'shelf'],
  );
  /** @type {(e: TestEvent) => void} */ (cover.props.onContextMenu)(event());
  const menu = h.calls.menus[0];
  assert.ok('inst' in menu);
  assert.equal(menu.inst.id, 'shelf');
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
  let banner = ownComponent(h.home(), 'Lead');
  let button = ofType(banner, parts.Button)[0];
  assert.equal(textOf(button), 'Open');
  assert.equal(banner.props['aria-label'], 'Instance busy');
  const sessionPill = ownComponent(banner, 'SessionChip');
  assert.equal(textOf(sessionPill), 'Settling process output');
  assert.equal(sessionPill.props.icon, 'clock');
  assert.equal(sessionPill.props.tone, 'accent');
  delete h.store.launchSessions.value.busy;
  banner = ownComponent(h.home(), 'Lead');
  assert.ok(ofType(banner, parts.Chip).some((n) => textOf(n) === 'Waiting for Java'));
  h.store.launchSessions.value.busy = { viewModel: { playing: false, label: 'Settling process output' } };
  h.installs.clear();
  banner = ownComponent(h.home(), 'Lead');
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
  assert.match(textOf(row), /Backend loader Backend build/);
  assert.match(textOf(row), /Backend Minecraft/);
  assert.equal(nodes(row, (n) => n.props.class === 'cp-library-cell')[1].props.children, 3);
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
  const lead = rows.RowLead(ofType(row, rows.RowLead)[0].props);
  ofType(lead, parts.SelectionCheckbox)[0].props.onToggle(selectEvent);
  assert.equal(selectEvent.stopped, 1);
  assert.ok(h.selectedIds.has('row'));
  const menuEvent = event();
  const overflow = ofType(rows.RowMenu(ofType(row, rows.RowMenu)[0].props), parts.IconButton)[0];
  assert.equal(overflow.props.tooltip, 'Actions for Instance row');
  overflow.props.onClick(menuEvent);
  assert.equal(menuEvent.stopped, 1);
  assert.equal(h.calls.menus.length, 1);
  assert.equal(h.calls.navigations.length, 2);
});

test('library distinguishes an empty library from an unmatched filter', () => {
  const h = libraryHarness();
  let library = h.library();
  assert.equal(ofType(library, parts.Empty)[0].props.title, 'No instances yet');
  assert.ok(ofType(library, parts.Empty)[0].props.actions);
  assert.equal(ofType(library, parts.Button).length, 1);
  h.store.instances.value = [instance('existing')];
  ofType(library, parts.Input)[0].props.onChange('unmatched');
  library = h.library();
  assert.equal(ofType(library, parts.Empty)[0].props.title, 'No matches');
  assert.equal(ofType(library, parts.Empty)[0].props.actions, false);
  assert.equal(ofType(library, parts.Button).length, 1);
});

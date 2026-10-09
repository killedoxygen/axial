import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import type { EnrichedInstance } from '../../src/types-instance';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const requireDependency = createRequire(resolve(frontend, 'package.json'));
const ts: typeof import('typescript') = requireDependency('typescript');
const signals: typeof import('@preact/signals') = requireDependency('@preact/signals');
const jsx = requireDependency('preact/jsx-runtime');
type Node = { type: unknown; props: Record<string, unknown>; ref?: { current: unknown } };
type Event = {
  key?: string;
  target: Element;
  shiftKey?: boolean;
  defaultPrevented: boolean;
  stopped: boolean;
  preventDefault(): void;
  stopPropagation(): void;
};
type Listener = (event: Event) => void;

function nodes(value: unknown): Node[] {
  if (Array.isArray(value)) return value.flatMap(nodes);
  if (!value || typeof value !== 'object' || !('props' in value)) return [];
  const node = value as Node;
  return [node, ...nodes(node.props.children)];
}

function source<T>(path: string, imports: Record<string, unknown>, globals: Record<string, unknown>): T {
  const filename = resolve(frontend, 'src', path);
  const { outputText } = ts.transpileModule(readFileSync(filename, 'utf8'), {
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
    outputText,
    {
      exports,
      ...globals,
      require(id: string): unknown {
        if (id === '@preact/signals') return signals;
        if (id === 'preact/jsx-runtime') return jsx;
        if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
        throw new Error(`Unreviewed modal dependency: ${id}`);
      },
    },
    { filename },
  );
  return exports as T;
}

function hooks() {
  let cursor = 0;
  const values: unknown[] = [];
  const effects = new Map<number, { dependencies: unknown[]; cleanup?: () => void }>();
  let pending: Array<() => void> = [];
  return {
    begin() {
      cursor = 0;
    },
    flush() {
      const callbacks = pending;
      pending = [];
      for (const callback of callbacks) callback();
    },
    useRef<T>(initial: T) {
      const index = cursor++;
      values[index] ??= { current: initial };
      return values[index] as { current: T };
    },
    useState<T>(initial: T | (() => T)): [T, (next: T | ((previous: T) => T)) => void] {
      const index = cursor++;
      if (!(index in values)) values[index] = typeof initial === 'function' ? (initial as () => T)() : initial;
      return [
        values[index] as T,
        (next) => {
          values[index] = typeof next === 'function' ? (next as (previous: T) => T)(values[index] as T) : next;
        },
      ];
    },
    useMemo<T>(compute: () => T) {
      return compute();
    },
    useCallback<T>(callback: T) {
      return callback;
    },
    unmount() {
      for (const effect of effects.values()) effect.cleanup?.();
      effects.clear();
      values.length = 0;
      pending = [];
    },
    useEffect(run: () => void | (() => void), dependencies: unknown[]) {
      const index = cursor++;
      const previous = effects.get(index);
      if (
        previous &&
        dependencies.length === previous.dependencies.length &&
        dependencies.every((value, offset) => Object.is(value, previous.dependencies[offset]))
      )
        return;
      pending.push(() => {
        previous?.cleanup?.();
        effects.set(index, { dependencies, cleanup: run() || undefined });
      });
    },
  };
}

class Element {
  inert = false;
  disabled = false;
  slot = '';
  tabIndex = 0;
  isConnected = true;
  offsetParent = {};
  parent: Element | null = null;
  children: Element[] = [];
  listeners = new Map<string, Set<Listener>>();
  constructor(
    readonly name: string,
    readonly onFocus: (target: Element) => void,
  ) {}
  focus() {
    if (this.isConnected && !this.disabled && !this.closest('[inert]')) this.onFocus(this);
  }
  matches(selector: string): boolean {
    if (selector === ':disabled') return this.disabled;
    if (selector === '[inert]') return this.inert;
    if (selector === '[data-slot="modal-content"]') return this.slot === 'modal-content';
    throw new Error(`Unreviewed focus selector: ${selector}`);
  }
  closest(selector: string): Element | null {
    return this.matches(selector) ? this : (this.parent?.closest(selector) ?? null);
  }
  contains(target: Element | null): boolean {
    return target !== null && (target === this || this.children.some((child) => child.contains(target)));
  }
  querySelector() {
    return this.children[0];
  }
  querySelectorAll() {
    return this.children.filter((child) => !child.disabled);
  }
  addEventListener(name: string, listener: Listener) {
    if (!this.listeners.has(name)) this.listeners.set(name, new Set());
    this.listeners.get(name)!.add(listener);
  }
  removeEventListener(name: string, listener: Listener) {
    this.listeners.get(name)?.delete(listener);
  }
}

const instance: EnrichedInstance = {
  id: 'screenshot-instance',
  name: 'Screenshot fixture',
  version_id: 'fixture',
  created_at: '2026-10-04T12:00:00Z',
  java_selection: { kind: 'inherited' },
  revision: 1,
  version_display: {
    loader_key: 'vanilla',
    loader_label: 'Vanilla',
    minecraft_label: 'Fixture',
    loader_version_label: '',
    loader_detail_label: '',
    summary_label: 'Fixture',
    supports_mods: false,
  },
  launchable: true,
  launch_action: { state_id: 'ready', label: 'Launch', tone: 'ok', launchable: true, primary_action: 'launch' },
  saves_count: 0,
  mods_count: 0,
  resource_count: 0,
  shader_count: 0,
};

function harness(
  apiResult: () => Promise<unknown> = async () => ({ status: 'ok', name: 'after.png' }),
  withPane = false,
) {
  const docListeners = new Map<string, Set<Listener>>();
  let active: Element;
  const element = (name: string): Element =>
    new Element(name, (target) => {
      active = target;
      for (const listener of docListeners.get('focusin') ?? []) listener(event(undefined, target));
    });
  const body = element('body');
  active = body;
  const panel = element('lightbox');
  const opener = element('Rename screenshot');
  const folder = element('Open screenshots folder');
  const remove = element('Delete screenshot');
  const close = element('Close lightbox');
  panel.children = [opener, folder, remove, close];
  for (const child of panel.children) child.parent = panel;
  const dialog = element('prompt');
  const input = element('filename');
  const cancel = element('Cancel');
  const confirm = element('Rename');
  for (const child of [input, cancel, confirm]) child.parent = dialog;
  const frames = new Map<number, () => void>();
  let frameId = 0;
  const globals = {
    Error,
    Image: class {
      src = '';
    },
    HTMLElement: Element,
    document: {
      body,
      get activeElement() {
        return active;
      },
      addEventListener(name: string, listener: Listener) {
        if (!docListeners.has(name)) docListeners.set(name, new Set());
        docListeners.get(name)!.add(listener);
      },
      removeEventListener(name: string, listener: Listener) {
        docListeners.get(name)?.delete(listener);
      },
    },
    window: {
      requestAnimationFrame(callback: () => void) {
        const id = ++frameId;
        frames.set(id, callback);
        return id;
      },
      cancelAnimationFrame(id: number) {
        frames.delete(id);
      },
    },
  };
  const dialogHooks = hooks();
  const dialogs = source<typeof import('../../src/ui/Dialog')>(
    'ui/Dialog.tsx',
    {
      'preact/hooks': dialogHooks,
      './Atoms': { Button: 'Button', Input: 'Input' },
    },
    globals,
  );
  let closed = 0;
  const modalHooks = hooks();
  const modal = source<typeof import('../../src/ui/Modal')>(
    'ui/Modal.tsx',
    {
      preact: { createContext: () => ({}) },
      'preact/compat': { createPortal: (children: unknown) => children },
      'preact/hooks': {
        ...modalHooks,
        useContext: () => ({
          close: () => {
            closed++;
          },
        }),
      },
      './Icons': { Icon: 'Icon' },
      '../utils': { cn: (...names: string[]) => names.filter(Boolean).join(' ') },
      './Dialog': dialogs,
    },
    globals,
  );
  const calls: Parameters<typeof import('../../src/api').api>[] = [];
  const requestEntered = heldResponse();
  const api = {
    api: (...args: Parameters<typeof import('../../src/api').api>) => {
      calls.push(args);
      requestEntered.resolve(undefined);
      return apiResult();
    },
    apiResourceUrl: (path: string) => path,
  };
  const notices: string[] = [];
  const toast = { toast: (message: string) => notices.push(message) };
  const utils = source<typeof import('../../src/utils')>('utils.ts', { './store': {}, './toast': toast }, globals);
  const dto = source<typeof import('../../src/dto-contract')>('dto-contract.ts', {}, globals);
  const resources = source<typeof import('../../src/views/instance/resources')>(
    'views/instance/resources.ts',
    { '../../api': api, '../../dto-core': {}, '../../dto-contract': dto },
    globals,
  );
  const mutations = source<typeof import('../../src/views/instance/bulk-actions')>(
    'views/instance/bulk-actions.ts',
    { '../../ui/Dialog': dialogs, '../../toast': toast, '../../utils': utils },
    globals,
  );
  const actions = source<typeof import('../../src/views/instance/screenshot-actions')>(
    'views/instance/screenshot-actions.ts',
    {
      '../../api': api,
      '../../toast': toast,
      '../../ui/Dialog': dialogs,
      './instance-actions': {},
      './bulk-actions': mutations,
      '../../dto-contract': dto,
      './resources': resources,
    },
    globals,
  );
  const atoms = source<typeof import('../../src/ui/Atoms')>('ui/Atoms.tsx', { './Icons': { Icon: 'Icon' } }, globals);
  const lightboxHooks = hooks();
  let mutation: Promise<void> | undefined;
  let shot = { name: 'before.png', size: 512, modified_at: '2026-10-04T12:00:00Z' };
  const renamed: string[] = [];
  const lightbox = source<typeof import('../../src/views/instance/components/screenshot-lightbox')>(
    'views/instance/components/screenshot-lightbox.tsx',
    {
      'preact/hooks': lightboxHooks,
      '../../../ui/Modal': modal,
      '../../../ui/Atoms': atoms,
      '../../../format': source('format.ts', {}, globals),
      '../instance-actions': {},
      '../bulk-actions': mutations,
      './resource-bits': { ResourceMutationStatus: 'ResourceMutationStatus' },
      '../screenshot-actions': {
        ...actions,
        renameScreenshot(...args: Parameters<typeof actions.renameScreenshot>) {
          mutation = actions.renameScreenshot(...args);
          return mutation;
        },
      },
    },
    globals,
  );
  const paneHooks = hooks();
  const pane = source<typeof import('../../src/views/instance/tabs/ScreenshotsPane')>(
    'views/instance/tabs/ScreenshotsPane.tsx',
    {
      'preact/hooks': paneHooks,
      '../../../ui/Icons': { Icon: 'Icon' },
      '../../../ui/Atoms': atoms,
      '../../../ui/ContextMenu': {},
      '../../../ui/SelectionActionTray': {
        SelectionActionTray: 'SelectionActionTray',
        SelectionCheckbox: 'SelectionCheckbox',
      },
      '../../../ui/selection': source('ui/selection.ts', { 'preact/hooks': paneHooks }, globals),
      '../../../format': source('format.ts', {}, globals),
      '../instance-actions': {},
      '../components/resource-bits': {
        ResourceEmpty: 'ResourceEmpty',
        ResourceStatus: 'ResourceStatus',
        ResourceMutationStatus: 'ResourceMutationStatus',
      },
      '../components/screenshot-lightbox': lightbox,
      '../screenshot-actions': actions,
      '../bulk-actions': mutations,
    },
    globals,
  );
  let refreshes = 0;
  let paneTree: Node[] = [];
  let lightboxTree: Node[] = [];
  let resourceState: import('../../src/views/instance/resources').ResourceLoadState = {
    status: 'ready',
    data: { ...resources.emptyResources(), screenshots: [shot, { ...shot, name: 'neighbor.png' }] },
  };
  let renameClick: (() => void) | undefined;
  let modalTree: Node[] = [];
  let dialogTree: Node[] = [];
  function render() {
    if (withPane) {
      paneHooks.begin();
      paneTree = nodes(
        pane.ScreenshotsPane({
          inst: instance,
          resources: resourceState,
          onRefresh: () => {
            refreshes++;
            resourceState = { status: 'loading', data: resourceState.data };
          },
        }),
      );
      paneHooks.flush();
    }
    const selectedLightbox = paneTree.find((node) => node.type === lightbox.ScreenshotLightbox);
    lightboxHooks.begin();
    const tree = nodes(
      withPane
        ? selectedLightbox
          ? lightbox.ScreenshotLightbox(selectedLightbox.props as Parameters<typeof lightbox.ScreenshotLightbox>[0])
          : null
        : lightbox.ScreenshotLightbox({
            inst: instance,
            shots: [shot],
            name: shot.name,
            onSelect: () => undefined,
            onClose: () => {
              closed++;
            },
            onRename: (_previous, name) => {
              renamed.push(name);
              shot = { ...shot, name };
            },
            onRefresh: () => undefined,
          }),
    );
    lightboxTree = tree;
    lightboxHooks.flush();
    if (withPane && tree.length === 0) {
      if (!selectedLightbox) lightboxHooks.unmount();
      modalHooks.unmount();
      modalTree = [];
      panel.isConnected = false;
    } else {
      panel.isConnected = true;
      for (const [label, target] of [
        ['Rename', opener],
        ['Delete', remove],
      ] as const) {
        const iconButton = tree.find((node) => node.type === atoms.IconButton && node.props.tooltip === label)!;
        const button = nodes(atoms.IconButton(iconButton.props as Parameters<typeof atoms.IconButton>[0]))[0]!;
        target.disabled = button.props.disabled === true;
        if (target === opener) renameClick = button.props.onClick as (() => void) | undefined;
      }
      const modalContent = tree.find((node) => node.type === modal.ModalContent)!;
      modalHooks.begin();
      modalTree = nodes(modal.ModalContent(modalContent.props));
      const content = modalTree.find((node) => node.props['data-slot'] === 'modal-content')!;
      panel.slot = content.props['data-slot'] as string;
      panel.tabIndex = content.props.tabIndex as number;
      panel.inert = content.props.inert === true;
      if (panel.inert && panel.contains(active)) active = body;
      assert.ok(content.ref);
      content.ref.current = panel;
      modalHooks.flush();
    }
    // Dialog initializes its input draft in an effect; commit its ensuing render too.
    for (let pass = 0; pass < 2; pass++) {
      dialogHooks.begin();
      dialogTree = nodes(dialogs.DialogHost());
      const root = dialogTree.find((node) => node.props.role === 'dialog');
      dialog.isConnected = Boolean(root);
      for (const child of [input, cancel, confirm]) child.isConnected = Boolean(root);
      if (root) {
        assert.ok(root.ref);
        root.ref.current = dialog;
        dialog.children = dialogTree.some((node) => node.type === 'Input')
          ? [input, cancel, confirm]
          : [cancel, confirm];
        const field = dialogTree.find((node) => node.type === 'Input');
        if (field?.props.inputRef) (field.props.inputRef as { current: unknown }).current = input;
        for (const node of dialogTree.filter((node) => node.type === 'Button' && node.props.buttonRef)) {
          const button = node.props.children === 'Cancel' ? cancel : confirm;
          button.disabled = node.props.disabled === true;
          (node.props.buttonRef as { current: unknown }).current = button;
        }
      } else if (dialog.contains(active)) active = body;
      dialogHooks.flush();
    }
  }
  function event(key: string | undefined, target = active, shiftKey = false): Event {
    return {
      key,
      target,
      shiftKey,
      defaultPrevented: false,
      stopped: false,
      preventDefault() {
        this.defaultPrevented = true;
      },
      stopPropagation() {
        this.stopped = true;
      },
    };
  }
  function sendKey(key: string, shiftKey = false): Event {
    const value = event(key, active, shiftKey);
    if (dialog.contains(active)) {
      if (active === input) {
        const field = dialogTree.find((node) => node.type === 'Input');
        (field?.props.onKeyDown as ((event: Event) => void) | undefined)?.(value);
      }
      for (const listener of dialog.listeners.get('keydown') ?? []) listener(value);
    }
    if (!value.stopped) for (const listener of docListeners.get('keydown') ?? []) listener(value);
    if (!value.defaultPrevented && !value.stopped && panel.contains(active) && !panel.inert) {
      if (key === 'Tab') {
        const buttons = panel.querySelectorAll();
        const index = buttons.indexOf(active);
        buttons[index + (shiftKey ? -1 : 1)]?.focus();
      } else if (key === ' ' && active === opener && !opener.disabled) renameClick?.();
    }
    return value;
  }
  render();
  return {
    dialogs,
    body,
    panel,
    opener,
    input,
    cancel,
    confirm,
    element,
    render,
    sendKey,
    calls,
    notices,
    renamed,
    view(name: string) {
      const button = paneTree.find((node) => node.props['aria-label'] === `View ${name}`);
      assert.ok(button);
      (button.props.onClick as () => void)();
      render();
    },
    lightboxAction(label: string) {
      const button = lightboxTree.find((node) => node.type === atoms.IconButton && node.props.tooltip === label);
      assert.ok(button);
      (button.props.onClick as () => void)();
      render();
    },
    viewer: () => lightboxTree.find((node) => node.type === modal.ModalContent)?.props['aria-label'],
    refreshes: () => refreshes,
    completeRefresh() {
      resourceState = {
        status: 'ready',
        data: {
          ...resources.emptyResources(),
          screenshots: [
            { ...shot, name: 'after.png' },
            { ...shot, name: 'neighbor.png' },
          ],
        },
      };
      render();
      render();
    },
    requested: () => requestEntered.promise,
    mutationState: () => mutations.resourceMutationState(instance.id),
    async settled() {
      assert.ok(mutation, 'the real lightbox must have started a mutation');
      await mutation;
      render();
    },
    draft(value: string) {
      const field = dialogTree.find((node) => node.type === 'Input');
      assert.ok(field);
      (field.props.onChange as (value: string) => void)(value);
      render();
    },
    active: () => active,
    closed: () => closed,
    modalTree: () => modalTree,
    dialogTree: () => dialogTree,
    frames() {
      const callbacks = [...frames.values()];
      frames.clear();
      for (const callback of callbacks) callback();
    },
    pendingFrames: () => frames.size,
    focusListeners: () => docListeners.get('focusin')?.size ?? 0,
    click(label: string) {
      const button = dialogTree.find((node) => node.type === 'Button' && node.props.children === label);
      assert.ok(button);
      (button.props.onClick as () => void)();
    },
    prompt() {
      const result = dialogs.prompt('New name', 'before.png', { title: 'Rename screenshot', confirmText: 'Rename' });
      render();
      return result;
    },
  };
}

test('the real prompt suspends the lightbox and Escape cancels only the foreground dialog', async () => {
  const h = harness();
  assert.equal(h.active(), h.opener);
  const result = h.prompt();
  assert.equal(h.active(), h.body, 'making the lightbox inert releases its focused opener');
  h.frames();
  assert.equal(h.active(), h.input, 'the production focus frame focuses the prompt input');
  const modal = h.modalTree().find((node) => node.props.role === 'dialog')!;
  const overlay = h.modalTree().find((node) => node.props['data-slot'] === 'modal-overlay')!;
  assert.equal(modal.props['aria-modal'], undefined);
  assert.equal(modal.props['aria-hidden'], 'true');
  assert.equal(modal.props.inert, true);
  assert.equal(overlay.props.inert, true);
  assert.equal(overlay.props.onClick, undefined);
  assert.equal(h.dialogTree().find((node) => node.props.role === 'dialog')?.props['aria-modal'], 'true');
  h.confirm.focus();
  assert.equal(h.sendKey('Tab').defaultPrevented, true);
  assert.equal(h.active(), h.input);
  h.sendKey('Tab', true);
  assert.equal(h.active(), h.confirm);
  h.input.focus();
  h.sendKey('Escape');
  assert.equal(await result, null);
  assert.equal(h.closed(), 0);
  h.render();
  assert.equal(h.panel.inert, false);
  assert.equal(h.modalTree().find((node) => node.props.role === 'dialog')?.props['aria-modal'], 'true');
  h.frames();
  assert.equal(h.active(), h.opener, 'focus returns to the opener captured before autofocus and inert');
  assert.equal(h.pendingFrames(), 0);
  assert.equal(h.focusListeners(), 0);
  h.sendKey('Escape');
  assert.equal(h.closed(), 1);
});

test('confirming and replacing dialogs preserves the original opener without stale autofocus', async () => {
  const h = harness();
  const previous = h.prompt();
  const replacement = h.dialogs.showConfirm('Keep this name?', { title: 'Confirm rename' });
  h.render();
  assert.equal(await previous, null);
  h.frames();
  assert.equal(h.active(), h.confirm);
  h.click('Confirm');
  assert.equal(await replacement, true);
  h.render();
  h.frames();
  assert.equal(h.active(), h.opener);
  assert.equal(h.closed(), 0);
});

test('deferred focus return cannot steal focus across a new dialog lifetime or intentional focus movement', async () => {
  const h = harness();
  const first = h.prompt();
  h.frames();
  h.sendKey('Escape');
  await first;
  h.render();
  const other = h.element('other action');
  other.focus();
  const second = h.prompt();
  h.frames();
  h.sendKey('Escape');
  await second;
  h.render();
  h.frames();
  assert.equal(h.active(), other, 'opening and closing before the prior frame cancels its focus return');

  const third = h.prompt();
  h.frames();
  h.sendKey('Escape');
  await third;
  h.render();
  h.opener.focus();
  h.frames();
  assert.equal(h.active(), h.opener, 'a deliberate focus move wins over pending restoration');
  assert.equal(h.pendingFrames(), 0);
  assert.equal(h.focusListeners(), 0);
});

test('a dialog opened by the prior result inherits its opener before the return frame', async () => {
  const h = harness();
  const first = h.prompt();
  h.frames();
  const chained = first.then(() => h.dialogs.showConfirm('Continue?'));
  h.sendKey('Escape');
  await Promise.resolve();
  h.render();
  h.frames();
  assert.equal(h.active(), h.confirm);
  h.click('Cancel');
  assert.equal(await chained, false);
  h.render();
  h.frames();
  assert.equal(h.active(), h.opener);
  assert.equal(h.pendingFrames(), 0);
  assert.equal(h.focusListeners(), 0);
});

test('closing a prompt before its focus frame cannot focus its detached input', async () => {
  const h = harness();
  const result = h.prompt();
  assert.equal(h.active(), h.body);
  h.click('Cancel');
  assert.equal(await result, null);
  h.render();
  const other = h.element('other action');
  other.focus();
  h.frames();
  assert.equal(h.active(), other);
  assert.equal(h.pendingFrames(), 0);
  assert.equal(h.focusListeners(), 0);
});

function heldResponse() {
  let resolve!: (value: unknown) => void;
  const promise = new Promise<unknown>((yes) => {
    resolve = yes;
  });
  return { promise, resolve };
}

function submitScreenshotRename(h: ReturnType<typeof harness>) {
  assert.equal(h.active(), h.opener);
  h.sendKey(' ');
  h.render();
  h.frames();
  assert.equal(h.active(), h.input);
  assert.equal(h.mutationState().status, 'pending');
  h.draft('after.png');
  h.sendKey('Enter');
}

for (const outcome of ['success', 'refusal'] as const) {
  test(`a slow screenshot rename ${outcome} retains modal keyboard focus while its invoker is disabled`, async () => {
    const response = heldResponse();
    const h = harness(() => response.promise);
    submitScreenshotRename(h);
    await h.requested();
    h.render();
    assert.equal(
      JSON.stringify(h.calls),
      JSON.stringify([['PUT', '/instances/screenshot-instance/screenshots/before.png', { name: 'after.png' }]]),
    );
    assert.equal(h.opener.disabled, true);
    assert.equal(h.active(), h.body);
    assert.equal(h.panel.inert, false);
    assert.equal(h.panel.tabIndex, -1);
    assert.equal(h.modalTree().find((node) => node.props.role === 'dialog')?.props['aria-modal'], 'true');
    h.frames();
    assert.equal(h.active(), h.panel, 'the still-disabled invoker falls back to its existing modal panel');
    assert.equal(h.pendingFrames(), 0);
    assert.equal(h.focusListeners(), 0);

    response.resolve(
      outcome === 'success' ? { status: 'ok', name: 'after.png' } : { error: 'Screenshot name already exists.' },
    );
    await h.settled();
    h.frames();
    assert.equal(h.opener.disabled, false);
    assert.equal(h.mutationState().status, outcome === 'success' ? 'idle' : 'error');
    assert.deepEqual(h.renamed, outcome === 'success' ? ['after.png'] : []);
    assert.deepEqual(
      h.notices,
      outcome === 'success' ? ['Screenshot renamed'] : ['Rename screenshot: Screenshot name already exists.'],
    );
    assert.equal(h.active(), h.panel, 'settlement does not introduce a later focus transfer');
    h.sendKey('Tab');
    assert.equal(h.active(), h.opener);
    h.sendKey(' ');
    h.render();
    h.frames();
    assert.equal(h.active(), h.input, 'Tab then Space reopens the real screenshot prompt');
    h.draft('invalid.jpg');
    assert.equal(h.confirm.disabled, true);
    h.sendKey('Escape');
    await h.settled();
    h.frames();
    assert.equal(h.active(), h.opener, 'cancellation still restores the exact enabled invoker');
    assert.equal(h.calls.length, 1);
    assert.equal(h.closed(), 0);
  });
}

test('a screenshot rename settled before the return frame restores its exact invoker', async () => {
  const response = heldResponse();
  const h = harness(() => response.promise);
  submitScreenshotRename(h);
  await h.requested();
  h.render();
  assert.equal(h.opener.disabled, true);
  response.resolve({ status: 'ok', name: 'after.png' });
  await h.settled();
  assert.equal(h.opener.disabled, false);
  h.frames();
  assert.equal(h.active(), h.opener);
  assert.deepEqual(h.renamed, ['after.png']);
  assert.equal(h.mutationState().status, 'idle');
  assert.equal(h.pendingFrames(), 0);
  assert.equal(h.focusListeners(), 0);
});

test('a pending screenshot rename cannot steal intentional focus for its modal fallback', async () => {
  const response = heldResponse();
  const h = harness(() => response.promise);
  submitScreenshotRename(h);
  await h.requested();
  h.render();
  const other = h.element('other action');
  other.focus();
  h.frames();
  assert.equal(h.active(), other);
  response.resolve({ status: 'ok', name: 'after.png' });
  await h.settled();
  h.frames();
  assert.equal(h.active(), other);
  assert.equal(h.pendingFrames(), 0);
  assert.equal(h.focusListeners(), 0);
});

for (const choice of ['unchanged', 'close', 'navigate', 'reopen'] as const) {
  test(`a completed screenshot rename preserves the pane's ${choice} viewer choice`, async () => {
    const response = heldResponse();
    const h = harness(() => response.promise, true);
    h.view('before.png');
    submitScreenshotRename(h);
    await h.requested();
    h.render();
    try {
      if (choice === 'close' || choice === 'reopen') h.lightboxAction('Close');
      if (choice === 'navigate') h.lightboxAction('Next');
      if (choice === 'reopen') h.view('before.png');
    } finally {
      response.resolve({ status: 'ok', name: 'after.png' });
      await h.settled();
      h.frames();
    }
    assert.equal(
      h.viewer(),
      choice === 'close' || choice === 'reopen' ? undefined : choice === 'navigate' ? 'neighbor.png' : 'after.png',
    );
    assert.equal(h.refreshes(), 1, 'the successful rename still refreshes inventory');
    assert.deepEqual(h.notices, ['Screenshot renamed']);
    assert.equal(h.calls.length, 1);
    h.completeRefresh();
    assert.equal(
      h.viewer(),
      choice === 'close' || choice === 'reopen' ? undefined : choice === 'navigate' ? 'neighbor.png' : 'after.png',
    );
  });
}

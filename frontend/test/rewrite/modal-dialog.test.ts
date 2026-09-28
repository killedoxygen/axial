import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';

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
    useState<T>(initial: T): [T, (next: T) => void] {
      const index = cursor++;
      if (!(index in values)) values[index] = initial;
      return [
        values[index] as T,
        (next) => {
          values[index] = next;
        },
      ];
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
    if (this.isConnected && !this.closest('[inert]')) this.onFocus(this);
  }
  closest(_selector: string): Element | null {
    return this.inert ? this : (this.parent?.closest('[inert]') ?? null);
  }
  contains(target: Element | null): boolean {
    return target !== null && (target === this || this.children.some((child) => child.contains(target)));
  }
  querySelector() {
    return this.children[0];
  }
  querySelectorAll() {
    return this.children;
  }
  addEventListener(name: string, listener: Listener) {
    if (!this.listeners.has(name)) this.listeners.set(name, new Set());
    this.listeners.get(name)!.add(listener);
  }
  removeEventListener(name: string, listener: Listener) {
    this.listeners.get(name)?.delete(listener);
  }
}

function harness() {
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
  const close = element('Close lightbox');
  panel.children = [opener, close];
  for (const child of panel.children) child.parent = panel;
  const dialog = element('prompt');
  const input = element('filename');
  const cancel = element('Cancel');
  const confirm = element('Rename');
  for (const child of [input, cancel, confirm]) child.parent = dialog;
  const frames = new Map<number, () => void>();
  let frameId = 0;
  const globals = {
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
  let modalTree: Node[] = [];
  let dialogTree: Node[] = [];
  function render() {
    modalHooks.begin();
    modalTree = nodes(modal.ModalContent({ children: null, showCloseButton: false }));
    const content = modalTree.find((node) => node.props['data-slot'] === 'modal-content')!;
    panel.inert = content.props.inert === true;
    if (panel.inert && panel.contains(active)) active = body;
    assert.ok(content.ref);
    content.ref.current = panel;
    modalHooks.flush();
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
          (node.props.buttonRef as { current: unknown }).current = node.props.children === 'Cancel' ? cancel : confirm;
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

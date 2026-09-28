import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import * as contract from '../../src/dto-contract';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const dependency = createRequire(resolve(frontend, 'package.json'));
const ts: typeof import('typescript') = dependency('typescript');
const jsx: typeof import('preact/jsx-runtime') = dependency('preact/jsx-runtime');
const tick = (): Promise<void> => new Promise((done) => setImmediate(done));

interface Node {
  type: unknown;
  props: Record<string, unknown>;
}

function nodes(value: unknown): Node[] {
  if (Array.isArray(value)) return value.flatMap(nodes);
  if (!value || typeof value !== 'object' || !('props' in value)) return [];
  const node = value as Node;
  return [node, ...Object.values(node.props).flatMap(nodes)];
}

function source<T>(path: string, imports: Record<string, unknown>, globals: Record<string, unknown> = {}): T {
  const filename = resolve(frontend, 'src', path);
  const { outputText } = ts.transpileModule(readFileSync(filename, 'utf8'), {
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
    outputText,
    {
      exports,
      require(id: string): unknown {
        if (id === 'preact/jsx-runtime') return jsx;
        if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
        throw new Error(`Unreviewed reset dependency: ${id}`);
      },
      ...globals,
    },
    { filename },
  );
  return exports as T;
}

function harness(options: { native?: boolean; development?: boolean; devLab?: boolean } = {}) {
  const commands: string[] = [];
  const confirmations: Array<{ message: string; destructive: boolean; confirmText: string }> = [];
  const window: { __TAURI__?: unknown } =
    options.native === false
      ? {}
      : {
          __TAURI__: {
            core: {
              invoke: async (command: string) => {
                commands.push(command);
              },
            },
          },
        };
  const native = source<typeof import('../../src/native')>('native.ts', { './dto-contract': contract }, { window });
  const devMode = { value: options.development !== false };
  let confirm: ((answer: boolean) => void) | undefined;
  const hooks: unknown[] = [];
  let cursor = 0;
  const section = source<typeof import('../../src/views/settings/AdvancedSettingsSection')>(
    'views/settings/AdvancedSettingsSection.tsx',
    {
      'preact/hooks': {
        useEffect() {},
        useState<T>(initial: T) {
          const index = cursor++;
          if (!(index in hooks)) hooks[index] = initial;
          return [
            hooks[index],
            (value: T) => {
              hooks[index] = value;
            },
          ];
        },
        useRef<T>(initial: T) {
          const index = cursor++;
          if (!(index in hooks)) hooks[index] = { current: initial };
          return hooks[index];
        },
      },
      '../../hooks/use-autosave': { saveConfigPatch: async () => {} },
      '../../native': native,
      '../../ui/Atoms': { Button: 'Button', Toggle: 'Toggle' },
      '../../ui/SettingsSheet': { SettingRow: 'SettingRow', SettingsSection: 'SettingsSection' },
      '../../ui-state': { navigate() {} },
      '../../store': { config: { value: { telemetry_enabled: false } }, devMode },
      '../../toast': { toast() {} },
      '../../utils': { errMessage: String },
      './InstanceImportRow': { InstanceImportRow: 'InstanceImportRow' },
      '../../preferences/persistence': { reloadApplication() {} },
      '../../ui/Dialog': {
        showConfirm(message: string, options: { destructive: boolean; confirmText: string }) {
          confirmations.push({ message, ...options });
          return new Promise<boolean>((resolve) => {
            confirm = resolve;
          });
        },
      },
    },
    { __AXIAL_ENABLE_DEV_LAB__: options.devLab ?? false },
  );
  return {
    commands,
    confirmations,
    devMode,
    window,
    render() {
      cursor = 0;
      return nodes(section.AdvancedSettingsSection());
    },
    confirm(answer: boolean) {
      assert.ok(confirm);
      confirm(answer);
      confirm = undefined;
    },
  };
}

function resetButton(tree: Node[]): Node {
  const button = tree.find((node) => node.type === 'Button' && node.props.children === 'Reset');
  assert.ok(button, 'the native development Reset button must remain available in a packaged frontend');
  return button;
}

test('packaged native development Reset confirms once, supports cancellation, and invokes the native owner', async () => {
  const h = harness();
  const tree = h.render();
  assert.ok(!tree.some((node) => node.props.children === 'Open lab'));
  const click = resetButton(tree).props.onClick as () => void;
  click();
  click();
  await tick();
  assert.equal(h.confirmations.length, 1);
  assert.equal(h.confirmations[0].destructive, true);
  assert.equal(h.confirmations[0].confirmText, 'Reset');
  assert.match(h.confirmations[0].message, /this isolated Axial rewrite development profile/);
  assert.deepEqual(h.commands, []);
  h.confirm(false);
  await tick();
  assert.deepEqual(h.commands, []);
  (resetButton(h.render()).props.onClick as () => void)();
  await tick();
  assert.equal(h.confirmations.length, 2);
  h.confirm(true);
  await tick();
  assert.deepEqual(h.commands, ['app_reset']);
  assert.ok(
    h.render().some((node) => node.type === 'Button' && node.props.children === 'Resetting…' && node.props.disabled),
  );
});

test('release and browser settings never render the native development Reset control', () => {
  for (const devLab of [false, true]) {
    for (const options of [
      { native: true, development: false },
      { native: false, development: true },
    ]) {
      const h = harness({ ...options, devLab });
      assert.ok(!h.render().some((node) => node.props.title === 'Reset launcher'));
      assert.deepEqual(h.commands, []);
    }
  }
});

test('a previously rendered Reset handler rechecks development mode and native availability', async () => {
  for (const change of ['development', 'native']) {
    const h = harness({ devLab: true });
    const click = resetButton(h.render()).props.onClick as () => void;
    if (change === 'development') h.devMode.value = false;
    else delete h.window.__TAURI__;
    click();
    await tick();
    assert.deepEqual(h.confirmations, []);
    assert.deepEqual(h.commands, []);
  }
});

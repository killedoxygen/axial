import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, dirname, posix, relative, resolve } from 'node:path';
import vm from 'node:vm';

export const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
export const requireDependency = createRequire(resolve(frontend, 'package.json'));
const ts: typeof import('typescript') = requireDependency('typescript');
const sourceRoot = resolve(frontend, 'src');

/**
 * Runs one source module with its imports replaced. Relative imports are keyed by their path under `src`
 * without an extension (`app/toast`, `install/queue`), so a stub names the module it stands in for wherever it is
 * imported from. Packages are keyed by name and load for real unless stubbed. A missing stub is an error, so a test
 * reviews every collaborator its module reaches.
 */
export function loadSource<T>(
  path: string,
  stubs: Record<string, unknown> = {},
  globals: Record<string, unknown> = {},
): T {
  const filename = resolve(sourceRoot, path);
  const { outputText } = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: {
      module: ts.ModuleKind.CommonJS,
      target: ts.ScriptTarget.ES2022,
      jsx: ts.JsxEmit.ReactJSX,
      jsxImportSource: 'preact',
      esModuleInterop: true,
    },
  });
  const exports: Record<string, unknown> = {};
  const require = (id: string): unknown => {
    const key = id.startsWith('.') ? sourceKey(resolve(dirname(filename), id)) : id;
    if (Object.prototype.hasOwnProperty.call(stubs, key)) return stubs[key];
    if (!id.startsWith('.')) return requireDependency(id);
    throw new Error(`Unreviewed dependency of ${path}: ${key}`);
  };
  vm.runInNewContext(
    outputText,
    {
      exports,
      module: { exports },
      require,
      console,
      Error,
      TypeError,
      Promise,
      URL,
      URLSearchParams,
      structuredClone,
      setTimeout,
      clearTimeout,
      queueMicrotask,
      __AXIAL_MOCK_API__: false,
      ...globals,
    },
    { filename },
  );
  return exports as T;
}

function sourceKey(file: string): string {
  return relative(sourceRoot, file)
    .split(/[\\/]/)
    .join(posix.sep)
    .replace(/\.(tsx?|mjs|js)$/, '');
}

/** A request the transport would send; the test answers it. */
export type Call = (method: string, path: string, body?: unknown) => Promise<unknown>;

export function isApiError(error: unknown): error is Error & { status: number; payload?: unknown } {
  return (
    error instanceof Error && error.name === 'ApiError' && typeof (error as { status?: unknown }).status === 'number'
  );
}

/**
 * A real feature `api.ts` over a fake transport, so its paths and bodies are the ones under test. `subscribe` stands
 * in for the event stream.
 */
export function apiModule<T>(
  path: string,
  call: Call,
  subscribe: (
    path: string,
    options: { onValue(value: unknown, event: string, revision: number | null): void; onError?(error: unknown): void },
  ) => () => void = () => () => {},
): T {
  return loadSource<T>(path, {
    'platform/transport': {
      request: call,
      send: (method: string, target: string, body: unknown) => call(method, target, body),
      apiResourceUrl: (target: string) => `/api/v1${target}`,
      isApiError,
    },
    'platform/events': { subscribeApiEvents: subscribe },
  });
}

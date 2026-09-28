import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { stripTypeScriptTypes } from 'node:module';
import { basename, resolve } from 'node:path';
import { test } from 'node:test';
import vm from 'node:vm';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const source = await readFile(resolve(frontend, 'src/error-reporting.ts'), 'utf8');
const executable = stripTypeScriptTypes(source)
  .replace("import { api } from './api';", '')
  .replace("import { config } from './store';", '')
  .replace(/export function /g, 'function ');

/**
 * @typedef {[method: string, path: string, payload: { kind: 'error' | 'unhandledrejection' | 'render', name: string, message: string }]} ReportCall
 * @typedef {(event: { reason: unknown }) => void} RejectionHandler
 * @typedef {(message: string, source: string, lineno: number, colno: number, error: unknown) => boolean | void} ErrorHandler
 */
/** @param {boolean | null} [consent] @param {(...args: ReportCall) => Promise<void>} [send] */
function reporter(consent = true, send = async () => {}) {
  /** @type {ReportCall[]} */
  const calls = [];
  /** @type {Map<string, RejectionHandler>} */
  const events = new Map();
  const settings = { value: consent === null ? null : { telemetry_enabled: consent } };
  /** @type {{ onerror: ErrorHandler | null, addEventListener: (name: string, callback: RejectionHandler) => void }} */
  const window = { onerror: null, addEventListener: (name, callback) => events.set(name, callback) };
  const context = vm.createContext({
    window,
    config: { peek: () => settings.value },
    /** @param {ReportCall} args */
    api: (...args) => {
      calls.push(args);
      return send(...args);
    },
    Error,
    TypeError,
    ReferenceError,
    RangeError,
    SyntaxError,
    URIError,
    EvalError,
  });
  vm.runInContext(`${executable}\nglobalThis.reporter = { initErrorReporting, reportRenderError };`, context);
  /** @type {typeof import('../../src/error-reporting')} */
  const exported = context.reporter;
  return { ...exported, calls, events, window, settings };
}

const settle = () => new Promise((resolve) => setImmediate(resolve));

test('unknown or disabled consent suppresses reports without examining thrown values', async () => {
  let reads = 0;
  const source = new Proxy(
    {},
    {
      getPrototypeOf() {
        reads += 1;
        throw new Error('private');
      },
    },
  );
  for (const enabled of [null, false]) {
    const app = reporter(enabled);
    app.initErrorReporting();
    app.reportRenderError(source);
    assert.ok(app.window.onerror);
    app.window.onerror('token=private', '/Users/private/file.ts', 4, 8, source);
    const onRejection = app.events.get('unhandledrejection');
    assert.ok(onRejection);
    onRejection({ reason: source });
    await settle();
    assert.equal(app.calls.length, 0);
  }
  assert.equal(reads, 0);
});

test('frontend payload contains only closed class, kind and fixed summary', async () => {
  const app = reporter();
  const error = new TypeError('/Users/alice/token=canary user@example.com');
  Object.defineProperty(error, 'message', {
    get() {
      throw new Error('message was read');
    },
  });
  Object.defineProperty(error, 'stack', {
    get() {
      throw new Error('stack was read');
    },
  });
  Object.defineProperty(error, 'constructor', {
    get() {
      throw new Error('constructor was read');
    },
  });
  app.reportRenderError(error);
  await settle();
  assert.deepEqual(JSON.parse(JSON.stringify(app.calls)), [
    ['POST', '/telemetry/frontend-error', { kind: 'render', name: 'TypeError', message: 'Render failed.' }],
  ]);
});

test('browser hooks install once and retain the existing error handler', async () => {
  const app = reporter();
  let previousCalls = 0;
  app.window.onerror = () => {
    previousCalls += 1;
    return true;
  };
  app.initErrorReporting();
  const installed = app.window.onerror;
  app.initErrorReporting();
  assert.equal(app.window.onerror, installed);
  assert.ok(app.window.onerror);
  assert.equal(app.window.onerror('secret', 'private.js', 1, 2, new Error('secret')), true);
  await settle();
  const onRejection = app.events.get('unhandledrejection');
  assert.ok(onRejection);
  onRejection({
    reason: {
      toString() {
        throw new Error('must not stringify');
      },
    },
  });
  await settle();
  assert.equal(previousCalls, 1);
  assert.equal(app.events.size, 1);
  assert.equal(app.calls.length, 2);
  assert.equal(app.calls[1][2].message, 'Unhandled promise rejection.');
});

test('deduplication and five-report session cap bound the error storm', async () => {
  const app = reporter();
  app.reportRenderError(new Error('first'));
  await settle();
  app.reportRenderError(new Error('different private message'));
  await settle();
  assert.equal(app.calls.length, 1);
  for (const ErrorType of [TypeError, RangeError, SyntaxError, URIError, ReferenceError, EvalError]) {
    app.reportRenderError(new ErrorType('private'));
    await settle();
  }
  assert.equal(app.calls.length, 5);
});

test('in-flight reporting suppresses recursion and consent is rechecked for every event', async () => {
  /** @type {() => void} */
  let finish = () => { throw new Error('Reporting request did not start'); };
  const app = reporter(
    true,
    () =>
      new Promise(/** @param {(value?: void) => void} resolve */ (resolve) => {
        finish = resolve;
      }),
  );
  app.reportRenderError(new TypeError('one'));
  app.reportRenderError(new RangeError('two'));
  assert.equal(app.calls.length, 1);
  finish();
  await settle();
  app.settings.value = { telemetry_enabled: false };
  app.reportRenderError(new RangeError('three'));
  await settle();
  assert.equal(app.calls.length, 1);
});

test('three asynchronous reporting failures stop further attempts', async () => {
  const app = reporter(true, async () => {
    throw new Error('private provider response');
  });
  for (let index = 0; index < 10; index += 1) {
    app.reportRenderError(new Error('private'));
    await settle();
  }
  assert.equal(app.calls.length, 3);
});

test('synchronous transport errors remain contained and do not strand the reporter', async () => {
  const app = reporter(true, () => {
    throw new Error('private transport exception');
  });
  for (let attempt = 0; attempt < 4; attempt += 1) {
    assert.doesNotThrow(() => app.reportRenderError(new Error('private')));
    await settle();
  }
  assert.equal(app.calls.length, 3);
});

test('success clears consecutive failures and consent restoration permits fresh reports', async () => {
  let fail = true;
  const app = reporter(true, async () => {
    if (fail) throw new Error('private');
  });
  app.reportRenderError(new Error('private'));
  await settle();
  app.reportRenderError(new Error('private'));
  await settle();
  fail = false;
  app.reportRenderError(new Error('private'));
  await settle();
  app.settings.value = { telemetry_enabled: false };
  app.reportRenderError(new TypeError('private'));
  await settle();
  assert.equal(app.calls.length, 3);
  app.settings.value = { telemetry_enabled: true };
  app.reportRenderError(new TypeError('private'));
  await settle();
  assert.equal(app.calls.length, 4);
});

test('hostile thrown objects cannot escape the reporter or expose raw diagnostics', async () => {
  const app = reporter();
  app.initErrorReporting();
  const error = new Proxy(
    {},
    {
      getPrototypeOf() {
        throw new Error('token=private');
      },
    },
  );
  assert.doesNotThrow(() => app.reportRenderError(error));
  const onError = app.window.onerror;
  assert.ok(onError);
  assert.doesNotThrow(() => onError('private', '/Users/private.ts', 1, 2, error));
  await settle();
  assert.equal(app.calls.length, 0);
  app.reportRenderError(new TypeError('private'));
  await settle();
  assert.equal(app.calls.length, 1);
  assert.deepEqual(JSON.parse(JSON.stringify(app.calls[0][2])), {
    kind: 'render',
    name: 'TypeError',
    message: 'Render failed.',
  });
});

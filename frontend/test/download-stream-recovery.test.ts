import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';

// The rewrite has one authenticated API stream, not a native invoke bridge.
// Exercise the same ownership/recovery guarantees against that actual owner.
const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const requireDependency = createRequire(resolve(frontend, 'package.json'));
const ts: typeof import('typescript') = requireDependency('typescript');
const flush = (): Promise<void> => new Promise((done) => setImmediate(done));

function deferred<T>() {
  let resolveValue!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolveValue = done; });
  return { promise, resolve: resolveValue };
}

function harness() {
  const sources: FakeEventSource[] = [];
  const seen: number[] = [];
  const errors: unknown[] = [];
  const tickets: string[] = [];
  const timers = new Map<number, { callback: () => void; delay: number }>();
  let nextTimer = 0;
  let ticketRead: () => Promise<string> = async () => `/read-ticket-${tickets.length}`;
  class FakeEventSource {
    readonly listeners = new Map<string, (event: { data: string }) => void>();
    onopen?: () => void;
    onerror?: () => void;
    closes = 0;
    constructor(readonly url: string) { sources.push(this); }
    addEventListener(name: string, listener: (event: { data: string }) => void): void { this.listeners.set(name, listener); }
    close(): void { this.closes++; }
    emit(value: number): void { this.raw(JSON.stringify({ revision: value, value })); }
    raw(data: string): void { this.listeners.get('queue')?.({ data }); }
  }
  const filename = resolve(frontend, 'src/backend/events.ts');
  const compiled = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename, compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.CommonJS },
  });
  const exports = {};
  vm.runInNewContext(compiled.outputText, {
    exports, Error, EventSource: FakeEventSource,
    require(id: string): unknown {
      assert.equal(id, '../api');
      return { apiEventSourceUrl: async (path: string) => { tickets.push(path); return ticketRead(); } };
    },
    setTimeout(callback: () => void, delay: number): number {
      const id = ++nextTimer; timers.set(id, { callback, delay }); return id;
    },
    clearTimeout(id: number): void { timers.delete(id); },
  }, { filename });
  const events = exports as typeof import('../src/backend/events');
  return {
    sources, seen, errors, tickets, timers,
    ticket(next: () => Promise<string>): void { ticketRead = next; },
    connect(onError: (error: unknown) => void = (error) => errors.push(error)): () => void {
      return events.subscribeApiEvents('/install/queue/events', {
        events: ['queue'],
        decode(value: unknown): number { assert.equal(typeof value, 'number'); return value as number; },
        onValue: (value) => { seen.push(value); }, onError,
      });
    },
    retry(): void {
      const timer = timers.entries().next().value;
      assert.ok(timer, 'expected a recovery timer');
      timers.delete(timer[0]); timer[1].callback();
    },
  };
}

test('a quiet connected install stream preserves its owner without replaying a command', async () => {
  const h = harness(); const close = h.connect(); await flush();
  h.sources[0].onopen?.(); h.sources[0].emit(1); await flush();
  assert.equal(h.tickets.length, 1);
  assert.equal(h.sources[0].closes, 0);
  assert.equal(h.timers.size, 0);
  close();
});

test('an interrupted install stream closes and obtains a fresh read ticket once', async () => {
  const h = harness(); const close = h.connect(); await flush();
  h.sources[0].onerror?.();
  assert.equal(h.sources[0].closes, 1);
  assert.equal(h.timers.size, 1);
  h.retry(); await flush();
  assert.equal(h.sources.length, 2);
  assert.notEqual(h.sources[0].url, h.sources[1].url);
  assert.deepEqual(h.tickets, ['/install/queue/events', '/install/queue/events']);
  close();
});

test('closing a recovering install owner cancels its pending reconnect', async () => {
  const h = harness(); const close = h.connect(); await flush();
  h.sources[0].onerror?.(); close();
  assert.equal(h.timers.size, 0);
  h.sources[0].onerror?.(); await flush();
  assert.equal(h.sources.length, 1);
});

test('a delayed read ticket cannot resurrect a closed install owner', async () => {
  const h = harness(); const ticket = deferred<string>(); h.ticket(() => ticket.promise);
  const close = h.connect(); close(); ticket.resolve('/late-ticket'); await flush();
  assert.equal(h.sources.length, 0);
  assert.equal(h.timers.size, 0);
});

test('delayed events lose ownership before they can mutate a replacement stream', async () => {
  const h = harness(); const close = h.connect(); await flush();
  h.sources[0].emit(2); h.sources[0].onerror?.(); h.retry(); await flush();
  h.sources[0].emit(100); h.sources[0].onerror?.(); h.sources[1].emit(3);
  assert.deepEqual(h.seen, [2, 3]);
  assert.equal(h.timers.size, 0);
  close();
});

test('malformed install snapshots immediately quarantine their source and recover', async () => {
  const h = harness(); const close = h.connect(); await flush();
  h.sources[0].raw('{'); h.sources[0].emit(10);
  assert.equal(h.sources[0].closes, 1);
  assert.deepEqual(h.seen, []);
  assert.equal(h.errors.length, 1);
  h.retry(); await flush(); h.sources[1].emit(11);
  assert.deepEqual(h.seen, [11]);
  close();
});

test('same-source recovery requests join instead of multiplying reconnects', async () => {
  const h = harness(); const close = h.connect(); await flush();
  h.sources[0].onerror?.(); h.sources[0].onerror?.(); h.sources[0].raw('bad');
  assert.equal(h.timers.size, 1); assert.equal(h.errors.length, 1);
  h.retry(); await flush();
  assert.equal(h.sources.length, 2); assert.equal(h.tickets.length, 2);
  close();
});

test('failed ticket reads retry with backoff and do not multiply user-facing errors', async () => {
  const h = harness(); h.ticket(async () => { throw new Error('Unavailable'); });
  const close = h.connect(); await flush();
  assert.equal([...h.timers.values()][0].delay, 250);
  h.retry(); await flush();
  assert.equal([...h.timers.values()][0].delay, 500);
  assert.equal(h.sources.length, 0); assert.equal(h.errors.length, 1);
  h.ticket(async () => '/recovered'); h.retry(); await flush(); h.sources[0].emit(1);
  assert.deepEqual(h.seen, [1]); close();
});

test('an error observer cannot interrupt recovery ownership', async () => {
  const h = harness(); const close = h.connect(() => { throw new Error('Observer failed'); }); await flush();
  h.sources[0].onerror?.(); assert.equal(h.timers.size, 1);
  h.retry(); await flush(); h.sources[1].emit(1);
  assert.deepEqual(h.seen, [1]); close();
});

test('a new connection delivers its full snapshot even when the backend revision restarts', async () => {
  const h = harness(); const close = h.connect(); await flush();
  h.sources[0].emit(80); h.sources[0].emit(79); h.sources[0].onerror?.();
  h.retry(); await flush(); h.sources[1].emit(1); h.sources[1].emit(1);
  assert.deepEqual(h.seen, [80, 1]); close();
});

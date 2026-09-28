import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import * as contracts from '../../src/dto-contract';
import * as core from '../../src/dto-core';
import * as installs from '../../src/dto-install';
import * as presenters from '../../src/create-presenters';
import type { EnrichedInstance } from '../../src/types-instance';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const ts: typeof import('typescript') = createRequire(resolve(frontend, 'package.json'))('typescript');

function response() {
  return {
    id: 'fixture-instance', name: 'Fixture pack', version_id: '1.21.4', created_at: '2026-09-27T00:00:00Z',
    launchable: false, install_target: null,
    launch_action: { state_id: 'setup_pending', label: 'Resume setup', tone: 'warn', launchable: false, primary_action: 'install' },
    version_display: { loader_key: 'vanilla', loader_label: 'Vanilla', minecraft_label: '1.21.4',
      loader_version_label: '', loader_detail_label: '', summary_label: '1.21.4', supports_mods: false },
    saves_count: 0, mods_count: 0, resource_count: 0, shader_count: 0,
    view_model: { tone: 'success', summary: 'Setup queued.', detail: null },
    install_queue: {
      queue_epoch: 'fixture-queue', revision: 3, registry_revision: 0, active: null, items: [], latest_failure: null,
      view_model: { state_id: 'idle', status_label: 'Idle', title: 'Downloads', summary: 'Ready', queued_count: 0,
        queued_count_label: 'No queued downloads', queued_item_label: 'No items queued', section_title: 'Queue',
        empty_title: 'Nothing downloading', empty_summary: 'Ready' },
    },
  };
}

function harness(options: { response?: unknown; requestError?: Error; queueError?: Error; hold?: boolean } = {}) {
  const requests: unknown[][] = [];
  const notices: unknown[][] = [];
  const queues: unknown[] = [];
  const reads: unknown[][] = [];
  const state = { instances: [core.enrichedInstanceResponse(response())] };
  let release!: () => void;
  const held = new Promise<void>((resolve) => { release = resolve; });
  const dependencies: Record<string, unknown> = {
    './api': { async api(...args: unknown[]) {
      requests.push(args);
      if (options.hold) await held;
      if (options.requestError) throw options.requestError;
      return options.response ?? response();
    } },
    './actions': { updateInstanceInList(instance: EnrichedInstance) {
      state.instances = state.instances.map((existing) => existing.id === instance.id ? instance : existing);
    } },
    './create-presenters': presenters,
    './dto-contract': contracts,
    './dto-core': core,
    './dto-install': installs,
    './instance-readiness': { async refreshInstanceReadiness(...args: unknown[]) { reads.push(['instance', ...args]); } },
    './machines/downloads': {
      async applyInstallQueueResponse(value: unknown) {
        queues.push(value);
        if (options.queueError) throw options.queueError;
      },
      async refreshInstallQueue(...args: unknown[]) { reads.push(['queue', ...args]); },
    },
    './toast': { toast(...args: unknown[]) { notices.push(args); } },
    './utils': { errMessage(error: unknown) { return error instanceof Error ? error.message : String(error); } },
  };
  const filename = resolve(frontend, 'src/instance-setup.ts');
  const compiled = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.CommonJS },
  });
  const exports = {};
  vm.runInNewContext(compiled.outputText, {
    exports, Error, encodeURIComponent,
    require(id: string): unknown {
      if (Object.prototype.hasOwnProperty.call(dependencies, id)) return dependencies[id];
      throw new Error(`Unreviewed setup dependency: ${id}`);
    },
  }, { filename });
  return { ...(exports as typeof import('../../src/instance-setup')), requests, notices, queues, reads, state, release };
}

test('resume uses the registered identity, shares repeated clicks and preserves backend availability', async () => {
  const h = harness({ hold: true });
  const first = h.resumeInstanceSetup('fixture-instance');
  const repeated = h.resumeInstanceSetup('fixture-instance');
  assert.equal(first, repeated);
  assert.equal(h.requests.length, 1);
  assert.equal(h.requests[0][0], 'POST');
  assert.equal(h.requests[0][1], '/instances/fixture-instance/setup/resume');
  h.release();
  assert.equal(await first, true);
  assert.equal(h.state.instances.length, 1);
  assert.equal(h.state.instances[0].launchable, false);
  assert.equal(h.state.instances[0].launch_action.label, 'Resume setup');
  assert.equal(h.queues.length, 1);
  assert.equal(h.reads.length, 0);
});

test('uncertain resume is reconciled with reads and never repeats creation or setup', async () => {
  const h = harness({ requestError: new Error('Response lost') });
  assert.equal(await h.resumeInstanceSetup('fixture-instance'), false);
  assert.equal(h.requests.length, 1);
  assert.deepEqual(h.reads.map((read) => read[0]).sort(), ['instance', 'queue']);
  assert.equal(h.state.instances.length, 1);
  assert.equal(h.queues.length, 0);
});

test('queue failure cannot turn a confirmed resume into another mutation', async () => {
  const h = harness({ queueError: new Error('Stream unavailable') });
  assert.equal(await h.resumeInstanceSetup('fixture-instance'), true);
  assert.equal(h.requests.length, 1);
  assert.equal(h.reads.length, 2);
  assert.match(String(h.notices[h.notices.length - 1]?.[0]), /download status could not be refreshed/);
});

test('a mismatched response identity is rejected before registry or queue publication', async () => {
  const h = harness({ response: { ...response(), id: 'unrelated-instance' } });
  assert.equal(await h.resumeInstanceSetup('fixture-instance'), false);
  assert.equal(h.state.instances[0].id, 'fixture-instance');
  assert.equal(h.queues.length, 0);
  assert.equal(h.reads.length, 2);
});

test('a late accepted response cannot recreate an instance removed from the current registry', async () => {
  const h = harness({ hold: true });
  const work = h.resumeInstanceSetup('fixture-instance');
  h.state.instances = [];
  h.release();
  assert.equal(await work, true);
  assert.deepEqual(h.state.instances, []);
});

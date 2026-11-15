import assert from 'node:assert/strict';
import test from 'node:test';
import { loadSource } from './source';
import { enrichedInstance, queueSnapshot } from './wire';
import * as presenters from '../src/create/presenters';
import type { CreateInstanceResponse } from '../src/generated/CreateInstanceResponse';

function response(): CreateInstanceResponse {
  return {
    ...enrichedInstance({
      name: 'Fixture pack',
      launchable: false,
      launch_action: {
        state_id: 'setup_pending',
        label: 'Resume setup',
        tone: 'warn',
        launchable: false,
        primary_action: 'install',
      },
    }),
    view_model: { state_id: 'setup_queued', tone: 'ok', title: 'Setup queued', summary: 'Setup queued.', detail: null },
    install_queue: queueSnapshot({ revision: 3 }),
  };
}

function harness(
  options: { response?: CreateInstanceResponse; requestError?: Error; queueError?: Error; hold?: boolean } = {},
) {
  const requests: string[] = [];
  const notices: unknown[][] = [];
  const queues: unknown[] = [];
  const reads: unknown[][] = [];
  const state = { instances: [enrichedInstance({ ...response(), launchable: false })] };
  let release!: () => void;
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  const setup = loadSource<typeof import('../src/instances/setup')>('instances/setup.ts', {
    'instances/api': {
      async resumeSetup(id: string) {
        requests.push(id);
        if (options.hold) await held;
        if (options.requestError) throw options.requestError;
        return options.response ?? response();
      },
    },
    'create/presenters': presenters,
    'instances/readiness': {
      async refreshInstanceReadiness(...args: unknown[]) {
        reads.push(['instance', ...args]);
      },
    },
    'install/queue': {
      async applyInstallQueueResponse(value: unknown) {
        queues.push(value);
        if (options.queueError) throw options.queueError;
      },
      async refreshInstallQueue(...args: unknown[]) {
        reads.push(['queue', ...args]);
      },
    },
    'app/toast': {
      toast(...args: unknown[]) {
        notices.push(args);
      },
      errMessage(error: unknown) {
        return error instanceof Error ? error.message : String(error);
      },
    },
  });
  return { ...setup, requests, notices, queues, reads, state, release };
}

test('resume uses the registered identity, shares repeated clicks and preserves backend availability', async () => {
  const h = harness({ hold: true });
  const first = h.resumeInstanceSetup('fixture-instance');
  const repeated = h.resumeInstanceSetup('fixture-instance');
  assert.equal(first, repeated);
  assert.equal(h.requests.length, 1);
  assert.deepEqual(h.requests, ['fixture-instance']);
  h.release();
  assert.equal(await first, true);
  assert.equal(h.state.instances.length, 1);
  assert.equal(h.state.instances[0].launchable, false);
  assert.equal(h.state.instances[0].launch_action.label, 'Resume setup');
  assert.equal(h.queues.length, 1);
  assert.deepEqual(h.reads, [['instance', 'fixture-instance']]);
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

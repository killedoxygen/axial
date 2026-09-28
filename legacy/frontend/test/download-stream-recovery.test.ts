import assert from 'node:assert/strict';
import test from 'node:test';

import {
  applyInstallStreamRecovery,
  awaitOwnedInstallValue,
  createInstallRecoveryCoordinator,
  terminalInstallReconciliationNeedsRefresh,
} from '../src/machines/downloads';

function deferred<T>(): {
  promise: Promise<T>;
  resolve(value: T): void;
} {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((onResolve) => {
    resolve = onResolve;
  });
  return { promise, resolve };
}

test('long silent native install with active status preserves its bridge without reinvoking', async () => {
  let bridgeInvokes = 1;
  let closes = 0;

  await applyInstallStreamRecovery('active', {
    preserveActiveSource: true,
    closeSource: () => {
      closes += 1;
    },
    restart: async () => {
      bridgeInvokes += 1;
    },
  });

  assert.equal(closes, 0);
  assert.equal(bridgeInvokes, 1);
});

test('unavailable native install status closes and reconnects the bridge once', async () => {
  let bridgeInvokes = 1;
  let closes = 0;

  await applyInstallStreamRecovery('unavailable', {
    preserveActiveSource: true,
    closeSource: () => {
      closes += 1;
    },
    restart: async () => {
      bridgeInvokes += 1;
    },
  });

  assert.equal(closes, 1);
  assert.equal(bridgeInvokes, 2);
});

test('resolved install status leaves completion cleanup with the current owner', async () => {
  let restarted = false;

  await applyInstallStreamRecovery('resolved', {
    preserveActiveSource: true,
    closeSource: () => assert.fail('resolved status must not run recovery cleanup'),
    restart: async () => {
      restarted = true;
    },
  });

  assert.equal(restarted, false);
});

test('SSE silence closes and reconnects when status remains active', async () => {
  let closes = 0;
  let reconnects = 0;

  await applyInstallStreamRecovery('active', {
    preserveActiveSource: false,
    closeSource: () => {
      closes += 1;
    },
    restart: async () => {
      reconnects += 1;
    },
  });

  assert.equal(closes, 1);
  assert.equal(reconnects, 1);
});

test('delayed status loses ownership before it can mutate a replacement install', async () => {
  const response = deferred<{ progress: number }>();
  let current = true;
  let mutations = 0;
  const pending = awaitOwnedInstallValue(
    () => response.promise,
    () => current,
  );

  current = false;
  response.resolve({ progress: 90 });
  const owned = await pending;
  if (owned.current) mutations += 1;

  assert.deepEqual(owned, { current: false });
  assert.equal(mutations, 0);
});

test('terminal queue reconciliation refreshes only active or unavailable status', () => {
  assert.deepEqual(
    (['active', 'unavailable', 'resolved', 'stale'] as const).map((status) => [
      status,
      terminalInstallReconciliationNeedsRefresh(status),
    ]),
    [
      ['active', true],
      ['unavailable', true],
      ['resolved', false],
      ['stale', false],
    ],
  );
});

test('delayed completion queue response is not applied after replacement ownership begins', async () => {
  const response = deferred<{ active: string }>();
  let completionOwner = true;
  let applied = 0;
  const pending = awaitOwnedInstallValue(
    () => response.promise,
    () => completionOwner,
  );

  completionOwner = false;
  response.resolve({ active: 'replacement' });
  const owned = await pending;
  if (owned.current) applied += 1;

  assert.deepEqual(owned, { current: false });
  assert.equal(applied, 0);
});

test('malformed source supersedes an in-flight silence reconciliation', async () => {
  const coordinator = createInstallRecoveryCoordinator();
  const source = {};
  const releaseSilence = deferred<void>();
  let staleSilencePreserves = 0;
  let closes = 0;
  let reconnects = 0;

  const silence = coordinator.run('install-a', source, 'silence', async (isCurrent) => {
    await releaseSilence.promise;
    if (isCurrent()) staleSilencePreserves += 1;
  });
  const malformed = coordinator.run('install-a', source, 'replace', async (isCurrent) => {
    assert.equal(isCurrent(), true);
    closes += 1;
    reconnects += 1;
  });

  await malformed;
  releaseSilence.resolve();
  await silence;

  assert.equal(closes, 1);
  assert.equal(reconnects, 1);
  assert.equal(staleSilencePreserves, 0);
});

test('malformed replacement source supersedes an older strong recovery', async () => {
  const coordinator = createInstallRecoveryCoordinator();
  const firstSource = {};
  const replacementSource = {};
  const releaseFirst = deferred<void>();
  let firstRetainedOwnership = false;
  let replacementRuns = 0;

  const first = coordinator.run('install-a', firstSource, 'replace', async (isCurrent) => {
    await releaseFirst.promise;
    firstRetainedOwnership = isCurrent();
  });
  const replacement = coordinator.run('install-a', replacementSource, 'replace', async (isCurrent) => {
    assert.equal(isCurrent(), true);
    replacementRuns += 1;
  });

  await replacement;
  releaseFirst.resolve();
  await first;

  assert.equal(replacementRuns, 1);
  assert.equal(firstRetainedOwnership, false);
});

test('same-source strong recovery joins instead of multiplying reconnects', async () => {
  const coordinator = createInstallRecoveryCoordinator();
  const source = {};
  const release = deferred<void>();
  let runs = 0;

  const first = coordinator.run('install-a', source, 'replace', async () => {
    runs += 1;
    await release.promise;
  });
  const joined = coordinator.run('install-a', source, 'replace', async () => {
    runs += 1;
  });

  assert.equal(first, joined);
  release.resolve();
  await Promise.all([first, joined]);
  assert.equal(runs, 1);
});

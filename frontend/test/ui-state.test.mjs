import assert from 'node:assert/strict';
import test from 'node:test';
import * as ui from '../src/ui-state';

class FakeView {
  #listeners = new Map();
  #scrollTop = 0;

  clientHeight = 400;
  firstElementChild = {};
  scrollHeight = 400;

  get scrollTop() {
    return this.#scrollTop;
  }

  set scrollTop(top) {
    this.#scrollTop = Math.max(0, Math.min(top, this.scrollHeight - this.clientHeight));
  }

  addEventListener(type, listener) {
    const listeners = this.#listeners.get(type) ?? new Set();
    listeners.add(listener);
    this.#listeners.set(type, listeners);
  }

  removeEventListener(type, listener) {
    this.#listeners.get(type)?.delete(listener);
  }

  emit(type) {
    for (const listener of this.#listeners.get(type) ?? []) listener();
  }
}

function installDom(view) {
  const original = {
    cancelAnimationFrame: globalThis.cancelAnimationFrame,
    document: globalThis.document,
    MutationObserver: globalThis.MutationObserver,
    requestAnimationFrame: globalThis.requestAnimationFrame,
    ResizeObserver: globalThis.ResizeObserver,
  };
  const animationFrames = new Map();
  const mutationObservers = [];
  const resizeObservers = [];
  let nextFrame = 1;

  globalThis.document = { querySelector: () => view };
  globalThis.requestAnimationFrame = (callback) => {
    const id = nextFrame++;
    animationFrames.set(id, callback);
    return id;
  };
  globalThis.cancelAnimationFrame = (id) => animationFrames.delete(id);
  globalThis.MutationObserver = class {
    constructor(callback) {
      this.callback = callback;
      this.disconnected = false;
      mutationObservers.push(this);
    }
    disconnect() {
      this.disconnected = true;
    }
    observe() {}
  };
  globalThis.ResizeObserver = class {
    constructor(callback) {
      this.callback = callback;
      this.disconnected = false;
      resizeObservers.push(this);
    }
    disconnect() {
      this.disconnected = true;
    }
    observe() {}
  };

  return {
    activeObserverCount() {
      return [...mutationObservers, ...resizeObservers].filter((observer) => !observer.disconnected).length;
    },
    flushResize() {
      for (const observer of resizeObservers) observer.callback([]);
      const queued = [...animationFrames.values()];
      animationFrames.clear();
      for (const callback of queued) callback(0);
    },
    restore() {
      Object.assign(globalThis, original);
    },
  };
}

test('route scroll keys include every route identity field', () => {
  assert.notEqual(
    ui.routeScrollKey({ name: 'discover', target: 'alpha' }),
    ui.routeScrollKey({ name: 'discover', target: 'beta' }),
  );
  assert.notEqual(
    ui.routeScrollKey({ name: 'content', id: 'project', target: 'alpha' }),
    ui.routeScrollKey({ name: 'content', id: 'project', target: 'beta' }),
  );
  assert.notEqual(
    ui.routeScrollKey({ name: 'content', id: 'a:b', target: 'c' }),
    ui.routeScrollKey({ name: 'content', id: 'a', target: 'b:c' }),
  );
});

test('scroll memory is bounded and promotes entries on reads', () => {
  const memory = ui.createViewScrollMemory(2);
  memory.set('a', 100);
  memory.set('b', 200);
  assert.equal(memory.get('a'), 100);
  memory.set('c', 300);

  assert.equal(memory.size, 2);
  assert.equal(memory.get('b'), undefined);
  assert.equal(memory.get('a'), 100);
  assert.equal(memory.get('c'), 300);

  memory.set('a', 0);
  assert.equal(memory.get('a'), undefined);
});

test('restoration retries after mounted async content becomes tall enough', () => {
  const view = new FakeView();
  view.scrollHeight = 1_400;
  const dom = installDom(view);
  const savedRoute = { name: 'discover', target: 'restore-after-load' };

  try {
    ui.route.value = savedRoute;
    view.scrollTop = 700;
    ui.navigate({ name: 'home' });
    view.scrollHeight = 400;
    ui.navigate(savedRoute);

    const key = ui.routeScrollKey(savedRoute);
    ui.prepareViewScroll(key);
    assert.equal(view.scrollTop, 0);
    const stop = ui.restoreViewScroll(key);

    view.scrollHeight = 1_200;
    dom.flushResize();
    assert.equal(view.scrollTop, 700);
    stop();
  } finally {
    dom.restore();
  }
});

test('explicit reset cancels a pending restore and forgets its position', () => {
  const view = new FakeView();
  view.scrollHeight = 1_400;
  const dom = installDom(view);
  const savedRoute = { name: 'discover', target: 'reset-instance' };

  try {
    ui.route.value = savedRoute;
    view.scrollTop = 650;
    ui.navigate({ name: 'home' });
    view.scrollHeight = 400;
    ui.navigate(savedRoute);
    ui.restoreViewScroll(ui.routeScrollKey(savedRoute));

    ui.resetViewScroll();
    view.scrollHeight = 1_200;
    dom.flushResize();
    assert.equal(view.scrollTop, 0);

    ui.navigate({ name: 'home' });
    ui.navigate(savedRoute);
    ui.prepareViewScroll(ui.routeScrollKey(savedRoute));
    assert.equal(view.scrollTop, 0);
  } finally {
    dom.restore();
  }
});

test('manual scrolling cancels a pending async restore', () => {
  const view = new FakeView();
  view.scrollHeight = 1_400;
  const dom = installDom(view);
  const savedRoute = { name: 'discover', target: 'manual-scroll' };

  try {
    ui.route.value = savedRoute;
    view.scrollTop = 600;
    ui.navigate({ name: 'home' });
    view.scrollHeight = 400;
    ui.navigate(savedRoute);
    ui.restoreViewScroll(ui.routeScrollKey(savedRoute));

    view.emit('wheel');
    view.scrollHeight = 1_200;
    dom.flushResize();
    assert.equal(view.scrollTop, 0);
  } finally {
    dom.restore();
  }
});

test('focus movement cancels a pending async restore', () => {
  const view = new FakeView();
  view.scrollHeight = 1_400;
  const dom = installDom(view);
  const savedRoute = { name: 'discover', target: 'keyboard-focus' };

  try {
    ui.route.value = savedRoute;
    view.scrollTop = 600;
    ui.navigate({ name: 'home' });
    view.scrollHeight = 400;
    ui.navigate(savedRoute);
    ui.restoreViewScroll(ui.routeScrollKey(savedRoute));

    view.emit('focusin');
    view.scrollHeight = 1_200;
    dom.flushResize();
    assert.equal(view.scrollTop, 0);
    assert.equal(dom.activeObserverCount(), 0);
  } finally {
    dom.restore();
  }
});

test('restoration deadline releases observers for permanently short content', async () => {
  const view = new FakeView();
  view.scrollHeight = 1_400;
  const dom = installDom(view);
  const savedRoute = { name: 'discover', target: 'short-content' };

  try {
    ui.route.value = savedRoute;
    view.scrollTop = 600;
    ui.navigate({ name: 'home' });
    view.scrollHeight = 400;
    ui.navigate(savedRoute);
    ui.restoreViewScroll(ui.routeScrollKey(savedRoute), 0);
    await new Promise((resolve) => setTimeout(resolve, 0));

    assert.equal(dom.activeObserverCount(), 0);
    view.scrollHeight = 1_200;
    dom.flushResize();
    assert.equal(view.scrollTop, 0);
  } finally {
    dom.restore();
  }
});

test('views with remount-local tabs and filters do not retain scroll', () => {
  const view = new FakeView();
  view.scrollHeight = 1_400;
  const dom = installDom(view);
  const settingsRoute = { name: 'settings' };

  try {
    ui.route.value = settingsRoute;
    view.scrollTop = 700;
    ui.navigate({ name: 'home' });
    view.scrollTop = 300;
    ui.navigate(settingsRoute);
    ui.prepareViewScroll(ui.routeScrollKey(settingsRoute));

    assert.equal(ui.routeSupportsViewScroll(settingsRoute), false);
    assert.equal(view.scrollTop, 0);
  } finally {
    dom.restore();
  }
});

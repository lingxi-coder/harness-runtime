import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';
import test from 'node:test';

const root = fileURLToPath(new URL('../../crates/plugins/lingxi-local-app/assets/templates/', import.meta.url));
const revision = 'r4';
const families = ['react-dom', 'canvas-2d', 'three-3d', 'phaser-2d', 'babylon-3d'];

function load(family, file, globals = {}) {
  let source = readFileSync(`${root}${family}/${revision}/lib/${file}`, 'utf8');
  const names = [...source.matchAll(/export (?:async )?(?:function|const) (\w+)/g)].map((match) => match[1]);
  source = source.replace(/^import [\s\S]*? from [^;]+;\n/gm, '').replace(/export /g, '');
  source = source.replace(/return \(\s*<LingXiBridgeContext.Provider[\s\S]*?<\/LingXiBridgeContext.Provider>\s*\);/, 'return value;');
  const context = vm.createContext({ console, ...globals });
  return vm.runInContext(`${source}\n;({${names.join(',')}})`, context);
}

function target() {
  const listeners = new Map();
  return {
    addEventListener(name, fn) { if (!listeners.has(name)) listeners.set(name, new Set()); listeners.get(name).add(fn); },
    removeEventListener(name, fn) { listeners.get(name)?.delete(fn); },
    emit(name) { for (const fn of [...(listeners.get(name) ?? [])]) fn(); },
    count() { return [...listeners.values()].reduce((sum, set) => sum + set.size, 0); },
  };
}

function browser() {
  const frames = new Map();
  let nextFrame = 0;
  const window = Object.assign(target(), {
    devicePixelRatio: 2,
    requestAnimationFrame(fn) { frames.set(++nextFrame, fn); return nextFrame; },
    cancelAnimationFrame(id) { frames.delete(id); },
    visualViewport: target(),
  });
  const document = Object.assign(target(), { visibilityState: 'visible' });
  let observer;
  class ResizeObserver {
    constructor(callback) { this.callback = callback; observer = this; }
    observe() {}
    disconnect() { this.disconnected = true; }
  }
  const canvas = { width: 0, height: 0, getBoundingClientRect: () => ({ width: 100, height: 80 }) };
  return { window, document, ResizeObserver, canvas, frames,
    get observer() { return observer; },
    tick(timestamp) { const pending = [...frames.values()]; frames.clear(); for (const fn of pending) fn(timestamp); },
  };
}

for (const family of families) {
  test(`${family}: Host v2 data mutations preserve native tagged contract`, async () => {
    const calls = [];
    const bridge = load(family, 'lingxi-bridge.js', { window: { lingxi: { v2: { data: { mutate: (request) => calls.push(request) } } } } });
    await bridge.upsertRecord('entries', 'one', { value: 3 }, 2);
    await bridge.deleteRecord('entries', 'one');
    assert.deepEqual(JSON.parse(JSON.stringify(calls)), [
      { collection: 'entries', operations: [{ kind: 'upsert', recordId: 'one', document: { value: 3 }, expectedRevision: 2 }] },
      { collection: 'entries', operations: [{ kind: 'delete', recordId: 'one' }] },
    ]);
  });
  test(`${family}: context getters remain live and invalid dimensions are normalized`, () => {
    let width = 320;
    const bridge = load(family, 'lingxi-bridge.js', { window: { lingxi: { v2: { deviceContext: { os: 'ios', get viewport() { return { width, height: -1 }; } } } } } });
    assert.equal(bridge.getDeviceContext().viewport.width, 320);
    width = 700;
    assert.equal(bridge.getDeviceContext().viewport.width, 700);
    assert.equal(bridge.getDeviceContext().viewport.height, 0);
  });
}

for (const family of families.filter((name) => name !== 'react-dom')) {
  test(`${family}: frame loop clamps resume delta and releases owned resources`, () => {
    const env = browser();
    const deltas = [];
    const { createFrameLoop } = load(family, 'frame-loop.js', env);
    const loop = createFrameLoop(env.canvas, { onFrame: (frame) => deltas.push(frame.dt) });
    loop.start();
    loop.start();
    assert.equal(env.frames.size, 1);
    assert.equal(env.canvas.width, 200);
    env.tick(100);
    env.tick(5100);
    assert.equal(deltas[0], 0);
    assert.equal(deltas[1], 1 / 15);
    env.document.emit('visibilitychange');
    env.tick(10100);
    assert.equal(deltas[2], 0);
    loop.stop();
    loop.stop();
    assert.equal(env.frames.size, 0);
    assert.equal(env.window.count(), 0);
    assert.equal(env.document.count(), 0);
    assert.equal(env.observer.disconnected, true);
  });
}

for (const family of families.filter((name) => name !== 'react-dom')) {
  test(`${family}: DPR changes resize backing store even with ResizeObserver available`, () => {
    const env = browser();
    const { createFrameLoop } = load(family, 'frame-loop.js', env);
    const loop = createFrameLoop(env.canvas);
    loop.start();
    env.window.devicePixelRatio = 3;
    env.window.emit('resize');
    assert.equal(env.canvas.width, 300);
    env.window.devicePixelRatio = 1;
    env.document.emit('visibilitychange');
    assert.equal(env.canvas.width, 100);
    loop.stop();
    assert.equal(env.window.count(), 0);
  });
}

function reactHarness() {
  const states = [];
  const effects = [];
  let stateIndex = 0;
  let effectIndex = 0;
  let pending = [];
  const hooks = {
    createContext: () => ({}),
    useContext: () => null,
    useMemo: (fn) => fn(),
    useState(init) {
      const index = stateIndex++;
      if (!(index in states)) states[index] = typeof init === 'function' ? init() : init;
      return [states[index], (update) => { states[index] = typeof update === 'function' ? update(states[index]) : update; }];
    },
    useEffect(fn, dependencies) {
      const index = effectIndex++;
      if (!effects[index] || dependencies.some((value, i) => !Object.is(value, effects[index].dependencies[i]))) {
        pending.push(() => { effects[index]?.cleanup?.(); effects[index] = { dependencies, cleanup: fn() }; });
      }
    },
  };
  return { hooks,
    render(component) {
      stateIndex = 0; effectIndex = 0;
      const value = component({ children: null });
      const run = pending; pending = []; for (const effect of run) effect();
      return value;
    },
    cleanup() { for (const effect of effects) effect?.cleanup?.(); effects.length = 0; },
  };
}

for (const family of families) {
  test(`${family}: provider catches resume/viewport updates and StrictMode cleans listeners`, () => {
    const env = browser();
    const media = new Map();
    env.window.matchMedia = (query) => {
      if (!media.has(query)) media.set(query, target());
      return media.get(query);
    };
    env.document.documentElement = { dataset: {}, style: { setProperty() {} }, classList: { toggle() {} } };
    let width = 320;
    env.window.lingxi = { v2: { deviceContext: {
      os: 'ios', formFactor: 'iphone', get viewport() { return { width, height: 600 }; },
    } } };
    const bridge = load(family, 'lingxi-bridge.js', env);
    const platform = load(family, 'platform-adapter.js', { ...env, ...bridge });
    const react = reactHarness();
    const { LingXiBridgeProvider } = load(family, 'lingxi-provider.jsx', { ...env, ...bridge, ...platform, ...react.hooks });
    assert.equal(react.render(LingXiBridgeProvider).device.viewport.width, 320);
    width = 400;
    env.window.emit('pageshow');
    assert.equal(react.render(LingXiBridgeProvider).device.viewport.width, 400);
    width = 450;
    env.document.emit('visibilitychange');
    assert.equal(react.render(LingXiBridgeProvider).device.viewport.width, 450);
    width = 470;
    env.window.visualViewport.emit('scroll');
    assert.equal(react.render(LingXiBridgeProvider).device.viewport.width, 470);
    react.cleanup();
    assert.equal(env.window.count() + env.window.visualViewport.count() + env.document.count(), 0);
    assert.equal([...media.values()].reduce((sum, item) => sum + item.count(), 0), 0);
    react.render(LingXiBridgeProvider);
    width = 500;
    env.window.emit('resize');
    assert.equal(react.render(LingXiBridgeProvider).device.viewport.width, 500);
    react.cleanup();
  });
}

function runtimeLoop() {
  const state = { starts: 0, stops: 0 };
  return { state, createFrameLoop: () => ({ start() { state.starts += 1; }, stop() { state.stops += 1; }, size: { width: 100, height: 80 } }) };
}

test('Phaser: disposal between READY and its microtask destroys once; terminal stop cannot restart', async () => {
  const scheduling = runtimeLoop();
  let game;
  class Game {
    constructor() {
      game = this;
      this.isBooted = true;
      this.isRunning = true;
      this.events = { once: (_event, fn) => { this.ready = fn; } };
      this.loop = { stop() {} };
      this.destroyCount = 0;
    }
    destroy() { this.destroyCount += 1; }
    step() {}
  }
  const { createPhaserRuntime } = load('phaser-2d', 'phaser-runtime.js', {
    Phaser: { Game, CANVAS: 1, Scale: { NONE: 0 } }, ...scheduling, queueMicrotask, performance,
  });
  const runtime = createPhaserRuntime(browser().canvas);
  runtime.start();
  game.ready();
  runtime.stop();
  await Promise.resolve();
  runtime.start();
  runtime.stop();
  assert.equal(game.destroyCount, 1);
  assert.equal(scheduling.state.starts, 1);
  assert.equal(scheduling.state.stops, 1);
});

test('Phaser: stop before asynchronous boot releases the eventual game exactly once', async () => {
  let game;
  class Game {
    constructor() {
      game = this; this.isBooted = false; this.isRunning = false; this.destroyCount = 0;
      this.events = { once: (_event, fn) => { this.ready = fn; } };
      this.loop = { stop() {} };
    }
    destroy() { this.destroyCount += 1; }
    step() {}
  }
  const scheduling = runtimeLoop();
  const { createPhaserRuntime } = load('phaser-2d', 'phaser-runtime.js', {
    Phaser: { Game, CANVAS: 1, Scale: { NONE: 0 } }, ...scheduling, queueMicrotask, performance,
  });
  const runtime = createPhaserRuntime(browser().canvas);
  runtime.stop();
  game.isBooted = true; game.isRunning = true;
  game.ready();
  await Promise.resolve();
  assert.equal(game.destroyCount, 1);
  runtime.start();
  assert.equal(scheduling.state.starts, 0);
});

test('Babylon: terminal cleanup prevents restarting a disposed engine and late physics initialization', async () => {
  const scheduling = runtimeLoop();
  let disposed = 0;
  let physicsEnabled = 0;
  let resolveHavok;
  class Engine { dispose() { disposed += 1; } }
  class Scene { dispose() { disposed += 1; } enablePhysics() { physicsEnabled += 1; return true; } }
  class Vector3 { static Zero() { return new Vector3(); } }
  class ArcRotateCamera { attachControl() {} }
  class Value {}
  const { createBabylonRuntime } = load('babylon-3d', 'babylon-runtime.js', {
    ...scheduling, Engine, Scene, Vector3, ArcRotateCamera,
    Color3: Value, Color4: Value, HemisphericLight: Value, StandardMaterial: Value,
    HavokPlugin: Value, PhysicsAggregate: Value, PhysicsShapeType: { BOX: 0 },
    MeshBuilder: { CreateBox: () => ({ position: {} }) },
    HavokPhysics: () => new Promise((resolve) => { resolveHavok = resolve; }),
  });
  const runtime = createBabylonRuntime(browser().canvas);
  runtime.start(); runtime.stop(); runtime.start(); runtime.stop();
  resolveHavok({});
  await Promise.resolve();
  assert.equal(physicsEnabled, 0);
  assert.equal(disposed, 2);
  assert.equal(scheduling.state.starts, 1);
  assert.equal(scheduling.state.stops, 1);
});


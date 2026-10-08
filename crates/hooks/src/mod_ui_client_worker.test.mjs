import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { readFileSync } from 'node:fs';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
import { tmpdir } from 'node:os';
import { setTimeout as delay } from 'node:timers/promises';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { ClientSurfaceManager } from './mod_ui_client_worker.mjs';

const SURFACE_RUNTIME_SOURCE = readFileSync(new URL('./mod_ui_client_helpers.mjs', import.meta.url), 'utf8');
const HOOKS_TYPES_SOURCE = readFileSync(new URL('./mod_ui_hooks_types.mjs', import.meta.url), 'utf8');

async function loadPrepared(request, id, message) {
  const prepared = await request(id, {
    kind: 'prepare', root: message.root, module: message.module,
    sourceGraph: {
      entry: message.module,
      compilerVersion: 'worker-test sourceGraph (plain JavaScript passthrough)',
      files: [{ file: message.module, source: await readFile(message.module, 'utf8') }],
      links: [],
    },
  });
  assert.equal(prepared.kind, 'prepared', JSON.stringify(prepared));
  return request(id, { ...message, kind: 'load', preparedToken: prepared.token });
}

function mountOptions({
  runtimeId = 'drawing-1',
  environmentId = 'env-1',
  manifestHash,
  source = `
    import { Box, Text, Button, h } from 'claude:surface-runtime';
    import { label } from './labels.mjs';
    export function Board(props, surface) {
      if (!surface.state.ready) {
        surface.setState({ ready: true });
        surface.onPointer(event => { surface.post({ kind: 'pointer', event }); });
        surface.onKey(event => { surface.post({ kind: 'key', event }); });
        surface.every(12, () => surface.setState(state => ({ ticks: (state.ticks || 0) + 1 })));
        surface.post({ kind: 'mounted', label: props.label });
      }
      return h(Box, { flexDirection: 'column', children: [
        h(Text, {}, label + ':' + props.label + ':' + surface.columns + 'x' + surface.rows),
        h(Button, { label: 'Press', onPress: event => surface.post({ kind: 'press', event }) }),
      ] });
    }
  `,
} = {}) {
  return {
    plugin: 'example',
    environmentId,
    ...(manifestHash === undefined ? {} : { manifestHash }),
    runtimeId,
    parent: { surface: 'desktop', component: 'panel', requestId: 'render-7' },
    client: { key: 'board', module: 'board.mjs' },
    moduleGraph: {
      modules: [
        { module: 'board.mjs', component: 'Board', source },
        { module: 'labels.mjs', source: `export const label = 'status';` },
      ],
      manifest: {
        runtime: 'claude:surface-runtime',
        modules: [{ module: 'board.mjs', entry: 'board.mjs', component: 'Board' }],
        files: [
          { key: 'claude:surface-runtime', source: SURFACE_RUNTIME_SOURCE },
          { key: 'board.mjs', source },
          { key: 'labels.mjs', source: `export const label = 'status';` },
        ],
      },
    },
    props: { label: 'ready' },
    columns: 80,
    rows: 24,
  };
}

test('mounts a module graph into a bounded VM tree and routes held, pointer, key, post, state, timer and resize', async () => {
  const events = [];
  const manager = new ClientSurfaceManager({ send: message => events.push(message) });
  const mounted = await manager.mount(mountOptions());

  assert.equal(mounted.frameSequence, 1);
  assert.equal(Object.hasOwn(mounted.tree, 'frameSequence'), false,
    'adapter ordering metadata stays outside the canonical Client tree');
  assert.deepEqual(mounted.tree, {
    type: 'Box',
    props: { flexDirection: 'column' },
    children: [
      { type: 'Text', props: {}, children: ['status:ready:80x24'] },
      { type: 'Button', props: { label: 'Press' }, held: 1, children: [] },
    ],
  });
  assert.equal(mounted.hasPointerListener, true);
  assert.equal(mounted.hasKeyListener, true);
  const schedule = events.find(event => event.kind === 'ui.client.event' && event.action === 'schedule');
  assert(schedule);
  assert.equal(schedule.frameSequence, undefined,
    'schedule is only a trigger; only the generated tree frame consumes a sequence');
  assert(events.some(event => event.action === 'post' && event.json === '{"kind":"mounted","label":"ready"}'));

  const pointer = { type: 'move', x: 7 };
  const key = { key: 'Enter', repeat: false };
  assert.equal(manager.pointer('drawing-1', pointer), true);
  assert.equal(manager.key('drawing-1', key), true);
  assert.equal(manager.runHeld('drawing-1', 1, { pointerId: 4 }), true);
  assert(events.some(event => event.action === 'post' && event.json === JSON.stringify({ kind: 'pointer', event: pointer })));
  assert(events.some(event => event.action === 'post' && event.json === JSON.stringify({ kind: 'key', event: key })));
  assert(events.some(event => event.action === 'post' && event.json === '{"kind":"press","event":{"pointerId":4}}'));

  const resized = manager.resize('drawing-1', 100, 30);
  assert.equal(resized.frameSequence, 2);
  assert.equal(resized.tree.children[0].children[0], 'status:ready:100x30');
  const updated = manager.setProps('drawing-1', { label: 'updated' });
  assert.equal(updated.frameSequence, 3);
  assert.equal(updated.tree.children[0].children[0], 'status:updated:100x30');

  await delay(25);
  assert(events.filter(event => event.action === 'schedule').length >= 2,
    'surface state changes schedule a later render, including a timer callback');
  assert.equal(manager.unmount('drawing-1'), true);
  assert.equal(manager.runHeld('drawing-1', 1, {}), false);
  manager.dispose();
});

test('frame sequences are generation-local and reject out-of-order frame delivery', async () => {
  const manager = new ClientSurfaceManager({ send() {} });
  const first = await manager.mount(mountOptions({
    runtimeId: 'ordered',
    source: `export function Board(props, surface) {
      return surface.elements.Text({ children: props.label });
    }`,
  }));
  const second = await manager.setProps('ordered', { label: 'second' });
  assert.equal(first.generation, second.generation);
  assert.equal(first.frameSequence, 1);
  assert.equal(second.frameSequence, 2);

  let latestSequence = 0;
  const deliver = frame => {
    if (!Number.isSafeInteger(frame.frameSequence) || frame.frameSequence <= latestSequence) return false;
    latestSequence = frame.frameSequence;
    return true;
  };
  assert.equal(deliver(second), true, 'the newest generated frame may arrive first');
  assert.equal(deliver(first), false, 'a late older RPC frame must be rejected');

  assert.equal(manager.unmount('ordered'), true);
  const remounted = await manager.mount(mountOptions({
    runtimeId: 'ordered',
    source: `export function Board(props, surface) {
      return surface.elements.Text({ children: props.label });
    }`,
  }));
  assert.notEqual(remounted.generation, first.generation);
  assert.equal(remounted.frameSequence, 1, 'a new runtime generation starts its own sequence');
  manager.dispose();
});

test('replaces the plugin environment when its environment ID or manifest changes', async () => {
  const manager = new ClientSurfaceManager({ send: () => {} });
  const source = `
    let loads = 0;
    export function Board(_props, surface) {
      loads += 1;
      return surface.elements.Text({ children: String(loads) });
    }
  `;
  const first = await manager.mount(mountOptions({ runtimeId: 'a', source }));
  const second = await manager.mount(mountOptions({ runtimeId: 'b', source }));
  assert.equal(first.tree.children[0], '1');
  assert.equal(second.tree.children[0], '2', 'same environment reuses evaluated module state');

  const other = await manager.mount(mountOptions({ runtimeId: 'c', environmentId: 'env-2', source }));
  assert.equal(other.tree.children[0], '1', 'a new environment starts a fresh VM/module cache');
  assert.equal(manager.runtimeInstances.has('a'), false, 'changing environment ID disposes the old plugin instance');
  assert.equal(manager.runtimeInstances.has('b'), false, 'the old plugin environment has no remaining instances');
  assert.equal(manager.environments.size, 1);

  const replacement = await manager.mount(mountOptions({ runtimeId: 'd', manifestHash: 'manifest-2', source }));
  assert.equal(replacement.tree.children[0], '1', 'a changed manifest replaces the cached environment');
  assert.equal(manager.runtimeInstances.has('c'), false,
    'changed plugin manifest disposes every older environment ID');
  assert.equal(manager.environments.size, 1, 'only the current manifest environment remains registered');
  manager.dispose();
});

test('mounts the canonical manifest file graph and selects its declared component entry', async () => {
  const manager = new ClientSurfaceManager({ send: () => {} });
  const options = mountOptions({ runtimeId: 'manifest-client', source: '' });
  options.client.module = 'board.tsx';
  options.moduleGraph = {
    modules: [{
      module: 'board.tsx',
      modulePath: '/plugin/board.tsx',
      component: 'Panel',
      source: 'export function Panel() { return null; }',
      linked: [{ file: '/plugin/dependency.mjs', source: 'export const label = "unused";' }],
      links: [{ from: 'board.tsx', spelled: './dependency.mjs', file: 'surface:///dependency.mjs' }],
    }],
    manifest: {
      hash: 'canonical-hash',
      modules: [{ module: 'board.tsx', entry: 'surface:///compiled/board.js', component: 'Panel' }],
      runtime: 'claude:surface-runtime',
      limits: { nodes: 20_000, depth: 32, chars: 100_000, values: 20_000, dataDepth: 32 },
      files: [
        { key: 'claude:surface-runtime', source: SURFACE_RUNTIME_SOURCE },
        {
          key: 'surface:///compiled/board.js',
          source: `import { Box, Text, h } from 'claude:surface-runtime';
            import { label } from 'surface:///dependency.mjs';
            export function Panel(props, surface) {
              return h(Box, { children: [h(Text, { children: label + props.label + surface.columns })] });
            }`,
        },
        { key: 'surface:///dependency.mjs', source: `export const label = 'from-file/';` },
      ],
    },
  };
  const mounted = await manager.mount(options);
  assert.deepEqual(mounted.tree, {
    type: 'Box', props: {}, children: [{ type: 'Text', props: {}, children: ['from-file/ready80'] }],
  });
  manager.dispose();
});

test('runs the manifest surface-runtime helper source for h, Fragment and all eight elements', async () => {
  const manager = new ClientSurfaceManager({ send: () => {} });
  const mounted = await manager.mount(mountOptions({
    runtimeId: 'helper-contract',
    source: `
      import { Box, Button, Code, Fragment, h, Input, Link, Markdown, Select, Text } from 'claude:surface-runtime';
      export function Board() {
        return h(Fragment, null,
          h(Text, { children: [Box, Text, Button, Input, Select, Link, Code, Markdown].join(' ') }),
          h(Box, { marker: 'fragment-flattened' }));
      }
    `,
  }));
  assert.deepEqual(mounted.tree, [
    { type: 'Text', props: {}, children: ['Box Text Button Input Select Link Code Markdown'] },
    { type: 'Box', props: { marker: 'fragment-flattened' }, children: [] },
  ]);
  manager.dispose();
});

test('does not synthesize the surface runtime when its manifest file is missing', async () => {
  const options = mountOptions({ runtimeId: 'missing-runtime' });
  options.moduleGraph.manifest.files = options.moduleGraph.manifest.files
    .filter(file => file.key !== 'claude:surface-runtime');
  const manager = new ClientSurfaceManager({ send: () => {} });
  await assert.rejects(manager.mount(options), error => error.phase === 'load'
    && error.message.includes('claude:surface-runtime'));
  manager.dispose();
});

test('reports load, render, and run faults with the parent Client identity', async () => {
  const events = [];
  const manager = new ClientSurfaceManager({ send: message => events.push(message) });
  await assert.rejects(manager.mount(mountOptions({
    runtimeId: 'load-error',
    source: 'export const Board = 3;',
  })), error => error.phase === 'load');
  assert.equal(manager.runtimeInstances.has('load-error'), false,
    'an initial load failure unregisters the worker entry');
  assert([...manager.environments.values()].every(environment => !environment.instances.has('load-error')),
    'an initial load failure unregisters the environment entry');
  assert(events.some(event => event.action === 'fault' && event.phase === 'load'
    && event.runtimeId === 'load-error' && event.parent.requestId === 'render-7'));

  await assert.rejects(manager.mount(mountOptions({
    runtimeId: 'render-error',
    source: `export function Board() { return { type: 'Client', props: {} }; }`,
  })), error => error.phase === 'render');
  assert.equal(manager.runtimeInstances.has('render-error'), false,
    'an initial frame failure has no surviving worker instance after the Host drops its mapping');
  assert([...manager.environments.values()].every(environment => !environment.instances.has('render-error')),
    'initial render failure unregisters the environment instance');
  assert(events.some(event => event.action === 'fault' && event.phase === 'render'
    && event.runtimeId === 'render-error'));

  await manager.mount(mountOptions({
    runtimeId: 'run-error',
    source: `export function Board(_props, surface) {
      surface.onPointer(() => { throw new Error('pointer failed'); });
      return surface.elements.Text({ children: 'ok' });
    }`,
  }));
  assert.throws(() => manager.pointer('run-error', { x: 1 }), error => error.phase === 'run');
  assert(events.some(event => event.action === 'fault' && event.phase === 'run'
    && event.runtimeId === 'run-error' && event.element === 'board'));
  manager.dispose();
});

test('a VM fault tears down timers and held callbacks until an explicit remount', async () => {
  const events = [];
  const manager = new ClientSurfaceManager({ send: message => events.push(message) });
  const source = `
    let failPointerOnce = true;
    export function Board(_props, surface) {
      surface.every(5, () => {
        surface.post({ kind: 'tick' });
        surface.setState(state => ({ ticks: (state.ticks ?? 0) + 1 }));
      });
      surface.onPointer(() => {
        if (failPointerOnce) {
          failPointerOnce = false;
          throw new Error('stop this surface');
        }
      });
      return surface.elements.Button({ onPress: event => surface.post({ kind: 'held', event }) });
    }
  `;
  const mounted = await manager.mount(mountOptions({ runtimeId: 'faulted-vm', source }));
  const heldHandle = mounted.tree.held;
  assert.equal(typeof heldHandle, 'number');
  await delay(20);
  assert(events.some(event => event.action === 'post' && event.json === '{"kind":"tick"}'),
    'the timer is live before the fault');

  events.length = 0;
  assert.throws(() => manager.pointer('faulted-vm', { type: 'move' }), error =>
    error.phase === 'run' && error.message === 'stop this surface');
  assert.deepEqual(manager.runtimeInstances.get('faulted-vm').failure, {
    status: 'failed', phase: 'run', reason: 'stop this surface',
  });
  assert.equal(events.filter(event => event.action === 'fault').length, 1);
  assert.equal(manager.runHeld('faulted-vm', heldHandle, { type: 'press' }), false,
    'fault cleanup releases the serialized held callback');
  assert.equal(manager.pointer('faulted-vm', { type: 'move' }), false,
    'the failed surface no longer has input listeners');
  assert.deepEqual(await manager.operation({ type: 'render', runtimeId: 'faulted-vm' }), { stale: true });
  assert.deepEqual(manager.setProps('faulted-vm', { label: 'updated while failed' }), { stale: true });
  assert.deepEqual(manager.resize('faulted-vm', 90, 20), { stale: true });

  await delay(25);
  assert.deepEqual(events.filter(event => ['post', 'schedule'].includes(event.action)), [],
    'unmounted timer and queued surface APIs produce no later events');
  assert.equal(events.filter(event => event.action === 'fault').length, 1,
    'later operations do not emit duplicate fault events');

  const remounted = await manager.mount(mountOptions({ runtimeId: 'faulted-vm', source }));
  assert(remounted.generation > mounted.generation);
  assert.equal(remounted.frameSequence, 1);
  assert.equal(manager.pointer('faulted-vm', { type: 'move' }), true,
    'a fresh generation restores the surface after the one-shot failure');
  manager.dispose();
});

test('a delayed async callback rejection cannot replace the first failed transition', async () => {
  const events = [];
  const manager = new ClientSurfaceManager({ send: message => events.push(message) });
  const mounted = await manager.mount(mountOptions({
    runtimeId: 'async-fault',
    source: `export function Board(_props, surface) {
      surface.onPointer(() => Promise.resolve().then(() => { throw new Error('late async failure'); }));
      surface.onKey(() => { throw new Error('first sync failure'); });
      return surface.elements.Text({ children: 'ready' });
    }`,
  }));
  assert(mounted.frameSequence > 0);
  assert.equal(manager.pointer('async-fault', { type: 'move' }), true);
  assert.throws(() => manager.key('async-fault', { key: 'x' }), error =>
    error.phase === 'run' && error.message === 'first sync failure');
  await delay(0);
  assert.deepEqual(manager.runtimeInstances.get('async-fault').failure, {
    status: 'failed', phase: 'run', reason: 'first sync failure',
  });
  assert.equal(events.filter(event => event.runtimeId === 'async-fault' && event.action === 'fault').length, 1,
    'the late promise rejection cannot emit a second fault or replace the first reason');
  assert.deepEqual(manager.resize('async-fault', 80, 24), { stale: true });
  manager.dispose();
});

test('rejects escaped imports, nested Client nodes, and oversized trees without consulting toJSON', async () => {
  const events = [];
  const manager = new ClientSurfaceManager({ send: message => events.push(message) });
  await assert.rejects(manager.mount(mountOptions({
    runtimeId: 'escaped',
    source: `import '../outside.mjs'; export function Board() { return null; }`,
  })), error => error.phase === 'load');
  await assert.rejects(manager.mount(mountOptions({
    runtimeId: 'nested',
    source: `export function Board() { return { type: 'Client', props: {} }; }`,
  })), error => error.phase === 'render');
  let toJsonCalled = false;
  const noToJson = await manager.mount(mountOptions({
    runtimeId: 'no-to-json',
    source: `export function Board() {
      return { type: 'Text', props: { toJSON() { throw new Error('must not run'); }, label: 'safe' }, children: ['ok'] };
    }`,
  }));
  assert.deepEqual(noToJson.tree, { type: 'Text', props: { label: 'safe' }, children: ['ok'] });
  void toJsonCalled;
  await assert.rejects(manager.mount(mountOptions({
    runtimeId: 'oversized',
    source: `export function Board(_props, surface) {
      return surface.elements.Box({ children: Array.from({ length: 20001 }, () => surface.elements.Text({ children: 'x' })) });
    }`,
  })), error => error.phase === 'render');
  assert(events.filter(event => event.action === 'fault').length >= 3);
  manager.dispose();
});

test('serializes one-way worker event frames and operation results', async () => {
  const frames = [];
  const manager = new ClientSurfaceManager({ send: frame => frames.push(frame) });
  const result = await manager.operation({ type: 'mount', ...mountOptions({ runtimeId: 'op-1' }) });
  assert.equal(result.runtimeId, 'op-1');
  assert.equal(result.frameSequence, 1);
  assert.equal(result.tree.type, 'Box');
  assert(frames.every(frame => frame.id === 0 && frame.kind === 'ui.client.event'));
  const unmounted = await manager.operation({ type: 'unmount', runtimeId: 'op-1' });
  assert.deepEqual(unmounted, { unmounted: true });
  manager.dispose();
});

test('mods worker accepts ui.client.operation and returns tree plus asynchronous event frames', async t => {
  const workerPath = fileURLToPath(new URL('./mods_worker.mjs', import.meta.url));
  const worker = spawn(process.execPath, ['--experimental-vm-modules', workerPath], {
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  t.after(() => { if (!worker.killed) worker.kill(); });

  const frames = [];
  const pending = new Map();
  const lines = createInterface({ input: worker.stdout, crlfDelay: Infinity });
  lines.on('line', line => {
    const frame = JSON.parse(line);
    frames.push(frame);
    if (frame.id !== 0) {
      pending.get(frame.id)?.(frame);
      pending.delete(frame.id);
    }
  });
  worker.once('exit', code => {
    for (const resolve of pending.values()) resolve({ kind: 'worker.exit', code });
    pending.clear();
  });
  const request = (id, message) => new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`worker request ${id} timed out`));
    }, 3_000);
    pending.set(id, frame => { clearTimeout(timeout); resolve(frame); });
    worker.stdin.write(`${JSON.stringify({ id, ...message })}\n`);
  });

  const mounted = await request(1, {
    kind: 'ui.client.operation',
    operation: {
      type: 'mount',
      ...mountOptions({
        runtimeId: 'worker-drawing',
        source: `export function Board(_props, surface) {
          if (!surface.state.ready) {
            surface.setState({ ready: true });
            surface.post({ kind: 'worker-mounted' });
            surface.every(5, () => surface.post({ kind: 'tick' }));
          }
          return surface.elements.Text({ children: 'worker tree' });
        }`,
      }),
    },
  });
  assert.equal(mounted.kind, 'ui.client.result');
  assert.equal(mounted.result.frameSequence, 1);
  assert.equal(Object.hasOwn(mounted.result.tree, 'frameSequence'), false);
  assert.deepEqual(mounted.result.tree, { type: 'Text', props: {}, children: ['worker tree'] });
  assert(frames.some(frame => frame.id === 0 && frame.kind === 'ui.client.event'
    && frame.runtimeId === 'worker-drawing' && frame.action === 'schedule'));
  assert(frames.some(frame => frame.kind === 'ui.client.event'
    && frame.runtimeId === 'worker-drawing' && frame.action === 'post'
    && frame.json === '{"kind":"worker-mounted"}'));
  await delay(15);
  assert(frames.some(frame => frame.kind === 'ui.client.event'
    && frame.runtimeId === 'worker-drawing' && frame.action === 'post'
    && frame.json === '{"kind":"tick"}'));

  const unmounted = await request(2, {
    kind: 'ui.client.operation',
    operation: { type: 'unmount', runtimeId: 'worker-drawing' },
  });
  assert.deepEqual(unmounted.result, { unmounted: true });

  const failedLoad = await request(3, {
    kind: 'ui.client.operation',
    operation: {
      type: 'mount',
      ...mountOptions({ runtimeId: 'worker-failed-load', source: 'export const Board = 3;' }),
    },
  });
  assert.deepEqual(failedLoad.error, { phase: 'load', reason: 'surface module board.mjs export Board is not a function (props, surface) => tree' });
  assert(frames.some(frame => frame.kind === 'ui.client.event'
    && frame.runtimeId === 'worker-failed-load' && frame.action === 'fault' && frame.phase === 'load'));

  const failedRender = await request(4, {
    kind: 'ui.client.operation',
    operation: {
      type: 'mount',
      ...mountOptions({
        runtimeId: 'worker-failed-render',
        source: `export function Board() { return { type: 'Client', props: {} }; }`,
      }),
    },
  });
  assert.equal(failedRender.error.phase, 'render');
  assert(frames.some(frame => frame.kind === 'ui.client.event'
    && frame.runtimeId === 'worker-failed-render' && frame.action === 'fault' && frame.phase === 'render'));
  const afterFailure = await request(5, {
    kind: 'ui.client.operation',
    operation: { type: 'render', runtimeId: 'worker-failed-render' },
  });
  assert.deepEqual(afterFailure.result, { stale: true },
    'a failed load/render generation never creates another worker frame');
  assert.equal(frames.filter(frame => frame.runtimeId === 'worker-failed-render'
    && frame.action === 'fault').length, 1,
  'the failed generation publishes its fault only once');
  worker.stdin.end();
  const [exitCode] = await once(worker, 'exit');
  assert.equal(exitCode, 0);
});

test('prepared desktop ui.resolve constructors render a validated tree and route parent press handles', async t => {
  const root = await mkdtemp(join(tmpdir(), 'mods-ui-client-parent-'));
  const modulePath = join(root, 'hooks.mjs');
  await writeFile(modulePath, `
    const callbacks = [];
    let inputHookCalls = 0;
    let selectHookCalls = 0;
    export function register(on) {
      on('ui.resolve', { surface: 'desktop', component: 'Pane' }, ($, event) => $.ui.resolve(event));
      on('ui.input', async (_$, event, next) => {
        inputHookCalls += 1;
        if (event.value === 'stale') return { handled: true, element: event.element, value: 'stale-hook' };
        return next({ ...event, value: event.element === 'long' ? 'x'.repeat(16_385) : event.value.trim() });
      });
      on('ui.select', async (_$, event, next) => {
        selectHookCalls += 1;
        const value = event.value === 'slow' ? 'rewritten-option'
          : event.value === 'fast' ? 'y'.repeat(16_385) : event.value;
        return next({ ...event, value });
      });
      on('ui.press', async (_$, event, next) => next(event.link === undefined ? event : {
        ...event, link: { href: event.element === 'large-link' ? 'x'.repeat(20_001)
          : 'https://rewritten.example' },
      }));
      on('ui.render', { surface: 'desktop', component: 'Pane' }, ($, event) => {
        const { Box, Text, Button, Input, Select, Link, Code, Markdown, Svg, Client, h } = $.ui.resolve(event);
        return h(Box, { key: 'root', flexDirection: 'row', display: 'none',
          hover: { borderColor: 'cyan', ...(event.requestId === 'hidden-invalid' ? {} : { display: 'flex' }) } },
          h(Text, { color: 'cyan', hover: { scope: 'status', bold: true } }, 'status'),
          h('div', { testId: 'container' }, h('span', {}, 'inline'), h('b', {}, 'bold')),
          h(Button, { key: 'go', label: 'Go', hover: { scope: 'button', bold: true },
            onPress: (...args) => callbacks.push({ callback: 'button', argCount: args.length,
              element: args[0]?.element, eventKeys: Object.keys(args[0] ?? {}).sort().join(',') }) }),
          h(Input, { key: 'query', onSubmit: (value, event) => callbacks.push({
            callback: 'submit', value, kind: event.kind, eventValue: event.value, element: event.element,
            eventKeys: Object.keys(event).sort().join(','),
          }), onInput: (value, event) => callbacks.push({
            callback: 'input', value, kind: event.kind, eventValue: event.value, element: event.element,
          }) }),
          h(Select, { key: 'mode', options: [{ value: 'fast' }, { value: 'slow' }],
            onSelect: (value, event) => callbacks.push(value.length > 100
              ? { callback: 'long-select', length: value.length }
              : { callback: 'select', value, eventValue: event.value }) }),
          h(Link, { href: 'https://example.com', label: 'Docs' }, 'docs'),
          h(Code, { source: 'const answer = 42;' }),
          h(Markdown, { key: 'readme', text: '[safe](https://example.com)',
            pressableLinks: ['www.Example.com'], onLinkPress: (link, event) => callbacks.push({
              callback: 'link', href: link?.href, eventHref: event.link?.href,
              eventKeys: Object.keys(event).sort().join(','),
            }) }),
          h(Svg, { source: '<svg/>', alt: 'icon', width: 24 }),
          h(Client, { key: 'card', module: 'cards/card.tsx', data: { title: 'card' } }),
          h(Input, { key: 'no-change', onSubmit: () => callbacks.push({ callback: 'no-change-submit' }) }),
          h(Text, {}, JSON.stringify({ callbacks, inputHookCalls, selectHookCalls })),
          { type: 'engine', ref: 7 },
          h(Markdown, { key: 'any-link', text: 'any link',
            onLinkPress: (link, event) => callbacks.push({
              callback: 'empty-link', href: link?.href, eventHref: event.link?.href,
            }) }),
          h(Input, { key: 'long', onSubmit: () => {}, onInput: value => callbacks.push({
            callback: 'long-input', length: value.length,
          }) }),
          h(Markdown, { key: 'large-link', text: 'large href', pressableLinks: ['https://allowed'],
            onLinkPress: link => callbacks.push({ callback: 'long-href', length: link.href.length }) }));
      });
    }
  `);
  t.after(() => rm(root, { recursive: true, force: true }));

  const workerPath = fileURLToPath(new URL('./mods_worker.mjs', import.meta.url));
  const worker = spawn(process.execPath, ['--experimental-vm-modules', workerPath], {
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  t.after(() => { if (!worker.killed) worker.kill(); });

  const pending = new Map();
  const lines = createInterface({ input: worker.stdout, crlfDelay: Infinity });
  lines.on('line', line => {
    const frame = JSON.parse(line);
    if (frame.kind === 'api.callers') {
      worker.stdin.write(`${JSON.stringify({ id: frame.id, kind: 'api.callers.result', callId: frame.callId })}\n`);
      return;
    }
    if (frame.id !== 0 && frame.kind !== 'progress') {
      pending.get(frame.id)?.(frame);
      pending.delete(frame.id);
    }
  });
  worker.once('exit', () => {
    for (const resolve of pending.values()) resolve({ kind: 'worker.exit' });
    pending.clear();
  });
  const request = (id, message) => new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`worker request ${id} timed out`));
    }, 3_000);
    pending.set(id, frame => { clearTimeout(timeout); resolve(frame); });
    worker.stdin.write(`${JSON.stringify({ id, ...message })}\n`);
  });

  const loaded = await loadPrepared(request, 1, {
    plugin: 'desktop-plugin', storageId: 'desktop-plugin@user', tier: 'user',
    root, module: modulePath,
  });
  assert.equal(loaded.kind, 'loaded');
  const pathBase = '/Users/luolingfeng/lingxi/harness-runtime';
  const preflightCases = [
    { href: 'www.Example.com/a', pressableLinks: ['http://www.example.com/a'], admitted: true },
    { href: 'www.Example.com:443/a/../b',
      pressableLinks: ['http://www.example.com:443/b'], admitted: true },
    { href: 'WWW.Example.COM:80/a/../b',
      pressableLinks: ['HTTP://www.example.com/b'], admitted: true },
    { href: 'HTTPS://EXAMPLE.com:443/a/../b?Q=A#Frag',
      pressableLinks: ['https://example.com/b?Q=A#Frag'], admitted: true },
    { href: 'relative/a.md', pressableLinks: ['relative/a.md'], admitted: true },
    { href: 'bad%ZZ.md', pressableLinks: ['bad%ZZ.md'], admitted: true },
    { href: '/tmp/a.md', pressableLinks: ['file:///tmp/a.md'], admitted: true },
    { href: '/tmp/a.md?Q=A#Frag', pressableLinks: ['file:///tmp/a.md?Q=A#Frag'], admitted: true },
    { href: 'file:relative/a.md',
      pressableLinks: ['file:///Users/luolingfeng/lingxi/harness-runtime/relative/a.md'], admitted: true },
    { href: 'file://localhost/tmp/a.md', pressableLinks: ['file:///tmp/a.md'], admitted: true },
    { href: 'file://LOCALHOST/foo.md',
      pressableLinks: ['file:///Users/luolingfeng/lingxi/harness-runtime/LOCALHOST/foo.md'], admitted: true },
    { href: 'file://server/foo.md',
      pressableLinks: ['file:///Users/luolingfeng/lingxi/harness-runtime/server/foo.md'], admitted: true },
    { href: '/net/server/a.md', pressableLinks: ['file:///net/server/a.md'], admitted: false },
    { href: '/.file/secret.md', pressableLinks: ['file:///.file/secret.md'], admitted: false },
    { href: '/Network/Servers/server/a.md',
      pressableLinks: ['file:///Network/Servers/server/a.md'], admitted: false },
    { href: 'empty-list.md', pressableLinks: [], admitted: false },
  ];
  for (const [index, item] of preflightCases.entries()) {
    const result = await request(20 + index, {
      kind: 'ui.press.preflight', cwd: pathBase,
      href: item.href, pressableLinks: item.pressableLinks,
    });
    assert.equal(result.kind, 'ui.press.preflight.result');
    assert.equal(result.admitted, item.admitted, `Native Gpn preflight: ${item.href}`);
  }
  const noLinkList = await request(40, {
    kind: 'ui.press.preflight', href: '', cwd: pathBase,
  });
  assert.equal(noLinkList.admitted, true, 'an absent Native pressableLinks list admits any href');
  const ticketedPreflight = await request(41, {
    kind: 'ui.press.preflight',
    apiContextTicket: '00000000-0000-4000-8000-000000000001',
    cwd: pathBase,
    href: 'www.Example.com:443/a/../b',
    pressableLinks: ['http://www.example.com:443/b'],
  });
  assert.equal(ticketedPreflight.admitted, true,
    'the host-only origin ticket is transport metadata, not part of href admission');
  for (const [index, apiContextTicket] of [null, 7, ''].entries()) {
    const malformedTicket = await request(42 + index, {
      kind: 'ui.press.preflight', apiContextTicket, cwd: pathBase,
      href: 'www.Example.com/a', pressableLinks: ['http://www.example.com/a'],
    });
    assert.equal(malformedTicket.admitted, false, 'invalid host ticket types are rejected');
  }
  const unknownField = await request(45, {
    kind: 'ui.press.preflight', apiContextTicket: 'opaque-host-ticket',
    cwd: pathBase, href: 'www.Example.com/a',
    pressableLinks: ['http://www.example.com/a'], plugin: 'not-a-preflight-field',
  });
  assert.equal(unknownField.admitted, false, 'unknown business fields remain rejected');
  const rendered = await request(2, {
    kind: 'dispatch', event: 'ui.render', input: {
      surface: 'desktop', component: 'Pane', requestId: 'render-1', props: {},
      viewport: { columns: 80, rows: 24 },
    },
  });
  assert.equal(rendered.kind, 'result');
  assert.equal(rendered.result.type, 'Box');
  assert.equal(rendered.result.props.key, 'root');
  assert.deepEqual(rendered.result.children.map(node => node.type), [
    'Text', 'div', 'Button', 'Input', 'Select', 'Link', 'Code', 'Markdown', 'Svg', 'Client',
    'Input', 'Text', 'engine', 'Markdown', 'Input', 'Markdown',
  ]);
  assert.deepEqual(rendered.result.children[1].children.map(node => node.type), ['span', 'b']);
  assert.equal(rendered.result.children[2].press.plugin, 'desktop-plugin');
  assert.equal(Object.keys(rendered.result.children[2].press).sort().join(','), 'handle,plugin');
  assert.equal(rendered.result.children[3].press.plugin, 'desktop-plugin');
  assert.deepEqual(rendered.result.children[0].group, { plugin: 'desktop-plugin' });
  assert.deepEqual(rendered.result.children[0].hover, { scope: 'status', bold: true });
  assert.equal(rendered.result.props.display, 'none');
  assert.equal(rendered.result.hover.display, 'flex');
  assert.equal(rendered.result.children[7].press.plugin, 'desktop-plugin',
    'pressable Markdown receives a parent handle');
  assert.deepEqual(rendered.result.children[9].client, { plugin: 'desktop-plugin' });

  const press = rendered.result.children[2].press;
  const pressed = await request(3, {
    kind: 'dispatch', event: 'ui.press', input: {
      plugin: 'desktop-plugin', element: 'go', component: 'Pane', requestId: 'render-1', surface: 'desktop',
    }, pressToken: press,
  });
  assert.deepEqual(pressed.result, { handled: true, element: 'go' });

  const markdownToken = rendered.result.children[7].press;
  const linked = await request(4, {
    kind: 'dispatch', event: 'ui.press', input: {
      plugin: 'desktop-plugin', element: 'readme', component: 'Pane', requestId: 'render-1',
      surface: 'desktop', link: { href: 'http://www.example.com/' },
    }, pressToken: markdownToken, pressHrefAdmitted: true,
  });
  assert.deepEqual(linked.result, { handled: true, element: 'readme' });
  const emptyLinkToken = rendered.result.children[13].press;
  const emptyLink = await request(15, {
    kind: 'dispatch', event: 'ui.press', input: {
      plugin: 'desktop-plugin', element: 'any-link', component: 'Pane', requestId: 'render-1',
      surface: 'desktop', link: { href: '' },
    }, pressToken: emptyLinkToken, pressHrefAdmitted: true,
  });
  assert.deepEqual(emptyLink.result, { handled: true, element: 'any-link' },
    'without a pressableLinks list the native schema allows an empty href');
  const unpressableLink = await request(5, {
    kind: 'dispatch', event: 'ui.press', input: {
      plugin: 'desktop-plugin', element: 'readme', component: 'Pane', requestId: 'render-1',
      surface: 'desktop', link: { href: 'https://unlisted.example' },
    }, pressToken: markdownToken, pressHrefAdmitted: false,
  });
  assert.deepEqual(unpressableLink.result, { handled: false },
    'the original link must be admitted before ui.press hooks run');

  const inputToken = rendered.result.children[3].press;
  const submitted = await request(6, {
    kind: 'dispatch', event: 'ui.input', input: {
      plugin: 'desktop-plugin', element: 'query', component: 'Pane', requestId: 'render-1',
      surface: 'desktop', kind: 'submit', value: '  answer  ',
    }, pressToken: inputToken,
  });
  assert.deepEqual(submitted.result, { handled: true, element: 'query', value: 'answer' });

  const changed = await request(7, {
    kind: 'dispatch', event: 'ui.input', input: {
      plugin: 'desktop-plugin', element: 'query', component: 'Pane', requestId: 'render-1',
      surface: 'desktop', kind: 'change', value: ' typed ',
    }, pressToken: inputToken,
  });
  assert.deepEqual(changed.result, { handled: true, element: 'query', value: 'typed' });

  const longInputToken = rendered.result.children[14].press;
  const longInput = await request(16, {
    kind: 'dispatch', event: 'ui.input', input: {
      plugin: 'desktop-plugin', element: 'long', component: 'Pane', requestId: 'render-1',
      surface: 'desktop', kind: 'change', value: 'short',
    }, pressToken: longInputToken,
  });
  assert.equal(longInput.result.value.length, 16_385,
    'the Hook validator permits an unbounded value rewrite after the Host control gate');

  const missingInputToken = rendered.result.children[10].press;
  const changedWithoutOnInput = await request(8, {
    kind: 'dispatch', event: 'ui.input', input: {
      plugin: 'desktop-plugin', element: 'no-change', component: 'Pane', requestId: 'render-1',
      surface: 'desktop', kind: 'change', value: 'ignored',
    }, pressToken: missingInputToken,
  });
  assert.deepEqual(changedWithoutOnInput.result,
    { handled: true, element: 'no-change', value: 'ignored' },
    'the held Input wrapper is reached even when optional onInput is absent');

  const selectToken = rendered.result.children[4].press;
  const selected = await request(9, {
    kind: 'dispatch', event: 'ui.select', input: {
      plugin: 'desktop-plugin', element: 'mode', component: 'Pane', requestId: 'render-1',
      surface: 'desktop', value: 'slow',
    }, pressToken: selectToken,
  });
  assert.deepEqual(selected.result, { handled: true, element: 'mode', value: 'rewritten-option' });

  const invalidSelection = await request(10, {
    kind: 'dispatch', event: 'ui.select', input: {
      plugin: 'desktop-plugin', element: 'mode', component: 'Pane', requestId: 'render-1',
      surface: 'desktop', value: 'unknown',
    }, pressToken: selectToken,
  });
  assert.deepEqual(invalidSelection.result, { handled: false },
    'Select accepts only values from its held options');
  const longSelection = await request(17, {
    kind: 'dispatch', event: 'ui.select', input: {
      plugin: 'desktop-plugin', element: 'mode', component: 'Pane', requestId: 'render-1',
      surface: 'desktop', value: 'fast',
    }, pressToken: selectToken,
  });
  assert.equal(longSelection.result.value.length, 16_385,
    'the Hook validator permits an unbounded selected value rewrite');
  const longHrefToken = rendered.result.children[15].press;
  const longHref = await request(18, {
    kind: 'dispatch', event: 'ui.press', input: {
      plugin: 'desktop-plugin', element: 'large-link', component: 'Pane', requestId: 'render-1',
      surface: 'desktop', link: { href: 'https://allowed' },
    }, pressToken: longHrefToken, pressHrefAdmitted: true,
  });
  assert.deepEqual(longHref.result, { handled: true, element: 'large-link' });

  const staleInput = await request(11, {
    kind: 'dispatch', event: 'ui.input', input: {
      plugin: 'desktop-plugin', element: 'query', component: 'Pane', requestId: 'render-1',
      surface: 'desktop', kind: 'submit', value: 'stale',
    }, pressToken: selectToken,
  });
  assert.deepEqual(staleInput.result, { handled: false }, 'a token for another element cannot fire');

  const rerendered = await request(12, {
    kind: 'dispatch', event: 'ui.render', input: {
      surface: 'desktop', component: 'Pane', requestId: 'render-1', props: {},
      viewport: { columns: 80, rows: 24 },
    },
  });
  assert.equal(rerendered.kind, 'result');
  assert.deepEqual(JSON.parse(rerendered.result.children[11].children[0]), {
    callbacks: [
      { callback: 'button', argCount: 1, element: 'go',
        eventKeys: 'component,element,plugin,requestId,surface' },
      { callback: 'link', href: 'https://rewritten.example', eventHref: 'https://rewritten.example',
        eventKeys: 'component,element,link,plugin,requestId,surface' },
      { callback: 'empty-link', href: 'https://rewritten.example',
        eventHref: 'https://rewritten.example' },
      { callback: 'submit', value: 'answer', kind: 'submit', eventValue: 'answer', element: 'query',
        eventKeys: 'component,element,kind,plugin,requestId,surface,value' },
      { callback: 'input', value: 'typed', kind: 'change', eventValue: 'typed', element: 'query' },
      { callback: 'long-input', length: 16_385 },
      { callback: 'select', value: 'rewritten-option', eventValue: 'rewritten-option' },
      { callback: 'long-select', length: 16_385 },
      { callback: 'long-href', length: 20_001 },
    ],
    inputHookCalls: 4,
    selectHookCalls: 2,
  }, 'the original option is gated before hooks, and the final rewritten value reaches onSelect');

  const staleAfterRerender = await request(13, {
    kind: 'dispatch', event: 'ui.input', input: {
      plugin: 'desktop-plugin', element: 'query', component: 'Pane', requestId: 'render-1',
      surface: 'desktop', kind: 'submit', value: 'stale',
    }, pressToken: inputToken,
  });
  assert.deepEqual(staleAfterRerender.result, { handled: false },
    'rerender revokes the previous Parent callback handle');

  const hiddenWithoutReveal = await request(14, {
    kind: 'dispatch', event: 'ui.render', input: {
      surface: 'desktop', component: 'Pane', requestId: 'hidden-invalid', props: {},
      viewport: { columns: 80, rows: 24 },
    },
  });
  assert.equal(hiddenWithoutReveal.kind, 'result');
  assert.deepEqual(hiddenWithoutReveal.result, {
    type: 'engine', ref: 'desktop:Pane:hidden-invalid',
  }, 'a hidden keyed Box with hover descendants must reveal on hover');
  worker.stdin.end();
  const [exitCode] = await once(worker, 'exit');
  assert.equal(exitCode, 0);
});

test('Client render ownership permits downstream prop edits but rejects key/module replacement', async t => {
  const root = await mkdtemp(join(tmpdir(), 'mods-ui-client-owner-'));
  const parentPath = join(root, 'parent.mjs');
  const childPath = join(root, 'child.mjs');
  await writeFile(parentPath, `
    export function register(on) {
      on('ui.render', { surface: 'desktop', component: 'Pane' }, async (_api, event, next) => {
        const child = await next(event);
        return { ...child, props: { ...child.props,
          ...(event.requestId === 'replace' ? { module: 'cards/other.tsx' } : {}),
          props: { title: 'parent edit' } } };
      });
    }
  `);
  await writeFile(childPath, `
    export function register(on) {
      on('ui.render', { surface: 'desktop', component: 'Pane' }, ($, event) => {
        const { Client } = $.ui.resolve(event);
        return Client({ key: 'card', module: 'cards/card.tsx', props: { title: 'child' } });
      });
    }
  `);
  t.after(() => rm(root, { recursive: true, force: true }));

  const workerPath = fileURLToPath(new URL('./mods_worker.mjs', import.meta.url));
  const worker = spawn(process.execPath, ['--experimental-vm-modules', workerPath], {
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  t.after(() => { if (!worker.killed) worker.kill(); });
  const pending = new Map();
  const lines = createInterface({ input: worker.stdout, crlfDelay: Infinity });
  lines.on('line', line => {
    const frame = JSON.parse(line);
    if (frame.kind === 'api.callers') {
      worker.stdin.write(`${JSON.stringify({ id: frame.id, kind: 'api.callers.result', callId: frame.callId })}\n`);
      return;
    }
    if (frame.id !== 0 && frame.kind !== 'progress') {
      pending.get(frame.id)?.(frame);
      pending.delete(frame.id);
    }
  });
  worker.once('exit', () => {
    for (const resolve of pending.values()) resolve({ kind: 'worker.exit' });
    pending.clear();
  });
  const request = (id, message) => new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`worker request ${id} timed out`));
    }, 3_000);
    pending.set(id, frame => { clearTimeout(timeout); resolve(frame); });
    worker.stdin.write(`${JSON.stringify({ id, ...message })}\n`);
  });

  for (const [id, plugin, modulePath, tier] of [
    [1, 'parent-plugin', parentPath, 'user'],
    [2, 'child-plugin', childPath, 'append'],
  ]) {
    const loaded = await loadPrepared(request, id, {
      plugin, storageId: `${plugin}@${tier}`, tier, root, module: modulePath,
    });
    assert.equal(loaded.kind, 'loaded');
  }
  const render = requestId => request(requestId === 'replace' ? 4 : 3, {
    kind: 'dispatch', event: 'ui.render', input: {
      surface: 'desktop', component: 'Pane', requestId, props: {},
    },
  });
  const accepted = await render('reuse');
  assert.equal(accepted.result.type, 'Client');
  assert.deepEqual(accepted.result.client, { plugin: 'child-plugin' });
  assert.deepEqual(accepted.result.props.props, { title: 'parent edit' });

  const replaced = await render('replace');
  assert.equal(replaced.result.type, 'Client');
  assert.deepEqual(replaced.result.client, { plugin: 'child-plugin' });
  assert.equal(replaced.result.props.module, 'cards/card.tsx',
    'an upstream hook cannot change a downstream Client module identity');
  assert.deepEqual(replaced.result.props.props, { title: 'child' },
    'the rejected replacement falls back to the accepted downstream node');
  worker.stdin.end();
  const [exitCode] = await once(worker, 'exit');
  assert.equal(exitCode, 0);
});

test('scoped hover ownership stamps current Box/Text and preserves foreign group only through next', async t => {
  const root = await mkdtemp(join(tmpdir(), 'mods-ui-hover-owner-'));
  const parentPath = join(root, 'parent.mjs');
  const childPath = join(root, 'child.mjs');
  await writeFile(parentPath, `
    export function register(on) {
      on('ui.render', { surface: 'desktop', component: 'Pane' }, async (_$, event, next) => {
        if (event.requestId === 'forged') {
          return { type: 'Box', props: { key: 'root' }, children: [
            { type: 'Text', props: { color: 'red' }, hover: { scope: 'shared' },
              group: { plugin: 'child-plugin' }, children: ['forged'] },
          ] };
        }
        const child = await next(event);
        return { ...child, children: child.children.map(node => ({
          ...node, props: { ...node.props, color: 'green' }, children: ['inherited'],
        })) };
      });
    }
  `);
  await writeFile(childPath, `
    export function register(on) {
      on('ui.render', { surface: 'desktop', component: 'Pane' }, ($, event) => {
        const { Box, Text } = $.ui.resolve(event);
        return Box({ key: 'root', children: [Text({ hover: { scope: 'shared' }, children: ['child'] })] });
      });
    }
  `);
  t.after(() => rm(root, { recursive: true, force: true }));

  const workerPath = fileURLToPath(new URL('./mods_worker.mjs', import.meta.url));
  const worker = spawn(process.execPath, ['--experimental-vm-modules', workerPath], {
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  t.after(() => { if (!worker.killed) worker.kill(); });
  const pending = new Map();
  const lines = createInterface({ input: worker.stdout, crlfDelay: Infinity });
  lines.on('line', line => {
    const frame = JSON.parse(line);
    if (frame.kind === 'api.callers') {
      worker.stdin.write(`${JSON.stringify({ id: frame.id, kind: 'api.callers.result', callId: frame.callId })}\n`);
      return;
    }
    if (frame.id !== 0 && frame.kind !== 'progress') {
      pending.get(frame.id)?.(frame);
      pending.delete(frame.id);
    }
  });
  worker.once('exit', () => {
    for (const resolve of pending.values()) resolve({ kind: 'worker.exit' });
    pending.clear();
  });
  const request = (id, message) => new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`worker request ${id} timed out`));
    }, 3_000);
    pending.set(id, frame => { clearTimeout(timeout); resolve(frame); });
    worker.stdin.write(`${JSON.stringify({ id, ...message })}\n`);
  });

  assert.equal((await loadPrepared(request, 1, {
    plugin: 'parent-plugin', storageId: 'parent-plugin@user', tier: 'prepend',
    root, module: parentPath,
  })).kind, 'loaded');
  assert.equal((await loadPrepared(request, 2, {
    plugin: 'child-plugin', storageId: 'child-plugin@user', tier: 'user',
    root, module: childPath,
  })).kind, 'loaded');

  const render = requestId => request(requestId === 'inherit' ? 3 : 4, {
    kind: 'dispatch', event: 'ui.render', input: {
      surface: 'desktop', component: 'Pane', requestId, props: {},
    },
  });
  const inherited = await render('inherit');
  assert.equal(inherited.kind, 'result');
  assert.deepEqual(inherited.result.children[0], {
    type: 'Text', props: { color: 'green' }, hover: { scope: 'shared' },
    group: { plugin: 'child-plugin' }, children: ['inherited'],
  });

  const forged = await render('forged');
  assert.equal(forged.kind, 'result');
  assert.deepEqual(forged.result.children[0], {
    type: 'Text', hover: { scope: 'shared' },
    group: { plugin: 'child-plugin' }, children: ['child'],
  }, 'foreign owner without a downstream next capability falls through to the child result');

  worker.stdin.end();
  const [exitCode] = await once(worker, 'exit');
  assert.equal(exitCode, 0);
});

test('prepare consumes an immutable compiled hook graph without reading JSX paths or fabricating aliases', async t => {
  const root = '/plugins/prepared-hook';
  const modulePath = `${root}/hooks.tsx`;
  const workerPath = fileURLToPath(new URL('./mods_worker.mjs', import.meta.url));
  const worker = spawn(process.execPath, ['--experimental-vm-modules', workerPath], {
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  t.after(() => { if (!worker.killed) worker.kill(); });
  const pending = new Map();
  const lines = createInterface({ input: worker.stdout, crlfDelay: Infinity });
  lines.on('line', line => {
    const frame = JSON.parse(line);
    if (frame.kind === 'api.callers') {
      worker.stdin.write(`${JSON.stringify({ id: frame.id, kind: 'api.callers.result', callId: frame.callId })}\n`);
      return;
    }
    if (frame.id !== 0 && frame.kind !== 'progress') {
      pending.get(frame.id)?.(frame);
      pending.delete(frame.id);
    }
  });
  worker.once('exit', () => {
    for (const resolve of pending.values()) resolve({ kind: 'worker.exit' });
    pending.clear();
  });
  const request = (id, message) => new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`worker request ${id} timed out`));
    }, 3_000);
    pending.set(id, frame => { clearTimeout(timeout); resolve(frame); });
    worker.stdin.write(`${JSON.stringify({ id, ...message })}\n`);
  });

  const graph = {
    entry: modulePath,
    compilerVersion: 'Bun fixture prepared by host',
    files: [{ file: modulePath, source: `
      import { atom, derive, memberOf } from 'claude-code';
      export function register(on) {
        on('ui.message', (_api, event) => {
          const family = atom({ plugin: 'prepared-plugin', key: 'rows', id: 'family', extra: true },
            { text: 'seed' }, { shape: 'row-v1' });
          const member = memberOf(family, { requestId: 'row-7' });
          const derived = derive([member], row => row.text);
          return { prepared: true, kind: event.kind, family: family.ref,
            member: member.ref, derivedSource: derived.sources[0].ref,
            computed: derived.compute({ text: 'derived' }) };
        });
      }
    ` }, { file: 'claude:hooks-types', source: HOOKS_TYPES_SOURCE }],
    links: [{ from: modulePath, spelled: 'claude-code', file: 'claude:hooks-types' }],
  };
  const prepared = await request(1, {
    kind: 'prepare', root, module: modulePath, sourceGraph: graph,
  });
  assert.equal(prepared.kind, 'prepared');
  const loaded = await request(2, {
    kind: 'load', root, module: modulePath, preparedToken: prepared.token,
    plugin: 'prepared-plugin', storageId: 'prepared-plugin@user', tier: 'user',
  });
  assert.equal(loaded.kind, 'loaded');
  const dispatched = await request(3, {
    kind: 'dispatch', event: 'ui.message', pluginScope: 'prepared-plugin',
    input: { kind: 'prepared-message' },
  });
  assert.deepEqual(dispatched.result, {
    prepared: true,
    kind: 'prepared-message',
    family: { plugin: 'prepared-plugin', key: 'rows', id: 'family' },
    member: { plugin: 'prepared-plugin', key: 'rows', id: 'row-7' },
    derivedSource: { plugin: 'prepared-plugin', key: 'rows', id: 'row-7' },
    computed: 'derived',
  });

  const missingBuiltin = await request(4, {
    kind: 'prepare', root, module: modulePath, sourceGraph: {
      ...graph,
      files: [{ file: modulePath, source: `import 'claude-code'; export function register() {}` }],
      links: [{ from: modulePath, spelled: 'claude-code', file: 'claude:hooks-types' }],
    },
  });
  assert.equal(missingBuiltin.kind, 'error');
  assert.match(missingBuiltin.message, /claude:hooks-types is missing from sourceGraph/);

  const missingGraph = await request(5, { kind: 'prepare', root, module: modulePath });
  assert.equal(missingGraph.kind, 'error');
  assert.match(missingGraph.message, /requires a prepared sourceGraph/);
  const missingToken = await request(6, {
    kind: 'load', root, module: modulePath, plugin: 'prepared-plugin', tier: 'user',
  });
  assert.equal(missingToken.kind, 'error');
  assert.match(missingToken.message, /requires a prepared sourceGraph token/);
  worker.stdin.end();
  const [exitCode] = await once(worker, 'exit');
  assert.equal(exitCode, 0);
});

test('ui.listener queries registrations without running hooks and pluginScope filters the dispatch chain', async t => {
  const root = await mkdtemp(join(tmpdir(), 'mods-ui-client-listener-'));
  const modulePath = join(root, 'hooks.mjs');
  await writeFile(modulePath, `
    export function register(on, options) {
      let calls = 0;
      for (const name of ['ui.fault', 'ui.message', 'ui.press', 'ui.input', 'ui.select']) {
        on(name, { surface: name === 'ui.press' ? 'terminal' : 'desktop' }, (_api, event) => {
          calls += 1;
          if (name === 'ui.press') return { handled: options.owner === event.plugin, element: event.element };
          if (name === 'ui.input' || name === 'ui.select') {
            return { handled: false };
          }
          return { owner: options.owner, hasPlugin: Object.hasOwn(event, 'plugin'), calls };
        });
      }
    }
  `);
  t.after(() => rm(root, { recursive: true, force: true }));

  const workerPath = fileURLToPath(new URL('./mods_worker.mjs', import.meta.url));
  const worker = spawn(process.execPath, ['--experimental-vm-modules', workerPath], {
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  t.after(() => { if (!worker.killed) worker.kill(); });
  const pending = new Map();
  const frames = [];
  const lines = createInterface({ input: worker.stdout, crlfDelay: Infinity });
  lines.on('line', line => {
    const frame = JSON.parse(line);
    frames.push(frame);
    if (frame.kind === 'api.callers') {
      worker.stdin.write(`${JSON.stringify({ id: frame.id, kind: 'api.callers.result', callId: frame.callId })}\n`);
      return;
    }
    if (frame.id !== 0 && !['progress', 'trace'].includes(frame.kind)) {
      pending.get(frame.id)?.(frame);
      pending.delete(frame.id);
    }
  });
  worker.once('exit', code => {
    for (const resolve of pending.values()) resolve({ kind: 'worker.exit', code });
    pending.clear();
  });
  const request = (id, message) => new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`worker request ${id} timed out`));
    }, 3_000);
    pending.set(id, frame => { clearTimeout(timeout); resolve(frame); });
    worker.stdin.write(`${JSON.stringify({ id, ...message })}\n`);
  });

  for (const plugin of ['plugin-a', 'plugin-b']) {
    const loaded = await loadPrepared(request, plugin === 'plugin-a' ? 1 : 2, {
      plugin, storageId: `${plugin}@test`, tier: 'user', root,
      module: modulePath, options: { owner: plugin },
    });
    assert.equal(loaded.kind, 'loaded');
  }
  const inputs = {
    'ui.fault': {
      surface: 'desktop', component: 'Pane', requestId: 'render-4',
      element: 'board', module: './board.tsx', phase: 'render', reason: 'broken',
    },
    'ui.message': { surface: 'desktop', message: {
      surface: 'desktop', component: 'Pane', requestId: 'render-4',
      element: 'board', module: './board.tsx', data: { kind: 'note' },
    } },
    'ui.press': {
      plugin: 'plugin-b', element: 'go', component: 'AbovePrompt', requestId: 'render-4', surface: 'terminal',
    },
    'ui.input': {
      plugin: 'plugin-b', element: 'query', component: 'Pane', requestId: 'render-4', surface: 'desktop',
      kind: 'submit', value: 'hello',
    },
    'ui.select': {
      plugin: 'plugin-b', element: 'mode', component: 'Pane', requestId: 'render-4', surface: 'desktop',
      value: 'fast',
    },
  };
  let id = 3;
  for (const event of Object.keys(inputs)) {
    const listener = await request(id++, {
      kind: 'ui.listener', event, plugin: 'plugin-b', input: inputs[event],
    });
    assert.deepEqual(listener, { id: id - 1, kind: 'ui.listener.result', matched: true }, `${event} has a matching plugin listener`);
  }
  const absent = await request(id++, {
    kind: 'ui.listener', event: 'ui.fault', plugin: 'plugin-missing', input: inputs['ui.fault'],
  });
  assert.equal(absent.matched, false);

  let expectedCalls = 0;
  for (const event of Object.keys(inputs)) {
    const message = {
      kind: 'dispatch', event, pluginScope: 'plugin-b', input: inputs[event],
    };
    if (event === 'ui.press' || event === 'ui.input' || event === 'ui.select') {
      message.pressToken = event === 'ui.press'
        ? { handle: 1, workerEpoch: 'worker-test', renderRevision: 1 }
        : { plugin: 'plugin-b', handle: 1 };
    }
    const scoped = await request(id++, message);
    assert.equal(scoped.kind, 'result', `${event}: ${scoped.message ?? ''}`);
    expectedCalls += 1;
    if (event === 'ui.press') {
      assert.deepEqual(scoped.result, { handled: true, element: 'go' });
    } else if (event === 'ui.input' || event === 'ui.select') {
      assert.deepEqual(scoped.result, { handled: false });
    } else {
      assert.deepEqual(scoped.result, {
        owner: 'plugin-b',
        hasPlugin: Object.hasOwn(inputs[event], 'plugin'),
        calls: expectedCalls,
      });
    }
  }
  worker.stdin.end();
  const [exitCode] = await once(worker, 'exit');
  assert.equal(exitCode, 0);
});

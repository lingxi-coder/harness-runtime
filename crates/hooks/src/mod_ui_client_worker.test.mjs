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
import { Script } from 'node:vm';
import { ClientSurfaceManager } from './mod_ui_client_worker.mjs';
import { decodeModsIpcFrame, encodeModsIpcFrame } from './mods_ipc_codec.mjs';

const SURFACE_RUNTIME_SOURCE = readFileSync(new URL('./mod_ui_client_helpers.mjs', import.meta.url), 'utf8');
const HOOKS_TYPES_SOURCE = readFileSync(new URL('./mod_ui_hooks_types.mjs', import.meta.url), 'utf8');

function acknowledgeBackground(worker, frame) {
  if (typeof frame.backgroundEventId === 'string') {
    worker.stdin.write(`${encodeModsIpcFrame({id:0, kind:'ui.client.backgroundComplete',
      backgroundEventId:frame.backgroundEventId, accepted:true})}\n`);
  }
}

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
  assert.equal(await manager.runHeld('drawing-1', 1, { pointerId: 4 }), true);
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
  assert.equal(await manager.runHeld('drawing-1', 1, {}), false);
  manager.dispose();
});

test('Client worker mirrors Native Button, Input and Select adapters and keeps Markdown unheld', async () => {
  const events = [];
  const manager = new ClientSurfaceManager({ send: message => events.push(message) });
  const mounted = await manager.mount(mountOptions({
    runtimeId: 'native-callback-contract',
    source: `
      import { Box, Button, Input, Select, Markdown, h } from 'claude:surface-runtime';
      export function Board(_props, surface) {
        return h(Box, { children: [
          h(Button, { key: 'save', label: 'Save', onPress: event => surface.post({ callback: 'button', event }) }),
          h(Input, {
            key: 'query',
            onSubmit: (value, event) => surface.post({ callback: 'submit', value, kind: event.kind }),
            onInput: (value, event) => surface.post({ callback: 'input', value, kind: event.kind }),
          }),
          h(Select, {
            key: 'mode', options: [{ value: 'fast' }],
            onSelect: (value, event) => surface.post({ callback: 'select', value, eventValue: event.value }),
          }),
          h(Markdown, {
            key: 'links', text: '[docs](https://example.test)',
            pressableLinks: ['https://example.test'],
            onLinkPress: undefined,
          }),
          h(Input, {
            key: 'optional-input', onInput: undefined,
            onSubmit: (value, event) => surface.post({ callback: 'optional-submit', value, kind: event.kind }),
          }),
        ] });
      }
    `,
  }));

  assert.deepEqual(mounted.tree.children.map(node => node.type), ['Button', 'Input', 'Select', 'Markdown', 'Input']);
  assert.deepEqual(mounted.tree.children.map(node => node.held), [1, 2, 3, undefined, 4]);
  for (const node of mounted.tree.children) {
    for (const callback of ['onPress', 'onSubmit', 'onInput', 'onSelect', 'onLinkPress', 'onEvent']) {
      assert.equal(Object.hasOwn(node.props, callback), false, `${node.type} callback ${callback} is not serialized`);
    }
  }

  const [button, input, select, markdown, optionalInput] = mounted.tree.children;
  assert.deepEqual(markdown.props, {
    key: 'links', text: '[docs](https://example.test)',
    pressableLinks: ['https://example.test'],
  }, 'native Client exportTree keeps pressableLinks as data without a callback');
  assert.equal(Object.hasOwn(markdown, 'held'), false,
    'Native Client does not expose a Markdown held callback');
  assert.equal(await manager.runHeld(mounted.runtimeId, button.held, { kind: 'press' }), true);
  assert.equal(await manager.runHeld(mounted.runtimeId, input.held, { kind: 'change', value: ' typed ' }), true);
  assert.equal(await manager.runHeld(mounted.runtimeId, input.held, { kind: 'submit', value: ' answer ' }), true);
  assert.equal(await manager.runHeld(mounted.runtimeId, select.held, { value: 'fast' }), true);
  assert.equal(await manager.runHeld(mounted.runtimeId, optionalInput.held, {
    kind: 'change', value: 'ignored without optional onInput',
  }), true, 'an explicitly undefined optional onInput is treated as absent');
  assert.deepEqual(events.filter(event => event.action === 'post').map(event => JSON.parse(event.json)), [
    { callback: 'button', event: { kind: 'press' } },
    { callback: 'input', value: ' typed ', kind: 'change' },
    { callback: 'submit', value: ' answer ', kind: 'submit' },
    { callback: 'select', value: 'fast', eventValue: 'fast' },
  ]);

  for (const [runtimeId, body, message] of [
    ['legacy-input-event', `h(Input, { key: 'query', onEvent() {} })`, 'Input does not accept onEvent'],
    ['legacy-select-event', `h(Select, { key: 'mode', onEvent() {} })`, 'Select does not accept onEvent'],
    ['markdown-with-on-link-press', `h(Markdown, { key: 'links', text: 'link', onLinkPress() {} })`, 'a Markdown with onLinkPress inside the tree; a surface module answers its own pointer with surface.onPointer'],
  ]) {
    await assert.rejects(manager.mount(mountOptions({
      runtimeId,
      source: `import { Box, Input, Select, Markdown, h } from 'claude:surface-runtime';
        export function Board() { return h(Box, { children: ${body} }); }`,
    })), error => error.phase === 'render' && error.message.includes(message));
  }
  manager.dispose();
});

test('runHeld correlates posts through async completion and still rejects a replaced handle', async () => {
  const events = [];
  const manager = new ClientSurfaceManager({ send: message => events.push(message) });
  const mounted = await manager.mount(mountOptions({
    runtimeId: 'post-action-correlation',
    source: `
      import { Button, h } from 'claude:surface-runtime';
      export function Board(_props, surface) {
        return h(Button, { onPress: async () => {
          surface.post({ stage: 'synchronous' });
          await Promise.resolve();
          surface.post({ stage: 'promise-continuation' });
        } });
      }
    `,
  }));
  const handle = mounted.tree.held;
  await assert.rejects(manager.operation({
    type: 'runHeld', runtimeId: mounted.runtimeId, handle, payload: { kind: 'press' },
  }), /requires a host post action ID/);
  const action = await manager.operation({
    type: 'runHeld',
    runtimeId: mounted.runtimeId,
    handle,
    payload: { kind: 'press' },
    postActionId: 'host-action-7',
  });
  assert.deepEqual(action, { accepted: true, postIds: [0, 1] });
  const posts = events.filter(event => event.action === 'post');
  assert.equal(posts.length, 2);
  assert.equal(posts[0].postActionId, 'host-action-7');
  assert.equal(posts[0].postId, 0);
  assert.deepEqual(JSON.parse(posts[0].json), { stage: 'synchronous' });
  assert.equal(posts[1].postActionId, 'host-action-7');
  assert.equal(posts[1].postId, 1, 'a post after await belongs to the same live action');

  manager.render(mounted.runtimeId);
  const stale = await manager.operation({
    type: 'runHeld',
    runtimeId: mounted.runtimeId,
    handle,
    payload: { kind: 'press' },
    postActionId: 'host-action-stale',
  });
  assert.deepEqual(stale, { accepted: false, postIds: [] });
  assert.equal(events.filter(event => event.action === 'post').length, 2,
    'a replaced handle does not invoke its callback or emit a post');
  manager.dispose();
});


test('async Input waits for its own posts while unrelated timer posts stay uncorrelated', async () => {
  const events = [];
  const manager = new ClientSurfaceManager({ send: message => events.push(message) });
  const mounted = await manager.mount(mountOptions({
    runtimeId: 'async-input',
    source: `
      let scheduled = false;
      export function Board(_props, surface) {
        if (!scheduled) {
          scheduled = true;
          const stop = surface.every(5, () => { stop(); surface.post({ stage:'unrelated' }); });
        }
        return surface.elements.Input({
          value: surface.state.value || '', onSubmit() {},
          onInput: async value => {
            surface.post({ stage:'before' });
            await new Promise(resolve => {
              const stop = surface.every(40, () => { stop(); resolve(); });
            });
            surface.post({ stage:'after', value });
            surface.setState({ value });
          },
        });
      }
    `,
  }));
  let completed = false;
  const operation = manager.operation({ type:'runHeld', runtimeId:mounted.runtimeId,
    handle:mounted.tree.held, payload:{kind:'change', value:'typed'}, postActionId:'input-1' });
  operation.then(() => { completed = true; });
  await delay(15);
  assert.equal(completed, false, 'the old tree cannot be acknowledged while callback is suspended');
  assert.equal(manager.render(mounted.runtimeId).tree.props.value, '');
  const result = await operation;
  assert.deepEqual(result, { accepted:true, postIds:[0, 1, 2] });
  const posts = events.filter(event => event.action === 'post');
  const byStage = Object.fromEntries(posts.map(event => [JSON.parse(event.json).stage, event]));
  assert.equal(byStage.before.postActionId, 'input-1');
  assert.equal(byStage.after.postActionId, 'input-1');
  assert.equal(byStage.after.postId, 1);
  assert.equal(Object.hasOwn(byStage.unrelated, 'postActionId'), false);
  assert.equal(manager.render(mounted.runtimeId).tree.props.value, 'typed');
  manager.dispose();
});

test('Client events retain the Host revision that produced them', async () => {
  const events=[];
  const manager=new ClientSurfaceManager({send:event=>events.push(event)});
  const source=`let sent=false;export function Board(props,surface){
    if(props.trigger&&!sent){sent=true;surface.post({stage:'revision-two'});}
    return surface.elements.Text({children:'ready'});
  }`;
  const options=mountOptions({runtimeId:'revision-owner',source});
  options.hostRenderRevision=1;
  const mounted=await manager.operation({type:'mount',...options});
  const updated=await manager.operation({type:'setProps',runtimeId:'revision-owner',generation:mounted.generation,
    hostRenderRevision:2,props:{trigger:true}});
  assert.equal(updated.tree.type,'Text');
  const post=events.find(event=>event.action==='post');
  assert.equal(post.hostRenderRevision,2);
  assert.deepEqual(await manager.operation({type:'render',runtimeId:'revision-owner',generation:mounted.generation,
    hostRenderRevision:1}),{stale:true},'an older Host operation cannot roll back the event revision');
  manager.dispose();
});

test('cancel invalidates every surface sharing the VM and remount isolates a late continuation', async () => {
  const events = [];
  const manager = new ClientSurfaceManager({ send: message => events.push(message) });
  const source = `
    let hidden = '';
    export function Board(_props, surface) {
      if (!surface.state.timerStarted) {
        surface.setState({ timerStarted:true });
        surface.every(5, () => surface.post({stage:'old-timer'}));
      }
      return surface.elements.Input({value:hidden, onSubmit(){}, onInput:async value => {
        surface.post({stage:'before', value});
        await globalThis.testGate;
        hidden = value;
        surface.post({stage:'after', value});
        surface.setState({value});
      }});
    }
  `;
  const first = await manager.mount(mountOptions({runtimeId:'old-one', source}));
  const sibling = await manager.mount(mountOptions({runtimeId:'old-two', source}));
  const oldEnvironment = manager.runtimeInstances.get('old-one').environment;
  assert.equal(manager.runtimeInstances.get('old-two').environment, oldEnvironment,
    'the two surfaces really share module state');
  let resume;
  oldEnvironment.context.testGate = new Promise(resolve => { resume = resolve; });
  const controller = new AbortController();
  const pending = manager.operation({type:'runHeld', runtimeId:'old-one', handle:first.tree.held,
    payload:{kind:'change', value:'cancelled-draft'}, postActionId:'cancel-me'}, {signal:controller.signal});
  await delay(2);
  controller.abort();
  await assert.rejects(pending, error => error.phase === 'run' && error.outcomeUnknown === true);
  assert.deepEqual(await manager.operation({type:'render', runtimeId:'old-one'}), {stale:true});
  assert.deepEqual(await manager.operation({type:'render', runtimeId:'old-two'}), {stale:true});
  await assert.rejects(manager.operation({type:'runHeld', runtimeId:'old-two', handle:sibling.tree.held,
    payload:{kind:'change',value:'must-not-run'}, postActionId:'stale'}),
    error => error.phase === 'run' && error.notStartedUnavailable === true);
  assert.equal(events.filter(event => event.action === 'fault').length, 2,
    'all surfaces sharing the invalidated environment receive a fault');
  const oldPostCount = events.filter(event => event.action === 'post' && event.runtimeId.startsWith('old-')).length;
  await delay(20);
  assert.equal(events.filter(event => event.action === 'post' && event.runtimeId.startsWith('old-')).length, oldPostCount,
    'timers belonging to the failed VM stop');
  const fresh = await manager.mount(mountOptions({runtimeId:'fresh', source}));
  assert.notEqual(manager.runtimeInstances.get('fresh').environment, oldEnvironment,
    'an explicit remount creates a new VM even for the same plugin and manifest');
  assert.equal(fresh.tree.props.value, '');
  resume();
  await delay(0);
  assert.equal(manager.render('fresh').tree.props.value, '',
    'a resumed old continuation cannot change the new VM module state');
  assert.equal(events.filter(event => event.action === 'post' && event.json.includes('"stage":"after"')).length, 0);
  assert.deepEqual(await manager.operation({type:'runHeld', runtimeId:'fresh', handle:manager.render('fresh').tree.held,
    payload:{kind:'change',value:'fresh-value'}, postActionId:'fresh-action'}), {accepted:true, postIds:[0,1,2]});
  assert.equal(manager.render('fresh').tree.props.value, 'fresh-value');
  manager.dispose();
});

test('an async handler rejection or timeout is unknown, faults the runtime and never confirms a no-op', async () => {
  for (const mode of ['reject', 'timeout']) {
    const events = [];
    const manager = new ClientSurfaceManager({send: event => events.push(event), asyncHeldBudgetMs:25});
    const body = mode === 'reject'
      ? "await Promise.resolve(); throw new Error('failure after post');"
      : 'await new Promise(() => {});';
    const mounted = await manager.mount(mountOptions({runtimeId:`async-${mode}`, source:`
      export function Board(_props, surface) {
        return surface.elements.Button({onPress: async () => {
          surface.post({stage:'before'}); ${body}
        }});
      }
    `}));
    await assert.rejects(manager.operation({type:'runHeld', runtimeId:mounted.runtimeId,
      handle:mounted.tree.held, payload:{kind:'press'}, postActionId:`action-${mode}`}),
      error => error.phase === 'run' && error.outcomeUnknown === true);
    assert.equal(events.filter(event => event.action === 'post').length, 1);
    assert.equal(events.filter(event => event.action === 'fault').length, 1);
    assert.equal(manager.runtimeInstances.get(mounted.runtimeId).failure.status, 'failed');
    assert.deepEqual(await manager.operation({type:'render', runtimeId:mounted.runtimeId}), {stale:true});
    manager.dispose();
  }
});

test('Client tree JSON matches Native exportTree field order and Input/Select omissions', async () => {
  const manager = new ClientSurfaceManager({ send() {} });
  const mounted = await manager.mount(mountOptions({
    runtimeId: 'native-export-tree-bytes',
    source: `export function Board(_props, surface) { return surface.elements.Text({ children: 'ready' }); }`,
  }));
  const instance = manager.runtimeInstances.get(mounted.runtimeId);
  assert(instance, 'the mounted runtime exposes the production serializer');

  const makeTreeAndSerialize = new Script(`(serializeTree, bridge) => serializeTree({
    type: 'Box', props: {}, children: [
      { type: 'Button', props: { label: 'Save', onPress() {} }, hover: { borderColor: 'cyan' }, children: [] },
      { type: 'Input', props: { key: 'query', onSubmit() {} }, hover: { borderColor: 'cyan' }, children: [] },
      { type: 'Select', props: { key: 'mode', options: [{ value: 'fast' }], onSelect() {} }, hover: { borderColor: 'cyan' }, children: [] },
      { type: 'Markdown', props: { text: 'docs' }, hover: { dimColor: true }, children: [] },
    ],
  }, bridge)`).runInContext(instance.environment.context);
  let nextHandle = 0;
  const actualRawJson = makeTreeAndSerialize(instance.environment.serializeTree, {
    hold() { nextHandle += 1; return nextHandle; },
  });

  // Fixed Native 2.1.295 exportTree source: skill-display-name-source/src_192995347.js,
  // SHA cfaaecaee8d26184f098f50f71d96e032ee78508ff5e67f8e8e7a979344223a3,
  // binary offset 192995347, local bytes 296856-301053. Compare raw JSON so key order
  // and omitted Input/Select children remain observable.
  assert.equal(actualRawJson,
    '{"type":"Box","props":{},"children":[{"type":"Button","props":{"label":"Save"},"hover":{"borderColor":"cyan"},"held":1,"children":[]},{"type":"Input","props":{"key":"query"},"hover":{"borderColor":"cyan"},"held":2},{"type":"Select","props":{"key":"mode","options":[{"value":"fast"}]},"hover":{"borderColor":"cyan"},"held":3},{"type":"Markdown","props":{"text":"docs"},"hover":{"dimColor":true},"children":[]}]}');
  manager.dispose();
});

test('Client plain-tree copy matches Native enumerable data and property budgets', async () => {
  const manager = new ClientSurfaceManager({ send() {} });
  const mounted = await manager.mount(mountOptions({
    runtimeId: 'native-plain-copy',
    source: `export function Board(_props, surface) { return surface.elements.Text({ children: 'ready' }); }`,
  }));
  const instance = manager.runtimeInstances.get(mounted.runtimeId);
  assert(instance);
  const invoke = new Script('(serializeTree, tree, bridge) => serializeTree(tree, bridge)')
    .runInContext(instance.environment.context);
  let nextHandle = 0;
  const serialize = tree => invoke(instance.environment.serializeTree, tree, {
    hold() { nextHandle += 1; return nextHandle; },
  });

  assert.equal(
    serialize({ type: 'Text', props: { children: { source: 'metadata' } } }),
    '{"type":"Text","props":{"children":{"source":"metadata"}}}',
    'raw Text props.children is metadata and is preserved independently of tree children',
  );

  let getterReads = 0;
  const array = [undefined, () => {}, 1n, Symbol('array-value'), 'kept'];
  array.extra = 'ignored';
  array[Symbol('array-key')] = 'ignored';
  const inheritedArray = new Array(1);
  const arrayPrototype = Object.create(Array.prototype);
  Object.defineProperty(arrayPrototype, '0', { value: 'inherited', enumerable: true });
  Object.setPrototypeOf(inheritedArray, arrayPrototype);
  inheritedArray.extra = 'ignored';
  const props = Object.create({ inherited: 'ignored' });
  Object.defineProperties(props, {
    visible: { enumerable: true, value: 7 },
    read: { enumerable: true, get() { getterReads += 1; return 'got'; } },
    bigint: { enumerable: true, value: 1n },
    symbol: { enumerable: true, value: Symbol('object-value') },
    callback: { enumerable: true, value() {} },
    array: { enumerable: true, value: array },
    inheritedArray: { enumerable: true, value: inheritedArray },
    nullable: { enumerable: true, value: null },
    missing: { enumerable: true, value: undefined },
    nonfinite: { enumerable: true, value: Infinity },
    toJSON: { enumerable: true, value() { throw new Error('toJSON must not run'); } },
    hidden: { enumerable: false, get() { throw new Error('non-enumerable getter must not run'); } },
  });
  props[Symbol('object-key')] = 'ignored';
  const node = Object.create({ type: 'inherited-type', props: { inherited: 'ignored' } });
  node.type = 'Text';
  node.props = props;
  assert.equal(serialize(node),
    '{"type":"Text","props":{"visible":7,"read":"got","array":[null,null,null,null,"kept"],"inheritedArray":["inherited"],"nullable":null,"nonfinite":null}}');
  assert.equal(getterReads, 1, 'Object.keys properties are read once, including enumerable getters');

  assert.equal(serialize('root string'), '"root string"', 'a root string is a valid Native tree');
  assert.equal(serialize({ type: 'Text', props: 'plain' }), '{"type":"Text","props":"plain"}');
  assert.equal(serialize({ type: 'Text', props: null }), '{"type":"Text","props":null}');
  assert.equal(serialize({ type: 'Text', props: 1n }), '{"type":"Text"}', 'unsupported primitive props disappear');
  assert.equal(serialize({ type: 'Text', props: Symbol('root') }), '{"type":"Text"}');

  const inheritedChildren = new Array(1);
  const childrenPrototype = Object.create(Array.prototype);
  Object.defineProperty(childrenPrototype, '0', {
    value: { type: 'Text', props: { label: 'inherited child' } }, enumerable: true,
  });
  Object.setPrototypeOf(inheritedChildren, childrenPrototype);
  assert.equal(serialize({ type: 'Box', props: {}, children: inheritedChildren }),
    '{"type":"Box","props":{},"children":[{"type":"Text","props":{"label":"inherited child"}}]}',
    'Native reads children[i], including an inherited index');
  assert.throws(() => serialize({ type: 'Box', props: {}, children: new Array(1) }),
    error => error.message === 'a child that is neither a string nor an element');

  assert.throws(() => serialize({ type: 'Text', props: { values: new Array(1) } }),
    error => error.message === 'props with a hole in an array');
  const tooDeep = (() => { let value = 'leaf'; for (let index = 0; index < 34; index += 1) value = { next: value }; return value; })();
  assert.throws(() => serialize({ type: 'Text', props: tooDeep }),
    error => error.message === 'props nesting deeper than 32');
  const overBudget = { first: Array(9999).fill(0), second: Array(9999).fill(0) };
  assert.throws(() => serialize({ type: 'Text', props: overBudget }),
    error => error.message === 'props of more than 20000 values');

  const inputAtBudget = serialize({
    type: 'Input',
    props: {
      onSubmit() {}, onInput: undefined, query: 'q',
      first: Array(9999).fill(0), second: Array(9997).fill(0),
    },
    children: [],
  });
  assert.equal(JSON.parse(inputAtBudget).held, nextHandle,
    'both Input callback keys, including explicit undefined onInput, are excluded from Native props accounting');
  assert.equal(Object.hasOwn(JSON.parse(inputAtBudget), 'children'), false);
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
        if (failPointerOnce && !_props.allowPointer) {
          failPointerOnce = false;
          throw new Error('stop this surface');
        }
      });
      return surface.elements.Button({ label:failPointerOnce ? 'fresh' : 'changed',
        onPress: event => surface.post({ kind: 'held', event }) });
    }
  `;
  const mounted = await manager.mount(mountOptions({ runtimeId: 'faulted-vm', source }));
  const oldEnvironment = manager.runtimeInstances.get('faulted-vm').environment;
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
  assert.equal(await manager.runHeld('faulted-vm', heldHandle, { type: 'press' }), false,
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

  const options = mountOptions({ runtimeId: 'faulted-vm', source });
  options.props = {allowPointer:true};
  const remounted = await manager.mount(options);
  assert.notEqual(manager.runtimeInstances.get('faulted-vm').environment, oldEnvironment);
  assert(remounted.generation > mounted.generation);
  assert.equal(remounted.frameSequence, 1);
  assert.equal(remounted.tree.props.label, 'fresh', 'module state resets in the replacement VM');
  assert.equal(manager.pointer('faulted-vm', { type: 'move' }), true,
    'a fresh environment restores the surface with corrected props');
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


test('mods worker admits matching setProps while an async held action waits for that update', async t => {
  const workerPath = fileURLToPath(new URL('./mods_worker.mjs', import.meta.url));
  const worker = spawn(process.execPath, ['--experimental-vm-modules', workerPath],
    { stdio:['pipe', 'pipe', 'pipe'] });
  t.after(() => { if (!worker.killed) worker.kill(); });
  const pending = new Map();
  const events = [];
  let wakePost;
  const posted = new Promise(resolve => { wakePost = resolve; });
  createInterface({ input:worker.stdout, crlfDelay:Infinity }).on('line', line => {
    const frame = decodeModsIpcFrame(line);
    acknowledgeBackground(worker, frame);
    if (frame.id === 0) {
      events.push(frame);
      if (frame.action === 'post') wakePost();
    } else {
      pending.get(frame.id)?.(frame);
      pending.delete(frame.id);
    }
  });
  const request = (id, message) => new Promise((resolve, reject) => {
    const timeout = setTimeout(() => { pending.delete(id); reject(new Error(`worker request ${id} timed out`)); }, 2_000);
    pending.set(id, frame => { clearTimeout(timeout); resolve(frame); });
    worker.stdin.write(`${encodeModsIpcFrame({id, ...message})}\n`);
  });
  const mounted = await request(1, {kind:'ui.client.operation', operation:{type:'mount',
    ...mountOptions({runtimeId:'held-waits-for-props', source:`
      let latest = '';
      export function Board(props, surface) {
        latest = props.value || '';
        return surface.elements.Input({value:latest, onSubmit(){}, onInput:async value => {
          surface.post({value});
          await new Promise(resolve => {
            const stop = surface.every(5, () => { if (latest === value) { stop(); resolve(); } });
          });
          surface.post({completed:value});
        }});
      }
    `})}});
  let done = false;
  const held = request(2, {kind:'ui.client.operation', operation:{type:'runHeld',
    runtimeId:'held-waits-for-props', generation:mounted.result.generation, handle:mounted.result.tree.held,
    payload:{kind:'change',value:'typed'}, postActionId:'held-action'}});
  held.then(() => { done = true; });
  await posted;
  assert.equal(done, false);
  const updated = await request(3, {kind:'ui.client.operation', operation:{type:'setProps',
    runtimeId:'held-waits-for-props', generation:mounted.result.generation, postActionId:'held-action', props:{value:'typed'}}});
  assert.equal(updated.result.tree.props.value, 'typed');
  assert.deepEqual((await held).result, {accepted:true, postIds:[0,1]});
  assert.deepEqual(events.filter(event => event.action === 'post').map(event => event.postActionId),
    ['held-action', 'held-action']);
});

test('held post permits its nested Host API dispatch while unrelated dispatch remains queued', async t => {
  const root = await mkdtemp(join(tmpdir(), 'mods-held-nested-'));
  const hookPath = join(root, 'hook.mjs');
  await writeFile(hookPath, `export function register(on) {
    on('ui.message', async ($, event) => {
      await $.session.model();
      return {handled:true, props:{value:event.data.value}};
    });
  }`);
  t.after(() => rm(root, {recursive:true, force:true}));
  const workerPath = fileURLToPath(new URL('./mods_worker.mjs', import.meta.url));
  const worker = spawn(process.execPath, ['--experimental-vm-modules', workerPath], {stdio:['pipe','pipe','pipe']});
  t.after(() => { if (!worker.killed) worker.kill(); });
  const pending = new Map();
  let modelCalls = 0;
  let postResolve;
  const posted = new Promise(resolve => { postResolve = resolve; });
  const request = (id, message) => new Promise((resolve,reject) => {
    const timer = setTimeout(() => {pending.delete(id); reject(new Error(`request ${id} timed out`));}, 2_000);
    pending.set(id, frame => {clearTimeout(timer); resolve(frame);});
    worker.stdin.write(`${encodeModsIpcFrame({id,...message})}\n`);
  });
  createInterface({input:worker.stdout, crlfDelay:Infinity}).on('line', line => {
    const frame = decodeModsIpcFrame(line);
    acknowledgeBackground(worker, frame);
    if (frame.kind === 'api.callers') {
      worker.stdin.write(`${encodeModsIpcFrame({id:frame.id, kind:'api.callers.result', callId:frame.callId})}\n`);
    } else if (frame.kind === 'api' && frame.method === 'session.model') {
      modelCalls++;
      // Simulate the Host API middleware re-entry; logRouteId is the actual
      // parent dispatch ID that Rust supplies in dispatch_host_api_inner.
      request(5, {kind:'dispatch', event:'session.model', logRouteId:frame.id, input:{}}).then(() => {
        worker.stdin.write(`${encodeModsIpcFrame({id:frame.id, kind:'api.result', callId:frame.callId, result:'test-model'})}\n`);
      });
    } else if (frame.kind === 'next') {
      worker.stdin.write(`${encodeModsIpcFrame({id:frame.id, kind:'next.result', callId:frame.callId, result:{value:'test-model'}})}\n`);
    } else if (frame.id === 0) {
      if (frame.action === 'post') postResolve(frame);
    } else if (frame.kind !== 'progress' && frame.kind !== 'trace') {
      pending.get(frame.id)?.(frame); pending.delete(frame.id);
    }
  });
  assert.equal((await loadPrepared(request, 1, {plugin:'example',storageId:'example@user',tier:'user',root,module:hookPath})).kind, 'loaded');
  const source = `let latest=''; export function Board(props,surface) {
    latest=props.value || '';
    return surface.elements.Input({value:latest,onSubmit(){},onInput:async value => {
      surface.post({value});
      await new Promise(resolve => {const stop=surface.every(5,()=>{if(latest===value){stop();resolve();}})});
    }});
  }`;
  const options=mountOptions({runtimeId:'nested-held',source});
  options.parent.requestId = 'render-7\u0000client\u0000board';
  const mounted=await request(2,{kind:'ui.client.operation',operation:{type:'mount',...options}});
  const held=request(3,{kind:'ui.client.operation',operation:{type:'runHeld',runtimeId:'nested-held',
    generation:mounted.result.generation,handle:mounted.result.tree.held,payload:{kind:'change',value:'typed'},postActionId:'nested-action'}});
  const messageInput={surface:'desktop',component:'panel',requestId:'render-7',element:'board',module:'board.mjs',data:{value:'typed'}};
  const previouslyQueued=request(11,{kind:'dispatch',event:'ui.message',pluginScope:'example',input:messageInput});
  await posted;
  let unrelatedDone=false;
  const unrelated=request(8,{kind:'dispatch',event:'session.model',logRouteId:999,input:{}}).then(result=>{unrelatedDone=true;return result;});
  const unowned=await request(9,{kind:'dispatch',event:'ui.message',pluginScope:'example',input:messageInput});
  assert.equal(unowned.kind,'error');
  const wrongAction=await request(10,{kind:'dispatch',event:'ui.message',pluginScope:'example',
    clientPostActionId:'different-action',input:messageInput});
  assert.equal(wrongAction.kind,'error');
  assert.equal(modelCalls,0,'unrelated messages are rejected before executing the hook');
  const response=await request(4,{kind:'dispatch',event:'ui.message',pluginScope:'example',
    clientPostActionId:'nested-action',input:messageInput});
  assert.equal(response.kind,'result');
  assert.equal(response.result.props.value,'typed');
  assert.equal(modelCalls,1);
  assert.equal(unrelatedDone,false,'an unrelated nested request cannot bypass the held queue');
  await request(6,{kind:'ui.client.operation',operation:{type:'setProps',runtimeId:'nested-held',
    generation:mounted.result.generation,postActionId:'nested-action',props:{value:'typed'}}});
  assert.deepEqual((await held).result,{accepted:true,postIds:[0]});
  assert.equal((await previouslyQueued).kind,'error','queued messages recheck the held environment');
  assert.equal(modelCalls,1,'the queued unrelated message did not execute the hook');
  assert.equal((await request(3,{kind:'ui.client.finalize',postActionId:'nested-action'})).kind,'ui.client.finalized');
  assert.equal((await unrelated).kind,'result');
});

test('held action remains revocable after callback result until Host finalizes it', async t => {
  const workerPath = fileURLToPath(new URL('./mods_worker.mjs', import.meta.url));
  const worker = spawn(process.execPath, ['--experimental-vm-modules', workerPath], {stdio:['pipe','pipe','pipe']});
  t.after(() => { if (!worker.killed) worker.kill(); });
  const pending = new Map();
  const events = [];
  createInterface({input:worker.stdout, crlfDelay:Infinity}).on('line', line => {
    const frame = decodeModsIpcFrame(line);
    acknowledgeBackground(worker, frame);
    if (frame.id === 0) events.push(frame);
    else { pending.get(frame.id)?.(frame); pending.delete(frame.id); }
  });
  const request = (id, message) => new Promise((resolve,reject) => {
    const timer = setTimeout(() => {pending.delete(id); reject(new Error(`request ${id} timed out`));}, 2_000);
    pending.set(id, frame => {clearTimeout(timer); resolve(frame);});
    worker.stdin.write(`${encodeModsIpcFrame({id,...message})}\n`);
  });
  const source = `let hidden = '';
    export function Board(_props, surface) {
      return surface.elements.Input({value:hidden, onSubmit(){}, onInput:value => {hidden=value;}});
    }`;
  const mount = (id, runtimeId) => request(id, {kind:'ui.client.operation', operation:{type:'mount',
    ...mountOptions({runtimeId, source})}});
  const first = await mount(1, 'lease-one');
  const sibling = await mount(2, 'lease-sibling');
  assert.equal(first.result.environmentInstanceId, sibling.result.environmentInstanceId);
  const outcome = await request(3, {kind:'ui.client.operation', operation:{type:'runHeld',
    runtimeId:'lease-one', generation:first.result.generation, handle:first.result.tree.held,
    payload:{kind:'change',value:'unconfirmed'}, postActionId:'abandoned'}});
  assert.deepEqual(outcome.result, {accepted:true, postIds:[]});
  const busy = await request(4, {kind:'ui.client.operation', operation:{type:'render',
    runtimeId:'lease-sibling', generation:sibling.result.generation}});
  assert.equal(busy.error.busy, true, 'the sibling is refused before it can observe unconfirmed VM state');
  const unownedSetProps = await request(20, {kind:'ui.client.operation', operation:{type:'setProps',
    runtimeId:'lease-one', generation:first.result.generation, props:{value:'intruder'}}});
  assert.equal(unownedSetProps.error.busy, true, 'a same-runtime props update needs the Host action ID');
  const otherOptions = mountOptions({runtimeId:'other-runtime', source});
  otherOptions.plugin = 'other-plugin';
  otherOptions.environmentId = 'other-environment';
  const other = await request(21, {kind:'ui.client.operation', operation:{type:'mount', ...otherOptions}});
  const otherRender = await request(22, {kind:'ui.client.operation', operation:{type:'render',
    runtimeId:'other-runtime', generation:other.result.generation}});
  assert.equal(otherRender.result.tree.props.value, '', 'another VM is not blocked by the pending lease');
  worker.stdin.write(`${encodeModsIpcFrame({id:3,kind:'cancel'})}\n`);
  const immediate = await request(23, {kind:'ui.client.operation', operation:{type:'render',
    runtimeId:'lease-sibling', generation:sibling.result.generation}});
  assert(immediate.error?.busy === true || immediate.result?.stale === true,
    'a request racing cancellation cannot see the unconfirmed VM');
  for (let i=0; i<100 && events.filter(event=>event.action==='fault').length<2; i+=1) await delay(5);
  assert.equal(events.filter(event => event.action === 'fault').length, 2);
  const siblingAfterCancel = await request(24, {kind:'ui.client.operation', operation:{type:'render',
    runtimeId:'lease-sibling', generation:sibling.result.generation}});
  assert.deepEqual(siblingAfterCancel.result, {stale:true});

  const fresh = await mount(5, 'lease-fresh');
  assert.notEqual(fresh.result.environmentInstanceId, first.result.environmentInstanceId);
  assert.equal(fresh.result.tree.props.value, '');
  const accepted = await request(6, {kind:'ui.client.operation', operation:{type:'runHeld',
    runtimeId:'lease-fresh', generation:fresh.result.generation, handle:fresh.result.tree.held,
    payload:{kind:'change',value:'committed'}, postActionId:'accepted'}});
  assert.deepEqual(accepted.result, {accepted:true, postIds:[]});
  const ownRender = await request(7, {kind:'ui.client.operation', operation:{type:'render',
    runtimeId:'lease-fresh', generation:fresh.result.generation, postActionId:'accepted'}});
  assert.equal(ownRender.result.tree.props.value, 'committed');
  assert.equal((await request(6, {kind:'ui.client.finalize', postActionId:'accepted'})).kind, 'ui.client.finalized');
  const after = await request(8, {kind:'ui.client.operation', operation:{type:'render',
    runtimeId:'lease-fresh', generation:fresh.result.generation}});
  assert.equal(after.result.tree.props.value, 'committed');
  const freshSibling = await mount(9, 'lease-fresh-sibling');
  assert.equal(freshSibling.result.environmentInstanceId, fresh.result.environmentInstanceId);
  const beforeUnmount = await request(10, {kind:'ui.client.operation', operation:{type:'runHeld',
    runtimeId:'lease-fresh', generation:fresh.result.generation, handle:after.result.tree.held,
    payload:{kind:'change',value:'before-unmount'}, postActionId:'unmount-action'}});
  assert.deepEqual(beforeUnmount.result, {accepted:true, postIds:[]});
  const unmounted = await request(11, {kind:'ui.client.operation', operation:{type:'unmount',
    runtimeId:'lease-fresh', generation:fresh.result.generation}});
  assert.deepEqual(unmounted.result, {unmounted:true});
  for (let i=0; i<100 && events.filter(event=>event.action==='fault').length<3; i+=1) await delay(5);
  assert.equal(events.filter(event=>event.action==='fault').length, 3);
  const siblingAfter = await request(12, {kind:'ui.client.operation', operation:{type:'render',
    runtimeId:'lease-fresh-sibling', generation:freshSibling.result.generation}});
  assert.deepEqual(siblingAfter.result, {stale:true}, 'unmount revokes the pending action and shared VM');
});

test('background timer events wait for the held action and Host acknowledgements', async t => {
  const workerPath=fileURLToPath(new URL('./mods_worker.mjs',import.meta.url));
  const worker=spawn(process.execPath,['--experimental-vm-modules',workerPath],{stdio:['pipe','pipe','pipe']});
  t.after(()=>{if(!worker.killed)worker.kill();});
  const pending=new Map(), events=[];
  let coreCalls=0;
  createInterface({input:worker.stdout,crlfDelay:Infinity}).on('line',line=>{
    const frame=decodeModsIpcFrame(line);
    if(frame.kind==='next') {
      coreCalls++;
      worker.stdin.write(`${encodeModsIpcFrame({id:frame.id,kind:'next.result',callId:frame.callId,result:{handled:true}})}\n`);
    } else if(frame.id===0) events.push(frame);
    else {pending.get(frame.id)?.(frame);pending.delete(frame.id);}
  });
  const request=(id,message)=>new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>{pending.delete(id);reject(new Error(`request ${id} timed out`));},2_000);
    pending.set(id,frame=>{clearTimeout(timer);resolve(frame);});
    worker.stdin.write(`${encodeModsIpcFrame({id,...message})}\n`);
  });
  const source=`export function Board(_props,surface) {
    return surface.elements.Input({value:surface.state.value||'',onSubmit(){},onInput:()=>{
      const stop=surface.every(5,()=>{stop();surface.setState({value:'timer'});surface.post({stage:'timer'});});
    }});
  }`;
  const options=mountOptions({runtimeId:'background-owner',source});
  options.parent.requestId=['render-7','client','board'].join(String.fromCharCode(0));
  const mounted=await request(1,{kind:'ui.client.operation',operation:{type:'mount',...options}});
  const result=await request(2,{kind:'ui.client.operation',operation:{type:'runHeld',runtimeId:'background-owner',
    generation:mounted.result.generation,handle:mounted.result.tree.held,payload:{kind:'change',value:'x'},postActionId:'action'}});
  assert.deepEqual(result.result,{accepted:true,postIds:[]});
  await delay(30);
  assert.equal(events.filter(event=>event.action==='schedule'||event.action==='post').length,0,
    'unrelated timer effects remain queued while the action is unconfirmed');
  assert.equal((await request(2,{kind:'ui.client.finalize',postActionId:'action'})).kind,'ui.client.finalized');
  for(let i=0;i<100 && !events.some(event=>event.action==='schedule');i++) await delay(5);
  const scheduled=events.find(event=>event.action==='schedule');
  assert(scheduled?.backgroundEventId);
  assert.equal(events.some(event=>event.action==='post'),false,'a later event waits for the first Host acknowledgement');
  const unowned=await request(3,{kind:'ui.client.operation',operation:{type:'render',runtimeId:'background-owner',generation:mounted.result.generation}});
  assert.equal(unowned.error.busy,true);
  const rendered=await request(4,{kind:'ui.client.operation',operation:{type:'render',runtimeId:'background-owner',
    generation:mounted.result.generation,backgroundEventId:scheduled.backgroundEventId}});
  assert.equal(rendered.result.tree.props.value,'timer');
  worker.stdin.write(`${encodeModsIpcFrame({id:0,kind:'ui.client.backgroundComplete',backgroundEventId:scheduled.backgroundEventId,accepted:true})}\n`);
  for(let i=0;i<100 && !events.some(event=>event.action==='post');i++) await delay(5);
  const posted=events.find(event=>event.action==='post');
  assert(posted?.backgroundEventId);
  assert.notEqual(posted.backgroundEventId,scheduled.backgroundEventId);
  worker.stdin.write(`${encodeModsIpcFrame({id:0,kind:'ui.client.backgroundComplete',backgroundEventId:scheduled.backgroundEventId,accepted:false})}\n`);
  const input={surface:'desktop',component:'panel',requestId:'render-7',element:'board',module:'board.mjs',data:{stage:'timer'}};
  assert.equal((await request(5,{kind:'dispatch',event:'ui.message',pluginScope:'example',input})).kind,'error');
  assert.equal(coreCalls,0,'an unowned event must not execute the hook/core');
  assert.equal((await request(6,{kind:'dispatch',event:'ui.message',pluginScope:'example',
    clientBackgroundEventId:posted.backgroundEventId,input})).kind,'result');
  assert.equal(coreCalls,1);
  worker.stdin.write(`${encodeModsIpcFrame({id:0,kind:'ui.client.backgroundComplete',backgroundEventId:posted.backgroundEventId,accepted:true})}\n`);
  const after=await request(7,{kind:'ui.client.operation',operation:{type:'render',runtimeId:'background-owner',generation:mounted.result.generation}});
  assert.equal(after.result.tree.props.value,'timer');
});

test('pointer and key callbacks wait for promises and keep their post identity', async t => {
  const workerPath=fileURLToPath(new URL('./mods_worker.mjs',import.meta.url));
  const worker=spawn(process.execPath,['--experimental-vm-modules',workerPath],{stdio:['pipe','pipe','pipe']});
  t.after(()=>{if(!worker.killed)worker.kill();});
  const pending=new Map(),events=[];
  createInterface({input:worker.stdout,crlfDelay:Infinity}).on('line',line=>{
    const frame=decodeModsIpcFrame(line);
    acknowledgeBackground(worker,frame);
    if(frame.id===0)events.push(frame);else{pending.get(frame.id)?.(frame);pending.delete(frame.id);}
  });
  const request=(id,message)=>new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>{pending.delete(id);reject(new Error(`request ${id} timed out`));},2_000);
    pending.set(id,frame=>{clearTimeout(timer);resolve(frame);});
    worker.stdin.write(`${encodeModsIpcFrame({id,...message})}\n`);
  });
  const source=`export function Board(_props,surface){
    surface.onPointer(async value=>{surface.post({kind:'pointer-before',value});await Promise.resolve();surface.post({kind:'pointer-after'});});
    surface.onKey(async value=>{surface.post({kind:'key-before',value});await Promise.resolve();surface.post({kind:'key-after'});});
    return surface.elements.Text({children:'ready'});
  }`;
  const mounted=await request(1,{kind:'ui.client.operation',operation:{type:'mount',...mountOptions({runtimeId:'listener-owner',source})}});
  for(const [index,type] of ['pointer','key'].entries()) {
    const actionId=`listener-${type}`, requestId=index+2;
    const outcome=await request(requestId,{kind:'ui.client.operation',operation:{type,runtimeId:'listener-owner',
      generation:mounted.result.generation,payload:{value:type},postActionId:actionId}});
    assert.deepEqual(outcome.result,{accepted:true,postIds:[0,1]});
    const posts=events.filter(event=>event.action==='post'&&event.postActionId===actionId);
    assert.deepEqual(posts.map(event=>event.postId),[0,1]);
    assert.deepEqual(posts.map(event=>JSON.parse(event.json).kind),[`${type}-before`,`${type}-after`]);
    const frame=await request(index+10,{kind:'ui.client.operation',operation:{type:'render',runtimeId:'listener-owner',
      generation:mounted.result.generation,postActionId:actionId}});
    assert.equal(frame.result.tree.type,'Text');
    assert.equal((await request(requestId,{kind:'ui.client.finalize',postActionId:actionId})).kind,'ui.client.finalized');
  }
});

test('Worker reports an invoked callback rejection after invalidating its VM', async t => {
  const workerPath=fileURLToPath(new URL('./mods_worker.mjs',import.meta.url));
  const worker=spawn(process.execPath,['--experimental-vm-modules',workerPath],{stdio:['pipe','pipe','pipe']});
  t.after(()=>{if(!worker.killed)worker.kill();});
  const pending=new Map(),events=[];
  createInterface({input:worker.stdout,crlfDelay:Infinity}).on('line',line=>{
    const frame=decodeModsIpcFrame(line);
    if(frame.id===0)events.push(frame);else{pending.get(frame.id)?.(frame);pending.delete(frame.id);}
  });
  const request=(id,message)=>new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>{pending.delete(id);reject(new Error(`request ${id} timed out`));},2_000);
    pending.set(id,frame=>{clearTimeout(timer);resolve(frame);});
    worker.stdin.write(`${encodeModsIpcFrame({id,...message})}\n`);
  });
  const mounted=await request(1,{kind:'ui.client.operation',operation:{type:'mount',...mountOptions({
    runtimeId:'rejected-action',source:`export function Board(_props,surface){return surface.elements.Button({
      onPress:async()=>{surface.post({stage:'before-reject'});await Promise.resolve();throw new Error('callback rejected');}
    });}`,
  })}});
  const result=await request(2,{kind:'ui.client.operation',operation:{type:'runHeld',runtimeId:'rejected-action',
    generation:mounted.result.generation,handle:mounted.result.tree.held,payload:{kind:'press'},postActionId:'reject-action'}});
  assert(events.some(event=>event.action==='post' && event.postActionId==='reject-action'
    && JSON.parse(event.json).stage==='before-reject'));
  assert.equal(result.error.outcomeUnknown,true);
  assert.match(result.error.reason,/callback rejected/);
  assert(events.some(event=>event.action==='fault'));
});

test('background queue overflow faults only its shared VM and drops its queued events', async t => {
  const workerPath=fileURLToPath(new URL('./mods_worker.mjs',import.meta.url));
  const worker=spawn(process.execPath,['--experimental-vm-modules',workerPath],{stdio:['pipe','pipe','pipe']});
  t.after(()=>{if(!worker.killed)worker.kill();});
  const pending=new Map(), events=[];
  createInterface({input:worker.stdout,crlfDelay:Infinity}).on('line',line=>{
    const frame=decodeModsIpcFrame(line);
    if(frame.id===0) events.push(frame);
    else {pending.get(frame.id)?.(frame);pending.delete(frame.id);}
  });
  const request=(id,message)=>new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>{pending.delete(id);reject(new Error(`request ${id} timed out`));},2_000);
    pending.set(id,frame=>{clearTimeout(timer);resolve(frame);});
    worker.stdin.write(`${encodeModsIpcFrame({id,...message})}\n`);
  });
  const source=`export function Board(_props,surface){return surface.elements.Button({onPress:()=>{
    const stop=surface.every(5,()=>{stop();for(let i=0;i<129;i++)surface.post({i});});
  }});}`;
  const mount=(id,runtimeId)=>request(id,{kind:'ui.client.operation',operation:{type:'mount',...mountOptions({runtimeId,source})}});
  const first=await mount(1,'overflow-owner');
  const sibling=await mount(2,'overflow-sibling');
  const otherOptions=mountOptions({runtimeId:'overflow-other',source});otherOptions.plugin='other';
  const other=await request(3,{kind:'ui.client.operation',operation:{type:'mount',...otherOptions}});
  assert.deepEqual((await request(4,{kind:'ui.client.operation',operation:{type:'runHeld',runtimeId:'overflow-owner',
    generation:first.result.generation,handle:first.result.tree.held,payload:{kind:'press'},postActionId:'overflow'}})).result,
    {accepted:true,postIds:[]});
  for(let i=0;i<100 && events.filter(event=>event.action==='fault').length<2;i++)await delay(5);
  assert.equal(events.filter(event=>event.action==='fault').length,2);
  assert(events.filter(event=>event.action==='fault').every(event =>
    event.environmentInstanceId === first.result.environmentInstanceId),
    'each shared fault identifies the exact Worker VM');
  assert.equal(events.some(event=>event.action==='post'),false,'overflow never silently releases a partial queue');
  const stale=await request(5,{kind:'ui.client.operation',operation:{type:'render',runtimeId:'overflow-sibling',generation:sibling.result.generation}});
  assert.deepEqual(stale.result,{stale:true});
  const intact=await request(6,{kind:'ui.client.operation',operation:{type:'render',runtimeId:'overflow-other',generation:other.result.generation}});
  assert.equal(intact.result.tree.type,'Button');
  const fresh=await mount(7,'overflow-fresh');
  assert.notEqual(fresh.result.environmentInstanceId,first.result.environmentInstanceId);
});

async function backgroundOwnerEnds(t, mode) {
  const workerPath=fileURLToPath(new URL('./mods_worker.mjs',import.meta.url));
  const worker=spawn(process.execPath,['--experimental-vm-modules',workerPath],{stdio:['pipe','pipe','pipe']});
  t.after(()=>{if(!worker.killed)worker.kill();});
  const pending=new Map(),events=[];
  createInterface({input:worker.stdout,crlfDelay:Infinity}).on('line',line=>{
    const frame=decodeModsIpcFrame(line);
    if(frame.id===0)events.push(frame);
    else {pending.get(frame.id)?.(frame);pending.delete(frame.id);}
  });
  const request=(id,message)=>new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>{pending.delete(id);reject(new Error(`request ${id} timed out`));},2_000);
    pending.set(id,frame=>{clearTimeout(timer);resolve(frame);});
    worker.stdin.write(`${encodeModsIpcFrame({id,...message})}\n`);
  });
  const source=`let started=false;export function Board(props,surface){
    if(props.start&&!started){started=true;const stop=surface.every(5,()=>{stop();surface.post({stage:'background'});});}
    return surface.elements.Button({onPress(){}});
  }`;
  const mount=(id,runtimeId)=>request(id,{kind:'ui.client.operation',operation:{type:'mount',...mountOptions({runtimeId,source})}});
  const owner=await mount(1,'background-ending-owner');
  const sibling=await mount(2,'background-ending-sibling');
  await request(8,{kind:'ui.client.operation',operation:{type:'setProps',
    runtimeId:'background-ending-owner',generation:owner.result.generation,props:{start:true}}});
  for(let i=0;i<100 && !events.some(event=>event.action==='post');i++)await delay(5);
  const post=events.find(event=>event.action==='post');
  assert(post?.backgroundEventId);
  if(mode==='reject') {
    worker.stdin.write(`${encodeModsIpcFrame({id:0,kind:'ui.client.backgroundComplete',backgroundEventId:post.backgroundEventId,accepted:false})}\n`);
  } else if(mode==='unmount') {
    const removed=await request(3,{kind:'ui.client.operation',operation:{type:'unmount',
      runtimeId:'background-ending-owner',generation:owner.result.generation}});
    assert.equal(removed.result.unmounted,true);
  }
  const budget=mode==='timeout'?32_000:2_000, start=Date.now();
  while(events.filter(event=>event.action==='fault').length<2 && Date.now()-start<budget)await delay(10);
  assert.equal(events.filter(event=>event.action==='fault').length,2);
  const stale=await request(4,{kind:'ui.client.operation',operation:{type:'render',runtimeId:'background-ending-sibling',generation:sibling.result.generation}});
  assert.deepEqual(stale.result,{stale:true});
  const fresh=await mount(5,'background-ending-fresh');
  assert.notEqual(fresh.result.environmentInstanceId,owner.result.environmentInstanceId);
  worker.stdin.write(`${encodeModsIpcFrame({id:0,kind:'ui.client.backgroundComplete',backgroundEventId:post.backgroundEventId,accepted:false})}\n`);
  const intact=await request(6,{kind:'ui.client.operation',operation:{type:'render',runtimeId:'background-ending-fresh',generation:fresh.result.generation}});
  assert.equal(intact.result.tree.type,'Button','a late acknowledgement cannot fence a newly-created VM');
  assert.equal(events.filter(event=>event.action==='post').length,1,'an uncertain event is never automatically replayed');
}

test('rejected background admission fences its shared VM', async t => backgroundOwnerEnds(t,'reject'));
test('unmount during a background event fences its shared VM', async t => backgroundOwnerEnds(t,'unmount'));
test('unacknowledged background event expires and fences its shared VM',
  {timeout:36_000}, async t => backgroundOwnerEnds(t,'timeout'));

async function timerFaultFencesSharedEnvironment(t, asynchronous) {
  const workerPath=fileURLToPath(new URL('./mods_worker.mjs',import.meta.url));
  const worker=spawn(process.execPath,['--experimental-vm-modules',workerPath],{stdio:['pipe','pipe','pipe']});
  t.after(()=>{if(!worker.killed)worker.kill();});
  const pending=new Map(),events=[];
  createInterface({input:worker.stdout,crlfDelay:Infinity}).on('line',line=>{
    const frame=decodeModsIpcFrame(line);
    if(frame.id===0)events.push(frame);else{pending.get(frame.id)?.(frame);pending.delete(frame.id);}
  });
  const request=(id,message)=>new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>{pending.delete(id);reject(new Error('request timeout'));},2_000);
    pending.set(id,frame=>{clearTimeout(timer);resolve(frame);});
    worker.stdin.write(`${encodeModsIpcFrame({id,...message})}\n`);
  });
  const callback=asynchronous
    ? "async()=>{stop();shared='poison';await Promise.resolve();throw new Error('timer fault');}"
    : "()=>{stop();shared='poison';throw new Error('timer fault');}";
  const source=`let shared='clean',started=false;export function Board(props,surface){
    if(props.start&&!started){started=true;const stop=surface.every(5,${callback});}
    return surface.elements.Text({children:shared});}`;
  const mount=(id,runtimeId)=>request(id,{kind:'ui.client.operation',operation:{type:'mount',...mountOptions({runtimeId,source})}});
  const owner=await mount(1,'timer-fault-owner'),sibling=await mount(2,'timer-fault-sibling');
  assert.equal(owner.result.environmentInstanceId,sibling.result.environmentInstanceId);
  await request(3,{kind:'ui.client.operation',operation:{type:'setProps',runtimeId:'timer-fault-owner',
    generation:owner.result.generation,props:{start:true}}});
  for(let i=0;i<100 && events.filter(event=>event.action==='fault').length<2;i++)await delay(5);
  assert.equal(events.filter(event=>event.action==='fault').length,2);
  const old=await request(4,{kind:'ui.client.operation',operation:{type:'render',runtimeId:'timer-fault-sibling',generation:sibling.result.generation}});
  assert.deepEqual(old.result,{stale:true},'the sibling cannot observe a partially-mutated shared module');
  const fresh=await mount(5,'timer-fault-fresh');
  assert.notEqual(fresh.result.environmentInstanceId,owner.result.environmentInstanceId);
  assert.deepEqual(fresh.result.tree.children,['clean']);
}

test('synchronous timer failure fences every surface in its VM', async t => timerFaultFencesSharedEnvironment(t,false));
test('async timer rejection fences every surface in its VM', async t => timerFaultFencesSharedEnvironment(t,true));

test('render failure fences every surface that shares mutated module state', async t => {
  const workerPath=fileURLToPath(new URL('./mods_worker.mjs',import.meta.url));
  const worker=spawn(process.execPath,['--experimental-vm-modules',workerPath],{stdio:['pipe','pipe','pipe']});
  t.after(()=>{if(!worker.killed)worker.kill();});
  const pending=new Map(),events=[];
  createInterface({input:worker.stdout,crlfDelay:Infinity}).on('line',line=>{
    const frame=decodeModsIpcFrame(line);
    if(frame.id===0)events.push(frame);else{pending.get(frame.id)?.(frame);pending.delete(frame.id);}
  });
  const request=(id,message)=>new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>{pending.delete(id);reject(new Error('request timeout'));},2_000);
    pending.set(id,frame=>{clearTimeout(timer);resolve(frame);});
    worker.stdin.write(`${encodeModsIpcFrame({id,...message})}\n`);
  });
  const source=`let shared='clean';export function Board(props,surface){
    if(props.fail){shared='poison';throw new Error('render fault');}
    return surface.elements.Text({children:shared});}`;
  const mount=(id,runtimeId)=>request(id,{kind:'ui.client.operation',operation:{type:'mount',...mountOptions({runtimeId,source})}});
  const owner=await mount(1,'render-fault-owner'),sibling=await mount(2,'render-fault-sibling');
  assert.equal(owner.result.environmentInstanceId,sibling.result.environmentInstanceId);
  const failed=await request(3,{kind:'ui.client.operation',operation:{type:'setProps',runtimeId:'render-fault-owner',
    generation:owner.result.generation,props:{fail:true}}});
  assert.equal(failed.error.phase,'render');
  const old=await request(4,{kind:'ui.client.operation',operation:{type:'render',runtimeId:'render-fault-sibling',generation:sibling.result.generation}});
  assert.deepEqual(old.result,{stale:true},'the sibling cannot render partially-mutated module state');
  assert.equal(events.filter(event=>event.action==='fault').length,2);
  const refused=await request(5,{kind:'ui.client.operation',operation:{type:'runHeld',runtimeId:'render-fault-sibling',
    generation:sibling.result.generation,handle:1,postActionId:'fenced-action'}});
  assert.equal(refused.error?.notStartedUnavailable,true,
    'a fenced callback is refused before execution with an explicit unavailable reason');
  const fresh=await mount(6,'render-fault-fresh');
  assert.notEqual(fresh.result.environmentInstanceId,owner.result.environmentInstanceId);
  assert.deepEqual(fresh.result.tree.children,['clean']);
});

test('module load failure fences every surface that shares mutated module state', async t => {
  const workerPath=fileURLToPath(new URL('./mods_worker.mjs',import.meta.url));
  const worker=spawn(process.execPath,['--experimental-vm-modules',workerPath],{stdio:['pipe','pipe','pipe']});
  t.after(()=>{if(!worker.killed)worker.kill();});
  const pending=new Map(),events=[];
  createInterface({input:worker.stdout,crlfDelay:Infinity}).on('line',line=>{
    const frame=decodeModsIpcFrame(line);
    if(frame.id===0)events.push(frame);else{pending.get(frame.id)?.(frame);pending.delete(frame.id);}
  });
  const request=(id,message)=>new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>{pending.delete(id);reject(new Error('request timeout'));},2_000);
    pending.set(id,frame=>{clearTimeout(timer);resolve(frame);});
    worker.stdin.write(`${encodeModsIpcFrame({id,...message})}\n`);
  });
  const source=`import {shared} from './shared.mjs';export function Board(_props,surface){
    return surface.elements.Text({children:shared.value});}`;
  const shared=`export const shared={value:'clean'};`;
  const bad=`import {shared} from './shared.mjs';shared.value='poison';throw new Error('load fault');
    export function Broken(){}`;
  const options=runtimeId=>{
    const input=mountOptions({runtimeId,source});
    input.moduleGraph.modules.push({module:'shared.mjs',source:shared},{module:'bad.mjs',component:'Broken',source:bad});
    input.moduleGraph.manifest.modules.push({module:'bad.mjs',entry:'bad.mjs',component:'Broken'});
    input.moduleGraph.manifest.files.push({key:'shared.mjs',source:shared},{key:'bad.mjs',source:bad});
    return input;
  };
  const healthy=await request(1,{kind:'ui.client.operation',operation:{type:'mount',...options('load-fault-sibling')}});
  const failing=options('load-fault-owner');failing.client={key:'bad',module:'bad.mjs'};
  const failed=await request(2,{kind:'ui.client.operation',operation:{type:'mount',...failing}});
  assert.equal(failed.error.phase,'load');
  const old=await request(3,{kind:'ui.client.operation',operation:{type:'render',runtimeId:'load-fault-sibling',generation:healthy.result.generation}});
  assert.deepEqual(old.result,{stale:true},'the sibling cannot render partially-mutated module state');
  assert.equal(events.filter(event=>event.action==='fault').length,2);
  const fresh=await request(4,{kind:'ui.client.operation',operation:{type:'mount',...options('load-fault-fresh')}});
  assert.notEqual(fresh.result.environmentInstanceId,healthy.result.environmentInstanceId);
  assert.deepEqual(fresh.result.tree.children,['clean']);
});

test('unmount interrupts a suspended held callback with an unknown result', async t => {
  const workerPath=fileURLToPath(new URL('./mods_worker.mjs',import.meta.url));
  const worker=spawn(process.execPath,['--experimental-vm-modules',workerPath],{stdio:['pipe','pipe','pipe']});
  t.after(()=>{if(!worker.killed) worker.kill();});
  const pending=new Map(); let postResolve;
  const posted=new Promise(resolve=>{postResolve=resolve;});
  createInterface({input:worker.stdout,crlfDelay:Infinity}).on('line',line=>{
    const frame=decodeModsIpcFrame(line);
    acknowledgeBackground(worker, frame);
    if(frame.id===0) { if(frame.action==='post') postResolve(); }
    else {pending.get(frame.id)?.(frame);pending.delete(frame.id);}
  });
  const request=(id,message)=>new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>{pending.delete(id);reject(new Error(`request ${id} timed out`));},2_000);
    pending.set(id,frame=>{clearTimeout(timer);resolve(frame);});
    worker.stdin.write(`${encodeModsIpcFrame({id,...message})}\n`);
  });
  const mounted=await request(1,{kind:'ui.client.operation',operation:{type:'mount',...mountOptions({
    runtimeId:'suspended-unmount',source:`export function Board(_props,surface){
      return surface.elements.Button({onPress:async ()=>{surface.post({started:true});await new Promise(()=>{});}});
    }`,
  })}});
  const held=request(2,{kind:'ui.client.operation',operation:{type:'runHeld',runtimeId:'suspended-unmount',
    generation:mounted.result.generation,handle:mounted.result.tree.held,payload:{kind:'press'},postActionId:'pending'}});
  await posted;
  const unmounted=await request(3,{kind:'ui.client.operation',operation:{type:'unmount',
    runtimeId:'suspended-unmount',generation:mounted.result.generation}});
  assert.deepEqual(unmounted.result,{unmounted:true});
  const result=await held;
  assert.equal(result.error.phase,'run');
  assert.equal(result.error.outcomeUnknown,true,'an invoked callback is never reported as a safe no-op');
});

test('a lost finalize acknowledgement revokes only the identified VM', async t => {
  const workerPath = fileURLToPath(new URL('./mods_worker.mjs', import.meta.url));
  const worker = spawn(process.execPath, ['--experimental-vm-modules', workerPath], {stdio:['pipe','pipe','pipe']});
  t.after(() => { if (!worker.killed) worker.kill(); });
  const pending = new Map();
  const events = [];
  createInterface({input:worker.stdout, crlfDelay:Infinity}).on('line', line => {
    const frame = decodeModsIpcFrame(line);
    acknowledgeBackground(worker, frame);
    if (frame.id === 0) events.push(frame);
    else { pending.get(frame.id)?.(frame); pending.delete(frame.id); }
  });
  const request = (id, message) => new Promise((resolve, reject) => {
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`request ${id} timed out`)); }, 2_000);
    pending.set(id, frame => { clearTimeout(timer); resolve(frame); });
    worker.stdin.write(`${encodeModsIpcFrame({id, ...message})}\n`);
  });
  const source = `let hidden=''; export function Board(_props, surface) {
    return surface.elements.Input({value:hidden, onSubmit(){}, onInput:value => {
      hidden=value; surface.every(5, () => {hidden='old timer';});
    }});
  }`;
  const mount = (id, runtimeId) => request(id, {kind:'ui.client.operation', operation:{type:'mount',
    ...mountOptions({runtimeId, source})}});
  const first = await mount(1, 'lost-ack-owner');
  const sibling = await mount(2, 'lost-ack-sibling');
  assert.equal(first.result.environmentInstanceId, sibling.result.environmentInstanceId);
  assert.deepEqual((await request(3, {kind:'ui.client.operation', operation:{type:'runHeld',
    runtimeId:'lost-ack-owner', generation:first.result.generation, handle:first.result.tree.held,
    payload:{kind:'change', value:'effect'}, postActionId:'lost-ack-action'}})).result,
    {accepted:true, postIds:[]});
  // The Worker has sent this acknowledgement, but the Host may be dropped
  // before it consumes it and therefore sends cancellation for the old VM.
  assert.equal((await request(3, {kind:'ui.client.finalize', postActionId:'lost-ack-action'})).kind,
    'ui.client.finalized');
  const oldEnvironment = first.result.environmentInstanceId;
  worker.stdin.write(`${encodeModsIpcFrame({id:3, kind:'cancel', clientEnvironmentInstanceId:oldEnvironment})}\n`);
  for (let i=0; i<100 && events.filter(event => event.action === 'fault').length<2; i+=1) await delay(5);
  assert.equal(events.filter(event => event.action === 'fault').length, 2);
  const oldRender = await request(4, {kind:'ui.client.operation', operation:{type:'render',
    runtimeId:'lost-ack-sibling', generation:sibling.result.generation}});
  assert.deepEqual(oldRender.result, {stale:true});
  const fresh = await mount(5, 'lost-ack-fresh');
  assert.notEqual(fresh.result.environmentInstanceId, oldEnvironment);
  assert.equal(fresh.result.tree.props.value, '');
  // Replaying a delayed cancel for the old UUID must not affect the new VM.
  worker.stdin.write(`${encodeModsIpcFrame({id:3, kind:'cancel', clientEnvironmentInstanceId:oldEnvironment})}\n`);
  await delay(30);
  const freshRender = await request(6, {kind:'ui.client.operation', operation:{type:'render',
    runtimeId:'lost-ack-fresh', generation:fresh.result.generation}});
  assert.equal(freshRender.result.tree.props.value, '');
});

test('an unfinalized held action expires and quarantines its VM',
  {skip:Boolean(process.versions.bun), timeout:36_000}, async t => {
  const workerPath=fileURLToPath(new URL('./mods_worker.mjs',import.meta.url));
  const worker=spawn(process.execPath,['--experimental-vm-modules',workerPath],{stdio:['pipe','pipe','pipe']});
  t.after(()=>{if(!worker.killed) worker.kill();});
  const pending=new Map();
  const events=[]; let faultResolve;
  const fault=new Promise(resolve=>{faultResolve=resolve;});
  createInterface({input:worker.stdout,crlfDelay:Infinity}).on('line',line=>{
    const frame=decodeModsIpcFrame(line);
    acknowledgeBackground(worker, frame);
    if(frame.id===0) {events.push(frame);if(frame.action==='fault') faultResolve();}
    else {pending.get(frame.id)?.(frame);pending.delete(frame.id);}
  });
  const request=(id,message)=>new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>{pending.delete(id);reject(new Error(`request ${id} timed out`));},34_000);
    pending.set(id,frame=>{clearTimeout(timer);resolve(frame);});
    worker.stdin.write(`${encodeModsIpcFrame({id,...message})}\n`);
  });
  const mounted=await request(1,{kind:'ui.client.operation',operation:{type:'mount',...mountOptions({
    runtimeId:'lease-expiry',source:`export function Board(_props,surface){
      return surface.elements.Button({onPress(){}});
    }`,
  })}});
  const result=await request(2,{kind:'ui.client.operation',operation:{type:'runHeld',runtimeId:'lease-expiry',
    generation:mounted.result.generation,handle:mounted.result.tree.held,payload:{kind:'press'},postActionId:'never-finalized'}});
  assert.deepEqual(result.result,{accepted:true,postIds:[]});
  const start=Date.now();
  const busy=await request(3,{kind:'ui.client.operation',operation:{type:'render',runtimeId:'lease-expiry',generation:mounted.result.generation}});
  assert.equal(busy.error.busy,true);
  await fault;
  assert(Date.now()-start >= 29_000,'the real lease deadline, not an immediate no-op, quarantined the VM');
  const stale=await request(4,{kind:'ui.client.operation',operation:{type:'render',runtimeId:'lease-expiry',generation:mounted.result.generation}});
  assert.deepEqual(stale.result,{stale:true});
  assert.equal(events.filter(event=>event.action==='fault').length,1);
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
    const frame = decodeModsIpcFrame(line);
    acknowledgeBackground(worker, frame);
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
    worker.stdin.write(`${encodeModsIpcFrame({ id, ...message })}\n`);
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
  for (let i=0; i<100 && !frames.some(frame => frame.kind === 'ui.client.event'
    && frame.runtimeId === 'worker-drawing' && frame.action === 'post'
    && frame.json === '{"kind":"worker-mounted"}'); i+=1) await delay(5);
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
  const clientCoreEvents = [];
  const lines = createInterface({ input: worker.stdout, crlfDelay: Infinity });
  lines.on('line', line => {
    const frame = decodeModsIpcFrame(line);
    acknowledgeBackground(worker, frame);
    if (frame.kind === 'api.callers') {
      worker.stdin.write(`${encodeModsIpcFrame({ id: frame.id, kind: 'api.callers.result', callId: frame.callId })}\n`);
      return;
    }
    if (frame.kind === 'next') {
      clientCoreEvents.push(frame.input);
      const result = { handled: true, element: frame.input.element };
      if (Object.hasOwn(frame.input, 'value')) result.value = frame.input.value;
      worker.stdin.write(`${encodeModsIpcFrame({
        id: frame.id, kind: 'next.result', callId: frame.callId, result,
      })}\n`);
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
    worker.stdin.write(`${encodeModsIpcFrame({ id, ...message })}\n`);
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

  const clientEvent = (element, values = {}) => ({
    plugin: 'desktop-plugin', element, component: 'Pane', requestId: 'client-render-1',
    surface: 'desktop', ...values,
  });
  const clientContext = (event, input) => ({
    kind: 'client-ui-action', plugin: input.plugin, clientKey: 'card',
    component: input.component, requestId: input.requestId, surface: input.surface,
    element: input.element, event,
  });
  const clientPressInput = clientEvent('client-button');
  const clientPressed = await request(30, {
    kind: 'dispatch', event: 'ui.press', pluginScope: 'desktop-plugin', input: clientPressInput,
    pressToken: clientContext('ui.press', clientPressInput),
  });
  assert.equal(clientPressed.kind, 'result');
  assert.deepEqual(clientPressed.result, { handled: true, element: 'client-button' },
    'a host-admitted Client event returns its child key without looking up a Parent held action');

  const emptyClientKey = await request(38, {
    kind: 'dispatch', event: 'ui.press', pluginScope: 'desktop-plugin', input: clientPressInput,
    pressToken: { ...clientContext('ui.press', clientPressInput), clientKey: '' },
  });
  assert.equal(emptyClientKey.kind, 'result');
  assert.deepEqual(emptyClientKey.result, { handled: true, element: 'client-button' },
    'the Host identity allows an empty Client root key');

  const clientInput = clientEvent('client-query', { kind: 'change', value: ' typed ' });
  const clientChanged = await request(31, {
    kind: 'dispatch', event: 'ui.input', pluginScope: 'desktop-plugin', input: clientInput,
    pressToken: clientContext('ui.input', clientInput),
  });
  assert.equal(clientChanged.kind, 'result');
  assert.deepEqual(clientChanged.result, { handled: true, element: 'client-query', value: 'typed' });

  const clientSelect = clientEvent('client-mode', { value: 'client-option' });
  const clientSelected = await request(32, {
    kind: 'dispatch', event: 'ui.select', pluginScope: 'desktop-plugin', input: clientSelect,
    pressToken: clientContext('ui.select', clientSelect),
  });
  assert.equal(clientSelected.kind, 'result');
  assert.deepEqual(clientSelected.result, {
    handled: true, element: 'client-mode', value: 'client-option',
  });

  const wrongClientContext = await request(33, {
    kind: 'dispatch', event: 'ui.input', pluginScope: 'desktop-plugin', input: clientInput,
    pressToken: { ...clientContext('ui.input', clientInput), element: 'different-child' },
  });
  assert.equal(wrongClientContext.kind, 'error');
  assert.match(wrongClientContext.message, /ui\.input has no valid private action token/);

  const wrongClientScope = await request(37, {
    kind: 'dispatch', event: 'ui.press', pluginScope: 'plugin-a', input: clientPressInput,
    pressToken: clientContext('ui.press', clientPressInput),
  });
  assert.equal(wrongClientScope.kind, 'error');
  assert.match(wrongClientScope.message, /ui\.press has no valid private action token/);

  for (const [requestId, event, input] of [
    [34, 'ui.press', clientPressInput],
    [35, 'ui.input', clientInput],
    [36, 'ui.select', clientSelect],
  ]) {
    const missingContext = await request(requestId, {
      kind: 'dispatch', event, pluginScope: 'desktop-plugin', input,
    });
    assert.equal(missingContext.kind, 'error', `${event} without Host context is rejected`);
    assert.ok(missingContext.message.includes(`${event} has no valid private action token`));
  }
  assert.deepEqual(clientCoreEvents.map(({ element, value }) => ({ element, value })), [
    { element: 'client-button', value: undefined },
    { element: 'client-button', value: undefined },
    { element: 'client-query', value: 'typed' },
    { element: 'client-mode', value: 'client-option' },
  ], 'Client events reach the Host core with the child key and the rewritten value');

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
    const frame = decodeModsIpcFrame(line);
    acknowledgeBackground(worker, frame);
    if (frame.kind === 'api.callers') {
      worker.stdin.write(`${encodeModsIpcFrame({ id: frame.id, kind: 'api.callers.result', callId: frame.callId })}\n`);
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
    worker.stdin.write(`${encodeModsIpcFrame({ id, ...message })}\n`);
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
    const frame = decodeModsIpcFrame(line);
    acknowledgeBackground(worker, frame);
    if (frame.kind === 'api.callers') {
      worker.stdin.write(`${encodeModsIpcFrame({ id: frame.id, kind: 'api.callers.result', callId: frame.callId })}\n`);
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
    worker.stdin.write(`${encodeModsIpcFrame({ id, ...message })}\n`);
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
    const frame = decodeModsIpcFrame(line);
    acknowledgeBackground(worker, frame);
    if (frame.kind === 'api.callers') {
      worker.stdin.write(`${encodeModsIpcFrame({ id: frame.id, kind: 'api.callers.result', callId: frame.callId })}\n`);
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
    worker.stdin.write(`${encodeModsIpcFrame({ id, ...message })}\n`);
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
    const frame = decodeModsIpcFrame(line);
    acknowledgeBackground(worker, frame);
    frames.push(frame);
    if (frame.kind === 'api.callers') {
      worker.stdin.write(`${encodeModsIpcFrame({ id: frame.id, kind: 'api.callers.result', callId: frame.callId })}\n`);
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
    worker.stdin.write(`${encodeModsIpcFrame({ id, ...message })}\n`);
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

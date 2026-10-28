import { createHash, randomUUID } from 'node:crypto';
import { AsyncLocalStorage } from 'node:async_hooks';
import path from 'node:path';
import { createContext, Script, SourceTextModule } from 'node:vm';

const ELEMENTS = Object.freeze([
  'Box', 'Text', 'Button', 'Input', 'Select', 'Link', 'Code', 'Markdown',
]);
const MAX_SOURCE_FILES = 256;
const MAX_SOURCE_BYTES = 8 * 1024 * 1024;
const MAX_TREE_NODES = 20_000;
const MAX_TREE_DEPTH = 32;
const MAX_TREE_CHARS = 100_000;
const MAX_DATA_VALUES = 20_000;
const MAX_DATA_DEPTH = 32;
const LOAD_BUDGET_MS = 10_000;
const RUN_BUDGET_MS = 1_000;
// Must remain below the Host worker-response watchdog.
const ASYNC_HELD_BUDGET_MS = 10_000;

const CREATE_SURFACE = new Script(`
  (bridge, dimensions) => {
    let state = Object.create(null);
    let props = Object.create(null);
    let columns = dimensions.columns;
    let rows = dimensions.rows;
    let pending = false;
    let mounted = true;
    let pointerListener;
    let keyListener;

    const schedule = () => {
      if (!mounted || pending) return;
      pending = true;
      bridge.schedule();
    };
    const make = (type, value, ...children) => {
      const elementProps = value == null ? {} : value;
      const childList = children.length !== 0
        ? children
        : elementProps.children === undefined
          ? []
          : Array.isArray(elementProps.children)
            ? elementProps.children
            : [elementProps.children];
      const outputProps = Object.create(null);
      for (const key of Object.keys(elementProps)) if (key !== 'children') outputProps[key] = elementProps[key];
      return { type, props: outputProps, children: childList };
    };
    const elements = Object.create(null);
    for (const type of ${JSON.stringify(ELEMENTS)}) {
      elements[type] = (value, ...children) => make(type, value, ...children);
    }
    Object.freeze(elements);

    const surface = {
      elements,
      setState(next) {
        if (!mounted || !bridge.allowed()) return;
        const value = typeof next === 'function' ? next(state) : next;
        if (value == null || typeof value !== 'object' || Array.isArray(value)) {
          throw new TypeError('surface.setState expects an object or updater');
        }
        state = Object.assign(Object.create(null), state, value);
        schedule();
      },
      every(milliseconds, callback) {
        if (!Number.isFinite(milliseconds) || milliseconds < 0 || typeof callback !== 'function') {
          throw new TypeError('surface.every expects a non-negative interval and callback');
        }
        if (!mounted || !bridge.allowed()) return () => {};
        return bridge.every(milliseconds, callback);
      },
      onPointer(callback) {
        if (typeof callback !== 'function') throw new TypeError('surface.onPointer expects a callback');
        if (!mounted || !bridge.allowed()) return;
        pointerListener = callback;
      },
      onKey(callback) {
        if (typeof callback !== 'function') throw new TypeError('surface.onKey expects a callback');
        if (!mounted || !bridge.allowed()) return;
        keyListener = callback;
      },
      post(data) { if (mounted && bridge.allowed()) bridge.post(data); },
    };
    Object.defineProperties(surface, {
      state: { enumerable: true, get: () => state },
      columns: { enumerable: true, get: () => columns },
      rows: { enumerable: true, get: () => rows },
    });

    return {
      surface,
      render(fn, nextProps) {
        if (!mounted) return undefined;
        props = nextProps;
        pending = false;
        return fn(props, surface);
      },
      setProps(nextProps) { if (mounted) props = nextProps; },
      resize(nextColumns, nextRows) {
        if (mounted) { columns = nextColumns; rows = nextRows; }
      },
      listener(kind) { return kind === 'pointer' ? pointerListener : keyListener; },
      unmount() {
        mounted = false;
        pending = false;
        state = Object.create(null);
        props = Object.create(null);
        pointerListener = undefined;
        keyListener = undefined;
      },
    };
  }
`);

const SERIALIZE_TREE = new Script(`
  (tree, bridge) => {
    let nodeCount = 0;
    const allowed = new Set(${JSON.stringify(ELEMENTS)});
    const fail = message => { throw new TypeError(message); };
    const copyJson = (value, depth = 0, counter = { values: 0 }, omittedKeys = null) => {
      counter.values += 1;
      if (counter.values > ${MAX_DATA_VALUES}) fail('props of more than ' + ${MAX_DATA_VALUES} + ' values');
      if (value === null) return null;
      const kind = typeof value;
      if (kind === 'string' || kind === 'boolean') return value;
      if (kind === 'number') return Number.isFinite(value) ? value : null;
      if (kind !== 'object') return undefined;
      if (depth > ${MAX_DATA_DEPTH}) fail('props nesting deeper than ' + ${MAX_DATA_DEPTH});
      if (Array.isArray(value)) {
        if (value.length > ${MAX_DATA_VALUES} - counter.values) {
          fail('props of more than ' + ${MAX_DATA_VALUES} + ' values');
        }
        const result = [];
        for (let index = 0; index < value.length; index += 1) {
          if (!(index in value)) fail('props with a hole in an array');
          const child = copyJson(value[index], depth + 1, counter);
          result.push(child === undefined ? null : child);
        }
        return result;
      }
      const result = {};
      for (const key of Object.keys(value)) {
        if (depth === 0 && omittedKeys?.includes(key)) continue;
        const child = copyJson(value[key], depth + 1, counter);
        if (child !== undefined) {
          Object.defineProperty(result, key, {
            value: child, enumerable: true, writable: true, configurable: true,
          });
        }
      }
      return result;
    };
    const copyNode = (value, depth) => {
      if (typeof value === 'string') return value;
      if (typeof value !== 'object' || value === null || Array.isArray(value)) {
        fail('a child that is neither a string nor an element');
      }
      nodeCount += 1;
      if (nodeCount > ${MAX_TREE_NODES}) fail('surface tree exceeds its ${MAX_TREE_NODES} node limit');
      if (depth > ${MAX_TREE_DEPTH}) fail('surface tree exceeds its ${MAX_TREE_DEPTH} level limit');
      const type = value.type;
      if (type === 'Fragment') {
        const children = value.children;
        if (!Array.isArray(children)) fail('surface Fragment children must be an array');
        const flattened = [];
        for (let index = 0; index < children.length; index += 1) {
          if (!Object.hasOwn(children, index)) fail('surface tree contains a sparse children array');
          const child = children[index];
          if (typeof child === 'string') flattened.push(child);
          else {
            const copied = copyNode(child, depth + 1);
            flattened.push(...(Array.isArray(copied) ? copied : [copied]));
          }
        }
        return flattened;
      }
      if (typeof type !== 'string' || !allowed.has(type)) fail('surface tree contains an unsupported element');
      const result = Object.create(null);
      result.type = type;
      const props = value.props;
      let callbackKeys = [];
      let callback;
      let markdownChildren;
      let markdownChildrenRead = false;
      if (type === 'Button') {
        callbackKeys = ['onPress'];
        if (props !== undefined && (props === null || typeof props !== 'object' || Array.isArray(props))) {
          fail('Button props must be a plain object');
        }
        callback = props?.onPress;
      } else if (type === 'Select') {
        if (props === undefined) fail('Select requires onSelect');
        if (props === null || typeof props !== 'object' || Array.isArray(props)) {
          fail('Select props must be a plain object');
        }
        if (Object.hasOwn(props, 'onEvent')) fail('Select does not accept onEvent');
        const children = value.children;
        if (children !== undefined && (!Array.isArray(children) || children.length > 0)) {
          fail('Select does not accept children');
        }
        const onSelect = props.onSelect;
        if (typeof onSelect !== 'function') fail('Select requires an onSelect callback');
        callbackKeys = ['onSelect'];
        callback = event => onSelect(event?.value, event);
      } else if (type === 'Input') {
        if (props === undefined) fail('Input requires onSubmit');
        if (props === null || typeof props !== 'object' || Array.isArray(props)) {
          fail('Input props must be a plain object');
        }
        if (Object.hasOwn(props, 'onEvent')) fail('Input does not accept onEvent');
        const children = value.children;
        if (children !== undefined && (!Array.isArray(children) || children.length > 0)) {
          fail('Input does not accept children');
        }
        const onSubmit = props.onSubmit;
        const onInput = props.onInput;
        if (typeof onSubmit !== 'function') fail('Input requires an onSubmit callback');
        if (onInput !== undefined && typeof onInput !== 'function') fail('Input onInput must be a function');
        callbackKeys = ['onSubmit', 'onInput'];
        callback = event => event?.kind === 'submit'
          ? onSubmit(event.value, event)
          : onInput === undefined ? undefined : onInput(event?.value, event);
      } else if (type === 'Markdown') {
        const onLinkPress = props == null ? undefined : props.onLinkPress;
        markdownChildren = value.children;
        markdownChildrenRead = true;
        if (markdownChildren !== undefined && (!Array.isArray(markdownChildren) || markdownChildren.length > 0)) {
          fail('Markdown does not accept children');
        }
        if (onLinkPress !== undefined && typeof onLinkPress !== 'function') {
          fail('Markdown onLinkPress must be a function');
        }
        if (onLinkPress !== undefined) {
          fail('a Markdown with onLinkPress inside the tree; a surface module answers its own pointer with surface.onPointer');
        }
      }
      if (callbackKeys.length > 0 && props === undefined) fail(type + ' requires a callback');
      if (props !== undefined) {
        result.props = copyJson(props, 0, { values: 0 }, callbackKeys);
      }
      if ((type === 'Button' || type === 'Select') && typeof callback !== 'function') {
        fail(type + ' requires its event callback');
      }
      const hover = value.hover;
      if (hover !== undefined) result.hover = copyJson(hover);
      if (callback !== undefined) result.held = bridge.hold(callback);
      if (type === 'Input' || type === 'Select') return result;
      const children = markdownChildrenRead ? markdownChildren : value.children;
      if (children !== undefined) {
        if (!Array.isArray(children)) fail(type + ' children must be an array');
        const cleanChildren = [];
        for (let index = 0; index < children.length; index += 1) {
          const child = children[index];
          if (typeof child === 'string') cleanChildren.push(child);
          else {
            const copied = copyNode(child, depth + 1);
            cleanChildren.push(...(Array.isArray(copied) ? copied : [copied]));
          }
        }
        result.children = cleanChildren;
      }
      return result;
    };
    const copied = copyNode(tree, 0);
    const text = JSON.stringify(copied);
    if (text.length > ${MAX_TREE_CHARS}) fail('surface tree exceeds its ${MAX_TREE_CHARS} serialized character limit');
    return text;
  }
`);

const SERIALIZE_DATA = new Script(`
  value => {
    const fail = message => { throw new TypeError(message); };
    const copy = (item, depth = 0, counter = { values: 0 }) => {
      if (depth > ${MAX_DATA_DEPTH}) fail('plain data exceeds its ${MAX_DATA_DEPTH} level nesting limit');
      counter.values += 1;
      if (counter.values > ${MAX_DATA_VALUES}) fail('plain data exceeds its ${MAX_DATA_VALUES} value limit');
      if (item === null || typeof item === 'string' || typeof item === 'boolean') return item;
      if (typeof item === 'number') return Number.isFinite(item) ? item : null;
      if (item === undefined || typeof item === 'function') return undefined;
      if (typeof item === 'bigint' || typeof item === 'symbol') fail('value is not JSON data');
      if (Array.isArray(item)) {
        const result = [];
        for (let index = 0; index < item.length; index += 1) {
          if (!Object.hasOwn(item, index)) fail('sparse arrays are not plain data');
          const descriptor = Object.getOwnPropertyDescriptor(item, String(index));
          if (!descriptor || !Object.hasOwn(descriptor, 'value')) fail('accessor properties are not plain data');
          const child = copy(descriptor.value, depth + 1, counter);
          result.push(child === undefined ? null : child);
        }
        for (const key of Reflect.ownKeys(item)) {
          if (key !== 'length' && !(typeof key === 'string' && /^(0|[1-9]\\d*)$/.test(key))) {
            fail('arrays with extra properties are not plain data');
          }
        }
        return result;
      }
      if (item === null || typeof item !== 'object') fail('value is not plain JSON data');
      const prototype = Object.getPrototypeOf(item);
      if (prototype !== null && Object.getPrototypeOf(prototype) !== null) fail('value is not plain JSON data');
      const result = Object.create(null);
      for (const key of Reflect.ownKeys(item)) {
        if (typeof key !== 'string') fail('symbol keys are not plain data');
        const descriptor = Object.getOwnPropertyDescriptor(item, key);
        if (!descriptor || !Object.hasOwn(descriptor, 'value')) fail('accessor properties are not plain data');
        const child = copy(descriptor.value, depth + 1, counter);
        if (child !== undefined) result[key] = child;
      }
      return result;
    };
    const copied = copy(value);
    if (copied === undefined) fail('surface.post expects JSON data');
    const text = JSON.stringify(copied);
    if (text.length > ${MAX_TREE_CHARS}) fail('surface.post data exceeds its ${MAX_TREE_CHARS} character limit');
    return text;
  }
`);

const MAKE_BRIDGE = new Script(`
  (host, serializeData) => ({
    schedule: host.schedule,
    every: host.every,
    allowed: host.allowed,
    post(data) { host.post(serializeData(data)); },
  })
`);

function fail(message) {
  throw new TypeError(message);
}

// The Host still owns a longer process watchdog for CPU-bound continuations.
// This timer bounds promises that are pending while the event loop is healthy.
async function awaitHeldCallback(result, signal, budgetMs) {
  let timer;
  let onAbort;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error('Client callback timed out')), budgetMs);
  });
  const cancelled = new Promise((_, reject) => {
    onAbort = () => reject(new Error('Client callback cancelled'));
    if (signal.aborted) onAbort();
    else signal.addEventListener('abort', onAbort, { once: true });
  });
  try {
    await Promise.race([Promise.resolve(result), timeout, cancelled]);
  } finally {
    clearTimeout(timer);
    signal.removeEventListener('abort', onAbort);
  }
}

class ClientSurfaceError extends Error {
  constructor(phase, reason, cause) {
    super(reason, cause === undefined ? undefined : { cause });
    this.name = 'ClientSurfaceError';
    this.phase = phase;
  }
}

function clientError(phase, error) {
  if (error instanceof ClientSurfaceError) return error;
  return new ClientSurfaceError(phase, errorReason(error), error);
}

function errorReason(error) {
  try {
    if (error && typeof error.message === 'string') return error.message;
  } catch {}
  try { return String(error); } catch { return 'unknown Client surface error'; }
}

function safeBridgeFunction(fn) {
  Object.setPrototypeOf(fn, null);
  return fn;
}

function normalizeModuleName(value) {
  if (typeof value !== 'string' || value.length === 0) fail('surface module path is missing');
  if (value.startsWith('surface:///')) {
    const key = value.slice('surface:///'.length);
    if (key.length === 0 || key.split('/').some(part => part === '..' || part === '.')) {
      fail('surface module path leaves the plugin directory');
    }
    return value;
  }
  const normalized = path.posix.normalize(value.replaceAll('\\', '/')).replace(/^\.\//, '');
  if (normalized === '.' || normalized === '..' || normalized.startsWith('../') || normalized.startsWith('/')) {
    fail('surface module path leaves the plugin directory');
  }
  return normalized;
}

function graphRows(graph) {
  if (Array.isArray(graph)) return graph;
  if (Array.isArray(graph?.manifest?.files)) return graph.manifest.files;
  if (Array.isArray(graph?.modules)) return graph.modules;
  if (Array.isArray(graph?.files)) return graph.files;
  if (Array.isArray(graph?.surfaceModules)) return graph.surfaceModules;
  fail('surface module graph is missing its module files');
}

function descriptorPath(row) {
  return row?.key ?? row?.module ?? row?.path ?? row?.file ?? row?.modulePath;
}

function ownDataValue(object, key) {
  const descriptor = Object.getOwnPropertyDescriptor(object, key);
  if (!descriptor) return undefined;
  if (!Object.hasOwn(descriptor, 'value')) fail(`accessor property ${key} is not plain data`);
  return descriptor.value;
}

function plainObject(value) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === null || Object.getPrototypeOf(prototype) === null;
}

function copyJson(value, options = {}, depth = 0, counter = { values: 0 }) {
  const maxDepth = options.maxDepth ?? MAX_DATA_DEPTH;
  const maxValues = options.maxValues ?? MAX_DATA_VALUES;
  if (depth > maxDepth) fail(`plain data exceeds its ${maxDepth} level nesting limit`);
  counter.values += 1;
  if (counter.values > maxValues) fail(`plain data exceeds its ${maxValues} value limit`);
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return value;
  if (typeof value === 'number') return Number.isFinite(value) ? value : null;
  if (value === undefined) return undefined;
  if (typeof value === 'function') return undefined;
  if (Array.isArray(value)) {
    const result = [];
    for (let index = 0; index < value.length; index += 1) {
      if (!Object.hasOwn(value, index)) fail('sparse arrays are not plain data');
      const child = copyJson(ownDataValue(value, String(index)), options, depth + 1, counter);
      result.push(child === undefined ? null : child);
    }
    for (const key of Reflect.ownKeys(value)) {
      if (key !== 'length' && !(typeof key === 'string' && /^(0|[1-9]\d*)$/.test(key))) {
        fail('arrays with extra properties are not plain data');
      }
    }
    return result;
  }
  if (!plainObject(value)) fail('value is not plain JSON data');
  const result = Object.create(null);
  for (const key of Reflect.ownKeys(value)) {
    if (typeof key !== 'string') fail('symbol keys are not plain data');
    const child = copyJson(ownDataValue(value, key), options, depth + 1, counter);
    if (child !== undefined) result[key] = child;
  }
  return result;
}

function cloneInput(value) {
  if (value === undefined) return undefined;
  const copied = copyJson(value, { maxDepth: MAX_DATA_DEPTH, maxValues: MAX_DATA_VALUES });
  return copied;
}

function dataText(value, limit, what) {
  const copied = copyJson(value);
  if (copied === undefined) fail(`${what} must be JSON data`);
  const text = JSON.stringify(copied);
  if (text.length > limit) fail(`${what} serializes to ${text.length} characters, over the ${limit} limit`);
  return { copied, text };
}

function normalizeGraph(moduleGraph) {
  const rows = [...graphRows(moduleGraph)];
  for (const row of [...rows]) {
    for (const linked of Array.isArray(row?.linked) ? row.linked : []) {
      if (linked && typeof linked === 'object' && typeof linked.source === 'string') rows.push(linked);
    }
    for (const link of Array.isArray(row?.links) ? row.links : []) {
      if (link && typeof link === 'object' && typeof link.source === 'string') rows.push(link);
    }
  }
  const sourceMap = moduleGraph?.sources;
  if (sourceMap && typeof sourceMap === 'object' && !Array.isArray(sourceMap)) {
    for (const [module, source] of Object.entries(sourceMap)) {
      if (typeof source === 'string') rows.push({ module, source });
      else if (source && typeof source.source === 'string') rows.push({ module, ...source });
    }
  }
  if (rows.length === 0 || rows.length > MAX_SOURCE_FILES) {
    fail(`surface module graph must contain 1 to ${MAX_SOURCE_FILES} files`);
  }
  let bytes = 0;
  const modules = new Map();
  for (const row of rows) {
    const name = normalizeModuleName(descriptorPath(row));
    if (typeof row?.source !== 'string') fail(`surface module ${name} has no source`);
    const prior = modules.get(name);
    if (prior) {
      if (prior.source !== row.source || (prior.component && row.component && prior.component !== row.component)) {
        fail(`surface module graph contains conflicting records for ${name}`);
      }
      modules.set(name, { ...prior, ...row, links: row.links ?? prior.links, linked: row.linked ?? prior.linked });
      continue;
    }
    bytes += Buffer.byteLength(row.source, 'utf8');
    if (bytes > MAX_SOURCE_BYTES) fail(`surface module graph exceeds ${MAX_SOURCE_BYTES} bytes`);
    modules.set(name, row);
  }
  return modules;
}

function moduleDescriptors(moduleGraph) {
  const rows = Array.isArray(moduleGraph?.manifest?.modules)
    ? moduleGraph.manifest.modules
    : Array.isArray(moduleGraph?.modules) ? moduleGraph.modules : [];
  const descriptors = new Map();
  for (const row of rows) {
    if (typeof row?.module !== 'string' || typeof row?.component !== 'string') continue;
    descriptors.set(normalizeModuleName(row.module), {
      entry: typeof row.entry === 'string' ? row.entry : row.module,
      component: row.component,
    });
  }
  return descriptors;
}

function resolveModuleName(specifier, from, modules, links) {
  if (specifier === 'claude:surface-runtime') return 'claude:surface-runtime';
  if (modules.has(specifier)) return specifier;
  const linkedFile = links.get(`${from}\u0000${specifier}`);
  if (linkedFile) {
    if (!modules.has(linkedFile)) fail(`surface module link ${JSON.stringify(specifier)} points to missing module ${linkedFile}`);
    return linkedFile;
  }
  const fromRow = modules.get(from);
  const explicitLink = fromRow?.links?.find(link => (link?.spelled ?? link?.specifier) === specifier);
  if (explicitLink) {
    const target = normalizeModuleName(explicitLink.file ?? explicitLink.module);
    if (!modules.has(target)) fail(`surface module link ${JSON.stringify(specifier)} points to missing module ${target}`);
    return target;
  }
  if (typeof specifier !== 'string' || !specifier.startsWith('.')) {
    fail(`surface module import ${JSON.stringify(specifier)} is not local`);
  }
  const base = path.posix.normalize(path.posix.join(path.posix.dirname(from), specifier));
  if (base === '..' || base.startsWith('../') || base.startsWith('/')) {
    fail('surface module import leaves the plugin directory');
  }
  const suffixes = ['', '.ts', '.tsx', '.js', '.jsx', '.mjs', '.mts', '/index.ts', '/index.tsx', '/index.js', '/index.mjs'];
  const candidates = suffixes.map(suffix => `${base}${suffix}`);
  for (const candidate of candidates) if (modules.has(candidate)) return candidate;
  fail(`cannot resolve surface module import ${JSON.stringify(specifier)} from ${from}`);
}

function createEnvironment(manager, key, options) {
  const context = createContext(Object.create(null), {
    name: `Mod Client ${options.plugin}/${options.environmentId}`,
    codeGeneration: { strings: false, wasm: false },
  });
  const environment = {
    key,
    instanceId: randomUUID(),
    plugin: options.plugin,
    environmentId: options.environmentId,
    manifestHash: options.manifestHash,
    context,
    modules: normalizeGraph(options.moduleGraph),
    descriptors: moduleDescriptors(options.moduleGraph),
    links: new Map(),
    moduleCache: new Map(),
    instances: new Map(),
    disposed: false,
    nextInstance: 1,
    nextHeld: 1,
  };
  for (const row of options.moduleGraph?.modules ?? []) {
    for (const link of row?.links ?? []) {
      if (typeof link?.from === 'string' && typeof link?.spelled === 'string' && typeof link?.file === 'string') {
        try {
          environment.links.set(`${normalizeModuleName(link.from)}\u0000${link.spelled}`, normalizeModuleName(link.file));
        } catch {}
      }
    }
  }
  environment.parseJson = new Script('(() => { const parse = JSON.parse; return text => parse(text); })()')
    .runInContext(context);
  environment.cloneData = value => {
    if (value === undefined) return undefined;
    return environment.parseJson(JSON.stringify(cloneInput(value)));
  };
  environment.createSurface = CREATE_SURFACE.runInContext(context);
  environment.serializeData = SERIALIZE_DATA.runInContext(context);
  environment.makeBridge = MAKE_BRIDGE.runInContext(context);
  environment.serializeTree = SERIALIZE_TREE.runInContext(context);
  environment.invoke = (fn, args, budgetMs = RUN_BUDGET_MS) => {
    const slot = `__client_call_${randomUUID().replaceAll('-', '')}`;
    context[slot] = { fn, args };
    try {
      const script = new Script(`globalThis[${JSON.stringify(slot)}].fn(...globalThis[${JSON.stringify(slot)}].args)`, {
        filename: `${environment.plugin} Client`,
      });
      return script.runInContext(context, { timeout: budgetMs });
    } finally {
      delete context[slot];
    }
  };
  environment.sourceModule = name => {
    if (environment.moduleCache.has(name)) return environment.moduleCache.get(name);
    const row = environment.modules.get(name);
    if (!row) fail(`surface module ${name} is not in the scanned module graph`);
    const module = new SourceTextModule(row.source, { context, identifier: name });
    environment.moduleCache.set(name, module);
    return module;
  };
  environment.loadEntry = async (name, component) => {
    const entry = environment.sourceModule(name);
    if (entry.status === 'unlinked') {
      await entry.link((specifier, referencing) => environment.sourceModule(
        resolveModuleName(specifier, referencing.identifier, environment.modules, environment.links),
      ));
    }
    if (entry.status === 'linked') await entry.evaluate({ timeout: LOAD_BUDGET_MS });
    if (entry.status === 'errored') throw entry.error;
    const selected = entry.namespace[component];
    if (typeof selected !== 'function') {
      fail(`surface module ${name} export ${component} is not a function (props, surface) => tree`);
    }
    return selected;
  };
  environment.dispose = () => {
    if (environment.disposed) return;
    manager.onEnvironmentInvalidated(environment);
    environment.disposed = true;
    for (const instance of [...environment.instances.values()]) manager.unmount(instance.runtimeId);
    environment.moduleCache.clear();
    environment.modules.clear();
  };
  return environment;
}

function makeIdentity(input) {
  const parent = input.parent ?? {};
  const client = input.client ?? {};
  for (const key of ['plugin', 'environmentId', 'runtimeId']) {
    if (typeof input[key] !== 'string' || input[key].length === 0) fail(`Client ${key} is required`);
  }
  for (const key of ['surface', 'component', 'requestId']) {
    if (typeof parent[key] !== 'string' || parent[key].length === 0) fail(`Client parent ${key} is required`);
  }
  for (const key of ['key', 'module']) {
    if (typeof client[key] !== 'string' || client[key].length === 0) fail(`Client ${key} is required`);
  }
  return Object.freeze({
    plugin: input.plugin,
    environmentId: input.environmentId,
    runtimeId: input.runtimeId,
    parent: Object.freeze({ surface: parent.surface, component: parent.component, requestId: parent.requestId }),
    key: client.key,
    module: normalizeModuleName(client.module),
  });
}

function hasListener(instance, kind) {
  return typeof instance.binding.listener(kind) === 'function';
}

export class ClientSurfaceManager {
  constructor({ send, asyncHeldBudgetMs = ASYNC_HELD_BUDGET_MS, onEnvironmentInvalidated = () => {} } = {}) {
    if (typeof send !== 'function') fail('ClientSurfaceManager requires a host send callback');
    if (!Number.isFinite(asyncHeldBudgetMs) || asyncHeldBudgetMs <= 0) fail('invalid Client callback budget');
    this.send = send;
    this.onEnvironmentInvalidated = onEnvironmentInvalidated;
    this.asyncHeldBudgetMs = asyncHeldBudgetMs;
    // A per-async-chain context prevents unrelated timers and callbacks from
    // being attributed to a held action that happens to be awaiting.
    this.heldContext = new AsyncLocalStorage();
    this.environments = new Map();
    this.runtimeInstances = new Map();
    this.nextGeneration = 1;
    this.disposed = false;
  }

  environmentFor(input) {
    const manifestHash = input.manifestHash ?? createHash('sha256')
      .update([...normalizeGraph(input.moduleGraph)]
        .map(([name, row]) => JSON.stringify([name, row.source, row.component, row.links, row.linked]))
        .join('\u0000'))
      .digest('hex');
    const key = `${input.plugin}\u0000${input.environmentId}`;
    const existing = this.environments.get(key);
    if (existing && existing.manifestHash === manifestHash) return existing;
    // Native keeps one current surface environment per plugin. Either a new
    // environment ID or a changed manifest replaces and disposes its older
    // environment, including instances still registered under that version.
    for (const [existingKey, environment] of this.environments) {
      if (environment.plugin !== input.plugin) continue;
      environment.dispose();
      this.environments.delete(existingKey);
    }
    const environment = createEnvironment(this, key, { ...input, manifestHash });
    this.environments.set(key, environment);
    return environment;
  }

  emit(instance, action, fields = {}) {
    if (instance.destroyed || this.disposed || (instance.failure && action !== 'fault')) return;
    const held = this.heldContext.getStore();
    if (action !== 'fault' && held?.instance === instance && held.cancelled) return;
    const postAction = ['post', 'schedule'].includes(action) && held?.instance === instance && held.active
      && typeof held.id === 'string' ? held : undefined;
    const postCorrelation = postAction
      ? { postActionId: postAction.id, postId: postAction.postIds.length }
      : undefined;
    if (postCorrelation) postAction.postIds.push(postCorrelation.postId);
    this.send({
      id: 0,
      kind: 'ui.client.event',
      runtimeId: instance.runtimeId,
      generation: instance.generation,
      parent: instance.identity.parent,
      plugin: instance.identity.plugin,
      element: instance.identity.key,
      module: instance.identity.module,
      action,
      ...fields,
      // Correlation is host-private metadata, appended after the opaque post
      // payload so authors cannot replace the action or sequence identity.
      environmentInstanceId: instance.environment.instanceId,
      hostRenderRevision: instance.hostRenderRevision,
      ...(postCorrelation ?? {}),
    });
  }

  emitFault(instance, phase, error) {
    if (instance.destroyed || instance.failure) return false;
    const reason = errorReason(error);
    instance.failure = Object.freeze({ status: 'failed', phase, reason });
    instance.activePostAction?.controller.abort();
    instance.scheduled = false;
    for (const timer of instance.timers.values()) clearInterval(timer);
    instance.timers.clear();
    instance.held.clear();
    instance.nextHeldCallback.clear();
    instance.binding?.unmount?.();
    instance.binding = undefined;
    instance.component = undefined;
    this.emit(instance, 'fault', { phase, reason });
    return true;
  }

  invalidateHeldEnvironment(instance, error) {
    this.invalidateEnvironment(instance.environment, error);
  }

  invalidateEnvironment(environment, error, phase = 'run') {
    if (environment.disposed) return;
    this.onEnvironmentInvalidated(environment);
    // Modules and module-level variables are shared by every surface in this
    // environment. A non-cancellable continuation could still mutate them, so
    // fence every member and never reuse this VM for a fresh mount.
    for (const member of [...environment.instances.values()]) this.emitFault(member, phase, error);
    environment.disposed = true;
    environment.instances.clear();
    environment.moduleCache.clear();
    environment.modules.clear();
    if (this.environments.get(environment.key) === environment) this.environments.delete(environment.key);
  }

  async mount(input) {
    if (this.disposed) fail('ClientSurfaceManager is disposed');
    if (input.hostRenderRevision !== undefined
        && (!Number.isSafeInteger(input.hostRenderRevision) || input.hostRenderRevision <= 0)) {
      fail('Client mount has an invalid Host render revision');
    }
    const identity = makeIdentity(input);
    if (this.runtimeInstances.has(identity.runtimeId)) this.unmount(identity.runtimeId);
    const environment = this.environmentFor({ ...input, ...identity });
    const instance = {
      identity,
      runtimeId: identity.runtimeId,
      vmInstanceId: environment.nextInstance++,
      generation: this.nextGeneration++,
      hostRenderRevision: input.hostRenderRevision,
      frameSequence: 0,
      environment,
      props: undefined,
      columns: Number.isFinite(input.columns) ? input.columns : 0,
      rows: Number.isFinite(input.rows) ? input.rows : 0,
      component: undefined,
      binding: undefined,
      held: new Map(),
      nextHeldCallback: new Map(),
      nextHeld: 1,
      timers: new Map(),
      nextTimer: 1,
      activePostAction: undefined,
      scheduled: false,
      destroyed: false,
      failure: undefined,
    };
    environment.instances.set(identity.runtimeId, instance);
    this.runtimeInstances.set(identity.runtimeId, instance);
    try {
      instance.props = environment.cloneData(input.props ?? {});
      const descriptor = environment.descriptors.get(identity.module);
      const componentName = input.exportName ?? descriptor?.component;
      const moduleEntry = descriptor?.entry ?? identity.module;
      instance.moduleEntry = moduleEntry;
      instance.component = await environment.loadEntry(moduleEntry, componentName);
      const hostBridge = {
        allowed: safeBridgeFunction(() => {
          const held = this.heldContext.getStore();
          return !(held?.instance === instance && held.cancelled);
        }),
        schedule: safeBridgeFunction(() => {
          if (instance.destroyed || instance.scheduled) return;
          instance.scheduled = true;
          this.emit(instance, 'schedule');
        }),
        post: safeBridgeFunction(json => {
          if (typeof json !== 'string') fail('surface.post serializer returned no JSON text');
          this.emit(instance, 'post', { json });
        }),
        every: safeBridgeFunction((milliseconds, callback) => this.startTimer(instance, milliseconds, callback)),
      };
      instance.binding = environment.createSurface(
        environment.makeBridge(hostBridge, environment.serializeData),
        { columns: instance.columns, rows: instance.rows },
      );
    } catch (error) {
      this.invalidateEnvironment(environment, error, 'load');
      this.unmount(identity.runtimeId);
      throw clientError('load', error);
    }
    try {
      return { ...this.render(identity.runtimeId), environmentInstanceId: environment.instanceId };
    } catch (error) {
      // The Host removes a Client whose initial frame failed, so there is no
      // owner left to send an explicit unmount for this VM entry.
      this.unmount(identity.runtimeId);
      throw error;
    }
  }

  render(runtimeId) {
    const instance = this.requireInstance(runtimeId);
    if (instance.failure) return { stale: true };
    instance.scheduled = false;
    instance.nextHeldCallback = new Map();
    try {
      const tree = instance.environment.invoke(
        instance.binding.render.bind(instance.binding, instance.component, instance.props), [], RUN_BUDGET_MS,
      );
      const heldBridge = {
        hold: safeBridgeFunction(callback => {
          const handle = instance.nextHeld++;
          instance.nextHeldCallback.set(handle, callback);
          return handle;
        }),
      };
      const text = instance.environment.invoke(
        instance.environment.serializeTree.bind(null, tree, heldBridge), [], RUN_BUDGET_MS,
      );
      if (typeof text !== 'string') fail('surface render did not serialize to a tree');
      instance.held = instance.nextHeldCallback;
      instance.nextHeldCallback = new Map();
      return {
        runtimeId,
        generation: instance.generation,
        frameSequence: this.nextFrameSequence(instance),
        tree: JSON.parse(text),
        hasPointerListener: hasListener(instance, 'pointer'),
        hasKeyListener: hasListener(instance, 'key'),
      };
    } catch (error) {
      instance.nextHeldCallback.clear();
      this.invalidateEnvironment(instance.environment, error, 'render');
      throw clientError('render', error);
    }
  }

  setProps(runtimeId, props) {
    const instance = this.requireInstance(runtimeId);
    try {
      instance.props = instance.environment.cloneData(props);
      if (instance.failure) return { stale: true };
      instance.binding.setProps(instance.props);
    } catch (error) {
      this.invalidateEnvironment(instance.environment, error, 'run');
      throw clientError('run', error);
    }
    return this.render(runtimeId);
  }

  resize(runtimeId, columns, rows) {
    const instance = this.requireInstance(runtimeId);
    if (!Number.isFinite(columns) || !Number.isFinite(rows)) fail('Client resize requires finite columns and rows');
    instance.columns = columns;
    instance.rows = rows;
    if (instance.failure) return { stale: true };
    instance.binding.resize(columns, rows);
    return this.render(runtimeId);
  }

  pointer(runtimeId, payload) { return this.dispatchListener(runtimeId, 'pointer', payload); }
  key(runtimeId, payload) { return this.dispatchListener(runtimeId, 'key', payload); }

  dispatchListener(runtimeId, kind, payload) {
    const instance = this.runtimeInstances.get(runtimeId);
    if (!instance || instance.destroyed || instance.failure) return false;
    const callback = instance.binding.listener(kind);
    if (typeof callback !== 'function') return false;
    try {
      this.invokeCallback(instance, callback, instance.environment.cloneData(payload));
      return true;
    } catch (error) {
      this.invalidateHeldEnvironment(instance, error);
      const failure = clientError('run', error);
      failure.outcomeUnknown = true;
      throw failure;
    }
  }

  async runHeld(runtimeId, handle, payload) {
    return (await this.runHeldCore(runtimeId, handle, payload)).accepted;
  }

  async runHeldWithPostIds(runtimeId, handle, payload, postActionId, signal) {
    return this.runHeldCore(runtimeId, handle, payload, postActionId, signal);
  }

  async runHeldCore(runtimeId, handle, payload, postActionId, signal) {
    const instance = this.runtimeInstances.get(runtimeId);
    if (!instance || instance.destroyed || instance.failure || signal?.aborted) {
      return { accepted: false, postIds: [] };
    }
    const callback = instance.held.get(handle);
    if (typeof callback !== 'function') return { accepted: false, postIds: [] };
    return this.runActionCore(instance, callback, payload, postActionId, signal);
  }

  async runListenerWithPostIds(runtimeId, kind, payload, postActionId, signal) {
    const instance = this.runtimeInstances.get(runtimeId);
    if (!instance || instance.destroyed || instance.failure || signal?.aborted) {
      return { accepted: false, postIds: [] };
    }
    const callback = instance.binding.listener(kind);
    if (typeof callback !== 'function') return { accepted: false, postIds: [] };
    return this.runActionCore(instance, callback, payload, postActionId, signal);
  }

  async runActionCore(instance, callback, payload, postActionId, signal) {
    if (instance.activePostAction) fail('Client post action is already active');
    const action = { id: postActionId, postIds: [], instance, active: true,
      cancelled: false, controller: new AbortController() };
    const abort = () => action.controller.abort();
    action.controller.signal.addEventListener('abort', () => { action.cancelled = true; }, { once: true });
    if (signal?.aborted) abort();
    else signal?.addEventListener('abort', abort, { once: true });
    instance.activePostAction = action;
    try {
      const input = instance.environment.cloneData(payload);
      const result = this.heldContext.run(action, () =>
        instance.environment.invoke(callback, [input], RUN_BUDGET_MS));
      await awaitHeldCallback(result, action.controller.signal, this.asyncHeldBudgetMs);
      if (instance.destroyed || instance.failure || action.controller.signal.aborted) {
        throw new Error('Client callback owner ended before completion');
      }
      action.active = false;
      return { accepted: true, postIds: action.postIds };
    } catch (error) {
      action.cancelled = true;
      action.controller.abort();
      this.invalidateHeldEnvironment(instance, error);
      const failure = clientError('run', error);
      failure.outcomeUnknown = true;
      throw failure;
    } finally {
      action.active = false;
      signal?.removeEventListener('abort', abort);
      if (instance.activePostAction === action) instance.activePostAction = undefined;
    }
  }

  invokeCallback(instance, callback, payload) {
    if (instance.destroyed || instance.failure) return;
    const result = instance.environment.invoke(callback, [payload], RUN_BUDGET_MS);
    if (result && typeof result.then === 'function') {
      result.catch(error => this.invalidateHeldEnvironment(instance, error));
    }
  }

  startTimer(instance, milliseconds, callback) {
    if (instance.destroyed || instance.failure) fail('Client surface is unmounted');
    const timerId = instance.nextTimer++;
    const timer = setInterval(() => {
      if (instance.destroyed || !instance.timers.has(timerId)) return;
      try { this.invokeCallback(instance, callback, undefined); }
      catch (error) { this.invalidateHeldEnvironment(instance, error); }
    }, Math.max(1, milliseconds));
    timer.unref?.();
    instance.timers.set(timerId, timer);
    let stopped = false;
    return safeBridgeFunction(() => {
      if (stopped) return;
      stopped = true;
      clearInterval(timer);
      instance.timers.delete(timerId);
    });
  }

  dropHeld(runtimeId, handles) {
    const instance = this.requireInstance(runtimeId);
    if (handles === undefined) {
      instance.held.clear();
      instance.nextHeldCallback.clear();
      return;
    }
    if (!Array.isArray(handles)) fail('dropHeld expects an array of numeric handles');
    for (const handle of handles) {
      if (typeof handle === 'number') {
        instance.held.delete(handle);
        instance.nextHeldCallback.delete(handle);
      }
    }
  }

  unmount(runtimeId) {
    const instance = this.runtimeInstances.get(runtimeId);
    if (!instance) return false;
    instance.destroyed = true;
    instance.activePostAction?.controller.abort();
    for (const timer of instance.timers.values()) clearInterval(timer);
    instance.timers.clear();
    instance.held.clear();
    instance.nextHeldCallback.clear();
    instance.binding?.unmount?.();
    instance.binding = undefined;
    instance.component = undefined;
    instance.environment.instances.delete(runtimeId);
    this.runtimeInstances.delete(runtimeId);
    return true;
  }

  disposeEnvironment(plugin, environmentId) {
    const key = `${plugin}\u0000${environmentId}`;
    const environment = this.environments.get(key);
    if (!environment) return false;
    environment.dispose();
    this.environments.delete(key);
    return true;
  }

  disposePlugin(plugin) {
    let disposed = 0;
    for (const [key, environment] of this.environments) {
      if (environment.plugin !== plugin) continue;
      environment.dispose();
      this.environments.delete(key);
      disposed += 1;
    }
    return disposed;
  }

  requireInstance(runtimeId) {
    const instance = this.runtimeInstances.get(runtimeId);
    if (!instance || instance.destroyed) fail(`no mounted Client surface ${runtimeId}`);
    return instance;
  }

  nextFrameSequence(instance) {
    if (!Number.isSafeInteger(instance.frameSequence)
        || instance.frameSequence >= Number.MAX_SAFE_INTEGER) {
      fail(`Client frame sequence exhausted for ${instance.runtimeId}`);
    }
    instance.frameSequence += 1;
    return instance.frameSequence;
  }

  async operation(operation, { signal } = {}) {
    if (!operation || typeof operation !== 'object' || Array.isArray(operation)) {
      fail('ui.client.operation requires an operation object');
    }
    if (!['mount', 'disposeEnvironment'].includes(operation.type)) {
      const current = this.runtimeInstances.get(operation.runtimeId);
      if (!current || (operation.generation !== undefined && operation.generation !== current.generation)) {
        if (operation.type === 'unmount') return { unmounted: false, stale: true };
        return { stale: true };
      }
      if (current.failure && ['runHeld', 'pointer', 'key'].includes(operation.type)) {
        // No callback was invoked by this operation. Report the previously
        // fenced environment separately from an ordinary missing handle.
        const failure = clientError(current.failure.phase, current.failure.reason);
        failure.notStartedUnavailable = true;
        throw failure;
      }
      if (operation.hostRenderRevision !== undefined && operation.type !== 'unmount') {
        if (!Number.isSafeInteger(operation.hostRenderRevision) || operation.hostRenderRevision <= 0) {
          fail('Client operation has an invalid Host render revision');
        }
        if (current.hostRenderRevision !== undefined && operation.hostRenderRevision < current.hostRenderRevision) {
          return { stale: true };
        }
        current.hostRenderRevision = operation.hostRenderRevision;
      }
    }
    switch (operation.type) {
      case 'mount': return this.mount(operation);
      case 'render': return this.render(operation.runtimeId);
      case 'setProps': return this.setProps(operation.runtimeId, operation.props);
      case 'resize': return this.resize(operation.runtimeId, operation.columns, operation.rows);
      case 'pointer':
      case 'key':
        if (typeof operation.postActionId !== 'string' || operation.postActionId.length === 0) {
          fail('Client listener operation requires a host post action ID');
        }
        return this.runListenerWithPostIds(
          operation.runtimeId, operation.type, operation.payload, operation.postActionId, signal,
        );
      case 'runHeld':
        if (typeof operation.postActionId !== 'string' || operation.postActionId.length === 0) {
          fail('Client runHeld operation requires a host post action ID');
        }
        return this.runHeldWithPostIds(
          operation.runtimeId, operation.handle, operation.payload, operation.postActionId, signal,
        );
      case 'dropHeld': this.dropHeld(operation.runtimeId, operation.handles); return { ok: true };
      case 'unmount': return { unmounted: this.unmount(operation.runtimeId) };
      case 'disposeEnvironment': return { disposed: this.disposeEnvironment(operation.plugin, operation.environmentId) };
      default: fail(`unknown Client operation: ${operation.type}`);
    }
  }

  dispose() {
    if (this.disposed) return;
    this.disposed = true;
    for (const environment of this.environments.values()) environment.dispose();
    for (const instance of [...this.runtimeInstances.values()]) this.unmount(instance.runtimeId);
    this.environments.clear();
    this.runtimeInstances.clear();
  }
}

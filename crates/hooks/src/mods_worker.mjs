import { createInterface } from 'node:readline';
import { resolve, isAbsolute, posix as posixPath, win32 as win32Path } from 'node:path';
import { SourceTextModule, Script, createContext } from 'node:vm';
import { randomUUID } from 'node:crypto';
import { pathToFileURL } from 'node:url';
import { ClientSurfaceManager } from './mod_ui_client_worker.mjs';

const registrations = [];
const pendingCore = new Map();
const pendingStreamCore = new Map();
const activeStreams = new Map();
const pendingApi = new Map();
const pendingApiCallerRegistrations = new Map();
const cancelledApiCalls = new Set();
const pendingVoidCalls = new Map();
const pendingTimers = new Map();
const lastToastByPlugin = new Map();
const secDefaultNotices = new Set();
let secDefaultPolicyAt = -Infinity;
let secDefaultPolicyPromise;
const activeDispatch = new Map();
const pendingDispatch = new Set();
const cancelledRequests = new Set();
const moduleCache = new Map();
const preparedModules = new Map();
// Element constructors are resolved once when a Mod loads (per supported
// surface/site), then reused by every ui.render dispatch for that Mod.
const resolvedUiElements = new Map();
// Press callbacks stay inside the Mod worker. The tree carries only the
// identity needed to route a later ui.press; epoch + renderRevision prevent a
// numeric handle from a previous worker/render from reaching a replacement.
const uiPressActions = new Map();
const uiStorageEpochs = new Map();
const uiPressCounters = new Map();
const activeUiRenderRevisions = new Map();
let uiRenderRevision = 0;
let parentUiPressCounter = 0;
const context = createContext({ AbortController, AbortSignal });
const invocationSlot = `__lingxi_mod_call_${randomUUID().replaceAll('-', '')}`;
const invokeScript = new Script(`globalThis[${JSON.stringify(invocationSlot)}].fn(...globalThis[${JSON.stringify(invocationSlot)}].args)`);
const supportedEvents = new Set(['plugin.register', 'tool.call', 'tool.check', 'tool.describe', 'tool.list', 'tool.register', 'command.list', 'command.register', 'command.run', 'command.describe', 'prompt.submit', 'prompt.context', 'prompt.attachment', 'prompt.section', 'prompt.compose', 'session.start', 'session.receive', 'session.end', 'session.compact', 'session.measure', 'session.append', 'session.attach', 'session.detach', 'turn.start', 'turn.step', 'turn.complete', 'agent.offer', 'agent.spawn', 'settings.read', 'telemetry.log', 'telemetry.mark', 'ui.render', 'ui.resolve', 'ui.press', 'ui.input', 'ui.select', 'ui.message', 'ui.fault', 'ui.selection', 'ui.log', 'ui.toast', 'ui.status', 'clock.now', 'clock.sleep', 'clock.after', 'clock.every', 'process.run', 'fs.read', 'fs.write', 'fs.exists', 'fs.list', 'fs.stat', 'fs.ancestors', 'store.get', 'store.set', 'store.delete', 'store.keys', 'state.get', 'state.set', 'env.get', 'env.set', 'session.cwd', 'session.root', 'session.model', 'session.id', 'session.turns', 'session.repo', 'session.version', 'session.messages', 'session.usage', 'session.surfaces', 'session.surface', 'model.fork', 'model.complete', 'model.classify']);
const operationEvents = new Set(['tool.check', 'tool.list', 'tool.register', 'command.list', 'command.register', 'settings.read', 'telemetry.log', 'telemetry.mark', 'ui.log', 'ui.toast', 'ui.status', 'ui.selection', 'clock.now', 'clock.sleep', 'clock.after', 'clock.every', 'process.run', 'fs.read', 'fs.write', 'fs.exists', 'fs.list', 'fs.stat', 'fs.ancestors', 'store.get', 'store.set', 'store.delete', 'store.keys', 'state.get', 'state.set', 'env.get', 'env.set', 'session.cwd', 'session.root', 'session.model', 'session.id', 'session.turns', 'session.repo', 'session.version', 'session.receive', 'session.messages', 'session.usage', 'session.surfaces', 'session.surface', 'model.fork', 'model.complete', 'model.classify']);
const commandRunHeldEvents = new Set(['tool.call', 'prompt.context', 'prompt.section', 'prompt.compose', 'command.run']);
const promptSubmitHeldEvents = new Set([
  'classic.PreToolUse', 'tool.call', 'agent.spawn', 'session.send', 'prompt.section',
  'prompt.context', 'prompt.attachment', 'prompt.compose', 'tool.describe',
  'command.run', 'command.describe', 'config.describe', 'config.set',
]);
const tiers = ['prepend', 'user', 'append', 'builtin', 'core'];
const nextToTargets = {
  prepend: ['append', 'builtin', 'core'],
  user: [],
  append: ['core'],
  builtin: [],
  core: [],
};
const hookBudgetMs = 10_000;
const catchBudgetMs = 1_000;
const secDefaultPlugin = 'cc-plugin-sec-default';
const secDefaultStorageId = 'cc-plugin-sec-default@builtin';
const secDefaultHookId = 0;
const modUtf16SidecarsField = '__lingxiModUtf16StringsV1';
const modUtf16KeySidecarsField = '__lingxiModUtf16KeysV1';
const modUtf16KeyPlaceholderPrefix = '__lingxiModUtf16KeyV1_';
let nextModUtf16KeyPlaceholder = 1;
const budgetExpired = Symbol('Mod hook budget expired');
let queue = Promise.resolve();
let nextCallId = 1;
let nextHookId = 1;
let nextApiCallerRegistrationId = 1;

function expiredCall() {
  const rejected = Promise.reject(budgetExpired);
  void rejected.catch(() => {});
  return rejected;
}

function pointerEscape(segment) {
  return segment.replaceAll('~', '~0').replaceAll('/', '~1');
}

function utf16CodeUnits(value) {
  const units = [];
  for (let index = 0; index < value.length; index++) units.push(value.charCodeAt(index));
  return units;
}

function projectWorkerMessage(value, pointer, sidecars, keySidecars) {
  if (typeof value === 'string') {
    const display = displayText(value);
    if (display !== value) sidecars.push({ pointer, code_units: utf16CodeUnits(value) });
    return display;
  }
  if (Array.isArray(value)) {
    return value.map((item, index) => projectWorkerMessage(
      item, `${pointer}/${index}`, sidecars, keySidecars,
    ));
  }
  if (value && typeof value === 'object') {
    const copy = Object.create(null);
    const usedKeys = new Set(Object.keys(value).filter(key => displayText(key) === key));
    for (const originalKey of Object.keys(value)) {
      let wireKey = originalKey;
      const exactKey = displayText(originalKey) !== originalKey;
      if (exactKey) {
        do {
          wireKey = `${modUtf16KeyPlaceholderPrefix}${nextModUtf16KeyPlaceholder++}__`;
        } while (Object.hasOwn(value, wireKey) || usedKeys.has(wireKey));
      }
      usedKeys.add(wireKey);
      copy[wireKey] = projectWorkerMessage(
        value[originalKey], `${pointer}/${pointerEscape(wireKey)}`, sidecars, keySidecars,
      );
      if (exactKey) {
        keySidecars.push({
          pointer,
          placeholder: wireKey,
          code_units: utf16CodeUnits(originalKey),
        });
      }
    }
    return copy;
  }
  return value;
}

function encodeWorkerMessage(value, pointer, sidecars, keySidecars) {
  // Let the engine perform the complete JSON serialization algorithm first:
  // toJSON, getters, wrapper unboxing, omission, and array null substitution
  // all have observable semantics. JSON.parse preserves escaped lone units so
  // the projection pass can safely replace them without reimplementing those
  // semantics or calling user hooks a second time.
  const serialized = JSON.stringify(value);
  if (serialized === undefined) return undefined;
  return projectWorkerMessage(JSON.parse(serialized), pointer, sidecars, keySidecars);
}

function send(message) {
  const sidecars = [];
  const keySidecars = [];
  const safeMessage = encodeWorkerMessage(message, '', sidecars, keySidecars);
  if (sidecars.length > 0 && safeMessage && typeof safeMessage === 'object'
      && !Array.isArray(safeMessage)) {
    safeMessage[modUtf16SidecarsField] = sidecars;
  }
  if (keySidecars.length > 0 && safeMessage && typeof safeMessage === 'object'
      && !Array.isArray(safeMessage)) {
    safeMessage[modUtf16KeySidecarsField] = keySidecars;
  }
  process.stdout.write(JSON.stringify(safeMessage) + '\n');
}

const clientSurfaceManager = new ClientSurfaceManager({ send });

function freeze(value) {
  if (value && typeof value === 'object' && !Object.isFrozen(value)) {
    for (const child of Object.values(value)) freeze(child);
    Object.freeze(value);
  }
  return value;
}

function displayText(value) {
  return value.replace(/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g, '\uFFFD');
}

function pointerSegments(pointer) {
  if (pointer === '') return [];
  if (typeof pointer !== 'string' || !pointer.startsWith('/')) return undefined;
  const segments = [];
  for (const encoded of pointer.slice(1).split('/')) {
    let decoded = '';
    for (let index = 0; index < encoded.length; index++) {
      const char = encoded[index];
      if (char !== '~') {
        decoded += char;
        continue;
      }
      const escaped = encoded[++index];
      if (escaped === '0') decoded += '~';
      else if (escaped === '1') decoded += '/';
      else return undefined;
    }
    segments.push(decoded);
  }
  return segments;
}

function valueAtPointer(root, pointer) {
  const segments = pointerSegments(pointer);
  if (!segments) return undefined;
  let value = root;
  for (const segment of segments) {
    if (Array.isArray(value)) {
      if (!/^(0|[1-9]\d*)$/.test(segment)) return undefined;
      value = value[Number(segment)];
    } else if (value && typeof value === 'object'
        && Object.hasOwn(value, segment)) {
      value = value[segment];
    } else return undefined;
  }
  return value;
}

function setAtPointer(root, pointer, replacement) {
  const segments = pointerSegments(pointer);
  if (!segments || segments.length === 0) return false;
  let parent = root;
  for (const segment of segments.slice(0, -1)) {
    if (Array.isArray(parent)) {
      if (!/^(0|[1-9]\d*)$/.test(segment)) return false;
      parent = parent[Number(segment)];
    } else if (parent && typeof parent === 'object'
        && Object.hasOwn(parent, segment)) {
      parent = parent[segment];
    } else return false;
  }
  const leaf = segments.at(-1);
  if (Array.isArray(parent)) {
    if (!/^(0|[1-9]\d*)$/.test(leaf)) return false;
    parent[Number(leaf)] = replacement;
  } else if (parent && typeof parent === 'object' && Object.hasOwn(parent, leaf)) {
    parent[leaf] = replacement;
  } else return false;
  return true;
}

function stringFromUtf16(units) {
  let result = '';
  for (let index = 0; index < units.length; index += 0x4000) {
    result += String.fromCharCode(...units.slice(index, index + 0x4000));
  }
  return result;
}

function hydrateModUtf16Sidecars(message) {
  if (!message || typeof message !== 'object' || Array.isArray(message)) return;
  const sidecars = message[modUtf16SidecarsField];
  delete message[modUtf16SidecarsField];
  if (sidecars !== undefined && !Array.isArray(sidecars)) {
    throw new Error('invalid Mod UTF-16 sidecar envelope');
  }
  const seen = new Set();
  for (const sidecar of sidecars ?? []) {
    if (!sidecar || typeof sidecar !== 'object' || Array.isArray(sidecar)
        || typeof sidecar.pointer !== 'string'
        || !Array.isArray(sidecar.code_units)
        || !sidecar.code_units.every(unit => Number.isInteger(unit) && unit >= 0 && unit <= 0xffff)
    ) {
      throw new Error('invalid Mod UTF-16 sidecar entry');
    }
    const segments = pointerSegments(sidecar.pointer);
    if (!segments || segments.length === 0) {
      throw new Error('invalid Mod UTF-16 JSON pointer');
    }
    const canonical = `/${segments.map(pointerEscape).join('/')}`;
    if (canonical !== sidecar.pointer || seen.has(canonical)) {
      throw new Error('duplicate or non-canonical Mod UTF-16 JSON pointer');
    }
    seen.add(canonical);
    const exact = stringFromUtf16(sidecar.code_units);
    const display = displayText(exact);
    if (display === exact || valueAtPointer(message, canonical) !== display
        || !setAtPointer(message, canonical, exact)) {
      throw new Error('Mod UTF-16 sidecar target is missing');
    }
  }

  const keySidecars = message[modUtf16KeySidecarsField];
  delete message[modUtf16KeySidecarsField];
  if (keySidecars !== undefined && !Array.isArray(keySidecars)) {
    throw new Error('invalid Mod UTF-16 key sidecar envelope');
  }
  const keyEntries = (keySidecars ?? []).map(sidecar => {
    if (!sidecar || typeof sidecar !== 'object' || Array.isArray(sidecar)
        || typeof sidecar.pointer !== 'string'
        || typeof sidecar.placeholder !== 'string'
        || !sidecar.placeholder.startsWith(modUtf16KeyPlaceholderPrefix)
        || !sidecar.placeholder.endsWith('__')
        || !/^[\x00-\x7f]+$/.test(sidecar.placeholder)
        || !Array.isArray(sidecar.code_units)
        || !sidecar.code_units.every(unit => Number.isInteger(unit) && unit >= 0 && unit <= 0xffff)) {
      throw new Error('invalid Mod UTF-16 key sidecar entry');
    }
    const segments = pointerSegments(sidecar.pointer);
    if (!segments || (segments.length > 0
        && `/${segments.map(pointerEscape).join('/')}` !== sidecar.pointer)) {
      throw new Error('invalid or non-canonical Mod UTF-16 key pointer');
    }
    const exact = stringFromUtf16(sidecar.code_units);
    if (displayText(exact) === exact) throw new Error('Mod UTF-16 key sidecar must contain an unpaired unit');
    return { ...sidecar, segments, exact };
  }).sort((left, right) => right.segments.length - left.segments.length);
  const seenKeys = new Set();
  for (const sidecar of keyEntries) {
    const identity = `${sidecar.pointer}\u0000${sidecar.placeholder}`;
    if (seenKeys.has(identity)) throw new Error('duplicate Mod UTF-16 object-key sidecar');
    seenKeys.add(identity);
    const parent = valueAtPointer(message, sidecar.pointer);
    if (!parent || typeof parent !== 'object' || Array.isArray(parent)
        || !Object.hasOwn(parent, sidecar.placeholder) || Object.hasOwn(parent, sidecar.exact)) {
      throw new Error('Mod UTF-16 key sidecar target is missing or collides');
    }
    const entries = Object.keys(parent).map(key => [key, parent[key]]);
    const rebuilt = entries.map(([key, value]) => [
      key === sidecar.placeholder ? sidecar.exact : key, value,
    ]);
    for (const key of Object.keys(parent)) delete parent[key];
    for (const [key, value] of rebuilt) {
      Object.defineProperty(parent, key, { value, enumerable: true, configurable: true, writable: true });
    }
  }
}

function rejectPendingUtf16Reply(message, error) {
  const reason = new Error(`invalid Mod UTF-16 protocol reply: ${String(error?.message || error)}`);
  const callId = message?.callId;
  if (Number.isSafeInteger(message?.id) && cancelledRequests.has(message.id)) return true;
  if (!Number.isSafeInteger(callId)) return false;
  if (message.kind === 'next.result' || message.kind === 'next.error') {
    const pending = pendingCore.get(callId);
    if (!pending || pending.requestId !== message.id) return false;
    pendingCore.delete(callId);
    pending.reject(reason);
    return true;
  }
  if (message.kind === 'api.result' || message.kind === 'api.error') {
    const pending = pendingApi.get(callId);
    if (!pending || pending.requestId !== message.id) {
      return cancelledApiCalls.delete(callId);
    }
    pendingApi.delete(callId);
    pending.reject(reason);
    return true;
  }
  if (message.kind === 'source.open.result' || message.kind === 'source.open.error'
      || message.kind === 'source.pull.result' || message.kind === 'source.pull.error') {
    const pending = pendingStreamCore.get(callId);
    if (!pending || pending.requestId !== message.id) return false;
    pendingStreamCore.delete(callId);
    pending.reject(reason);
    return true;
  }
  if (message.kind === 'api.callers.result' || message.kind === 'api.callers.error') {
    const pending = pendingApiCallerRegistrations.get(callId);
    if (!pending || pending.requestId !== message.id) return false;
    pendingApiCallerRegistrations.delete(callId);
    pending.reject(reason);
    return true;
  }
  return false;
}

function failModUtf16Hydration(message, error) {
  if (rejectPendingUtf16Reply(message, error)) return true;
  send({ id: message?.id, kind: 'protocol.error', message: String(error?.message || error) });
  return false;
}

function traceNode() {
  return { entry: undefined, beneath: undefined };
}

function traceBelow(node) {
  const entries = [];
  for (let link = node.beneath; link; link = link.beneath) {
    if (link.entry) entries.push(link.entry);
  }
  return Object.freeze(entries);
}

function secDefaultPolicy(api) {
  if (secDefaultPolicyPromise === undefined || Date.now() - secDefaultPolicyAt > 500) {
    secDefaultPolicyAt = Date.now();
    secDefaultPolicyPromise = api.settings.read({ source: 'policy' });
  }
  return secDefaultPolicyPromise;
}

function secDefaultDenyRulesHold(settings) {
  return settings?.pluginConfigs?.['cc-plugin-sec-default@builtin']?.options
    ?.allowModsToOverrideDenyRules !== true;
}

function secDefaultLooseners(trace) {
  const rank = { deny: 0, ask: 1, allow: 2 };
  return trace.filter((link, index) => {
    if (!['user', 'prepend'].includes(link.tier) || link.returned === undefined) return false;
    const below = trace.slice(index + 1).find(entry => entry.returned !== undefined)?.returned;
    return rank[link.returned.decision] > rank[below?.decision ?? 'deny'];
  }).map(link => link.plugin);
}

function secDefaultCaughtAnswer(answer, trace) {
  return answer !== undefined && (answer.decision === 'deny' || secDefaultLooseners(trace).length === 0)
    ? answer
    : { decision: 'deny', reason: 'the deny rules in your settings could not be checked for this call, so it is refused' };
}

function secDefaultOrgServers(policy) {
  const allowed = Array.isArray(policy.allowedMcpServers) ? policy.allowedMcpServers : [];
  const managed = policy.managedMcpServers && typeof policy.managedMcpServers === 'object'
    && !Array.isArray(policy.managedMcpServers) ? policy.managedMcpServers : {};
  return [...allowed.flatMap(entry => typeof entry?.serverName === 'string' && entry.serverName
    ? [entry.serverName] : []), ...Object.keys(managed)];
}

function secDefaultRestoreTools(policy, managed, ordinary) {
  if (policy === undefined || managed.value === undefined || ordinary.value === undefined) {
    return managed;
  }
  const servers = secDefaultOrgServers(policy);
  const isOrg = tool => servers.some(server => tool.name.startsWith(`mcp__${server}__`));
  return { value: [...managed.value.filter(isOrg), ...ordinary.value.filter(tool => !isOrg(tool))] };
}

async function secDefaultToolList(api, event, next) {
  let policy;
  try { policy = await secDefaultPolicy(api); }
  catch { policy = undefined; }
  return secDefaultRestoreTools(policy, await next.to(event, 'append'), await next(event));
}

async function secDefaultToolRegister(api, event, next) {
  if (next.origin.tier === 'prepend' || next.origin.tier === 'append') return next.to(event, 'append');
  if (next.origin.tier === 'user') {
    let managedAllowlistOrUnavailable = true;
    try {
      managedAllowlistOrUnavailable = Array.isArray((await secDefaultPolicy(api))?.allowedMcpServers);
    } catch { /* The policy read failed: preserve the managed refusal. */ }
    if (managedAllowlistOrUnavailable) {
      return { deny: 'allowedMcpServers (managed): plugins outside policy may not add tools' };
    }
  }
  return next(event);
}

function secDefaultToolDescribe(_api, event, next) {
  return ['user', 'builtin', 'core'].includes(event.provider?.tier)
    ? next(event)
    : next.to(event, 'append');
}

function secDefaultManagedModsOnly(policy) {
  const value = policy?.pluginConfigs?.['cc-plugin-sec-default@builtin']?.options?.allowManagedModsOnly;
  return value !== undefined && value !== false;
}

function secDefaultPluginRefusal(name) {
  return `mods are limited to your organization's by policy (allowManagedModsOnly); ${name} was not loaded`;
}

async function secDefaultPluginRegister(api, event, next) {
  return secDefaultManagedModsOnly(await secDefaultPolicy(api))
    ? { refuse: secDefaultPluginRefusal(event.name) } : next(event);
}

async function secDefaultPluginRegisterCatch(api, event, next) {
  api.ui.log(`plugin.register policy check failed for ${event.name}: ${next.error?.message ?? next.error?.kind}`, { to: 'debug' });
  return next.called ? next(event) : { refuse: secDefaultPluginRefusal(event.name) };
}

async function secDefaultToolCheck(api, event, next) {
  const answer = await next(event);
  const looseners = secDefaultLooseners(next.trace);
  if (answer.decision === 'deny' || looseners.length === 0) return answer;
  let hold;
  try { hold = secDefaultDenyRulesHold(await secDefaultPolicy(api)); }
  catch { hold = true; }
  if (!hold) return answer;
  const managed = await next.to(event, 'append');
  if (managed.decision !== 'deny' || managed.rule === undefined) return answer;
  for (const plugin of looseners) {
    if (secDefaultNotices.has(plugin)) continue;
    secDefaultNotices.add(plugin);
    api.ui.log(`${plugin} tried to lift a deny rule in your settings from a ${event.tool} call (${managed.rule}); the deny rule holds over the plugins you install (allowModsToOverrideDenyRules)`);
  }
  return managed;
}

async function secDefaultToolCheckCatch(api, event, next) {
  const answer = await next(event).catch(() => undefined);
  let hold;
  try { hold = secDefaultDenyRulesHold(await secDefaultPolicy(api)); }
  catch { hold = true; }
  return hold ? secDefaultCaughtAnswer(answer, next.trace) : answer;
}

function settleTrace(node, entry) {
  node.entry = freeze(entry);
}

function makeBudget(ms) {
  let remaining = ms;
  let runningSince = performance.now();
  let paused = 0;
  let expired = false;
  let finished = false;
  let timer;
  let expire;
  const deadline = new Promise(resolve => { expire = resolve; });
  function remainingNow() {
    return expired ? 0 : Math.max(0, remaining - (paused ? 0 : performance.now() - runningSince));
  }
  function markExpired() {
    if (expired || finished) return;
    expired = true;
    remaining = 0;
    clearTimeout(timer);
    expire(budgetExpired);
  }
  function arm() {
    if (finished || expired || paused) return;
    const left = remainingNow();
    if (left <= 0) markExpired();
    else {
      clearTimeout(timer);
      timer = setTimeout(markExpired, left);
      timer.unref?.();
    }
  }
  arm();
  return {
    deadline,
    read: () => freeze({ ms, remainingMs: remainingNow() }),
    isExpired: () => { if (remainingNow() <= 0) markExpired(); return expired; },
    pause: () => {
      if (finished || expired) return;
      const left = remainingNow();
      if (paused++ === 0) {
        remaining = left;
        clearTimeout(timer);
        if (remaining <= 0) markExpired();
      }
    },
    resume: () => {
      if (finished || expired || paused === 0) return;
      if (--paused === 0) {
        runningSince = performance.now();
        arm();
      }
    },
    finish: () => { finished = true; clearTimeout(timer); },
  };
}

// vm's timeout can interrupt the synchronous part of a hooks-module function.
// Continuations after an external Promise await still need separate isolation.
function invokeHook(fn, args, budget) {
  if (budget.isExpired()) throw budgetExpired;
  context[invocationSlot] = { fn, args };
  try {
    return invokeScript.runInContext(context, {
      timeout: Math.max(1, Math.ceil(budget.read().remainingMs)),
    });
  } catch (error) {
    if (error?.code === 'ERR_SCRIPT_EXECUTION_TIMEOUT') throw budgetExpired;
    throw error;
  } finally {
    delete context[invocationSlot];
  }
}

function stableResultKey(value) {
  try {
    return JSON.stringify(value, (_key, item) => {
      if (!item || typeof item !== 'object' || Array.isArray(item)) return item;
      return Object.fromEntries(Object.keys(item).sort().map(key => [key, item[key]]));
    });
  } catch {
    return Symbol('unserializable result');
  }
}

function toolCallContext(result, downstream) {
  if (result.context === undefined) return downstream.length === 0 || downstream.every(
    answer => answer.deny !== undefined || answer.context === undefined || answer.context.length === 0);
  const context = result.context;
  if (!Array.isArray(context)) return false;
  for (let i = 0; i < context.length; i++) {
    if (!Object.hasOwn(context, i) || typeof context[i] !== 'string' || context[i] === '') return false;
  }
  const answered = downstream.filter(answer => answer?.deny === undefined);
  const resultKey = stableResultKey(result.result);
  const matching = answered.filter(answer => stableResultKey(answer.result) === resultKey);
  const required = matching.length === 0 ? answered : matching;
  return required.every(answer => {
    const counts = new Map();
    for (const item of context) counts.set(item, (counts.get(item) ?? 0) + 1);
    for (const item of answer.context ?? []) {
      const remaining = counts.get(item) ?? 0;
      if (remaining === 0) return false;
      counts.set(item, remaining - 1);
    }
    return true;
  });
}

function validPromptSubmitContext(context) {
  if (context === undefined) return true;
  if (!Array.isArray(context)) return false;
  for (let index = 0; index < context.length; index++) {
    if (!Object.hasOwn(context, index) || typeof context[index] !== 'string'
        || context[index] === '') return false;
  }
  return true;
}

function promptSubmitForwarded(original, forwarded) {
  return forwarded && typeof forwarded === 'object' && !Array.isArray(forwarded)
    && typeof forwarded.text === 'string'
    && forwarded.wait === original.wait
    && equalJson(forwarded.origin, original.origin)
    && validPromptSubmitContext(forwarded.context);
}

function sessionReceiveForwarded(original, forwarded) {
  return forwarded && typeof forwarded === 'object' && !Array.isArray(forwarded)
    && typeof forwarded.text === 'string'
    && equalJson(forwarded.origin, original.origin)
    && equalJson(forwarded.event, original.event)
    && forwarded.agentId === original.agentId;
}

function validSessionCompactMessages(messages) {
  return Array.isArray(messages)
    && messages.every(message => message && typeof message === 'object' && !Array.isArray(message));
}

function sessionCompactForwarded(original, forwarded) {
  return forwarded && typeof forwarded === 'object' && !Array.isArray(forwarded)
    && forwarded.trigger === original.trigger
    && forwarded.agentId === original.agentId
    && (forwarded.instructions === undefined || typeof forwarded.instructions === 'string')
    && validSessionCompactMessages(forwarded.messages);
}

function validSessionAppendMessage(message) {
  return validExactEventObject(message,
    new Set(['type', 'name', 'role', 'isMeta', 'content']), ['type', 'content'])
    && typeof message.type === 'string'
    && (message.name === undefined || typeof message.name === 'string')
    && (message.role === undefined || typeof message.role === 'string')
    && (message.isMeta === undefined || typeof message.isMeta === 'boolean')
    && Array.isArray(message.content) && plainJson(message.content);
}

function sessionAppendArgumentError(event) {
  const keys = new Set(['message', 'door', 'origin', 'uuid', 'agentId']);
  if (!validExactEventObject(event, keys, ['message', 'door', 'origin', 'uuid'])) {
    return 'session.append takes { message, door, origin, uuid, agentId? }';
  }
  if (!validSessionAppendMessage(event.message)
      || typeof event.door !== 'string'
      || !plainJson(event.origin)
      || typeof event.uuid !== 'string' || event.uuid.length === 0
      || (event.agentId !== undefined
        && (typeof event.agentId !== 'string' || event.agentId.length === 0))) {
    return 'session.append needs a message with block content and string door/uuid data';
  }
  return undefined;
}

function sessionAppendForwarded(original, forwarded) {
  return validExactEventObject(forwarded,
    new Set(['message', 'door', 'origin', 'uuid', 'agentId']),
    ['message', 'door', 'origin', 'uuid'])
    && validSessionAppendMessage(forwarded.message)
    && forwarded.door === original.door
    && equalJson(forwarded.origin, original.origin)
    && forwarded.uuid === original.uuid
    && forwarded.agentId === original.agentId
    && forwarded.message.type === original.message.type
    && forwarded.message.name === original.message.name
    && forwarded.message.role === original.message.role
    && forwarded.message.isMeta === original.message.isMeta;
}

function validSessionAppendResult(input, result) {
  return validExactEventObject(result, new Set(['message', 'uuid']), ['message', 'uuid'])
    && typeof result.uuid === 'string' && result.uuid === input.uuid
    && validSessionAppendMessage(result.message)
    && result.message.type === input.message.type
    && result.message.name === input.message.name
    && result.message.role === input.message.role
    && result.message.isMeta === input.message.isMeta;
}

function validModUiViewport(viewport) {
  return viewport && typeof viewport === 'object' && !Array.isArray(viewport)
    && Object.keys(viewport).every(key => ['columns', 'rows', 'isFullscreen'].includes(key))
    && Number.isSafeInteger(viewport.columns) && viewport.columns > 0
    && Number.isSafeInteger(viewport.rows) && viewport.rows > 0
    && typeof viewport.isFullscreen === 'boolean';
}

function validModUiClientId(clientId) {
  return typeof clientId === 'string' && clientId.length >= 1 && clientId.length <= 64
    && /^[A-Za-z0-9._-]+$/.test(clientId);
}

function validRemoteModUiSurface(surface) {
  return surface === 'desktop' || surface === 'mobile' || surface === 'vscode';
}

function sessionAttachArgumentError(event) {
  const keys = new Set(['surface', 'clientId', 'viewport']);
  if (!validExactEventObject(event, keys, ['surface', 'clientId'])) {
    return 'session.attach takes { surface, clientId, viewport? }';
  }
  if (!validRemoteModUiSurface(event.surface) || !validModUiClientId(event.clientId)
      || (Object.hasOwn(event, 'viewport') && !validModUiViewport(event.viewport))) {
    return 'session.attach needs a remote surface, valid clientId, and optional measured viewport';
  }
  return undefined;
}

function sessionAttachForwarded(original, forwarded) {
  return validExactEventObject(forwarded,
    new Set(['surface', 'clientId', 'viewport']), ['surface', 'clientId'])
    && forwarded.surface === original.surface
    && forwarded.clientId === original.clientId
    && Object.hasOwn(forwarded, 'viewport') === Object.hasOwn(original, 'viewport')
    && (!Object.hasOwn(original, 'viewport')
      || equalJson(forwarded.viewport, original.viewport))
    && (!Object.hasOwn(forwarded, 'viewport') || validModUiViewport(forwarded.viewport));
}

function sessionDetachArgumentError(event) {
  const keys = new Set(['surface', 'clientId', 'reason']);
  if (!validExactEventObject(event, keys, ['surface', 'clientId', 'reason'])) {
    return 'session.detach takes { surface, clientId, reason }';
  }
  if (!validRemoteModUiSurface(event.surface) || !validModUiClientId(event.clientId)
      || event.reason !== 'detach' && event.reason !== 'end') {
    return 'session.detach needs a remote surface, valid clientId, and detach/end reason';
  }
  return undefined;
}

function sessionDetachForwarded(original, forwarded) {
  return validExactEventObject(forwarded,
    new Set(['surface', 'clientId', 'reason']), ['surface', 'clientId', 'reason'])
    && forwarded.surface === original.surface
    && forwarded.clientId === original.clientId
    && forwarded.reason === original.reason;
}

function validSessionSurfaceEventResult(input, result) {
  return validExactEventObject(result, new Set(['clientId']), ['clientId'])
    && result.clientId === input.clientId;
}

function uiPressArgumentError(event) {
  const keys = new Set(['plugin', 'element', 'component', 'requestId', 'surface', 'link']);
  if (!validExactEventObject(event, keys,
      ['plugin', 'element', 'component', 'requestId', 'surface'])) {
    return 'ui.press takes { plugin, element, component, requestId, surface }';
  }
  if (typeof event.plugin !== 'string' || event.plugin.length === 0
      || typeof event.element !== 'string'
      || typeof event.component !== 'string' || event.component.length === 0
      || typeof event.requestId !== 'string' || event.requestId.length === 0
      || event.surface !== 'terminal' && event.surface !== 'desktop') {
    return 'ui.press needs a plugin, element, component, requestId, and supported surface';
  }
  if (event.link !== undefined && !validUiPressLink(event.link)) {
    return 'ui.press link must be { href } with a string href of at most 2048 characters';
  }
  return undefined;
}

function validUiPressLink(link) {
  return link !== null && typeof link === 'object' && typeof link.href === 'string';
}

function uiPressForwarded(original, forwarded) {
  return validExactEventObject(forwarded,
    new Set(['plugin', 'element', 'component', 'requestId', 'surface', 'link']),
    ['plugin', 'element', 'component', 'requestId', 'surface'])
    && forwarded.plugin === original.plugin
    && forwarded.element === original.element
    && forwarded.component === original.component
    && forwarded.requestId === original.requestId
    && forwarded.surface === original.surface
    && (original.link === undefined
      ? forwarded.link === undefined : validUiPressLink(forwarded.link));
}

function uiInputArgumentError(event) {
  const keys = new Set(['plugin', 'element', 'component', 'requestId', 'surface', 'kind', 'value']);
  if (!validExactEventObject(event, keys,
      ['plugin', 'element', 'component', 'requestId', 'surface', 'kind', 'value'])) {
    return 'ui.input takes { plugin, element, component, requestId, surface, kind, value }';
  }
  if (typeof event.plugin !== 'string' || event.plugin.length === 0 || event.plugin.length > 256
      || typeof event.element !== 'string'
      || typeof event.component !== 'string'
      || !uiResolveSites.has(`${event.surface}:${event.component}`)
      || typeof event.requestId !== 'string' || event.requestId.length === 0 || event.requestId.length > 256
      || !validRemoteModUiSurface(event.surface)
      || event.kind !== 'change' && event.kind !== 'submit'
      || typeof event.value !== 'string') {
    return 'ui.input needs a valid Parent element identity, change/submit kind, and string value';
  }
  return undefined;
}

function uiInputForwarded(original, forwarded) {
  return validExactEventObject(forwarded,
    new Set(['plugin', 'element', 'component', 'requestId', 'surface', 'kind', 'value']),
    ['plugin', 'element', 'component', 'requestId', 'surface', 'kind', 'value'])
    && forwarded.plugin === original.plugin
    && forwarded.element === original.element
    && forwarded.component === original.component
    && forwarded.requestId === original.requestId
    && forwarded.surface === original.surface
    && (forwarded.kind === 'change' || forwarded.kind === 'submit')
    && typeof forwarded.value === 'string';
}

function uiSelectArgumentError(event) {
  const keys = new Set(['plugin', 'element', 'component', 'requestId', 'surface', 'value']);
  if (!validExactEventObject(event, keys,
      ['plugin', 'element', 'component', 'requestId', 'surface', 'value'])) {
    return 'ui.select takes { plugin, element, component, requestId, surface, value }';
  }
  if (typeof event.plugin !== 'string' || event.plugin.length === 0 || event.plugin.length > 256
      || typeof event.element !== 'string'
      || typeof event.component !== 'string'
      || !uiResolveSites.has(`${event.surface}:${event.component}`)
      || typeof event.requestId !== 'string' || event.requestId.length === 0 || event.requestId.length > 256
      || !validRemoteModUiSurface(event.surface)
      || typeof event.value !== 'string') {
    return 'ui.select needs a valid Parent element identity and string value';
  }
  return undefined;
}

function uiSelectForwarded(original, forwarded) {
  return validExactEventObject(forwarded,
    new Set(['plugin', 'element', 'component', 'requestId', 'surface', 'value']),
    ['plugin', 'element', 'component', 'requestId', 'surface', 'value'])
    && forwarded.plugin === original.plugin
    && forwarded.element === original.element
    && forwarded.component === original.component
    && forwarded.requestId === original.requestId
    && forwarded.surface === original.surface
    && typeof forwarded.value === 'string';
}

function validUiInputResult(input, result) {
  return validExactEventObject(result, new Set(['handled', 'element', 'value']), ['handled'])
    && typeof result.handled === 'boolean'
    && (result.element === undefined || result.element === input.element)
    && (result.value === undefined || typeof result.value === 'string');
}

function validUiSelectResult(input, result) {
  return validExactEventObject(result, new Set(['handled', 'element', 'value']), ['handled'])
    && typeof result.handled === 'boolean'
    && (result.element === undefined || result.element === input.element)
    && (result.value === undefined || typeof result.value === 'string');
}

function uiInputSelectActionMatches(event, token, expectedType, initialSelectValue) {
  const action = uiPressActions.get(uiPressActionKey(token));
  const siteRevision = activeUiRenderRevisions.get(
    uiPressSiteKey(event.surface, event.component, event.requestId),
  );
  return Boolean(action
    && action.plugin === event.plugin
    && action.key === event.element
    && action.surface === event.surface
    && action.component === event.component
    && action.requestId === event.requestId
    && action.elementType === expectedType
    && action.handle === token.handle
    && token.plugin === action.plugin
    && action.renderRevision === siteRevision
    && (expectedType !== 'Select' || action.selectValues?.includes(initialSelectValue)));
}

function uiPressActionIdentityMatches(event, token) {
  const action = uiPressActions.get(uiPressActionKey(token));
  const siteRevision = activeUiRenderRevisions.get(
    uiPressSiteKey(event.surface, event.component, event.requestId),
  );
  return Boolean(action
    && action.plugin === event.plugin
    && action.key === event.element
    && action.surface === event.surface
    && action.component === event.component
    && action.requestId === event.requestId
    && action.handle === token.handle
    && token.plugin === action.plugin
    && action.renderRevision === siteRevision);
}

function uiPressInputAdmitted(event, token, pressHrefAdmitted) {
  if (!uiPressActionIdentityMatches(event, token)) return false;
  const action = uiPressActions.get(uiPressActionKey(token));
  if (action.elementType === 'Markdown') {
    return validUiPressLink(event.link) && pressHrefAdmitted === true;
  }
  return action.elementType === 'Button' && event.link === undefined;
}

const NATIVE_CONTROL_CHARACTERS = /[\x00-\x1f\x7f-\x9f]/;
const NATIVE_INVISIBLE_PREFIX = new RegExp(
  '^[\\s\\u2800\\uFFF9-\\uFFFB\\p{Cc}\\p{M}\\p{Default_Ignorable_Code_Point}]+', 'u',
);
const NATIVE_PERCENT_RUN = /(?:%[0-9A-Fa-f]{2}){1,512}/g;
const NATIVE_PERCENT_START = /^%[0-9A-Fa-f]{2}/;
const NATIVE_WINDOWS_DEVICE_PREFIX = /^[\\/]\?\?[\\/]/;
const NATIVE_SPECIAL_PATH_COMPONENT = /^\.(?:vol|file|nofollow|resolve)$/i;

function normalizeParentPressHref(href, cwd) {
  const candidate = /^www\./i.test(href) ? `http://${href}` : nativeLocalHrefInput(href, cwd) ?? href;
  try {
    return new URL(candidate).href;
  } catch {
    return candidate;
  }
}

function nativeLocalHrefInput(input, cwd) {
  const isFile = /^file:/i.test(input);
  if (!isFile && !(input.startsWith('/') && !nativeUnsafeDevicePath(input))) return input;

  let pathAndSuffix = isFile ? input.slice(5) : input;
  if (pathAndSuffix.startsWith('//')) {
    pathAndSuffix = pathAndSuffix.slice(2);
    if (pathAndSuffix === 'localhost') pathAndSuffix = '/';
    else if (pathAndSuffix.startsWith('localhost/')) pathAndSuffix = pathAndSuffix.slice(9);
  }

  const suffixAt = pathAndSuffix.search(/[#?]/);
  const suffix = suffixAt === -1 ? '' : pathAndSuffix.slice(suffixAt);
  let pathname = suffixAt === -1 ? pathAndSuffix : pathAndSuffix.slice(0, suffixAt);
  if (pathname === '') return null;
  try {
    pathname = decodeURIComponent(pathname);
  } catch {
    // Native keeps malformed percent sequences and continues with path resolution.
  }
  if (/^\/[A-Za-z]:(?=[\\/]|$)/.test(pathname) && isAbsolute(pathname.slice(1))) {
    pathname = pathname.slice(1);
  }

  const absolutePath = isAbsolute(pathname) ? pathname : resolve(cwd, pathname);
  const fileHref = nativePathToSafeFileHref(absolutePath);
  if (fileHref === null) return null;
  const href = fileHref + suffix;
  return nativeUnsafeFileHref(href) ? null : href;
}

function nativePathToSafeFileHref(pathname) {
  try {
    const normalized = nativePathPrefix(pathname);
    const resolved = nativePathPrefix(resolve(normalized));
    if (nativeUnsafeResolvedPath(normalized)
        || nativeUnsafeDevicePath(resolved)
        || nativeUnsafePath(resolved)
        || nativeUnsafePath(normalized)) return null;
    const url = pathToFileURL(normalized);
    return url.hostname !== '' || nativeUnsafeFileHref(url.href) ? null : url.href;
  } catch {
    return null;
  }
}

function nativePathPrefix(pathname) {
  if (pathname.startsWith('\\\\?\\UNC\\')) return `\\\\${pathname.slice(8)}`;
  if (pathname.startsWith('\\\\?\\') && pathname.length >= 7 && pathname[5] === ':') {
    return pathname.slice(4);
  }
  return pathname;
}

function nativeUnsafeDevicePath(pathname) {
  const windowsNormalized = win32Path.normalize(pathname);
  return /^[\\/]{2}/.test(pathname)
    || NATIVE_WINDOWS_DEVICE_PREFIX.test(pathname)
    || pathname.includes('??') && NATIVE_WINDOWS_DEVICE_PREFIX.test(windowsNormalized);
}

function nativeUnsafeResolvedPath(pathname) {
  return nativeUnsafeDevicePath(pathname)
    || nativeHasNetworkMount(pathname)
    || nativeHasNetworkRoot(pathname)
    || nativeHasNetRoot(pathname)
    || nativeHasSpecialVolume(pathname);
}

function nativeUnsafePath(pathname) {
  return nativeUnsafeResolvedPath(pathname)
    || nativeUnsafePosixPath(pathname);
}

function nativeUnsafePosixPath(pathname) {
  const normalized = posixPath.normalize(pathname).replace(/^\.\.\//, '/');
  return nativeUnsafeResolvedPath(pathname)
    || nativeUnsafeResolvedPath(normalized)
    || (posixPath.normalize(pathname) === '..'
      || posixPath.normalize(pathname).startsWith('../'))
      && nativeUnsafeResolvedPath(`/${pathname}`);
}

function nativePathSegments(pathname) {
  const segments = [];
  for (const segment of pathname.split('/')) {
    if (segment === '' || segment === '.') continue;
    if (segment === '..') segments.pop();
    else segments.push(segment);
  }
  return segments;
}

function nativeFoldPathSegment(value) {
  return value.replace(/[\u200c-\u200f\u202a-\u202e\u206a-\u206f\ufeff]/g, '')
    .toUpperCase().toLowerCase();
}

function nativeHasNetworkMount(pathname) {
  if (!pathname.startsWith('/')) return false;
  const segments = nativePathSegments(pathname);
  return (segments.length >= 2 && nativeFoldPathSegment(segments[0]) === 'net')
    || (segments.length >= 3 && nativeFoldPathSegment(segments[0]) === 'network'
      && nativeFoldPathSegment(segments[1]) === 'servers');
}

function nativeHasNetworkRoot(pathname) {
  return pathname.startsWith('/')
    && nativePathSegments(pathname)[0]?.toLowerCase() === 'network';
}

function nativeHasNetRoot(pathname) {
  const segments = nativePathSegments(pathname);
  return pathname.startsWith('/') && segments.length === 1 && segments[0].toLowerCase() === 'net';
}

function nativeHasSpecialVolume(pathname) {
  if (!/\.(?:vol|file|nofollow|resolve)(?:\/|$)/i.test(pathname)
      || !pathname.startsWith('/')) return false;
  const segments = [];
  for (const segment of pathname.split('/')) {
    if (segment === '' || segment === '.') continue;
    if (segment === '..') segments.pop();
    else {
      segments.push(segment);
      if (segments.length === 1 && NATIVE_SPECIAL_PATH_COMPONENT.test(segment)) return true;
    }
  }
  return false;
}

function nativePercentVariants(pathname) {
  if (!pathname.includes('%')) return [pathname, pathname];
  const decoder = new TextDecoder('utf-8', { fatal: false, ignoreBOM: true });
  const decoded = pathname.replace(NATIVE_PERCENT_RUN, (run, offset) => {
    const next = pathname.slice(offset + run.length, offset + run.length + 3);
    return decoder.decode(nativePercentBytes(run), { stream: NATIVE_PERCENT_START.test(next) });
  });
  const byteString = pathname.replace(NATIVE_PERCENT_RUN, run =>
    Array.from(nativePercentBytes(run), byte => String.fromCharCode(byte)).join(''));
  return [decoded, byteString];
}

function nativePercentBytes(run) {
  return Uint8Array.from(run.slice(1).split('%'), pair => Number.parseInt(pair, 16));
}

function nativeUnsafeFileHref(href) {
  const path = href.slice(7);
  return nativeUnsafeFileUrlPath(path) || nativeUnsafeFileUrlPath(path.slice(1));
}

function nativeUnsafeFileUrlPath(pathname) {
  const variants = nativePercentVariants(pathname);
  return nativeUnsafeDevicePath(pathname)
    || nativeUnsafePosixPath(pathname)
    || NATIVE_CONTROL_CHARACTERS.test(variants[0])
    || variants.some(value => {
      const visible = value.replace(NATIVE_INVISIBLE_PREFIX, '');
      return nativeUnsafeDevicePath(visible) || nativeUnsafePosixPath(visible);
    });
}

function parentPressHrefPreflight(message) {
  const keys = new Set(['id', 'kind', 'href', 'pressableLinks', 'cwd', 'apiContextTicket']);
  if (!message || typeof message !== 'object' || Array.isArray(message)
      || Object.keys(message).some(key => !keys.has(key))
      || message.kind !== 'ui.press.preflight'
      || typeof message.href !== 'string' || message.href.length > 2_048
      || typeof message.cwd !== 'string' || !isAbsolute(message.cwd)
      || (Object.hasOwn(message, 'apiContextTicket')
        && (typeof message.apiContextTicket !== 'string' || message.apiContextTicket.length === 0))
      || Object.hasOwn(message, 'pressableLinks')
        && (!Array.isArray(message.pressableLinks) || message.pressableLinks.length > 256
          || message.pressableLinks.some(href => typeof href !== 'string' || href.length > 2_048))) {
    return false;
  }
  if (!Object.hasOwn(message, 'pressableLinks')) return true;
  const candidate = normalizeParentPressHref(message.href, message.cwd);
  return message.pressableLinks.some(href => normalizeParentPressHref(href, message.cwd) === candidate);
}

function uiSelectionArgumentError(event) {
  return validExactEventObject(event, new Set(), [])
    ? undefined : 'ui.selection takes no arguments';
}

function uiSelectionForwarded(_original, forwarded) {
  return uiSelectionArgumentError(forwarded) === undefined;
}

function validExactEventObject(value, keys, requiredKeys) {
  if (!value || typeof value !== 'object' || Array.isArray(value)
      || Object.keys(value).some(key => !keys.has(key))) return false;
  return requiredKeys.every(key => Object.hasOwn(value, key));
}

function agentOfferArgumentError(event) {
  const keys = new Set(['agent', 'description', 'source', 'provider']);
  if (!validExactEventObject(event, keys, ['agent', 'description', 'source'])) {
    return 'agent.offer takes only { agent, description, source, provider? }';
  }
  if (typeof event.agent !== 'string' || event.agent.length === 0
      || typeof event.description !== 'string' || typeof event.source !== 'string'
      || (Object.hasOwn(event, 'provider') && !plainJson(event.provider))) {
    return 'agent.offer needs string agent, description, and source fields with JSON provider data';
  }
  return undefined;
}

function restoreAgentOffer(original, forwarded) {
  if (!validExactEventObject(forwarded,
      new Set(['agent', 'description', 'source', 'provider']), ['agent', 'description', 'source'])
      || typeof forwarded.agent !== 'string' || forwarded.agent !== original.agent
      || typeof forwarded.description !== 'string'
      || typeof forwarded.source !== 'string' || forwarded.source !== original.source) return undefined;
  const restored = { ...forwarded };
  if (!Object.hasOwn(restored, 'provider') && Object.hasOwn(original, 'provider')) {
    restored.provider = original.provider;
  }
  return Object.hasOwn(restored, 'provider') === Object.hasOwn(original, 'provider')
      && equalJson(restored.provider, original.provider)
    ? restored : undefined;
}

function sessionMeasureArgumentError(event) {
  const keys = new Set(['context', 'rateLimits', 'cost', 'changed']);
  if (!validExactEventObject(event, keys, ['context', 'rateLimits', 'changed'])) {
    return 'session.measure takes only { context, rateLimits, cost?, changed }';
  }
  if (!plainJson(event.context) || !plainJson(event.rateLimits)
      || (Object.hasOwn(event, 'cost') && !plainJson(event.cost))
      || !Array.isArray(event.changed) || !plainJson(event.changed)) {
    return 'session.measure needs JSON context/rateLimits/cost data and a changed array';
  }
  return undefined;
}

function sessionMeasureForwarded(original, forwarded) {
  return validExactEventObject(forwarded,
    new Set(['context', 'rateLimits', 'cost', 'changed']), ['context', 'rateLimits', 'changed'])
    && equalJson(forwarded.context, original.context)
    && equalJson(forwarded.rateLimits, original.rateLimits)
    && Object.hasOwn(forwarded, 'cost') === Object.hasOwn(original, 'cost')
    && equalJson(forwarded.cost, original.cost)
    && equalJson(forwarded.changed, original.changed);
}

function sessionUsageArgumentError(event) {
  if (event === undefined) return undefined;
  if (!event || typeof event !== 'object' || Array.isArray(event)) {
    return 'takes { breakdown, columns } or nothing';
  }
  const extra = Object.keys(event).filter(key => key !== 'breakdown' && key !== 'columns');
  if (extra.length > 0) {
    return `takes { breakdown, columns } or nothing (not ${extra.join(', ')})`;
  }
  const breakdown = event.breakdown;
  if (breakdown !== undefined && breakdown !== 'summary' && breakdown !== 'full') {
    return `takes breakdown "summary" or "full" (got ${String(breakdown)})`;
  }
  const columns = event.columns;
  if (columns !== undefined && !(typeof columns === 'number' && Number.isInteger(columns) && columns > 0)) {
    return `takes columns, a positive whole number (got ${String(columns)})`;
  }
  return undefined;
}

function validSessionUsageValue(value) {
  const exactObject = (candidate, allowed, required) => candidate
    && typeof candidate === 'object' && !Array.isArray(candidate)
    && Object.keys(candidate).every(key => allowed.has(key))
    && required.every(key => Object.hasOwn(candidate, key));
  if (!exactObject(value,
    new Set(['startedAt', 'context', 'rateLimits', 'cost']),
    ['startedAt', 'context', 'rateLimits', 'cost'])
      || typeof value.startedAt !== 'string'
      || !exactObject(value.context,
        new Set(['window', 'tokens', 'percent', 'breakdown']), ['window'])
      || !Number.isSafeInteger(value.context.window) || value.context.window < 0
      || !Array.isArray(value.rateLimits)
      || !exactObject(value.cost, new Set(['usd']), ['usd'])
      || typeof value.cost.usd !== 'number' || !Number.isFinite(value.cost.usd) || value.cost.usd < 0) {
    return false;
  }
  const context = value.context;
  if (Object.hasOwn(context, 'tokens') !== Object.hasOwn(context, 'percent')
      || (Object.hasOwn(context, 'tokens')
        && (!Number.isSafeInteger(context.tokens) || context.tokens <= 0
          || typeof context.percent !== 'number' || !Number.isFinite(context.percent)
          || context.percent < 0 || context.percent > 100))
      || (Object.hasOwn(context, 'breakdown') && !plainJson(context.breakdown))) {
    return false;
  }
  return value.rateLimits.every(limit => exactObject(limit,
    new Set(['kind', 'percentUsed', 'resetsAt']), ['kind', 'percentUsed', 'resetsAt'])
    && ['five_hour', 'seven_day', 'spend_limit'].includes(limit.kind)
    && typeof limit.percentUsed === 'number' && Number.isFinite(limit.percentUsed)
    && limit.percentUsed >= 0 && limit.percentUsed <= 100
    && typeof limit.resetsAt === 'string');
}

function validSessionCompactUsage(usage) {
  return usage && typeof usage === 'object' && !Array.isArray(usage)
    && ['input_tokens', 'output_tokens', 'cache_read_input_tokens', 'cache_creation_input_tokens']
      .every(key => typeof usage[key] === 'number' && Number.isFinite(usage[key]) && usage[key] >= 0);
}

function validSessionCompactCoreResult(result) {
  return result && typeof result === 'object' && !Array.isArray(result)
    && validSessionCompactMessages(result.messages)
    && (result.tokensBefore === undefined
      || typeof result.tokensBefore === 'number' && Number.isFinite(result.tokensBefore)
        && result.tokensBefore >= 0)
    && (result.tokensAfter === undefined
      || typeof result.tokensAfter === 'number' && Number.isFinite(result.tokensAfter)
        && result.tokensAfter >= 0)
    && (result.usage === undefined || validSessionCompactUsage(result.usage));
}

function validSessionCompactResult(input, result, downstream) {
  if (!result || typeof result !== 'object' || Array.isArray(result)) return false;
  if (Object.hasOwn(result, 'skip')) {
    const compactRan = downstream.some(validSessionCompactCoreResult);
    return typeof result.skip === 'string' && result.skip.length > 0
      && !Object.hasOwn(result, 'messages')
      && (input.trigger === 'precompute' || !compactRan);
  }
  return validSessionCompactCoreResult(result);
}

function promptAttachmentForwarded(original, forwarded) {
  return forwarded && typeof forwarded === 'object' && !Array.isArray(forwarded)
    && forwarded.type === original.type
    && typeof forwarded.text === 'string'
    && (forwarded.origin === undefined || equalJson(forwarded.origin, original.origin))
    && (forwarded.agentId === undefined || forwarded.agentId === original.agentId);
}

function restorePromptAttachment(original, forwarded) {
  return { ...forwarded, origin: original.origin,
    ...(original.agentId === undefined ? {} : { agentId: original.agentId }) };
}

function toolDescribeForwarded(original, forwarded) {
  return forwarded && typeof forwarded === 'object' && !Array.isArray(forwarded)
    && forwarded.tool === original.tool
    && equalJson(forwarded.provider, original.provider)
    && typeof forwarded.description === 'string'
    && (forwarded.isDeferred === undefined || typeof forwarded.isDeferred === 'boolean');
}

function restoreToolDescribe(answer, previous) {
  if (answer.isDeferred !== undefined || previous?.isDeferred === undefined) return answer;
  return { ...answer, isDeferred: previous.isDeferred };
}

function validCommandDescribeResult(input, result) {
  if (!result || typeof result !== 'object' || Array.isArray(result)
      || typeof result.description !== 'string'
      || (result.argumentHint !== undefined && typeof result.argumentHint !== 'string')
      || typeof result.isHidden !== 'boolean') return false;
  return (result.description === input.description || result.description.length <= 4096)
    && (result.argumentHint === undefined || result.argumentHint === input.argumentHint
      || result.argumentHint.length <= 4096);
}

function commandDescribeForwarded(original, forwarded) {
  return forwarded && typeof forwarded === 'object' && !Array.isArray(forwarded)
    && forwarded.command === original.command
    && forwarded.immediate === original.immediate
    && (forwarded.provider === undefined || equalJson(forwarded.provider, original.provider))
    && validCommandDescribeResult(original, forwarded);
}

function restoreCommandDescribe(original, forwarded) {
  return { ...forwarded, provider: original.provider };
}

function validPromptSubmitResult(input, result, downstream) {
  if (!result || typeof result !== 'object' || Array.isArray(result)) return false;
  if (result.drop !== undefined) {
    return typeof result.drop === 'string'
      && (result.drop.length <= 4096 || downstream.some(answer => answer?.drop === result.drop))
      && result.context === undefined;
  }
  return typeof result.text === 'string'
    && (result.origin === undefined || equalJson(result.origin, input.origin))
    && validPromptSubmitContext(result.context);
}

function validOperationResult(event, input, result, downstream = []) {
  if (event === 'prompt.submit') return validPromptSubmitResult(input, result, downstream);
  if (event === 'agent.offer') return validExactEventObject(result,
    new Set(['isOffered']), ['isOffered']) && typeof result.isOffered === 'boolean';
  if (event === 'session.measure') return validExactEventObject(result,
    new Set(['changed']), ['changed']) && Array.isArray(result.changed) && plainJson(result.changed);
  if (event === 'ui.render') {
    if (validUiTree(result, 0, { value: 0 }, input?.surface ?? 'terminal')) return true;
    return result?.type === 'engine' && typeof result.ref === 'string'
      && downstream.some(answer => answer?.type === 'engine' && answer.ref === result.ref);
  }
  if (event === 'ui.resolve') return validUiElementTable(result);
  if (event === 'ui.press') return validExactEventObject(result,
    new Set(['handled', 'element']), ['handled'])
    && typeof result.handled === 'boolean'
    && (result.element === undefined || typeof result.element === 'string');
  if (event === 'ui.input') return validUiInputResult(input, result);
  if (event === 'ui.select') return validUiSelectResult(input, result);
  if (event === 'ui.selection') {
    if (!validExactEventObject(result, new Set(['value', 'deny']), [])) return false;
    if (Object.hasOwn(result, 'deny')) {
      return !Object.hasOwn(result, 'value') && typeof result.deny === 'string';
    }
    return Object.hasOwn(result, 'value')
      && (result.value === undefined || plainJson(result.value));
  }
  if (event === 'session.append') return validSessionAppendResult(input, result);
  if (event === 'session.attach' || event === 'session.detach') {
    return validSessionSurfaceEventResult(input, result);
  }
  if (event === 'session.receive') return result && typeof result === 'object'
    && !Array.isArray(result)
    && (typeof result.text === 'string'
      ? result.consumed === undefined
      : typeof result.consumed === 'string' && result.text === undefined);
  if (event === 'session.compact') return validSessionCompactResult(input, result, downstream);
  if (event === 'tool.call') {
    if (!result || typeof result !== 'object' || Array.isArray(result)) return false;
    if (result.deny !== undefined) {
      return typeof result.deny === 'string' && !Object.hasOwn(result, 'result');
    }
    return Object.hasOwn(result, 'result') && toolCallContext(result, downstream);
  }
  if (event === 'agent.spawn') return result && typeof result === 'object' && !Array.isArray(result)
    && (typeof result.deny === 'string'
      ? result.model === undefined && result.agentId === undefined
      : typeof result.model === 'string' && result.deny === undefined
        && (result.agentId === undefined || typeof result.agentId === 'string'));
  if (event === 'plugin.register') return result && typeof result === 'object' && !Array.isArray(result)
    && ((result.allow === true && result.refuse === undefined)
      || (typeof result.refuse === 'string' && result.allow === undefined));
  if (event === 'command.run') return result && typeof result === 'object' && !Array.isArray(result)
    && (result.text === undefined || typeof result.text === 'string')
    && (result.context === undefined || (Array.isArray(result.context)
      && result.context.every(line => typeof line === 'string')))
    && (result.ref === undefined || Number.isSafeInteger(result.ref));
  if (event === 'prompt.context') return validPromptContext(result);
  if (event === 'prompt.compose') return validPromptCompose(result);
  if (event === 'prompt.section') return result && typeof result === 'object'
    && !Array.isArray(result) && Object.hasOwn(result, 'text')
    && (typeof result.text === 'string' || result.text === null);
  if (event === 'prompt.attachment') return result && typeof result === 'object'
    && !Array.isArray(result) && Object.hasOwn(result, 'text')
    && (typeof result.text === 'string' || result.text === null);
  if (event === 'tool.describe') return result && typeof result === 'object'
    && !Array.isArray(result) && typeof result.description === 'string'
    && (result.isDeferred === undefined || typeof result.isDeferred === 'boolean');
  if (event === 'command.describe') return validCommandDescribeResult(input, result);
  if (event === 'session.start') return result && typeof result === 'object'
    && !Array.isArray(result) && typeof result.cwd === 'string';
  if (event === 'session.end') return result && typeof result === 'object'
    && !Array.isArray(result) && typeof result.sessionId === 'string';
  if (event === 'turn.start') return result && typeof result === 'object'
    && !Array.isArray(result) && typeof result.turnId === 'string';
  if (event === 'turn.complete') return result && typeof result === 'object'
    && !Array.isArray(result) && typeof result.text === 'string';
  if (!operationEvents.has(event)) return true;
  if (!result || typeof result !== 'object' || Array.isArray(result)) return false;
  if (event === 'tool.check') return ['allow', 'ask', 'deny'].includes(result.decision)
    && (result.reason === undefined || typeof result.reason === 'string')
    && (result.rule === undefined || typeof result.rule === 'string');
  if (Object.hasOwn(result, 'deny')) return typeof result.deny === 'string';
  if (!Object.hasOwn(result, 'value')) return false;
  const value = result.value;
  if (event === 'session.usage') return validSessionUsageValue(value);
  switch (event) {
    case 'ui.log':
    case 'telemetry.log':
    case 'telemetry.mark':
    case 'ui.toast':
    case 'ui.status':
    case 'clock.sleep':
    case 'clock.after':
    case 'clock.every': return value === undefined;
    case 'store.set':
    case 'store.delete': return value === undefined;
    case 'env.set': return value === undefined;
    case 'store.get': return true;
    case 'env.get': return value === undefined || typeof value === 'string';
    case 'store.keys': return Array.isArray(value) && value.every(key => typeof key === 'string');
    case 'tool.register': return value && typeof value === 'object' && !Array.isArray(value)
      && typeof value.tool === 'string';
    case 'tool.list': return Array.isArray(value) && value.every(tool => tool
      && typeof tool.name === 'string' && typeof tool.description === 'string'
      && typeof tool.mcp === 'boolean');
    case 'command.list': return Array.isArray(value) && value.every(command => command
      && typeof command.name === 'string' && typeof command.description === 'string'
      && ['builtin', 'plugin', 'user', 'mcp'].includes(command.source)
      && (command.plugin === undefined || typeof command.plugin === 'string'));
    case 'command.register': return value && typeof value === 'object' && !Array.isArray(value)
      && typeof value.command === 'string';
    case 'process.run': return value && typeof value === 'object' && !Array.isArray(value)
      && Number.isInteger(value.exitCode) && typeof value.stdout === 'string'
      && typeof value.stderr === 'string';
    case 'clock.now': return typeof value === 'number' && Number.isFinite(value);
    case 'session.cwd':
    case 'session.root':
    case 'session.model':
    case 'session.id': return typeof value === 'string';
    case 'session.turns': return Number.isSafeInteger(value) && value >= 0;
    case 'session.repo': return value === null || (value && typeof value === 'object' && !Array.isArray(value)
      && typeof value.root === 'string' && (value.remote === null || typeof value.remote === 'string')
      && typeof value.internal === 'boolean' && (value.name === null || typeof value.name === 'string'));
    case 'session.version': return true;
    case 'session.messages': return Array.isArray(value);
    case 'session.surfaces': return Array.isArray(value) && value.every(surface => ['terminal', 'desktop', 'mobile', 'vscode'].includes(surface));
    case 'session.surface': return value === null || ['terminal', 'desktop', 'mobile', 'vscode'].includes(value);
    case 'model.fork': return value && typeof value === 'object' && !Array.isArray(value)
      && typeof value.isAnswered === 'boolean'
      && (value.isAnswered ? typeof value.text === 'string' : typeof value.reason === 'string')
      && (value.reason === 'nothing-to-fork' && value.isAnswered === false && value.usage === undefined
        || value.usage && ['input_tokens', 'output_tokens', 'cache_read_input_tokens', 'cache_creation_input_tokens']
          .every(key => Number.isSafeInteger(value.usage[key]) && value.usage[key] >= 0));
    case 'model.complete': return value && typeof value === 'object' && !Array.isArray(value)
      && typeof value.isAnswered === 'boolean'
      && (value.isAnswered ? typeof value.text === 'string' : typeof value.reason === 'string')
      && value.usage && ['input_tokens', 'output_tokens', 'cache_read_input_tokens', 'cache_creation_input_tokens']
        .every(key => Number.isSafeInteger(value.usage[key]) && value.usage[key] >= 0);
    case 'model.classify': return value === undefined || typeof value === 'string';
    case 'state.get': return value && typeof value === 'object' && !Array.isArray(value)
      && Number.isSafeInteger(value.version) && value.version >= 0;
    case 'state.set': return value && typeof value === 'object' && !Array.isArray(value)
      && typeof value.isSet === 'boolean'
      && Number.isSafeInteger(value.version) && value.version >= 0;
    case 'settings.read': return value && typeof value === 'object' && !Array.isArray(value);
    case 'fs.read': return input?.as === 'bytes'
      ? value && typeof value === 'object' && typeof value.base64 === 'string'
      : typeof value === 'string';
    case 'fs.write': return value === undefined;
    case 'fs.exists': return typeof value === 'boolean';
    case 'fs.list': return Array.isArray(value);
    case 'fs.ancestors': return Array.isArray(value);
    case 'fs.stat': return value && typeof value === 'object' && !Array.isArray(value);
    default: return false;
  }
}

const instructionKinds = new Set(['managed', 'user', 'project', 'local', 'memory']);
const instructionPreamble = 'Codebase and user instructions are shown below. Be sure to adhere to these instructions. IMPORTANT: These instructions OVERRIDE any default behavior and you MUST follow them exactly as written.';
const instructionDescriptions = {
  managed: ' (organization-managed policy instructions)',
  user: " (user's private global instructions for all projects)",
  project: ' (project instructions, checked into the codebase)',
  local: " (user's private project instructions, not checked in)",
  memory: " (user's auto-memory, persists across conversations)",
};

function validPromptContext(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value) || !Array.isArray(value.blocks)
      || value.blocks.length > 32) return false;
  for (const block of value.blocks) {
    if (!block || typeof block !== 'object' || typeof block.name !== 'string'
        || !block.name || typeof block.text !== 'string') return false;
  }
  if (value.instructionFiles === undefined) return true;
  if (!Array.isArray(value.instructionFiles)) return false;
  const paths = new Set();
  for (const file of value.instructionFiles) {
    if (!file || typeof file !== 'object' || typeof file.path !== 'string' || !file.path
        || !instructionKinds.has(file.kind) || typeof file.content !== 'string'
        || (file.parent !== undefined && typeof file.parent !== 'string')
        || paths.has(file.path)) return false;
    paths.add(file.path);
  }
  return true;
}

function validPromptCompose(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)
      || !Array.isArray(value.sections)) return false;
  const ids = new Set();
  let inSession = false;
  for (const section of value.sections) {
    if (!section || typeof section !== 'object' || Array.isArray(section)
        || typeof section.id !== 'string' || !section.id || ids.has(section.id)
        || typeof section.text !== 'string'
        || !['shared', 'session'].includes(section.scope)) return false;
    if (section.scope === 'shared' && inSession) return false;
    if (section.scope === 'session') inSession = true;
    ids.add(section.id);
  }
  return true;
}

function renderInstructionFiles(files) {
  if (files.length === 0) return '';
  return instructionPreamble + '\n\n' + files.map(file =>
    `Contents of ${file.path}${instructionDescriptions[file.kind]}:\n\n${file.content.trim()}`
  ).join('\n\n');
}

function contextText(blocks) {
  return blocks?.find(block => block?.name === 'instructions')?.text;
}

function restorePromptContext(candidate, previous) {
  if (!validPromptContext(candidate) || !validPromptContext(previous)) return candidate;
  if (previous.instructionFiles === undefined) return { ...candidate, instructionFiles: undefined };
  const files = candidate.instructionFiles ?? previous.instructionFiles;
  const oldText = contextText(previous.blocks);
  const newText = contextText(candidate.blocks);
  const textChanged = oldText !== newText;
  const filesChanged = !equalJson(files, previous.instructionFiles);
  if (!textChanged && filesChanged) {
    const text = renderInstructionFiles(files);
    const blocks = candidate.blocks.some(block => block.name === 'instructions')
      ? candidate.blocks.map(block => block.name === 'instructions' ? { ...block, text } : block)
      : [{ name: 'instructions', text }, ...candidate.blocks];
    return { ...candidate, blocks, instructionFiles: files };
  }
  if (!textChanged || newText === renderInstructionFiles(files)) {
    return { ...candidate, instructionFiles: files };
  }
  return { ...candidate, instructionFiles: undefined };
}

function equalJson(left, right) {
  if (Object.is(left, right)) return true;
  if (!left || !right || typeof left !== 'object' || typeof right !== 'object') return false;
  if (Array.isArray(left) !== Array.isArray(right)) return false;
  const leftKeys = Object.keys(left).sort();
  const rightKeys = Object.keys(right).sort();
  return leftKeys.length === rightKeys.length && leftKeys.every((key, index) =>
    key === rightKeys[index] && equalJson(left[key], right[key]));
}

function pinnedToolCheck(original, forwarded) {
  return forwarded && typeof forwarded === 'object' && !Array.isArray(forwarded)
    && forwarded.tool === original?.tool
    && Object.hasOwn(forwarded, 'input') === Object.hasOwn(original, 'input')
    && equalJson(forwarded.input, original.input)
    && Object.hasOwn(forwarded, 'tool_use_id') === Object.hasOwn(original, 'tool_use_id')
    && forwarded.tool_use_id === original.tool_use_id;
}

function toolCallForwarded(original, forwarded) {
  return forwarded && typeof forwarded === 'object' && !Array.isArray(forwarded)
    && ['tool', 'tool_use_id', 'agentId', 'consent', '$shadowed']
      .every(key => Object.is(forwarded[key], original?.[key]));
}

function agentSpawnForwarded(original, forwarded) {
  if (!forwarded || typeof forwarded !== 'object' || Array.isArray(forwarded)
      || typeof forwarded.prompt !== 'string'
      || (forwarded.prompt !== original?.prompt && forwarded.prompt.trim() === '')
      || typeof forwarded.description !== 'string'
      || typeof forwarded.subagentType !== 'string'
      || typeof forwarded.background !== 'boolean'
      || (forwarded.model !== undefined && forwarded.model !== null
        && typeof forwarded.model !== 'string')
      || (forwarded.cwd !== original?.cwd
        && forwarded.cwd !== undefined
        && (typeof forwarded.cwd !== 'string' || !isAbsolute(forwarded.cwd)))) return undefined;
  // Native `restoreArgument` fills only parentAgentId and provider when a hook
  // omits them. The other identity fields must be passed through explicitly.
  const restored = { ...forwarded };
  for (const key of ['parentAgentId', 'provider']) {
    if (!Object.hasOwn(restored, key) && Object.hasOwn(original, key)) restored[key] = original[key];
  }
  for (const key of ['tool_use_id', 'name', 'fork', 'parentModel', 'permissionMode', 'parentAgentId', 'provider']) {
    if (!equalJson(restored[key], original[key])) return undefined;
  }
  return restored;
}

function commandRunForwarded(original, forwarded) {
  if (!forwarded || typeof forwarded !== 'object' || Array.isArray(forwarded)
      || forwarded.command !== original.command || typeof forwarded.args !== 'string'
      || !equalJson(forwarded.origin, original.origin)
      || (forwarded.presentation !== undefined
        && !equalJson(forwarded.presentation, original.presentation))) return undefined;
  return forwarded.presentation === undefined
    ? { ...forwarded, presentation: original.presentation } : forwarded;
}

function turnCompleteForwarded(original, forwarded) {
  if (!forwarded || typeof forwarded !== 'object' || Array.isArray(forwarded)
      || typeof forwarded.answer !== 'string'
      || (Object.hasOwn(forwarded, 'agentId') && forwarded.agentId !== original?.agentId)) {
    return undefined;
  }
  return original?.agentId !== undefined && !Object.hasOwn(forwarded, 'agentId')
    ? { ...forwarded, agentId: original.agentId } : forwarded;
}

function matches(pattern, value, depth = 0) {
  if (depth > 8) return false;
  if (typeof pattern === 'string' && value && typeof value === 'object'
      && typeof value.kind === 'string') return pattern === value.kind;
  if (Object.prototype.toString.call(pattern) === '[object RegExp]') {
    return typeof value === 'string' && pattern.test(value);
  }
  if (Array.isArray(pattern)) {
    if (Array.isArray(value)) {
      return value.some(item => pattern.some(option => matches(option, item, depth + 1)));
    }
    return pattern.some(option => matches(option, value, depth + 1));
  }
  if (pattern && typeof pattern === 'object') {
    if (Array.isArray(value)) return value.some(item => matches(pattern, item, depth + 1));
    if (!value || typeof value !== 'object') return false;
    return Object.entries(pattern).every(([key, expected]) =>
      Object.hasOwn(value, key) && matches(expected, value[key], depth + 1));
  }
  return Object.is(pattern, value);
}

function matcherKey(value) {
  if (Object.prototype.toString.call(value) === '[object RegExp]') {
    return `regexp:${value.source}/${value.flags}`;
  }
  if (Array.isArray(value)) return `array:[${value.map(matcherKey).join(',')}]`;
  if (value && typeof value === 'object') {
    return `object:{${Object.keys(value).sort().map(key =>
      JSON.stringify(key) + ':' + matcherKey(value[key])).join(',')}}`;
  }
  return JSON.stringify(value);
}

function selectsEvent(pattern, event) {
  if (typeof pattern !== 'string') return false;
  // The native wildcard does not subscribe to either telemetry stream.
  if (event.startsWith('telemetry.') && (pattern === '*' || pattern.startsWith('!'))) return false;
  if (pattern.startsWith('!')) {
    const positive = pattern.slice(1);
    return positive !== '*' && !positive.startsWith('!') && isSupportedPattern(positive)
      && !selectsEvent(positive, event);
  }
  if (pattern === '*') return true;
  if (pattern.endsWith('.*')) return pattern.length > 2 && event.startsWith(pattern.slice(0, -1));
  return pattern === event;
}

function isSupportedPattern(pattern) {
  if (supportedEvents.has(pattern) || pattern === '*') return true;
  if (pattern.startsWith('!')) {
    const positive = pattern.slice(1);
    return positive !== '*' && !positive.startsWith('!') && isSupportedPattern(positive);
  }
  return pattern.endsWith('.*') && [...supportedEvents]
    .some(event => event.startsWith(pattern.slice(0, -1)));
}

function markCancelledApiCall(callId) {
  cancelledApiCalls.add(callId);
  if (cancelledApiCalls.size > 1024) {
    cancelledApiCalls.delete(cancelledApiCalls.values().next().value);
  }
}

function projectToolCallApiResult(answer) {
  if (answer && typeof answer === 'object' && !Array.isArray(answer)
      && typeof answer.deny === 'string') {
    return { deny: answer.deny };
  }
  const result = answer && typeof answer === 'object' && !Array.isArray(answer)
    ? answer : {};
  const projected = { result: result.result, text: result.text };
  if (result.isError === true) projected.isError = true;
  return projected;
}

function callApi(plugin, storageId, tier, requestId, method, input, budget, hookId, signal,
  pauseBudget = true, waitForAbortResult = false, apiContextTicket,
  generationContextTicket, uiRenderReadScope) {
  if (budget?.isExpired()) return expiredCall();
  const signals = (Array.isArray(signal) ? signal : [signal]).filter(Boolean);
  const abortedSignal = signals.find(item => item.aborted);
  if (abortedSignal && !waitForAbortResult) {
    return Promise.reject(abortedSignal.reason ?? new Error('operation aborted'));
  }
  if (pauseBudget) budget?.pause();
  const callId = nextCallId++;
  let accept;
  let reject;
  const result = new Promise((resolve, rejectResult) => {
    accept = resolve;
    reject = rejectResult;
  });
  pendingApi.set(callId, { requestId, method, accept, reject, cancelSent: false });
  const abort = event => {
    const pending = pendingApi.get(callId);
    if (!pending) return;
    if (waitForAbortResult) {
      if (!pending.cancelSent) {
        pending.cancelSent = true;
        send({ id: requestId, kind: 'api.cancel', callId });
      }
      return;
    }
    pendingApi.delete(callId);
    markCancelledApiCall(callId);
    send({ id: requestId, kind: 'api.cancel', callId });
    pending.reject(event?.target?.reason ?? new Error('operation aborted'));
  };
  for (const item of signals) item.addEventListener('abort', abort, { once: true });
  send({ id: requestId, kind: 'api', callId, method, plugin, storageId, tier, hookId,
    ...(typeof apiContextTicket === 'string' ? { apiContextTicket } : {}),
    ...(typeof generationContextTicket === 'string' ? { generationContextTicket } : {}),
    ...(uiRenderReadScope ? { uiRenderReadScope } : {}), input });
  if (abortedSignal && waitForAbortResult) abort({ target: abortedSignal });
  const finished = result.finally(() => {
    for (const item of signals) item.removeEventListener('abort', abort);
    if (pauseBudget) budget?.resume();
  });
  void finished.catch(() => {});
  return finished;
}

function trackVoidCall(requestId, call) {
  let pending = pendingVoidCalls.get(requestId);
  if (!pending) pendingVoidCalls.set(requestId, pending = new Set());
  pending.add(call);
  void call.then(() => {}, () => {}).finally(() => {
    pending.delete(call);
    if (pending.size === 0) pendingVoidCalls.delete(requestId);
  });
}

async function drainVoidCalls(requestId) {
  while (pendingVoidCalls.get(requestId)?.size) {
    await Promise.allSettled([...pendingVoidCalls.get(requestId)]);
  }
}

function cancelTimers(storageId) {
  for (const timer of pendingTimers.get(storageId) ?? []) timer.cancel();
  pendingTimers.delete(storageId);
}

const uiResolveSites = new Set(['terminal:AbovePrompt']);
const desktopUiComponents = new Set([
  'AskUserQuestion', 'UserMessage', 'AssistantMessage', 'ToolUse', 'ToolResult',
  'ToolGroup', 'ToolProgress', 'CommandOutput', 'Spinner', 'TurnDuration',
  'InfoNotice', 'SessionMode', 'PromptHint', 'AbovePrompt', 'Pane',
]);
for (const surface of ['desktop', 'mobile', 'vscode']) {
  for (const component of desktopUiComponents) uiResolveSites.add(`${surface}:${component}`);
}
const uiTreeElements = new Set([
  'Box', 'Text', 'div', 'span', 'b', 'Button', 'Input', 'Select', 'Link', 'Code',
  'Markdown', 'Svg', 'Client', 'engine',
]);

function uiSiteKey(storageId, surface, component) {
  return `${storageId}\u0000${surface}\u0000${component}`;
}

function uiResolveArgumentError(event) {
  if (!event || typeof event !== 'object' || Array.isArray(event)) {
    return 'takes a ui.render argument (e.surface names the surface)';
  }
  return uiResolveSites.has(`${event.surface}:${event.component}`)
    ? undefined
    : 'takes a ui.render argument (e.surface and e.component name a supported render site)';
}

function uiRenderArgumentError(event) {
  const siteError = uiResolveArgumentError(event);
  if (siteError) return siteError;
  if (typeof event.requestId !== 'string' || event.requestId.length === 0) {
    return `takes a requestId naming this ${event.surface === 'terminal' ? 'AbovePrompt' : 'render'} instance`;
  }
  if (!event.props || typeof event.props !== 'object' || Array.isArray(event.props)
      || !plainJson(event.props)) return 'takes props as a plain JSON object';
  if (event.surface !== 'terminal') {
    if (event.clientId !== undefined && !validModUiClientId(event.clientId)) {
      return 'takes a clientId of 1 to 64 letters, digits, dot, underscore or hyphen';
    }
    if (event.onScreen !== undefined && event.onScreen !== null
        && (!event.onScreen || typeof event.onScreen !== 'object' || Array.isArray(event.onScreen)
          || Object.keys(event.onScreen).some(key => !['first', 'last', 'of'].includes(key))
          || !Number.isSafeInteger(event.onScreen.first) || event.onScreen.first < 0
          || !Number.isSafeInteger(event.onScreen.last) || event.onScreen.last < event.onScreen.first
          || !Number.isSafeInteger(event.onScreen.of) || event.onScreen.of <= event.onScreen.last)) {
      return 'takes onScreen as { first, last, of } when present';
    }
    if (event.contentRows !== undefined && (!Number.isSafeInteger(event.contentRows) || event.contentRows < 0)) {
      return 'takes contentRows as a non-negative integer';
    }
    if (event.keyed !== undefined && (!Array.isArray(event.keyed) || event.keyed.length > 512
        || event.keyed.some(row => !row || typeof row !== 'object' || Array.isArray(row)
          || Object.keys(row).some(key => !['plugin', 'key', 'top', 'bottom'].includes(key))
          || typeof row.plugin !== 'string' || typeof row.key !== 'string'
          || !Number.isSafeInteger(row.top) || row.top < 0
          || !Number.isSafeInteger(row.bottom) || row.bottom < row.top))) {
      return 'takes keyed as at most 512 positioned Client rows';
    }
    if (event.bench !== undefined && (!event.bench || typeof event.bench !== 'object'
        || Array.isArray(event.bench)
        || Object.keys(event.bench).some(key => !['seq', 't0'].includes(key))
        || !Number.isSafeInteger(event.bench.seq) || !Number.isFinite(event.bench.t0))) {
      return 'takes bench as { seq, t0 } when present';
    }
    if (event.viewport !== undefined && (!event.viewport || typeof event.viewport !== 'object'
        || Array.isArray(event.viewport)
        || Object.keys(event.viewport).some(key => !['columns', 'rows', 'isFullscreen'].includes(key))
        || !Number.isSafeInteger(event.viewport.columns) || event.viewport.columns < 1
        || !Number.isSafeInteger(event.viewport.rows) || event.viewport.rows < 1
        || event.viewport.isFullscreen !== undefined && typeof event.viewport.isFullscreen !== 'boolean')) {
      return 'takes viewport with positive columns and rows';
    }
    return undefined;
  }
  const props = event.props;
  const propKeys = new Set(['hasSurvey', 'isWorking', 'maxRows', 'bodyColumns', 'scroll', 'view']);
  if (Object.keys(props).some(key => !propKeys.has(key))
      || typeof props.hasSurvey !== 'boolean' || typeof props.isWorking !== 'boolean'
      || !Number.isSafeInteger(props.maxRows) || props.maxRows < 0
      || !Number.isSafeInteger(props.bodyColumns) || props.bodyColumns < 1) {
    return 'takes AbovePrompt props { hasSurvey, isWorking, maxRows, bodyColumns, scroll, view }';
  }
  if (!props.scroll || typeof props.scroll !== 'object' || Array.isArray(props.scroll)
      || Object.keys(props.scroll).some(key => key !== 'offset' && key !== 'bodyRows')
      || !Number.isSafeInteger(props.scroll.offset) || props.scroll.offset < 0
      || !Number.isSafeInteger(props.scroll.bodyRows) || props.scroll.bodyRows < 0) {
    return 'takes scroll { offset, bodyRows } as non-negative integers';
  }
  if (!props.view || typeof props.view !== 'object' || Array.isArray(props.view)
      || Object.keys(props.view).some(key => key !== 'agentId')
      || (props.view.agentId !== undefined
        && (typeof props.view.agentId !== 'string' || props.view.agentId.length === 0))) {
    return 'takes view as an object with optional agentId';
  }
  if (event.viewport !== undefined && (!event.viewport || typeof event.viewport !== 'object'
      || Array.isArray(event.viewport)
      || Object.keys(event.viewport).some(key => !['columns', 'rows', 'isFullscreen'].includes(key))
      || !Number.isSafeInteger(event.viewport.columns) || event.viewport.columns < 1
      || !Number.isSafeInteger(event.viewport.rows) || event.viewport.rows < 1
      || typeof event.viewport.isFullscreen !== 'boolean')) {
    return 'takes viewport { columns, rows, isFullscreen } when it is measured';
  }
  return undefined;
}

function uiRenderForwarded(original, forwarded) {
  return forwarded && typeof forwarded === 'object' && !Array.isArray(forwarded)
    && forwarded.surface === original.surface
    && forwarded.component === original.component
    && forwarded.requestId === original.requestId
    && equalJson(forwarded.viewport, original.viewport)
    && uiRenderArgumentError(forwarded) === undefined;
}

function uiResolveForwarded(original, forwarded) {
  return forwarded && typeof forwarded === 'object' && !Array.isArray(forwarded)
    && forwarded.surface === original.surface && forwarded.component === original.component;
}

function uiPressActionKey(token) {
  return token && token.workerEpoch === undefined
    ? `${token.plugin}\u0000${token.handle}`
    : `${token.workerEpoch}\u0000${token.renderRevision}\u0000${token.handle}`;
}

function uiPressSiteKey(surface, component, requestId) {
  return `${surface}\u0000${component}\u0000${requestId}`;
}

function collectUiPressActionKeys(tree, into = new Set()) {
  if (!tree || typeof tree !== 'object' || Array.isArray(tree)) return into;
  if (tree.press && typeof tree.press === 'object') {
    into.add(uiPressActionKey(tree.press));
  }
  if (Array.isArray(tree.children)) {
    for (const child of tree.children) collectUiPressActionKeys(child, into);
  }
  return into;
}

function commitUiPressActions(event, renderRevision, tree) {
  const siteKey = uiPressSiteKey(event.surface, event.component, event.requestId);
  if (activeUiRenderRevisions.get(siteKey) !== renderRevision) {
    for (const [key, action] of uiPressActions) {
      if (action.surface === event.surface && action.component === event.component
          && action.requestId === event.requestId
          && action.renderRevision === renderRevision) uiPressActions.delete(key);
    }
    return false;
  }
  const visible = collectUiPressActionKeys(tree);
  for (const [key, action] of uiPressActions) {
    if (action.surface === event.surface && action.component === event.component
        && action.requestId === event.requestId
        && (action.renderRevision !== renderRevision || !visible.has(key))) {
      uiPressActions.delete(key);
    }
  }
  return true;
}

function beginUiRenderRevision(event) {
  if (uiRenderRevision >= Number.MAX_SAFE_INTEGER) {
    throw new Error('ui.render revision space exhausted; restart the Mod worker');
  }
  uiRenderRevision += 1;
  const siteKey = uiPressSiteKey(event.surface, event.component, event.requestId);
  activeUiRenderRevisions.set(siteKey, uiRenderRevision);
  return { surface: event.surface, component: event.component,
    requestId: event.requestId, renderRevision: uiRenderRevision };
}

function revokeUiPressActionsForStorage(storageId) {
  for (const [key, action] of uiPressActions) {
    if (action.storageId === storageId) uiPressActions.delete(key);
  }
}

function plainJsonObjectPrototype(value) {
  const prototype = Object.getPrototypeOf(value);
  if (prototype === null || prototype === Object.prototype) return true;
  if (Object.getPrototypeOf(prototype) !== null) return false;
  const constructor = Object.getOwnPropertyDescriptor(prototype, 'constructor')?.value;
  return typeof constructor === 'function'
    && constructor.prototype === prototype
    && Function.prototype.toString.call(constructor) === 'function Object() { [native code] }';
}

function plainJson(value, depth = 0) {
  if (depth > 32) return false;
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return true;
  if (typeof value === 'number') return Number.isFinite(value);
  if (Array.isArray(value)) return value.every(item => plainJson(item, depth + 1));
  if (!value || typeof value !== 'object' || !plainJsonObjectPrototype(value)) return false;
  return Object.values(value).every(item => plainJson(item, depth + 1));
}

function validUiProps(type, props) {
  const keys = type === 'Box' ? new Set(['flexDirection', 'columnGap'])
    : type === 'Button' ? new Set(['key', 'label', 'hotkey', 'action', 'plain',
      'dimColor', 'variant', 'role', 'autoFocus']) : new Set();
  if (Object.keys(props).some(key => !keys.has(key))) return false;
  if (type === 'Box') {
    if (props.flexDirection !== undefined && !['row', 'column'].includes(props.flexDirection)) return false;
    return props.columnGap === undefined
      || Number.isSafeInteger(props.columnGap) && props.columnGap >= 0 && props.columnGap <= 16;
  }
  if (type !== 'Button') return Object.keys(props).length === 0;
  return typeof props.key === 'string' && props.key.length > 0
    && typeof props.label === 'string'
    && (props.hotkey === undefined || typeof props.hotkey === 'string'
      && /^[0-9a-z]$/.test(props.hotkey))
    && (props.action === undefined || typeof props.action === 'string' && props.action.length > 0)
    && (props.plain === undefined || props.plain === true)
    && (props.dimColor === undefined || typeof props.dimColor === 'boolean')
    && (props.variant === undefined || props.variant === 'primary' || props.variant === 'secondary')
    && (props.role === undefined || props.role === 'dismiss')
    && (props.autoFocus === undefined || props.autoFocus === true);
}

const desktopElementProps = {
  Box: new Set([
    'key', 'flexDirection', 'flexGrow', 'flexShrink', 'flexWrap', 'alignItems', 'alignSelf',
    'justifyContent', 'gap', 'columnGap', 'rowGap', 'width', 'height', 'minWidth', 'minHeight',
    'margin', 'marginX', 'marginY', 'marginTop', 'marginBottom', 'marginLeft', 'marginRight',
    'padding', 'paddingX', 'paddingY', 'paddingTop', 'paddingBottom', 'paddingLeft', 'paddingRight',
    'borderStyle', 'borderColor', 'borderDimColor', 'backgroundColor', 'overflow', 'display',
    'position', 'top', 'left', 'right', 'bottom',
  ]),
  Text: new Set(['color', 'backgroundColor', 'dimColor', 'bold', 'italic', 'underline',
    'strikethrough', 'inverse', 'wrap']),
  Button: new Set(['key', 'label', 'hotkey', 'action', 'plain', 'dimColor', 'variant', 'role', 'autoFocus']),
  Input: new Set(['key', 'label', 'placeholder', 'value', 'submitLabel', 'autoFocus']),
  Select: new Set(['key', 'label', 'options', 'value', 'autoFocus']),
  Link: new Set(['href', 'label']),
  Code: new Set(['source', 'language', 'path', 'startLine', 'format', 'wrap']),
  Markdown: new Set(['key', 'text', 'dimColor', 'pressableLinks']),
  Svg: new Set(['source', 'alt', 'width', 'height', 'isInteractive']),
  Client: new Set(['key', 'module', 'props', 'width', 'height', 'flexGrow']),
};

const desktopStyleEnums = {
  flexDirection: new Set(['row', 'column', 'row-reverse', 'column-reverse']),
  flexWrap: new Set(['nowrap', 'wrap', 'wrap-reverse']),
  alignItems: new Set(['flex-start', 'center', 'flex-end', 'stretch']),
  alignSelf: new Set(['flex-start', 'center', 'flex-end', 'auto']),
  justifyContent: new Set(['flex-start', 'center', 'flex-end', 'space-between', 'space-around', 'space-evenly']),
  overflow: new Set(['visible', 'hidden']),
  display: new Set(['flex', 'none']),
  position: new Set(['relative', 'absolute']),
  borderStyle: new Set(['single', 'double', 'round', 'bold', 'singleDouble', 'doubleSingle', 'classic', 'arrow', 'dashed', 'quote']),
  wrap: new Set(['wrap', 'end', 'middle', 'truncate-end', 'truncate', 'truncate-middle', 'truncate-start']),
};
const desktopDimensionProps = new Set(['width', 'height', 'minWidth', 'minHeight']);
const desktopNumericStyleProps = new Set([
  'flexGrow', 'flexShrink', 'gap', 'columnGap', 'rowGap', 'margin', 'marginX', 'marginY',
  'marginTop', 'marginBottom', 'marginLeft', 'marginRight', 'padding', 'paddingX', 'paddingY',
  'paddingTop', 'paddingBottom', 'paddingLeft', 'paddingRight',
]);
const desktopOffsetProps = new Set(['top', 'left', 'right', 'bottom']);
const desktopColorProps = new Set(['color', 'backgroundColor', 'borderColor']);
const desktopBooleanStyleProps = new Set([
  'dimColor', 'borderDimColor', 'bold', 'italic', 'underline', 'strikethrough', 'inverse',
]);

function validDesktopStyleProps(type, props) {
  const allowed = desktopElementProps[type];
  if (!allowed || Object.keys(props).some(key => !allowed.has(key))) return false;
  if (type === 'Box' && props.key !== undefined
      && (typeof props.key !== 'string' || props.key.length === 0)) return false;
  for (const [key, choices] of Object.entries(desktopStyleEnums)) {
    if (props[key] !== undefined && !choices.has(props[key])) return false;
  }
  for (const key of desktopDimensionProps) {
    const value = props[key];
    if (value !== undefined && !(typeof value === 'number' && Number.isFinite(value)
        && value >= 0 && value <= 10_000
        || typeof value === 'string' && /^\d{1,3}%$/.test(value))) return false;
  }
  for (const key of desktopNumericStyleProps) {
    const value = props[key];
    if (value !== undefined && !(typeof value === 'number' && Number.isFinite(value)
        && Math.abs(value) <= 10_000)) return false;
  }
  for (const key of desktopOffsetProps) {
    const value = props[key];
    if (value !== undefined && !(Number.isSafeInteger(value) && Math.abs(value) <= 10_000)) return false;
  }
  for (const key of desktopColorProps) {
    const value = props[key];
    if (value !== undefined && !(typeof value === 'string' && value.length >= 1 && value.length <= 40
        && /^[#a-zA-Z0-9_().,% -]+$/.test(value))) return false;
  }
  for (const key of desktopBooleanStyleProps) {
    if (props[key] !== undefined && typeof props[key] !== 'boolean') return false;
  }
  if (type === 'Text') return Object.keys(props).every(key => desktopElementProps.Text.has(key));
  return true;
}

function validClientProps(props) {
  if (!props || typeof props !== 'object' || Array.isArray(props)
      || Object.keys(props).some(key => !desktopElementProps.Client.has(key))
      || typeof props.key !== 'string' || props.key.length === 0
      || typeof props.module !== 'string' || props.module.length === 0
      || props.props !== undefined && !plainJson(props.props)) return false;
  for (const key of ['width', 'height']) {
    const value = props[key];
    if (value !== undefined && !(typeof value === 'number' && Number.isFinite(value)
        && value >= 0 && value <= 10_000
        || typeof value === 'string' && /^[0-9]{1,3}%$/.test(value))) return false;
  }
  return props.flexGrow === undefined || typeof props.flexGrow === 'number'
    && Number.isFinite(props.flexGrow) && Math.abs(props.flexGrow) <= 10_000;
}

function validDesktopUiProps(type, props) {
  if (!props || typeof props !== 'object' || Array.isArray(props) || !plainJson(props)) return false;
  if (type === 'Box' || type === 'Text') return validDesktopStyleProps(type, props);
  if (type === 'div' || type === 'span' || type === 'b') {
    return Object.values(props).every(value => typeof value === 'string'
      || typeof value === 'boolean' || typeof value === 'number' && Number.isFinite(value));
  }
  const keys = desktopElementProps[type];
  if (!keys || Object.keys(props).some(key => !keys.has(key))) return false;
  if (type === 'Client') return validClientProps(props);
  if (type === 'Button') return typeof props.key === 'string' && props.key.length > 0
    && typeof props.label === 'string'
    && (props.hotkey === undefined || typeof props.hotkey === 'string' && /^[0-9a-z]$/.test(props.hotkey))
    && (props.action === undefined || typeof props.action === 'string' && props.action.length > 0)
    && (props.plain === undefined || props.plain === true)
    && (props.dimColor === undefined || typeof props.dimColor === 'boolean')
    && (props.variant === undefined || props.variant === 'primary' || props.variant === 'secondary')
    && (props.role === undefined || props.role === 'dismiss')
    && (props.autoFocus === undefined || props.autoFocus === true);
  if (type === 'Input') return typeof props.key === 'string' && props.key.length > 0
    && ['label', 'placeholder', 'value', 'submitLabel'].every(key => props[key] === undefined || typeof props[key] === 'string')
    && (props.autoFocus === undefined || props.autoFocus === true);
  if (type === 'Select') return typeof props.key === 'string' && props.key.length > 0
    && Array.isArray(props.options) && props.options.length > 0 && props.options.length <= 64
    && props.options.every(option => option && typeof option === 'object' && !Array.isArray(option)
      && typeof option.value === 'string'
      && Object.keys(option).every(key => key === 'value' || key === 'label')
      && (option.label === undefined || typeof option.label === 'string'))
    && new Set(props.options.map(option => option.value)).size === props.options.length
    && ['label', 'value'].every(key => props[key] === undefined || typeof props[key] === 'string')
    && (props.autoFocus === undefined || props.autoFocus === true);
  if (type === 'Link') return typeof props.href === 'string' && props.href.trim() !== ''
    && props.href.length <= 2048
    && (props.label === undefined || typeof props.label === 'string'
      && props.label.trim() !== '' && props.label.length <= 10_000);
  if (type === 'Code') return typeof props.source === 'string'
    && ['language', 'path'].every(key => props[key] === undefined || typeof props[key] === 'string')
    && (props.startLine === undefined || Number.isSafeInteger(props.startLine)
      && props.startLine >= 1 && props.startLine <= 1_000_000_000)
    && (props.format === undefined || props.format === 'source' || props.format === 'diff')
    && (props.wrap === undefined || props.wrap === 'wrap' || props.wrap === 'truncate-end');
  if (type === 'Markdown') return typeof props.text === 'string'
    && (props.key === undefined || typeof props.key === 'string' && props.key.length > 0)
    && (props.dimColor === undefined || typeof props.dimColor === 'boolean')
    && (props.pressableLinks === undefined || Array.isArray(props.pressableLinks)
      && props.pressableLinks.length <= 256
      && props.pressableLinks.every(href => typeof href === 'string'
        && href.length > 0 && href.length <= 2048));
  if (type === 'Svg') return typeof props.source === 'string' && props.source.length > 0
    && props.source.length <= 131_072 && typeof props.alt === 'string'
    && props.alt.length <= 10_000 && !/[\u0000-\u001f\u007f-\u009f]/.test(props.alt)
    && ['width', 'height'].every(key => props[key] === undefined
      || typeof props[key] === 'number' && Number.isFinite(props[key])
        && props[key] > 0 && props[key] <= 4096)
    && (props.isInteractive === undefined || typeof props.isInteractive === 'boolean');
  return false;
}

function validDesktopGroup(group) {
  return group && typeof group === 'object' && !Array.isArray(group)
    && Object.keys(group).length === 1 && typeof group.plugin === 'string'
    && group.plugin.length > 0 && group.plugin.length <= 256;
}

function validDesktopHoverProps(type, hover, props, group, press, hasKeyedBox) {
  if (!hover || typeof hover !== 'object' || Array.isArray(hover) || !plainJson(hover)) return false;
  const allowed = type === 'Box'
    ? new Set(['scope', 'borderStyle', 'borderColor', 'borderDimColor', 'backgroundColor',
      'display', 'top', 'left', 'right', 'bottom'])
    : type === 'Text' || type === 'Button'
      ? new Set(['scope', 'color', 'backgroundColor', 'dimColor', 'bold', 'italic',
        'underline', 'strikethrough', 'inverse'])
      : new Set(['scope']);
  if (Object.keys(hover).some(key => !allowed.has(key))) return false;
  if (Object.values(hover).some(value => value === null || typeof value === 'object'
      || typeof value === 'number' && !Number.isFinite(value))) return false;
  if (hover.scope !== undefined && (typeof hover.scope !== 'string' || hover.scope.length === 0
      || hover.scope.length > 64 || /[\u0000-\u001f\u007f-\u009f]/.test(hover.scope))) return false;
  if (hover.scope === undefined && !hasKeyedBox) return false;
  if (hover.scope !== undefined && type !== 'Button' && !validDesktopGroup(group)) return false;
  if (hover.scope !== undefined && type === 'Button' && !validParentPressToken(press)) return false;

  const colorKeys = type === 'Box' ? ['borderColor', 'backgroundColor'] : ['color', 'backgroundColor'];
  for (const key of colorKeys) {
    const value = hover[key];
    if (value !== undefined && !(typeof value === 'string' && value.length >= 1 && value.length <= 40
        && /^[#a-zA-Z0-9_().,% -]+$/.test(value))) return false;
  }
  const booleanKeys = type === 'Box'
    ? ['borderDimColor'] : ['dimColor', 'bold', 'italic', 'underline', 'strikethrough', 'inverse'];
  if (booleanKeys.some(key => hover[key] !== undefined && typeof hover[key] !== 'boolean')) return false;
  if (type === 'Box') {
    if (hover.borderStyle !== undefined
        && (!desktopStyleEnums.borderStyle.has(hover.borderStyle) || props.borderStyle === undefined)) return false;
    if (hover.display !== undefined && !(hover.display === 'flex' && props.display === 'none')) return false;
    for (const key of ['top', 'left', 'right', 'bottom']) {
      if (hover[key] !== undefined && !(Number.isSafeInteger(hover[key])
          && Math.abs(hover[key]) <= 10_000 && props.position === 'absolute')) return false;
    }
  }
  return true;
}

function hasHoverDescendant(node) {
  if (!Array.isArray(node?.children)) return false;
  return node.children.some(child => child && typeof child === 'object' && !Array.isArray(child)
    && (child.hover !== undefined || hasHoverDescendant(child)));
}

function validUiPressToken(token) {
  return token && typeof token === 'object' && !Array.isArray(token)
    && Object.keys(token).every(key => ['plugin', 'handle', 'workerEpoch', 'renderRevision'].includes(key))
    && typeof token.plugin === 'string' && token.plugin.length > 0
    && Number.isSafeInteger(token.handle) && token.handle > 0
    && typeof token.workerEpoch === 'string' && token.workerEpoch.length > 0
    && Number.isSafeInteger(token.renderRevision) && token.renderRevision > 0;
}

function validParentPressToken(token) {
  return token && typeof token === 'object' && !Array.isArray(token)
    && Object.keys(token).length === 2
    && typeof token.plugin === 'string' && token.plugin.length > 0
    && Number.isSafeInteger(token.handle) && token.handle > 0;
}

function flattenDesktopChildren(values, into = []) {
  for (const child of values) {
    if (child === null || child === undefined || typeof child === 'boolean') continue;
    if (Array.isArray(child)) flattenDesktopChildren(child, into);
    else into.push(typeof child === 'number' ? String(child) : child);
  }
  return into;
}

function desktopChildren(props, extraChildren) {
  if (extraChildren !== undefined) return flattenDesktopChildren(extraChildren);
  return flattenDesktopChildren(props?.children === undefined ? [] : [props.children]);
}

function desktopHover(type, props) {
  const hover = props?.hover;
  if (hover === undefined) return undefined;
  if (!['Box', 'Text', 'Button'].includes(type)) {
    throw new TypeError(`${type} does not accept hover`);
  }
  if (!hover || typeof hover !== 'object' || Array.isArray(hover) || !plainJson(hover)) {
    throw new TypeError(`${type} hover must be an object of style props`);
  }
  return hover;
}

let unresolvedDesktopPressHandle = 0;

function unresolvedDesktopPress() {
  unresolvedDesktopPressHandle += 1;
  if (!Number.isSafeInteger(unresolvedDesktopPressHandle)) unresolvedDesktopPressHandle = 1;
  return { plugin: '', handle: unresolvedDesktopPressHandle };
}

function makeDesktopUiElement(type, props, extraChildren) {
  if (props === undefined || props === null) props = {};
  if (!props || typeof props !== 'object' || Array.isArray(props)) {
    throw new TypeError(`${type} takes an object of props`);
  }
  const children = desktopChildren(props, extraChildren);
  const hover = desktopHover(type, props);
  if (type === 'Box' || type === 'Text' || type === 'div' || type === 'span' || type === 'b') {
    const clean = {};
    for (const [name, value] of Object.entries(props)) {
      if (name === 'ref' || name === 'children' || value === null || value === undefined
          || name === 'hover') continue;
      if (name === 'key') {
        if (type === 'Box' && (typeof value === 'string' || typeof value === 'number')) {
          clean.key = String(value);
        }
        continue;
      }
      clean[name] = value;
    }
    return {
      type,
      ...(Object.keys(clean).length > 0 && { props: clean }),
      ...(hover !== undefined && { hover }),
      ...(children.length > 0 && { children }),
    };
  }
  if (type === 'Button') {
    const { onPress, hotkey, action, plain, dimColor, variant, role, autoFocus, key, label } = props;
    const childLabel = children.length === 1 && typeof children[0] === 'string' ? children[0] : undefined;
    const buttonLabel = label ?? childLabel;
    const buttonKey = key ?? buttonLabel;
    if (typeof buttonLabel !== 'string' || typeof buttonKey !== 'string' || buttonKey.length === 0
        || typeof onPress !== 'function') throw new TypeError('Button needs a key, label, and onPress function');
    if (children.length > 0 && (childLabel === undefined || label !== undefined)) {
      throw new TypeError('Button takes one string child as its label, or no children');
    }
    const buttonProps = { key: buttonKey, label: buttonLabel };
    for (const [name, value] of Object.entries({ hotkey, action, plain, dimColor, variant, role, autoFocus })) {
      if (value !== undefined) buttonProps[name] = value;
    }
    if (!validDesktopUiProps('Button', buttonProps)) throw new TypeError('Button props do not match the Desktop schema');
    return { type, props: buttonProps, ...(hover !== undefined && { hover }), press: unresolvedDesktopPress(), onPress };
  }
  if (type === 'Input') {
    const { key, label, placeholder, value, submitLabel, autoFocus, onInput, onSubmit } = props;
    if (typeof key !== 'string' || key.length === 0 || typeof onSubmit !== 'function'
        || (onInput !== undefined && typeof onInput !== 'function') || children.length > 0) {
      throw new TypeError('Input needs a key, onSubmit function, optional onInput function, and no children');
    }
    const inputProps = { key };
    for (const [name, fieldValue] of Object.entries({ label, placeholder, value, submitLabel, autoFocus })) {
      if (fieldValue !== undefined) inputProps[name] = fieldValue;
    }
    if (!validDesktopUiProps('Input', inputProps)) throw new TypeError('Input props do not match the Desktop schema');
    return {
      type, props: inputProps, press: unresolvedDesktopPress(),
      onEvent: event => event?.kind === 'submit'
        ? onSubmit(event.value, event) : onInput === undefined ? undefined : onInput(event?.value, event),
    };
  }
  if (type === 'Select') {
    const { key, label, options, value, autoFocus, onSelect } = props;
    if (typeof key !== 'string' || key.length === 0 || typeof onSelect !== 'function' || children.length > 0) {
      throw new TypeError('Select needs a key, onSelect function, and no children');
    }
    const selectProps = { key, options };
    for (const [name, fieldValue] of Object.entries({ label, value, autoFocus })) {
      if (fieldValue !== undefined) selectProps[name] = fieldValue;
    }
    if (!validDesktopUiProps('Select', selectProps)) throw new TypeError('Select props do not match the Desktop schema');
    return { type, props: selectProps, press: unresolvedDesktopPress(), onEvent: event => onSelect(event?.value, event) };
  }
  if (type === 'Link') {
    const { href, label } = props;
    const linkProps = label === undefined ? { href } : { href, label };
    if (children.some(child => typeof child !== 'string')) throw new TypeError('Link children must be text');
    if (!validDesktopUiProps('Link', linkProps)) throw new TypeError('Link props do not match the Desktop schema');
    return { type, props: linkProps, ...(children.length > 0 && { children }) };
  }
  if (type === 'Code') {
    if (children.length > 0) throw new TypeError('Code takes no children');
    const codeProps = { source: props.source };
    for (const name of ['language', 'path', 'startLine', 'format', 'wrap']) {
      if (props[name] !== undefined) codeProps[name] = props[name];
    }
    if (!validDesktopUiProps('Code', codeProps)) throw new TypeError('Code props do not match the Desktop schema');
    return { type, props: codeProps };
  }
  if (type === 'Svg') {
    if (children.length > 0) throw new TypeError('Svg takes no children');
    const svgProps = {};
    for (const name of ['source', 'alt', 'width', 'height', 'isInteractive']) {
      if (props[name] !== undefined) svgProps[name] = props[name];
    }
    if (!validDesktopUiProps('Svg', svgProps)) throw new TypeError('Svg props do not match the Desktop schema');
    return { type, props: svgProps };
  }
  if (type === 'Markdown') {
    const { key, text, dimColor, onLinkPress, pressableLinks } = props;
    if (children.length > 0 || onLinkPress !== undefined && typeof onLinkPress !== 'function'
        || pressableLinks !== undefined && onLinkPress === undefined
        || onLinkPress !== undefined && (typeof key !== 'string' || key.length === 0)) {
      throw new TypeError('Markdown links require a keyed onLinkPress callback and no children');
    }
    const markdownProps = { text };
    for (const [name, value] of Object.entries({ key, dimColor, pressableLinks })) {
      if (value !== undefined) markdownProps[name] = value;
    }
    if (!validDesktopUiProps('Markdown', markdownProps)) throw new TypeError('Markdown props do not match the Desktop schema');
    return onLinkPress === undefined
      ? { type, props: markdownProps }
      : { type, props: markdownProps, press: unresolvedDesktopPress(), onEvent: event => onLinkPress(event?.link, event) };
  }
  if (type === 'Client') {
    const { module, key, props: data, data: dataAlias, width, height, flexGrow } = props;
    if (children.length > 0) throw new TypeError('Client takes no children');
    const clientProps = { key, module };
    const clientData = data === undefined ? dataAlias : data;
    if (clientData !== undefined) clientProps.props = clientData;
    for (const [name, value] of Object.entries({ width, height, flexGrow })) {
      if (value !== undefined) clientProps[name] = value;
    }
    if (!validClientProps(clientProps)) throw new TypeError('Client props do not match the Desktop schema');
    return { type, props: clientProps, client: { plugin: '' } };
  }
  throw new TypeError(`unsupported Desktop element ${type}`);
}

function makeUiButtonElement(props) {
  const { children = [], ...buttonProps } = props;
  const childList = Array.isArray(children) ? children : [children];
  const { onPress, ...inputProps } = buttonProps;
  if (Object.keys(buttonProps).some(key => !['key', 'label', 'hotkey', 'action', 'plain',
      'dimColor', 'variant', 'role', 'autoFocus', 'onPress'].includes(key))) {
    throw new TypeError('Button props are outside the terminal AbovePrompt subset');
  }
  const childLabel = childList.length === 1 && typeof childList[0] === 'string'
    ? childList[0] : undefined;
  const label = inputProps.label ?? childLabel;
  const key = inputProps.key ?? label;
  if (typeof label !== 'string' || typeof key !== 'string' || key.length === 0
      || typeof onPress !== 'function') {
    throw new TypeError('Button needs a non-empty key, a string label, and an onPress callback');
  }
  if (childList.length > 0 && (childLabel === undefined || inputProps.label !== undefined)) {
    throw new TypeError('Button takes one string child as its label, or no children');
  }
  const publicProps = { key, label };
  for (const name of ['hotkey', 'action', 'plain', 'dimColor', 'variant', 'role', 'autoFocus']) {
    if (inputProps[name] !== undefined) publicProps[name] = inputProps[name];
  }
  if (!plainJson(publicProps) || !validUiProps('Button', publicProps)) {
    throw new TypeError('Button props are outside the terminal AbovePrompt subset');
  }
  return { type: 'Button', props: publicProps, children: [], onPress };
}

function makeUiElement(type, props) {
  if (props === undefined) props = {};
  if (!props || typeof props !== 'object' || Array.isArray(props)) {
    throw new TypeError(`${type} takes an object of props`);
  }
  if (type === 'Button') return makeUiButtonElement(props);
  const { children = [], ...elementProps } = props;
  const normalizedChildren = Array.isArray(children) ? children : [children];
  if (type === 'Text' && normalizedChildren.some(child => typeof child !== 'string')) {
    throw new TypeError('Text children must be strings');
  }
  if (type === 'Box' && normalizedChildren.some(child => typeof child !== 'string'
      && !validUiTree(child))) {
    throw new TypeError('Box children must be strings or Box/Text elements');
  }
  if (!plainJson(elementProps) || !validUiProps(type, elementProps)) {
    throw new TypeError(`${type} props are outside the terminal AbovePrompt subset`);
  }
  return freeze({ type, props: elementProps, children: normalizedChildren });
}

function nextParentPressToken(plugin) {
  parentUiPressCounter += 1;
  if (!Number.isSafeInteger(parentUiPressCounter) || parentUiPressCounter < 1) {
    throw new Error('UI press handle space exhausted; restart the Mod worker');
  }
  return { plugin, handle: parentUiPressCounter };
}

function bindUiElementTable(table, context) {
  if (!context) return table;
  const bound = Object.create(null);
  for (const [name, constructor] of Object.entries(table)) {
    if (context.surface === 'terminal' && name === 'Button') {
      bound[name] = props => {
        const element = constructor(props);
        if (!element || element.type !== 'Button' || typeof element.onPress !== 'function'
            || !validUiProps('Button', element.props)) {
          throw new TypeError('Button constructor did not return a pressable Button');
        }
        const handle = (uiPressCounters.get(context.storageId) ?? 0) + 1;
        if (!Number.isSafeInteger(handle) || handle < 1) {
          throw new Error('Button press handle space exhausted; reload this Mod');
        }
        uiPressCounters.set(context.storageId, handle);
        const workerEpoch = uiStorageEpochs.get(context.storageId);
        if (typeof workerEpoch !== 'string') {
          throw new Error('Button press identity has no loaded worker epoch');
        }
        const token = { plugin: context.plugin, handle, workerEpoch,
          renderRevision: context.renderRevision };
        uiPressActions.set(uiPressActionKey(token), {
          ...token, storageId: context.storageId, tier: context.tier, root: context.root,
          hookId: context.hookId, surface: context.surface,
          component: context.component, requestId: context.requestId, key: element.props.key,
          elementType: 'Button', onPress: element.onPress,
        });
        return freeze({ type: 'Button', props: element.props, children: [], press: token });
      };
      continue;
    }
    if (context.surface !== 'terminal' && ['Button', 'Input', 'Select', 'Markdown'].includes(name)) {
      bound[name] = props => {
        const element = constructor(props);
        const callback = element?.onPress ?? element?.onEvent;
        const needsPress = name !== 'Markdown' || element?.press !== undefined;
        if (!element || element.type !== name || !validDesktopUiProps(name, element.props)
            || needsPress && typeof callback !== 'function') {
          throw new TypeError(`${name} constructor did not return a valid pressable element`);
        }
        if (!needsPress) return freeze(element);
        const token = nextParentPressToken(context.plugin);
        uiPressActions.set(uiPressActionKey(token), {
          ...token,
          storageId: context.storageId,
          tier: context.tier,
          root: context.root,
          hookId: context.hookId,
          surface: context.surface,
          component: context.component,
          requestId: context.requestId,
          key: element.props.key ?? '',
          elementType: name,
          ...(name === 'Select'
            ? { selectValues: element.props.options.map(option => option.value) } : {}),
          ...(name === 'Markdown' && element.props.pressableLinks !== undefined
            ? { pressableLinks: [...element.props.pressableLinks] } : {}),
          ...(typeof element.onPress === 'function' ? { onPress: element.onPress } : {}),
          ...(typeof element.onEvent === 'function' ? { onEvent: element.onEvent } : {}),
          renderRevision: context.renderRevision,
        });
        const { onPress, onEvent, ...visible } = element;
        void onPress;
        void onEvent;
        return freeze({ ...visible, press: token });
      };
      continue;
    }
    bound[name] = constructor;
  }
  return freeze(bound);
}

function baseUiElements(surface, component) {
  if (!uiResolveSites.has(`${surface}:${component}`)) return undefined;
  if (surface === 'terminal') return freeze(Object.assign(Object.create(null), {
    Box: props => makeUiElement('Box', props),
    Text: props => makeUiElement('Text', props),
    Button: props => makeUiElement('Button', props),
  }));

  const table = Object.create(null);
  for (const name of ['Box', 'Text', 'Button', 'Input', 'Select', 'Link', 'Code', 'Markdown', 'Svg', 'Client']) {
    table[name] = function(props) { return makeDesktopUiElement(name, props); };
  }
  table.Fragment = function Fragment(props = {}) {
    return { type: 'Box', props: { flexDirection: 'column' }, children: props.children ?? [] };
  };
  table.h = function h(type, props, ...children) {
    const flattened = flattenDesktopChildren(children);
    if (typeof type === 'function') return type({ ...(props ?? {}), children: flattened });
    if (type === 'Box' || type === 'Text' || type === 'div' || type === 'span' || type === 'b') {
      return makeDesktopUiElement(type, props ?? {}, flattened);
    }
    throw new TypeError(`JSX element <${String(type)}> is not a UI element from $.ui.resolve(e)`);
  };
  return freeze(table);
}

function validUiTree(value, depth = 0, count = { value: 0 }, surface = 'terminal', hoverContext = {}) {
  if (depth > 32 || ++count.value > 512 || !value || typeof value !== 'object'
      || Array.isArray(value) || !uiTreeElements.has(value.type)
      || value.props !== undefined && (!value.props || typeof value.props !== 'object'
        || Array.isArray(value.props) || !plainJson(value.props))) return false;
  if (surface === 'terminal' && value.props === undefined) return false;
  if (surface !== 'terminal') {
    if (value.type === 'engine') {
      return Object.keys(value).length === 2
        && Object.hasOwn(value, 'type') && Object.hasOwn(value, 'ref')
        && Number.isSafeInteger(value.ref);
    }
    const props = value.props ?? {};
    if (value.type === 'Client') {
      return Object.keys(value).every(key => ['type', 'props', 'client'].includes(key))
        && validClientProps(value.props)
        && value.client && typeof value.client === 'object' && !Array.isArray(value.client)
        && Object.keys(value.client).every(key => key === 'plugin')
        && typeof value.client.plugin === 'string' && value.client.plugin.length > 0;
    }
    if (!validDesktopUiProps(value.type, props)) return false;
    const structural = ['Box', 'Text', 'div', 'span', 'b'].includes(value.type);
    const link = value.type === 'Link';
    const keys = new Set(['type', 'props']);
    if (structural || link) keys.add('children');
    if (['Button', 'Input', 'Select', 'Markdown'].includes(value.type)) {
      keys.add('press');
    }
    if (['Box', 'Text', 'Button'].includes(value.type)) {
      keys.add('hover');
    }
    if (['Box', 'Text', 'div', 'span', 'b'].includes(value.type)) {
      keys.add('group');
    }
    if (Object.keys(value).some(key => !keys.has(key))) return false;
    if (value.group !== undefined && !validDesktopGroup(value.group)) return false;
    if (['Button', 'Input', 'Select'].includes(value.type)
        && !validParentPressToken(value.press)) return false;
    if (value.type === 'Markdown' && (props.pressableLinks?.length ?? 0) > 0
        && !validParentPressToken(value.press)) return false;
    if (value.press !== undefined && !validParentPressToken(value.press)) return false;
    const currentBoxKey = value.type === 'Box' && typeof props.key === 'string' && props.key !== ''
      ? props.key : undefined;
    const hasKeyedBox = currentBoxKey !== undefined || hoverContext.hasKeyedBox === true;
    if (value.hover !== undefined
        && !validDesktopHoverProps(value.type, value.hover, props, value.group, value.press, hasKeyedBox)) {
      return false;
    }
    if (value.type === 'Box' && props.display === 'none' && hasHoverDescendant(value)
        && value.hover?.display !== 'flex') return false;
    if (structural) {
      if (value.children === undefined) return true;
      if (!Array.isArray(value.children)) return false;
      return value.children.every(child => typeof child === 'string'
        ? child.length <= 10000 : validUiTree(child, depth + 1, count, surface,
          { hasKeyedBox }));
    }
    if (link) return value.children === undefined || Array.isArray(value.children)
      && value.children.every(child => typeof child === 'string' && child.length <= 10000);
    return true;
  }
  if (!Array.isArray(value.children) || !validUiProps(value.type, value.props)) return false;
  if (value.type === 'Text') return value.children.every(child => typeof child === 'string'
    && child.length <= 10000);
  if (value.type === 'Button') {
    const press = value.press;
    return value.children.length === 0
      && Object.keys(value).every(key => ['type', 'props', 'children', 'press'].includes(key))
      && validUiProps('Button', value.props)
      && press && typeof press === 'object' && !Array.isArray(press)
      && Object.keys(press).every(key => ['plugin', 'handle', 'workerEpoch', 'renderRevision'].includes(key))
      && typeof press.plugin === 'string' && press.plugin.length > 0
      && Number.isSafeInteger(press.handle) && press.handle > 0
      && typeof press.workerEpoch === 'string' && press.workerEpoch.length > 0
      && Number.isSafeInteger(press.renderRevision) && press.renderRevision > 0;
  }
  return value.children.every(child => typeof child === 'string'
    ? child.length <= 10000 : validUiTree(child, depth + 1, count));
}

function clientIdentityKey(plugin, props) {
  return JSON.stringify([plugin, props?.key, props?.module]);
}

function collectClientIdentities(value, into) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return;
  if (value.type === 'Client' && typeof value.client?.plugin === 'string') {
    into.add(clientIdentityKey(value.client.plugin, value.props));
    return;
  }
  if (Array.isArray(value.children)) {
    for (const child of value.children) collectClientIdentities(child, into);
  }
}

function uiGroupCapabilityKey(plugin, scope) {
  return JSON.stringify([plugin, scope]);
}

function collectUiGroupCapabilities(value, into) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return into;
  if ((value.type === 'Box' || value.type === 'Text')
      && typeof value.hover?.scope === 'string'
      && typeof value.group?.plugin === 'string' && value.group.plugin.length > 0) {
    const key = uiGroupCapabilityKey(value.group.plugin, value.hover.scope);
    into.set(key, (into.get(key) ?? 0) + 1);
  }
  if (Array.isArray(value.children)) {
    for (const child of value.children) collectUiGroupCapabilities(child, into);
  }
  return into;
}

function stampUiHoverOwners(value, plugin, downstreamResults) {
  const capabilities = new Map();
  for (const result of downstreamResults) collectUiGroupCapabilities(result, capabilities);

  function visit(node) {
    if (!node || typeof node !== 'object' || Array.isArray(node)) return node;
    let group = node.group;
    if ((node.type === 'Box' || node.type === 'Text') && node.hover?.scope !== undefined) {
      const owner = group?.plugin;
      if (owner === undefined || owner === null || owner === '' || owner === plugin) {
        group = { plugin };
      } else {
        const key = uiGroupCapabilityKey(owner, node.hover.scope);
        const available = capabilities.get(key) ?? 0;
        if (available < 1) {
          throw new TypeError(`${plugin}: scoped hover group ${String(owner)} is not owned by a downstream ui.render result`);
        }
        capabilities.set(key, available - 1);
      }
    }
    const children = Array.isArray(node.children) ? node.children.map(visit) : node.children;
    if (group === node.group && children === node.children) return node;
    return { ...node, ...(group === undefined ? {} : { group }),
      ...(children === undefined ? {} : { children }) };
  }

  return visit(value);
}

function stampClientOwners(value, plugin, downstreamResults) {
  const previouslySeen = new Set();
  for (const result of downstreamResults) collectClientIdentities(result, previouslySeen);

  function visit(node) {
    if (!node || typeof node !== 'object' || Array.isArray(node)) return node;
    if (node.type === 'Client') {
      const props = node.props;
      const currentStamp = node.client?.plugin;
      if (currentStamp === undefined || currentStamp === null || currentStamp === '') {
        return { ...node, client: { plugin } };
      }
      if (typeof currentStamp !== 'string'
          || !previouslySeen.has(clientIdentityKey(currentStamp, props))) {
        throw new TypeError(`${plugin}: returned a Client it did not draw (${String(currentStamp)}/${String(props?.key)} ${String(props?.module)}); a render hook may keep the ones next(e) returned and change their props, not which plugin, key or module they name`);
      }
      return node;
    }
    if (!Array.isArray(node.children)) return node;
    return { ...node, children: node.children.map(visit) };
  }

  return visit(value);
}

function validUiElementTable(value) {
  return value && typeof value === 'object' && !Array.isArray(value)
    && Object.entries(value).every(([name, constructor]) => typeof name === 'string'
      && typeof constructor === 'function');
}

function retainGenerationContext(ticket) {
  if (typeof ticket === 'string') {
    send({ id: 0, kind: 'api.context.retain', generationContextTicket: ticket });
  }
}

function releaseGenerationContext(ticket) {
  if (typeof ticket === 'string') {
    send({ id: 0, kind: 'api.context.release', generationContextTicket: ticket });
  }
}

function makeApi(plugin, storageId, tier, root, requestId, budget, hookId, heldEvent,
  resolvedUiTable, uiRenderContext, apiContextTicket, generationContextTicket,
  currentSignal = () => undefined) {
  const call = (method, input, budgeted = true, signal, pauseBudget = true,
    waitForAbortResult = false) => {
    const live = activeDispatch.has(requestId);
    const tracksUiRenderState = method === 'state.get'
      || method === 'ui.invalidate' && input?.event === 'ui.render';
    const uiRenderReadScope = live && tracksUiRenderState && heldEvent === 'ui.render'
      && ['desktop', 'terminal'].includes(uiRenderContext?.surface)
      && Number.isSafeInteger(uiRenderContext.hostRenderRevision)
      && uiRenderContext.hostRenderRevision > 0
      ? { surface: uiRenderContext.surface, component: uiRenderContext.component,
        requestId: uiRenderContext.requestId, revision: uiRenderContext.hostRenderRevision,
        onScreen: uiRenderContext.onScreen }
      : undefined;
    return callApi(plugin, storageId, tier, live ? requestId : 0, method, input,
      budgeted && live ? budget() : undefined, hookId, signal, pauseBudget, waitForAbortResult,
      apiContextTicket, generationContextTicket, uiRenderReadScope);
  };
  const timer = (method, ms, fn) => {
    if (typeof ms !== 'number' || !Number.isFinite(ms) || ms < 0) {
      throw new Error(`${method} takes { ms }, a non-negative number of milliseconds (got ${String(ms)})`);
    }
    if (typeof fn !== 'function') throw new TypeError(`${method} takes a callback`);
    const controller = new AbortController();
    const held = pendingTimers.get(storageId) ?? new Set();
    pendingTimers.set(storageId, held);
    let cancelled = false;
    let contextLeaseHeld = typeof generationContextTicket === 'string';
    if (contextLeaseHeld) retainGenerationContext(generationContextTicket);
    const releaseTimerContext = () => {
      if (!contextLeaseHeld) return;
      contextLeaseHeld = false;
      releaseGenerationContext(generationContextTicket);
    };
    const handle = freeze({ cancel: () => {
      if (cancelled) return;
      cancelled = true;
      controller.abort(new Error(`${method} cancelled`));
      held.delete(handle);
      if (!held.size) pendingTimers.delete(storageId);
      releaseTimerContext();
    } });
    held.add(handle);
    void (async () => {
      try {
        while (!cancelled) {
          await callApi(plugin, storageId, tier, 0, method,
            { ms: method === 'clock.every' ? Math.max(1, ms) : ms },
            undefined, hookId, controller.signal, true, false, apiContextTicket,
            generationContextTicket);
          if (cancelled) break;
          const callbackBudget = makeBudget(hookBudgetMs);
          try {
            const callback = invokeHook(fn, [], callbackBudget);
            if (typeof generationContextTicket === 'string') {
              retainGenerationContext(generationContextTicket);
            }
            const callbackPromise = Promise.resolve(callback);
            void callbackPromise.finally(() => {
              releaseGenerationContext(generationContextTicket);
            }).catch(() => {});
            void Promise.race([callbackPromise, callbackBudget.deadline])
              .then(value => { if (value === budgetExpired) throw new Error('timer callback budget expired'); })
              .catch(error => send({ id: 0, kind: 'log.error', plugin, message: String(error?.stack || error) }))
              .finally(() => callbackBudget.finish());
          } catch (error) {
            callbackBudget.finish();
            send({ id: 0, kind: 'log.error', plugin, message: String(error?.stack || error) });
          }
          if (method === 'clock.after') break;
        }
      } catch (error) {
        if (!cancelled) send({ id: 0, kind: 'log.error', plugin, message: String(error?.message || error) });
      } finally { handle.cancel(); }
    })();
    return handle;
  };
  return freeze({
    plugin: { name: plugin, root },
    session: {
      cwd: () => call('session.cwd', {}),
      root: () => call('session.root', {}),
      model: () => call('session.model', {}),
      id: () => call('session.id', {}),
      turns: () => call('session.turns', {}),
      repo: () => call('session.repo', {}),
      version: () => call('session.version', {}),
      receive: input => call('session.receive', input),
      surfaces: () => call('session.surfaces', {}),
      surface: () => call('session.surface', {}),
      messages: args => {
        if (args === undefined) return call('session.messages', {});
        if (Object.prototype.toString.call(args) !== '[object Object]') {
          return Promise.reject(new Error(`${plugin}: $.session.messages takes { agentId, as } or nothing`));
        }
        const extra = Object.keys(args).filter(key => key !== 'agentId' && key !== 'as');
        if (extra.length > 0) {
          return Promise.reject(new Error(`${plugin}: $.session.messages takes { agentId, as } or nothing (not ${extra.join(', ')})`));
        }
        if (args.agentId !== undefined && (typeof args.agentId !== 'string' || args.agentId === '')) {
          return Promise.reject(new Error(`${plugin}: $.session.messages takes agentId, a non-empty string (got ${String(args.agentId)})`));
        }
        if (args.as !== undefined && args.as !== 'api') {
          return Promise.reject(new Error(`${plugin}: $.session.messages takes as "api" or none (got ${String(args.as)})`));
        }
        return call('session.messages', {
          ...(args.agentId !== undefined ? { agentId: args.agentId } : {}),
          ...(args.as === 'api' ? { as: 'api' } : {}),
        });
      },
      usage: args => {
        const error = sessionUsageArgumentError(args);
        if (error !== undefined) {
          return Promise.reject(new Error(`${plugin}: $.session.usage ${error}`));
        }
        if (args === undefined) return call('session.usage', {});
        return call('session.usage', {
          ...(args.breakdown !== undefined ? { breakdown: args.breakdown } : {}),
          ...(args.columns !== undefined ? { columns: args.columns } : {}),
        });
      },
    },
    clock: {
      now: () => call('clock.now', {}),
      sleep: (ms, options) => {
        if (typeof ms !== 'number' || !Number.isFinite(ms) || ms < 0) {
          return Promise.reject(new Error(`clock.sleep takes { ms }, a non-negative number of milliseconds (got ${String(ms)})`));
        }
        // A sleep consumes the hook's own time budget; other awaited APIs do not.
        return call('clock.sleep', { ms }, true, options?.signal, false).then(() => undefined);
      },
      after: (ms, fn) => timer('clock.after', ms, fn),
      every: (ms, fn) => timer('clock.every', ms, fn),
    },
    store: {
      get: key => call('store.get', { key }),
      set: (key, value) => {
        let serialized;
        try { serialized = JSON.stringify(value); }
        catch (error) { return Promise.reject(new Error(`value is not JSON data: ${String(error?.message || error)}`)); }
        if (typeof serialized !== 'string') {
          return Promise.reject(new Error(`value is not JSON data: ${value === undefined ? 'undefined' : `a ${typeof value}`}`));
        }
        if (serialized.length > 4 * 1024 * 1024) {
          return Promise.reject(new Error(`the value is ${serialized.length} characters, over the ${4 * 1024 * 1024} limit`));
        }
        return call('store.set', { key, value: JSON.parse(serialized) }).then(() => undefined);
      },
      delete: key => call('store.delete', { key }).then(() => undefined),
      keys: () => call('store.keys', {}),
    },
    state: {
      get: ref => {
        if (!ref || typeof ref.plugin !== 'string' || typeof ref.key !== 'string'
            || (ref.id !== undefined && typeof ref.id !== 'string')) {
          return Promise.reject(new Error(`${plugin}: $.state.get takes a reference { plugin, key } (and id for a family's member)`));
        }
        return call('state.get', { plugin: ref.plugin, key: ref.key,
          ...(ref.id === undefined ? {} : { id: ref.id }) });
      },
      set: (ref, value, options) => {
        if (!ref || typeof ref.plugin !== 'string' || typeof ref.key !== 'string'
            || (ref.id !== undefined && typeof ref.id !== 'string')) {
          return Promise.reject(new Error(`${plugin}: $.state.set takes a reference { plugin, key } (and id for a family's member)`));
        }
        let serialized;
        try { serialized = JSON.stringify(value); }
        catch (error) { return Promise.reject(new Error(`${plugin}: $.state.set: value is not JSON data (${String(error?.message || error)})`)); }
        if (typeof serialized !== 'string' || serialized.length > 4 * 1024 * 1024) {
          return Promise.reject(new Error(`${plugin}: $.state.set: value is not JSON data or is over the 4194304 character limit`));
        }
        return call('state.set', { plugin: ref.plugin, key: ref.key,
          ...(ref.id === undefined ? {} : { id: ref.id }), value: JSON.parse(serialized),
          ...(options?.ifVersion === undefined ? {} : { ifVersion: options.ifVersion }) });
      },
    },
    agent: {
      list: () => call('agent.list', {}),
      spawn: async input => {
        const prompt = input?.prompt;
        if (input === undefined || typeof prompt !== 'string' || prompt.trim() === '') {
          throw new Error(`${plugin}: $.agent.spawn takes { prompt, ... } (a non-empty prompt)`);
        }
        const response = await call('agent.spawn', {
          tool: 'Agent',
          prompt,
          description: input.description ?? prompt.split(/\s+/).slice(0, 5).join(' '),
          run_in_background: true,
          ...(input.model !== undefined ? { model: input.model } : {}),
          ...(input.model_profile !== undefined ? { model_profile: input.model_profile } : {}),
          ...(input.subagentType !== undefined ? { subagent_type: input.subagentType } : {}),
          ...(input.name !== undefined ? { name: input.name } : {}),
          ...(input.cwd !== undefined ? { cwd: input.cwd } : {}),
        });
        const deny = response.deny ?? (response.isError === true ? response.text : undefined);
        if (deny !== undefined) return { deny };
        const result = response.result;
        const resolvedModel = result && typeof result === 'object' && !Array.isArray(result)
          && typeof result.resolvedModel === 'string' ? result.resolvedModel : undefined;
        const agentId = result && typeof result === 'object' && !Array.isArray(result)
          && typeof result.agentId === 'string' ? result.agentId : undefined;
        const teammateId = result && typeof result === 'object' && !Array.isArray(result)
          && typeof result.teammate_id === 'string' ? result.teammate_id : undefined;
        return {
          model: resolvedModel ?? input.model ?? 'inherit',
          ...(agentId !== undefined ? { agentId } : {}),
          ...(teammateId !== undefined ? { teammateId } : {}),
        };
      },
    },
    env: {
      get: name => call('env.get', { name }),
      set: (name, value) => call('env.set', value === undefined ? { name } : { name, value })
        .then(() => undefined),
    },
    settings: {
      read: args => {
        if (args !== undefined && (!args || typeof args !== 'object' || Array.isArray(args))) {
          return Promise.reject(new Error(`${plugin}: $.settings.read takes { source } or nothing`));
        }
        return call('settings.read', args?.source === undefined ? {} : { source: args.source });
      },
    },
    process: {
      run: (argv, init) => call('process.run', { argv, init }),
    },
    tool: {
      list: () => call('tool.list', {}),
      call: input => {
        if (!input || typeof input !== 'object' || Array.isArray(input)) {
          return Promise.reject(new Error(`${plugin}: $.tool.call: input must be an object`));
        }
        if (typeof input.tool !== 'string' || input.tool.length === 0) {
          return Promise.reject(new Error(`${plugin}: $.tool.call takes the event's input: { tool, ...args }`));
        }
        return call('tool.call', input, true,
          [currentSignal()], true, true);
      },
      register: spec => {
        if (!spec || typeof spec !== 'object' || Array.isArray(spec)) {
          return Promise.reject(new Error('tool.register takes a tool spec object'));
        }
        if (typeof spec.name !== 'string' || !/^[A-Za-z0-9_-]{1,64}$/.test(spec.name)) {
          return Promise.reject(new Error('tool.register name uses letters, digits, _, or - and has at most 64 characters'));
        }
        if (typeof spec.description !== 'string') {
          return Promise.reject(new Error('tool.register description must be a string'));
        }
        if (spec.inputSchema !== undefined && (!spec.inputSchema || typeof spec.inputSchema !== 'object'
            || Array.isArray(spec.inputSchema))) {
          return Promise.reject(new Error('tool.register inputSchema must be an object'));
        }
        return call('tool.register', {
          ...spec,
          description: typeof spec.description === 'string' ? displayText(spec.description) : spec.description,
          inputSchema: spec.inputSchema === undefined ? { type: 'object' } : spec.inputSchema,
        });
      },
      check: args => {
        if (Object.hasOwn(args ?? {}, 'tool_use_id')) {
          return Promise.reject(new Error('tool.check queries cannot set tool_use_id'));
        }
        return call('tool.check', { tool: args?.tool, input: args?.input });
      },
    },
    command: {
      list: () => call('command.list', {}),
      register: spec => {
        if (!spec || typeof spec !== 'object' || Array.isArray(spec)
            || typeof spec.name !== 'string' || !/^[A-Za-z0-9_-]{1,64}$/.test(spec.name)) {
          return Promise.reject(new Error(`${plugin}: $.command.register takes { name, description, argumentHint?, immediate? }; name is letters, digits, _ or - (up to 64)`));
        }
        if (typeof spec.description !== 'string' || spec.description.trim() === '') {
          return Promise.reject(new Error(`${plugin}: $.command.register: ${spec.name} needs a description (what the menu shows)`));
        }
        return call('command.register', {
          name: spec.name,
          description: displayText(spec.description),
          ...(spec.argumentHint !== undefined ? { argumentHint: spec.argumentHint } : {}),
          ...(spec.immediate !== undefined ? { immediate: spec.immediate } : {}),
        });
      },
      run: input => {
        if (!input || typeof input !== 'object' || Array.isArray(input)
            || typeof input.command !== 'string' || input.command === '') {
          return Promise.reject(new Error(`${plugin}: $.command.run takes { command, args? } (the command's name without the slash)`));
        }
        if (activeDispatch.has(requestId) && commandRunHeldEvents.has(heldEvent)) {
          const reason = heldEvent === 'command.run'
            ? 'called from a command.run hook, it would wait on the turn this hook is holding; answer { text } instead, or run it from a later event (turn.complete)'
            : `called from a ${heldEvent} hook, it would wait on the turn this hook is holding; run it from a later event (turn.complete)`;
          return Promise.reject(new Error(`${plugin}: $.command.run: ${reason}`));
        }
        return call('command.run', { command: input.command, args: input.args ?? '' });
      },
    },
    prompt: {
      submit: input => {
        const held = activeDispatch.has(requestId) ? heldEvent : undefined;
        if (held === 'prompt.submit') {
          return Promise.reject(new Error(`${plugin}: $.prompt.submit: called from a prompt.submit hook, it would wait on the turn this hook is holding; answer { text } or next(e) instead, or submit from a later event (turn.complete)`));
        }
        if (held !== undefined && (promptSubmitHeldEvents.has(held)
            || (held.startsWith('classic.') && !['classic.SessionStart', 'classic.Setup', 'classic.SessionEnd'].includes(held)))) {
          const qualifier = promptSubmitHeldEvents.has(held) ? 'is holding' : 'may be holding';
          return Promise.reject(new Error(`${plugin}: $.prompt.submit: called from a ${held} hook, it would wait on the turn this hook ${qualifier}; submit from a later event (turn.complete)`));
        }
        if (!input || typeof input.text !== 'string' || input.text.trim() === '') {
          return Promise.reject(new Error(`${plugin}: $.prompt.submit takes { text } (a non-empty prompt)`));
        }
        if (input.text.trimStart().startsWith('/')) {
          return Promise.reject(new Error(`${plugin}: $.prompt.submit submits a prompt to the model; a text beginning with / would run a command as the user; run one with $.command.run({ command })`));
        }
        if (input.asUser !== undefined && typeof input.asUser !== 'boolean') {
          return Promise.reject(new Error(`${plugin}: $.prompt.submit takes { asUser } as a boolean`));
        }
        if (input.attachments?.length > 0) {
          return Promise.reject(new Error(`${plugin}: $.prompt.submit takes text alone; attachments cannot be submitted`));
        }
        return call('prompt.submit', { text: input.text, asUser: input.asUser === true });
      },
      context: input => {
        if (!validPromptContext(input)) {
          return Promise.reject(new Error(`${plugin}: $.prompt.context takes { blocks, instructionFiles? }`));
        }
        return call('prompt.context', input);
      },
      compose: args => {
        if (args !== undefined && (!args || typeof args !== 'object' || Array.isArray(args))) {
          return Promise.reject(new Error(`${plugin}: $.prompt.compose takes the facts to compose for`));
        }
        return call('prompt.compose', args ?? {});
      },
    },
    model: {
      classify: (text, labels, options) => {
        if (typeof text !== 'string' || !Array.isArray(labels)) {
          return Promise.reject(new Error(`${plugin}: $.model.classify takes { text, labels }`));
        }
        return call('model.classify', { text, labels, options });
      },
      complete: (request, options) => {
        if (!request || typeof request !== 'object' || Array.isArray(request)) {
          return Promise.reject(new Error(`${plugin}: $.model.complete takes { model, prompt, system?, maxTokens?, effort?, timeoutMs? }`));
        }
        const signal = options?.signal;
        const aborted = () => ({ isAnswered: false, reason: 'aborted', usage: {
          input_tokens: 0, output_tokens: 0, cache_read_input_tokens: 0, cache_creation_input_tokens: 0,
        } });
        if (signal?.aborted) return Promise.resolve(aborted());
        return call('model.complete', request, true, signal).catch(error => {
          if (signal?.aborted) return aborted();
          throw error;
        });
      },
      fork: request => {
        if (!request || typeof request.prompt !== 'string' || request.prompt.trim() === '') {
          return Promise.reject(new Error(`${plugin}: $.model.fork takes { prompt } (a non-empty prompt)`));
        }
        return call('model.fork', { prompt: request.prompt });
      },
    },
    telemetry: {
      log: entry => {
        const to = entry && typeof entry === 'object' && !Array.isArray(entry) ? entry.to : undefined;
        if (!entry || typeof entry !== 'object' || Array.isArray(entry)
            || (to !== undefined && to !== 'anthropic' && to !== 'collector')) {
          return Promise.reject(new Error(`${plugin}: $.telemetry.log: takes an entry ({ to?, event, ... }, to "anthropic" or "collector")`));
        }
        return call('telemetry.log', to === 'collector' ? entry : { ...entry, to: to ?? 'anthropic' })
          .then(() => undefined);
      },
      mark: entry => {
        if (!entry || typeof entry !== 'object' || Array.isArray(entry)) {
          return Promise.reject(new Error(`${plugin}: $.telemetry.mark: takes an entry ({ feature, kind, reason?, props? })`));
        }
        return call('telemetry.mark', entry).then(() => undefined);
      },
    },
    fs: {
      read: (path, options) => call('fs.read', {
        path, as: options?.as ?? 'text',
      }),
      write: (path, text) => call('fs.write', { path, text })
        .then(() => undefined),
      exists: path => call('fs.exists', { path }),
      list: path => call('fs.list', { path: path ?? '.' }),
      stat: (path, options) => call('fs.stat', {
        path, resolve: options?.resolve ?? false,
      }),
      ancestors: request => call('fs.ancestors', request),
    },
    ui: {
      resolve: event => {
        const error = uiResolveArgumentError(event);
        if (error) throw new Error(`${plugin}: $.ui.resolve ${error}`);
        const table = resolvedUiElements.get(uiSiteKey(storageId, event.surface, event.component))
          ?? (heldEvent === 'ui.resolve' ? resolvedUiTable : undefined);
        if (!table) {
          throw new Error(`${plugin}: $.ui.resolve was not prepared for ${event.surface}:${event.component}`);
        }
        return bindUiElementTable(table,
          heldEvent === 'ui.render' && uiRenderContext && event.requestId === uiRenderContext.requestId
            ? { plugin, storageId, tier, root, hookId, surface: event.surface,
              component: event.component, requestId: event.requestId,
              renderRevision: uiRenderContext.renderRevision }
            : undefined);
      },
      selection: () => call('ui.selection', {}),
      log: (text, options) => {
        // Claude Code's log() is void: enqueue the operation and report a
        // refusal through the worker's debug channel, never as a thrown call.
        const queued = call('ui.log', {
          text: String(text), to: options?.to ?? 'transcript',
        }, false).catch(error => send({ id: requestId, kind: 'log.error', plugin,
          message: String(error?.message || error) }));
        trackVoidCall(requestId, queued);
      },
      toast: (text, options) => {
        const queued = call('ui.toast', {
          text: typeof text === 'string' ? displayText(text) : text,
          timeoutMs: options?.timeoutMs,
        }, false)
          .catch(error => send({ id: requestId, kind: 'log.error', plugin,
            message: String(error?.message || error) }));
        trackVoidCall(requestId, queued);
      },
      status: text => {
        const queued = call('ui.status', {
          text: typeof text === 'string' ? displayText(text) : text,
        }, false).catch(error => send({ id: requestId, kind: 'log.error', plugin,
          message: String(error?.message || error) }));
        trackVoidCall(requestId, queued);
      },
      invalidate: event => {
        const queued = call('ui.invalidate', { event }, false)
          .catch(error => send({ id: requestId, kind: 'log.error', plugin,
            message: String(error?.message || error) }));
        trackVoidCall(requestId, queued);
      },
    },
  });
}

async function resolveUiElementsForStorage(storageId, apiContextTicket, generationContextTicket,
  site = { surface: 'terminal', component: 'AbovePrompt' }) {
  const renderEvent = freeze({ surface: site.surface, component: site.component });
  const cacheKey = uiSiteKey(storageId, renderEvent.surface, renderEvent.component);
  const base = baseUiElements(renderEvent.surface, renderEvent.component);
  const entries = registrations.filter(entry => entry.storageId === storageId
    && entry.event === 'ui.resolve'
    && (!entry.matcher || matches(entry.matcher, renderEvent)));

  async function run(index, event) {
    if (index >= entries.length) return base;
    const entry = entries[index];
    const budget = makeBudget(hookBudgetMs);
    const controller = new AbortController();
    const next = nextEvent => {
      if (nextEvent === undefined) {
        return Promise.reject(new Error(`next() requires a ui.resolve event in ${entry.plugin}`));
      }
      const error = uiResolveArgumentError(nextEvent);
      if (error) return Promise.reject(new Error(`ui.resolve ${error}`));
      return run(index + 1, freeze(nextEvent));
    };
    next.signal = controller.signal;
    next.origin = freeze({ plugin: entry.plugin, tier: entry.tier });
    next.trace = () => freeze([]);
    Object.defineProperty(next, 'budget', { get: () => budget.read() });
    const api = makeApi(entry.plugin, entry.storageId, entry.tier, entry.root, 0,
      () => budget, entry.hookId, 'ui.resolve', base, undefined,
      apiContextTicket, generationContextTicket, () => controller.signal);
    try {
      const pending = Promise.resolve(invokeHook(entry.handler, [api, event, next], budget));
      const answer = await Promise.race([pending, budget.deadline]);
      if (answer === budgetExpired || budget.isExpired()) return next(event);
      const table = answer === undefined ? await next(event) : answer;
      if (!validUiElementTable(table)) {
        throw new Error(`${entry.plugin}: ui.resolve returned something other than an element table`);
      }
      return freeze(table);
    } catch (error) {
      send({ id: 0, kind: 'log.error', plugin: entry.plugin,
        message: `${entry.plugin}: ui.resolve failed: ${String(error?.message || error)}` });
      return next(event);
    } finally {
      budget.finish();
      controller.abort(new Error('ui.resolve finished'));
    }
  }

  try {
    resolvedUiElements.set(cacheKey, await run(0, renderEvent));
  } catch (error) {
    resolvedUiElements.set(cacheKey, base);
    send({ id: 0, kind: 'log.error', plugin: storageId,
      message: `ui.resolve failed: ${String(error?.message || error)}` });
  }
}

function registerApiCallersWithHost(message, pending) {
  if (pending.length === 0) return Promise.resolve();
  const callId = nextApiCallerRegistrationId++;
  return new Promise((resolve, reject) => {
    pendingApiCallerRegistrations.set(callId, { requestId: message.id, resolve, reject });
    send({ id: message.id, kind: 'api.callers', callId,
      apiCallers: pending.map(item => ({ hookId: item.hookId, event: item.event })) });
  });
}

function normalizedPreparedHookGraph(sourceGraph) {
  if (!sourceGraph || typeof sourceGraph !== 'object' || Array.isArray(sourceGraph)
      || typeof sourceGraph.entry !== 'string' || sourceGraph.entry.length === 0
      || typeof sourceGraph.compilerVersion !== 'string'
      || !Array.isArray(sourceGraph.files) || !Array.isArray(sourceGraph.links)) {
    throw new Error('prepared hooks sourceGraph needs entry, files, links, and compilerVersion');
  }
  if (sourceGraph.files.length === 0 || sourceGraph.files.length > 256) {
    throw new Error('prepared hooks sourceGraph exceeds its file or link limit');
  }
  const files = new Map();
  let bytes = 0;
  for (const row of sourceGraph.files) {
    if (!row || typeof row !== 'object' || Array.isArray(row)
        || typeof row.file !== 'string' || row.file.length === 0
        || typeof row.source !== 'string') {
      throw new Error('prepared hooks sourceGraph has an invalid source file');
    }
    const name = row.file;
    if (name !== 'claude:hooks-types' && !isAbsolute(name)) {
      throw new Error(`prepared hooks source ${name} is not a plugin-lexical absolute path`);
    }
    bytes += Buffer.byteLength(row.source, 'utf8');
    if (bytes > 8 * 1024 * 1024) throw new Error('prepared hooks sourceGraph exceeds 8 MiB');
    const prior = files.get(name);
    if (prior !== undefined && prior !== row.source) {
      throw new Error(`prepared hooks sourceGraph has conflicting bodies for ${name}`);
    }
    files.set(name, row.source);
  }
  if (!files.has(sourceGraph.entry)) throw new Error('prepared hooks sourceGraph entry is missing');
  const links = new Map();
  for (const row of sourceGraph.links) {
    if (!row || typeof row !== 'object' || Array.isArray(row)
        || typeof row.from !== 'string' || typeof row.spelled !== 'string'
        || row.spelled.length === 0 || typeof row.file !== 'string' || row.file.length === 0
        || !files.has(row.from)) {
      throw new Error('prepared hooks sourceGraph has an invalid import link');
    }
    const key = `${row.from}\u0000${row.spelled}`;
    if (links.has(key) && links.get(key) !== row.file) {
      throw new Error(`prepared hooks sourceGraph has conflicting import links for ${row.spelled}`);
    }
    links.set(key, row.file);
  }
  return { entry: sourceGraph.entry, compilerVersion: sourceGraph.compilerVersion, files, links };
}

function loadPreparedHookSource(graph, name) {
  if (moduleCache.has(name)) return moduleCache.get(name);
  const source = graph.files.get(name);
  if (source === undefined) throw new Error(`prepared hooks import target ${name} is missing from sourceGraph`);
  const module = new SourceTextModule(source, { context, identifier: name });
  moduleCache.set(name, module);
  return module;
}

async function linkPreparedHookModule(module, graph) {
  await module.link(async (specifier, referencing) => {
    const target = graph.links.get(`${referencing.identifier}\u0000${specifier}`);
    if (target === undefined) {
      throw new Error(`prepared hooks import ${JSON.stringify(specifier)} from ${referencing.identifier} has no sourceGraph link`);
    }
    return loadPreparedHookSource(graph, target);
  });
}

async function prepareModule(message) {
  moduleCache.clear();
  if (message.sourceGraph === undefined) {
    throw new Error('hooks prepare requires a prepared sourceGraph');
  }
  const graph = normalizedPreparedHookGraph(message.sourceGraph);
  if (message.module !== graph.entry) {
    throw new Error('prepared hooks sourceGraph entry does not match the load module');
  }
  const module = loadPreparedHookSource(graph, graph.entry);
  await linkPreparedHookModule(module, graph);
  const sources = [...graph.files.values()];
  const token = randomUUID();
  preparedModules.set(token, {
    module, sources, root: message.root, path: message.module, graph,
  });
  if (preparedModules.size > 32) preparedModules.delete(preparedModules.keys().next().value);
  send({ id: message.id, kind: 'prepared', token, sources });
}

async function load(message) {
  const tier = message.tier || 'user';
  if (!tiers.includes(tier) || tier === 'core') throw new Error(`invalid Mod tier: ${tier}`);
  if (message.preparedToken == null) {
    throw new Error('hooks load requires a prepared sourceGraph token');
  }
  const prepared = preparedModules.get(message.preparedToken);
  if (!prepared || prepared.root !== message.root || prepared.path !== message.module) {
    throw new Error('prepared hooks module is missing or belongs to another plugin');
  }
  preparedModules.delete(message.preparedToken);
  const module = prepared.module;
  await module.evaluate();
  const register = module.namespace.register;
  if (typeof register !== 'function') throw new Error('hooks module must export register(on, options)');
  const plugin = message.plugin;
  const storageId = message.storageId ?? plugin;
  const tierOrder = Number.isSafeInteger(message.tierOrder) && message.tierOrder >= 0
    ? message.tierOrder : Number.MAX_SAFE_INTEGER;
  const pending = [];
  let registering = true;
  const on = (event, matcherOrHandler, maybeHandler) => {
    if (!registering) throw new Error('on after register() returned');
    const matcher = typeof matcherOrHandler === 'function' ? undefined : matcherOrHandler;
    const handler = typeof matcherOrHandler === 'function' ? matcherOrHandler : maybeHandler;
    if (typeof event !== 'string' || typeof handler !== 'function') throw new TypeError('on requires an event and handler');
    if (!isSupportedPattern(event)) {
      throw new Error('Unsupported Mod event: ' + event);
    }
    if (matcher !== undefined && (!matcher || typeof matcher !== 'object' || Array.isArray(matcher))) {
      throw new TypeError('on matcher must be an object');
    }
    if (event === 'ui.resolve' && matcher !== undefined
        && Object.keys(matcher).some(key => key !== 'surface' && key !== 'component')) {
      throw new Error('ui.resolve matcher takes surface and component only');
    }
    const telemetryPattern = event === 'telemetry.log' || event === 'telemetry.mark'
      || event === 'telemetry.*';
    const destinations = matcher?.to === undefined ? ['anthropic', 'collector']
      : Array.isArray(matcher.to) ? matcher.to : [matcher.to];
    if (tier !== 'builtin' && telemetryPattern
        && destinations.some(destination => destination !== 'collector')) {
      throw new Error('its hooks stand on the telemetry stream "anthropic" (a matcher names it, or a telemetry hook names no "to" and so stands on every stream), which is for the plugins built into the CLI; name the collector on each telemetry hook: on("telemetry.log", { to: "collector" }, hook)');
    }
    const key = matcherKey(matcher);
    if (pending.some(item => item.event === event && item.matcherKey === key)) {
      throw new Error(`on(${JSON.stringify(event)}) is registered twice${matcher === undefined ? ' without a matcher' : ' with the same matcher'}`);
    }
    const entry = { event, matcher, matcherKey: key, handler, root: message.root,
      plugin, storageId, tier, tierOrder, hookId: nextHookId++, catchHandler: undefined };
    pending.push(entry);
    return { catch: callback => {
      if (!registering) throw new Error('catch after register() returned');
      if (entry.catchHandler) throw new Error('a hook may have only one catch handler');
      if (typeof callback !== 'function') throw new TypeError('catch requires a function');
      entry.catchHandler = callback;
    } };
  };
  try { await register(on, freeze(message.options || {})); }
  finally { registering = false; }
  cancelTimers(storageId);
  for (let i = registrations.length - 1; i >= 0; i--) {
    if (registrations[i].storageId === storageId) registrations.splice(i, 1);
  }
  for (const key of resolvedUiElements.keys()) {
    if (key.startsWith(`${storageId}\u0000`)) resolvedUiElements.delete(key);
  }
  revokeUiPressActionsForStorage(storageId);
  uiStorageEpochs.set(storageId, randomUUID());
  uiPressCounters.set(storageId, 0);
  registrations.push(...pending);
  await registerApiCallersWithHost(message, pending);
  if (pending.some(item => item.event === 'ui.render' || item.event === 'ui.resolve')) {
    await resolveUiElementsForStorage(
      storageId,
      message.apiContextTicket,
      message.generationContextTicket,
    );
  }
  const sources = prepared.sources;
  const scanSources = sources.length <= 256
    && sources.reduce((bytes, source) => bytes + Buffer.byteLength(source, 'utf8'), 0) <= 8 * 1024 * 1024
      ? sources : [];
  send({ id: message.id, kind: 'loaded', hooks: pending.map(item => item.event),
    apiCallers: pending.map(item => ({ hookId: item.hookId, event: item.event })),
    sources: scanSources });
}

async function dispatch(message) {
  if (cancelledRequests.has(message.id)) throw new Error('dispatch cancelled');
  let uiRenderContext;
  let uiPressActionAvailable = true;
  let uiInputSelectActionAvailable = true;
  if (message.event === 'ui.render') {
    const error = uiRenderArgumentError(message.input);
    if (error) throw new Error(`ui.render ${error}`);
    uiRenderContext = beginUiRenderRevision(message.input);
    const onScreen = message.input.onScreen ?? message.input.on_screen
      ?? message.input.props?.onScreen;
    uiRenderContext.onScreen = typeof onScreen === 'boolean' ? onScreen : onScreen != null;
    if (Number.isSafeInteger(message.uiRenderHostRevision) && message.uiRenderHostRevision > 0) {
      uiRenderContext.hostRenderRevision = message.uiRenderHostRevision;
    }
  } else if (message.event === 'ui.resolve') {
    const error = uiResolveArgumentError(message.input);
    if (error) throw new Error(`ui.resolve ${error}`);
  } else if (message.event === 'ui.press') {
    const error = uiPressArgumentError(message.input);
    if (error) throw new Error(error);
    const token = message.pressToken;
    const terminalToken = token && typeof token === 'object' && !Array.isArray(token)
      && Object.keys(token).every(key => ['handle', 'workerEpoch', 'renderRevision'].includes(key))
      && Number.isSafeInteger(token.handle) && token.handle > 0
      && typeof token.workerEpoch === 'string' && token.workerEpoch.length > 0
      && Number.isSafeInteger(token.renderRevision) && token.renderRevision > 0;
    if (!validParentPressToken(token) && !terminalToken) {
      throw new Error('ui.press has no valid private action token');
    }
    if (message.input.surface === 'desktop') {
      uiPressActionAvailable = validParentPressToken(token)
        && uiPressInputAdmitted(message.input, token, message.pressHrefAdmitted);
    }
  } else if (message.event === 'ui.input') {
    const error = uiInputArgumentError(message.input);
    if (error) throw new Error(error);
    if (!validParentPressToken(message.pressToken)) {
      throw new Error('ui.input has no valid private action token');
    }
    uiInputSelectActionAvailable = uiInputSelectActionMatches(
      message.input, message.pressToken, 'Input', undefined,
    );
  } else if (message.event === 'ui.select') {
    const error = uiSelectArgumentError(message.input);
    if (error) throw new Error(error);
    if (!validParentPressToken(message.pressToken)) {
      throw new Error('ui.select has no valid private action token');
    }
    uiInputSelectActionAvailable = uiInputSelectActionMatches(
      message.input, message.pressToken, 'Select', message.input.value,
    );
  } else if (message.event === 'ui.selection') {
    const error = uiSelectionArgumentError(message.input);
    if (error) throw new Error(error);
  } else if (message.event === 'session.append') {
    const error = sessionAppendArgumentError(message.input);
    if (error) throw new Error(error);
  } else if (message.event === 'session.attach') {
    const error = sessionAttachArgumentError(message.input);
    if (error) throw new Error(error);
  } else if (message.event === 'session.detach') {
    const error = sessionDetachArgumentError(message.input);
    if (error) throw new Error(error);
  } else if (message.event === 'agent.offer') {
    const error = agentOfferArgumentError(message.input);
    if (error) throw new Error(error);
  } else if (message.event === 'session.measure') {
    const error = sessionMeasureArgumentError(message.input);
    if (error) throw new Error(error);
  } else if (message.event === 'session.usage') {
    const error = sessionUsageArgumentError(message.input);
    if (error) throw new Error(error);
  }
  if (message.event === 'ui.render' && message.input.surface !== 'terminal') {
    const site = { surface: message.input.surface, component: message.input.component };
    const storageIds = [...new Set(registrations
      .filter(item => item.event === 'ui.render'
        && (message.pluginScope === undefined || item.plugin === message.pluginScope))
      .map(item => item.storageId))];
    for (const storageId of storageIds) {
      const key = uiSiteKey(storageId, site.surface, site.component);
      if (!resolvedUiElements.has(key)) {
        await resolveUiElementsForStorage(
          storageId,
          message.apiContextTicket,
          message.generationContextTicket,
          site,
        );
      }
    }
  }
  const controller = new AbortController();
  activeDispatch.set(message.id, controller);
  try {
    const handlers = registrations
      .filter(item => uiInputSelectActionAvailable && uiPressActionAvailable
        && selectsEvent(item.event, message.event) && item.hookId !== message.skipHookId
        && (message.pluginScope === undefined || item.plugin === message.pluginScope))
    if (['plugin.register', 'prompt.context', 'settings.read', 'tool.check', 'tool.describe', 'tool.list', 'tool.register'].includes(message.event)
        && message.skipHookId !== 0
        && Number.isSafeInteger(message.secDefaultOrder)
        && message.secDefaultOrder >= -1) {
      handlers.push({ event: message.event, plugin: secDefaultPlugin,
        storageId: secDefaultStorageId, tier: 'prepend',
        tierOrder: message.secDefaultOrder,
        hookId: secDefaultHookId, root: message.cwd,
        matcher: message.event === 'plugin.register' ? { tier: 'user' } : undefined,
        handler: message.event === 'tool.check' ? secDefaultToolCheck
          : message.event === 'tool.describe' ? secDefaultToolDescribe
          : message.event === 'tool.list' ? secDefaultToolList
            : message.event === 'tool.register' ? secDefaultToolRegister
              : message.event === 'plugin.register' ? secDefaultPluginRegister
            : (_api, event, next) => next.to(event, 'append'),
        catchHandler: message.event === 'tool.check' ? secDefaultToolCheckCatch
          : message.event === 'plugin.register' ? secDefaultPluginRegisterCatch : undefined });
    }
    handlers.sort((left, right) => tiers.indexOf(left.tier) - tiers.indexOf(right.tier)
      || left.tierOrder - right.tierOrder);
    const matchingHandlers = ['tool.check', 'command.run', 'turn.complete'].includes(message.event)
      ? handlers.filter(item => !item.matcher || matches(item.matcher, message.input)) : [];
    const hooked = matchingHandlers.length > 0
      ? [...new Set(matchingHandlers.map(item => item.plugin))] : undefined;
    const uiRenderPlugins = message.event === 'ui.render'
      ? [...new Set(handlers
        .filter(item => !item.matcher || matches(item.matcher, message.input))
        .map(item => item.plugin))]
      : undefined;
    const allHookedBuiltin = message.event === 'command.run'
      && matchingHandlers.length > 0 && matchingHandlers.every(item => item.tier === 'builtin');
    async function run(index, event, node) {
      if (controller.signal.aborted) throw new Error('dispatch cancelled');
      if (index === handlers.length) {
        const started = performance.now();
        let outcome = 'rejected';
        let returned;
        try {
          if (message.event === 'prompt.context' && !validPromptContext(event)) {
            throw new Error('prompt.context has an invalid blocks or instructionFiles list');
          }
          if (message.event === 'turn.complete' && !turnCompleteForwarded(message.input, event)) {
            throw new Error('turn.complete agentId is pinned and answer must be text');
          }
          if (message.event === 'prompt.submit' && !promptSubmitForwarded(message.input, event)) {
            throw new Error('prompt.submit origin and wait are pinned');
          }
          if (message.event === 'session.receive' && !sessionReceiveForwarded(message.input, event)) {
            throw new Error('session.receive origin and event are pinned');
          }
          if (message.event === 'session.compact' &&
              !sessionCompactForwarded(message.input, event)) {
            throw new Error('session.compact trigger and agentId are pinned; instructions and messages must be valid');
          }
          if (message.event === 'session.append' &&
              !sessionAppendForwarded(message.input, event)) {
            throw new Error('session.append door, origin, uuid, agentId, and message identity are pinned');
          }
          if (message.event === 'session.attach' &&
              !sessionAttachForwarded(message.input, event)) {
            throw new Error('session.attach surface, clientId, and optional viewport are pinned');
          }
          if (message.event === 'session.detach' &&
              !sessionDetachForwarded(message.input, event)) {
            throw new Error('session.detach surface, clientId, and reason are pinned');
          }
          if (message.event === 'ui.press' && !uiPressForwarded(message.input, event)) {
            throw new Error('ui.press plugin, element, component, requestId, and surface are pinned');
          }
          if (message.event === 'ui.input' && !uiInputForwarded(message.input, event)) {
            throw new Error('ui.input identity is pinned; kind/value must remain valid');
          }
          if (message.event === 'ui.select' && !uiSelectForwarded(message.input, event)) {
            throw new Error('ui.select identity is pinned; value must remain valid');
          }
          if (message.event === 'prompt.compose' &&
              (!event || typeof event !== 'object' || !Array.isArray(event.tools))) {
            throw new Error('prompt.compose needs prompt facts');
          }
          if (message.event === 'prompt.section' &&
              (event?.name !== message.input?.name ||
                !(typeof event?.text === 'string' || event?.text === null))) {
            throw new Error('prompt.section name is pinned and text must be a string or null');
          }
          if (message.event === 'prompt.attachment' &&
              !promptAttachmentForwarded(message.input, event)) {
            throw new Error('prompt.attachment type and origin are pinned');
          }
          if (message.event === 'tool.describe' &&
              !toolDescribeForwarded(message.input, event)) {
            throw new Error('tool.describe tool and provider are pinned');
          }
          if (message.event === 'command.describe') {
            if (!commandDescribeForwarded(message.input, event)) {
              throw new Error('command.describe command, immediate, and provider are pinned');
            }
            event = restoreCommandDescribe(message.input, event);
          }
          if (message.event === 'agent.spawn' && !agentSpawnForwarded(message.input, event)) {
            throw new Error('agent.spawn identity is pinned');
          }
          if (message.event === 'agent.offer') {
            event = restoreAgentOffer(message.input, event);
            if (event === undefined) throw new Error('agent.offer agent, source, and provider are pinned');
          }
          if (message.event === 'session.measure' && !sessionMeasureForwarded(message.input, event)) {
            throw new Error('session.measure context, rateLimits, cost, and changed are pinned');
          }
          if (message.event === 'ui.status') {
            if (event?.text !== undefined &&
              (typeof event.text !== 'string' || event.text.length > 4096)) {
              throw new Error('ui.status takes text of at most 4096 characters or undefined');
            }
            send({ id: message.logRouteId ?? message.id, kind: 'status',
              plugin: message.origin?.plugin ?? 'engine',
              text: event.text === undefined ? null : displayText(event.text),
              generationContextTicket: message.generationContextTicket });
            returned = { value: undefined };
          } else if (message.event === 'ui.toast') {
            if (typeof event?.text !== 'string' || event.text.length > 4096) {
              throw new Error('ui.toast takes text of at most 4096 characters');
            }
            if (event.timeoutMs !== undefined &&
              (!Number.isSafeInteger(event.timeoutMs) || event.timeoutMs < 1 || event.timeoutMs > 60000)) {
              throw new Error('ui.toast timeoutMs must be an integer from 1 to 60000');
            }
            const plugin = message.origin?.plugin ?? 'engine';
            const now = performance.now();
            if (now - (lastToastByPlugin.get(plugin) ?? -Infinity) >= 2000) {
              lastToastByPlugin.set(plugin, now);
              send({ id: message.logRouteId ?? message.id, kind: 'toast',
                plugin, text: displayText(event.text), timeoutMs: event.timeoutMs ?? 4000,
                generationContextTicket: message.generationContextTicket });
            }
            returned = { value: undefined };
          } else if (message.event === 'ui.log') {
            if (typeof event?.text !== 'string' || event.text.length > 4096) {
              throw new Error('ui.log takes text of at most 4096 characters');
            }
            if (event.to !== 'transcript' && event.to !== 'debug') {
              throw new Error('ui.log to is transcript or debug');
            }
            send({ id: message.logRouteId ?? message.id, kind: 'log',
              plugin: message.origin?.plugin ?? 'engine', text: event.text, to: event.to,
              generationContextTicket: message.generationContextTicket });
            returned = { value: undefined };
          } else if (message.event === 'ui.render') {
            returned = { type: 'engine', ref: `${event.surface}:${event.component}:${event.requestId}` };
          } else if (message.event === 'ui.resolve') {
            returned = baseUiElements(event.surface, event.component) ?? Object.create(null);
          } else if (message.event === 'ui.press') {
            const token = message.pressToken;
            const action = uiPressActions.get(uiPressActionKey(token));
            const siteRevision = activeUiRenderRevisions.get(
              uiPressSiteKey(event.surface, event.component, event.requestId),
            );
            const isParentToken = token?.workerEpoch === undefined;
            const matchesAction = uiPressActionAvailable && action
              && action.plugin === event.plugin
              && action.key === event.element
              && action.surface === event.surface
              && action.component === event.component
              && action.requestId === event.requestId
              && action.handle === token.handle
              && (!isParentToken || token.plugin === action.plugin)
              && (isParentToken
                ? action.renderRevision === siteRevision
                : action.workerEpoch === token.workerEpoch
                  && action.renderRevision === token.renderRevision
                  && siteRevision === token.renderRevision);
            if (!matchesAction) {
              returned = { handled: false };
            } else {
              const budget = makeBudget(hookBudgetMs);
              const api = isParentToken ? undefined : makeApi(
                action.plugin, action.storageId, action.tier, action.root,
                message.id, () => budget, action.hookId, 'ui.press', undefined,
                undefined, message.apiContextTicket, message.generationContextTicket,
                () => controller.signal,
              );
              const callback = action.elementType === 'Markdown'
                ? action.onEvent : action.onPress ?? action.onEvent;
              const callbackArgs = isParentToken || action.elementType === 'Markdown'
                ? [freeze(event)] : [api, freeze(event)];
              try {
                await Promise.race([
                  Promise.resolve(invokeHook(callback, callbackArgs, budget)),
                  budget.deadline,
                ]);
                returned = { handled: true, element: event.element };
              } finally { budget.finish(); }
            }
          } else if (message.event === 'ui.input' || message.event === 'ui.select') {
            const token = message.pressToken;
            const expectedType = message.event === 'ui.input' ? 'Input' : 'Select';
            const matchesAction = uiInputSelectActionAvailable
              && uiInputSelectActionMatches(event, token, expectedType,
                message.event === 'ui.select' ? message.input.value : undefined);
            if (!matchesAction) {
              returned = { handled: false };
            } else {
              const action = uiPressActions.get(uiPressActionKey(token));
              const budget = makeBudget(hookBudgetMs);
              try {
                await Promise.race([
                  Promise.resolve(invokeHook(action.onEvent, [freeze(event)], budget)),
                  budget.deadline,
                ]);
                returned = { handled: true, element: event.element, value: event.value };
              } finally { budget.finish(); }
            }
          } else {
            returned = await new Promise((accept, reject) => {
              const callId = nextCallId++;
              pendingCore.set(callId, { requestId: message.id, event: message.event, accept, reject });
              send({ id: message.id, kind: 'next', callId, input: event });
            });
          }
          outcome = 'returned';
          return returned;
        } finally {
          settleTrace(node, { index, plugin: 'engine', tier: 'core', event: message.event,
            outcome, ms: performance.now() - started, received: event, returned });
        }
      }
      const entry = handlers[index];
      if (entry.matcher && !matches(entry.matcher, event)) return run(index + 1, event, node);
      const budget = makeBudget(hookBudgetMs);
      let activeBudget = budget;
      let catchBudget;
      const resolvedUiTable = message.event === 'ui.render'
        ? resolvedUiElements.get(uiSiteKey(entry.storageId, event.surface, event.component))
        : undefined;
      const api = makeApi(entry.plugin, entry.storageId, entry.tier, entry.root, message.id,
        () => activeBudget, entry.hookId, message.event, resolvedUiTable, uiRenderContext,
        message.apiContextTicket, message.generationContextTicket,
        () => catchController?.signal ?? hookController.signal);
      const hookController = new AbortController();
      let catchController;
      const abortHook = () => {
        hookController.abort(controller.signal.reason);
        catchController?.abort(controller.signal.reason);
      };
      if (controller.signal.aborted) abortHook();
      else controller.signal.addEventListener('abort', abortHook, { once: true });
      const started = performance.now();
      let downstreamStart = 0;
      let downstreamMs = 0;
      let activeDownstream = 0;
      let called = false;
      let inCatch = false;
      let lastNext;
      let lastNextResult;
      const downstreamResults = [];
      let outcome = 'rejected';
      let returned;
      function continueBelow(nextEvent, resume, head = traceNode(), leaf = head) {
        const waitBudget = activeBudget;
        if (waitBudget.isExpired()) return expiredCall();
        node.beneath = head;
        waitBudget.pause();
        if (activeDownstream++ === 0) downstreamStart = performance.now();
        const forwarded = message.event === 'prompt.context'
          ? restorePromptContext(nextEvent, event) : nextEvent;
        const promise = run(resume, forwarded, leaf);
        lastNext = promise;
        // A hook can start a next call without awaiting it. Keep its rejection
        // observable to that hook without making it an unhandled worker error.
        void promise.catch(() => {});
        void promise.then(value => {
          downstreamResults.push(value);
          if (lastNext === promise) lastNextResult = value;
        }, () => {}).finally(() => {
          if (--activeDownstream === 0) downstreamMs += performance.now() - downstreamStart;
          waitBudget.resume();
        });
        return promise;
      }
      function replayInCatch() {
        const waitBudget = activeBudget;
        if (waitBudget.isExpired()) return expiredCall();
        waitBudget.pause();
        return lastNext.finally(() => waitBudget.resume());
      }
      const next = function(nextEvent) {
        called = true;
        if (inCatch && lastNext) return replayInCatch();
        if (arguments.length === 0) {
          return Promise.reject(new Error(`next() requires an event in ${entry.plugin}`));
        }
        if (message.event === 'tool.check' && !pinnedToolCheck(message.input, nextEvent)) {
          return Promise.reject(new Error('tool.check identity is pinned'));
        }
        if (message.event === 'tool.call' && !toolCallForwarded(event, nextEvent)) {
          return Promise.reject(new Error('tool.call identity and consent are pinned'));
        }
        if (message.event === 'ui.render' && !uiRenderForwarded(event, nextEvent)) {
          return Promise.reject(new Error('ui.render surface, component, requestId, and viewport are pinned'));
        }
        if (message.event === 'ui.resolve' && !uiResolveForwarded(event, nextEvent)) {
          return Promise.reject(new Error('ui.resolve surface and component are pinned'));
        }
        if (message.event === 'ui.press' && !uiPressForwarded(event, nextEvent)) {
          return Promise.reject(new Error('ui.press plugin, element, component, requestId, and surface are pinned'));
        }
        if (message.event === 'ui.input' && !uiInputForwarded(event, nextEvent)) {
          return Promise.reject(new Error('ui.input identity is pinned; kind/value must remain valid'));
        }
        if (message.event === 'ui.select' && !uiSelectForwarded(event, nextEvent)) {
          return Promise.reject(new Error('ui.select identity is pinned; value must remain valid'));
        }
        if (message.event === 'ui.selection' && !uiSelectionForwarded(event, nextEvent)) {
          return Promise.reject(new Error('ui.selection takes no arguments'));
        }
        if (message.event === 'session.append' && !sessionAppendForwarded(message.input, nextEvent)) {
          return Promise.reject(new Error('session.append door, origin, uuid, agentId, and message identity are pinned'));
        }
        if (message.event === 'session.attach' && !sessionAttachForwarded(message.input, nextEvent)) {
          return Promise.reject(new Error('session.attach surface, clientId, and optional viewport are pinned'));
        }
        if (message.event === 'session.detach' && !sessionDetachForwarded(message.input, nextEvent)) {
          return Promise.reject(new Error('session.detach surface, clientId, and reason are pinned'));
        }
        if (message.event === 'tool.describe' && !toolDescribeForwarded(message.input, nextEvent)) {
          return Promise.reject(new Error('tool.describe tool and provider are pinned'));
        }
        if (message.event === 'command.describe') {
          if (!commandDescribeForwarded(message.input, nextEvent)) {
            return Promise.reject(new Error('command.describe command, immediate, and provider are pinned'));
          }
          nextEvent = restoreCommandDescribe(message.input, nextEvent);
        }
        if (message.event === 'prompt.submit' && !promptSubmitForwarded(message.input, nextEvent)) {
          return Promise.reject(new Error('prompt.submit origin and wait are pinned'));
        }
        if (message.event === 'session.receive' && !sessionReceiveForwarded(message.input, nextEvent)) {
          return Promise.reject(new Error('session.receive origin and event are pinned'));
        }
        if (message.event === 'session.compact' &&
            !sessionCompactForwarded(message.input, nextEvent)) {
          return Promise.reject(new Error('session.compact trigger and agentId are pinned; instructions and messages must be valid'));
        }
        if (message.event === 'agent.offer') {
          nextEvent = restoreAgentOffer(message.input, nextEvent);
          if (nextEvent === undefined) {
            return Promise.reject(new Error('agent.offer agent, source, and provider are pinned'));
          }
        }
        if (message.event === 'session.measure' && !sessionMeasureForwarded(message.input, nextEvent)) {
          return Promise.reject(new Error('session.measure context, rateLimits, cost, and changed are pinned'));
        }
        if (message.event === 'prompt.attachment') {
          if (!promptAttachmentForwarded(message.input, nextEvent)) {
            return Promise.reject(new Error('prompt.attachment type and origin are pinned'));
          }
          nextEvent = restorePromptAttachment(message.input, nextEvent);
        }
        if (message.event === 'agent.spawn') {
          nextEvent = agentSpawnForwarded(message.input, nextEvent);
          if (nextEvent === undefined) return Promise.reject(new Error('agent.spawn identity is pinned'));
        }
        if (message.event === 'command.run') {
          nextEvent = commandRunForwarded(message.input, nextEvent);
          if (nextEvent === undefined) return Promise.reject(new Error('command.run identity is pinned'));
        }
        if (message.event === 'turn.complete') {
          nextEvent = turnCompleteForwarded(message.input, nextEvent);
          if (nextEvent === undefined) return Promise.reject(new Error('turn.complete agentId is pinned and answer must be text'));
        }
        return continueBelow(nextEvent, index + 1);
      };
      next.to = function(nextEvent, target) {
        called = true;
        if (inCatch && lastNext) return replayInCatch();
        if (arguments.length < 2) {
          return Promise.reject(new Error(`${entry.plugin}: next.to() names no tier`));
        }
        if (message.event === 'tool.check' && !pinnedToolCheck(message.input, nextEvent)) {
          return Promise.reject(new Error('tool.check identity is pinned'));
        }
        if (message.event === 'tool.call' && !toolCallForwarded(event, nextEvent)) {
          return Promise.reject(new Error('tool.call identity and consent are pinned'));
        }
        if (message.event === 'ui.render' && !uiRenderForwarded(event, nextEvent)) {
          return Promise.reject(new Error('ui.render surface, component, requestId, and viewport are pinned'));
        }
        if (message.event === 'ui.resolve' && !uiResolveForwarded(event, nextEvent)) {
          return Promise.reject(new Error('ui.resolve surface and component are pinned'));
        }
        if (message.event === 'ui.press' && !uiPressForwarded(event, nextEvent)) {
          return Promise.reject(new Error('ui.press plugin, element, component, requestId, and surface are pinned'));
        }
        if (message.event === 'ui.input' && !uiInputForwarded(event, nextEvent)) {
          return Promise.reject(new Error('ui.input identity is pinned; kind/value must remain valid'));
        }
        if (message.event === 'ui.select' && !uiSelectForwarded(event, nextEvent)) {
          return Promise.reject(new Error('ui.select identity is pinned; value must remain valid'));
        }
        if (message.event === 'ui.selection' && !uiSelectionForwarded(event, nextEvent)) {
          return Promise.reject(new Error('ui.selection takes no arguments'));
        }
        if (message.event === 'session.append' && !sessionAppendForwarded(message.input, nextEvent)) {
          return Promise.reject(new Error('session.append door, origin, uuid, agentId, and message identity are pinned'));
        }
        if (message.event === 'session.attach' && !sessionAttachForwarded(message.input, nextEvent)) {
          return Promise.reject(new Error('session.attach surface, clientId, and optional viewport are pinned'));
        }
        if (message.event === 'session.detach' && !sessionDetachForwarded(message.input, nextEvent)) {
          return Promise.reject(new Error('session.detach surface, clientId, and reason are pinned'));
        }
        if (message.event === 'tool.describe' && !toolDescribeForwarded(message.input, nextEvent)) {
          return Promise.reject(new Error('tool.describe tool and provider are pinned'));
        }
        if (message.event === 'command.describe') {
          if (!commandDescribeForwarded(message.input, nextEvent)) {
            return Promise.reject(new Error('command.describe command, immediate, and provider are pinned'));
          }
          nextEvent = restoreCommandDescribe(message.input, nextEvent);
        }
        if (message.event === 'prompt.submit' && !promptSubmitForwarded(message.input, nextEvent)) {
          return Promise.reject(new Error('prompt.submit origin and wait are pinned'));
        }
        if (message.event === 'agent.offer') {
          nextEvent = restoreAgentOffer(message.input, nextEvent);
          if (nextEvent === undefined) {
            return Promise.reject(new Error('agent.offer agent, source, and provider are pinned'));
          }
        }
        if (message.event === 'session.measure' && !sessionMeasureForwarded(message.input, nextEvent)) {
          return Promise.reject(new Error('session.measure context, rateLimits, cost, and changed are pinned'));
        }
        if (message.event === 'prompt.attachment') {
          if (!promptAttachmentForwarded(message.input, nextEvent)) {
            return Promise.reject(new Error('prompt.attachment type and origin are pinned'));
          }
          nextEvent = restorePromptAttachment(message.input, nextEvent);
        }
        if (message.event === 'agent.spawn') {
          nextEvent = agentSpawnForwarded(message.input, nextEvent);
          if (nextEvent === undefined) return Promise.reject(new Error('agent.spawn identity is pinned'));
        }
        if (message.event === 'command.run') {
          nextEvent = commandRunForwarded(message.input, nextEvent);
          if (nextEvent === undefined) return Promise.reject(new Error('command.run identity is pinned'));
        }
        if (message.event === 'turn.complete') {
          nextEvent = turnCompleteForwarded(message.input, nextEvent);
          if (nextEvent === undefined) return Promise.reject(new Error('turn.complete agentId is pinned and answer must be text'));
        }
        if (!['append', 'builtin', 'core'].includes(target)) {
          return Promise.reject(new Error(`${entry.plugin}: next.to names "${String(target)}", which is not a tier a dispatch continues at (append, builtin, core)`));
        }
        const allowed = nextToTargets[entry.tier];
        if (!allowed?.length) {
          return Promise.reject(new Error(`${entry.plugin}: next.to is available to managed plugins (prependPlugins / appendPlugins) only, not to a ${entry.tier} hook`));
        }
        if (!allowed.includes(target)) {
          return Promise.reject(new Error(`${entry.plugin}: next.to("${target}") skips nothing from ${entry.tier}; a ${entry.tier} hook may continue at ${allowed.join(', ')}`));
        }
        const targetRank = tiers.indexOf(target);
        let resume = index + 1;
        const first = traceNode();
        let skipped = first;
        while (resume < handlers.length && tiers.indexOf(handlers[resume].tier) < targetRank) {
          const bypassed = handlers[resume];
          settleTrace(skipped, { index: resume, plugin: bypassed.plugin, tier: bypassed.tier,
            event: message.event, outcome: 'skipped', reason: `bypassed by ${entry.plugin}`,
            ms: 0, received: nextEvent, returned: undefined });
          skipped.beneath = traceNode();
          skipped = skipped.beneath;
          resume++;
        }
        return continueBelow(nextEvent, resume, first, skipped);
      };
      next.event = message.event;
      next.is = (pattern) => selectsEvent(pattern, message.event);
      next.origin = freeze(message.origin || { plugin: 'engine', tier: 'core' });
      next.signal = hookController.signal;
      Object.defineProperty(next, 'budget', {
        get: () => activeBudget.read(), enumerable: true,
      });
      Object.defineProperty(next, 'trace', { get: () => traceBelow(node), enumerable: true });
      try {
        const answer = await Promise.race([
          Promise.resolve().then(() => invokeHook(entry.handler, [api, freeze(event), next], budget)),
          budget.deadline,
        ]);
        if (answer === budgetExpired || budget.isExpired()) throw budgetExpired;
        const projectedAnswer = message.event === 'ui.render' && event.surface !== 'terminal'
          ? stampClientOwners(stampUiHoverOwners(answer, entry.plugin, downstreamResults),
            entry.plugin, downstreamResults) : answer;
        if (projectedAnswer === undefined
            || !validOperationResult(message.event, event, projectedAnswer, downstreamResults)) {
          throw new Error(`${entry.plugin}: returned the wrong shape`);
        }
        returned = message.event === 'prompt.context'
          ? restorePromptContext(projectedAnswer, lastNextResult ?? event)
          : message.event === 'tool.describe'
            ? restoreToolDescribe(projectedAnswer, lastNextResult ?? event) : projectedAnswer;
        outcome = lastNext && projectedAnswer === lastNextResult ? 'passed' : 'returned';
        return returned;
      } catch (error) {
        const timedOut = error === budgetExpired || budget.isExpired();
        if (timedOut) hookController.abort(new Error(`${entry.plugin}: hook budget expired`));
        budget.finish();
        if (entry.catchHandler) {
          next.error = freeze({ kind: timedOut ? 'timeout' : 'throw',
            ...timedOut ? {} : { message: String(error?.message || error) },
            budget: catchBudgetMs });
          next.called = called;
          inCatch = true;
          catchBudget = makeBudget(catchBudgetMs);
          activeBudget = catchBudget;
          catchController = new AbortController();
          if (controller.signal.aborted) catchController.abort(controller.signal.reason);
          next.signal = catchController.signal;
          try {
            const answer = await Promise.race([
              Promise.resolve().then(() => invokeHook(entry.catchHandler, [api, freeze(event), next], catchBudget)),
              catchBudget.deadline,
            ]);
            if (answer === budgetExpired || catchBudget.isExpired()) throw budgetExpired;
            const projectedAnswer = message.event === 'ui.render' && event.surface !== 'terminal'
              ? stampClientOwners(stampUiHoverOwners(answer, entry.plugin, downstreamResults),
                entry.plugin, downstreamResults) : answer;
            if (projectedAnswer === undefined
                || !validOperationResult(message.event, event, projectedAnswer, downstreamResults)) {
              throw new Error(`${entry.plugin}: .catch returned the wrong shape`);
            }
            returned = message.event === 'prompt.context'
              ? restorePromptContext(projectedAnswer, lastNextResult ?? event)
              : message.event === 'tool.describe'
                ? restoreToolDescribe(projectedAnswer, lastNextResult ?? event) : projectedAnswer;
            outcome = 'caught';
            return returned;
          } catch (caughtError) {
            if (caughtError === budgetExpired || catchBudget.isExpired()) {
              catchController.abort(new Error(`${entry.plugin}: catch budget expired`));
            }
            // A failed catch yields to its last next call or to the chain.
          }
          finally { catchBudget.finish(); }
        }
        if (lastNext) {
          const answer = await lastNext;
          returned = answer;
          outcome = timedOut ? 'expired' : 'kept';
          return answer;
        }
        const child = traceNode();
        node.beneath = child;
        const answer = await run(index + 1, event, child);
        outcome = timedOut ? 'expired' : 'skipped';
        return answer;
      } finally {
        budget.finish();
        catchBudget?.finish();
        controller.signal.removeEventListener('abort', abortHook);
        if (activeDownstream > 0) hookController.abort(new Error(`${entry.plugin} settled the call`));
        const ended = performance.now();
        settleTrace(node, { index, plugin: entry.plugin, tier: entry.tier, event: message.event,
          outcome, ms: Math.max(0, ended - started - downstreamMs
            - (activeDownstream ? ended - downstreamStart : 0)), received: event, returned });
        send({ id: message.id, kind: 'progress' });
      }
    }
    // Native's slow-plugin signal measures the render evaluator itself, not
    // the host IPC path surrounding it. Keep this clock on the worker side
    // and start it immediately before running the matched hook chain.
    const uiRenderEvaluationStartedAt = message.event === 'ui.render' ? Date.now() : undefined;
    const result = await run(0, message.input, traceNode());
    const uiRenderDurationMs = uiRenderEvaluationStartedAt === undefined ? undefined
      : Math.max(0, Date.now() - uiRenderEvaluationStartedAt);
    await drainVoidCalls(message.id);
    if (message.event === 'ui.render') {
      commitUiPressActions(message.input, uiRenderContext.renderRevision, result);
    }
    send({ id: message.id, kind: 'result', result: result ?? null, hooked, allHookedBuiltin,
      ...(uiRenderPlugins ? { uiRenderPlugins } : {}),
      ...(uiRenderDurationMs === undefined ? {} : { uiRenderDurationMs }) });
  } finally {
    await drainVoidCalls(message.id);
    for (const [callId, pending] of pendingApi) {
      if (pending.requestId === message.id) {
        pendingApi.delete(callId);
        markCancelledApiCall(callId);
        pending.reject(new Error('dispatch settled before API call completed'));
      }
    }
    activeDispatch.delete(message.id);
  }
}

function streamWithResult(makeIterator) {
  let resolveResult;
  let rejectResult;
  const result = new Promise((resolve, reject) => {
    resolveResult = resolve;
    rejectResult = reject;
  });
  void result.catch(() => {});
  const iterator = (async function*() {
    let settled = false;
    try {
      const answer = yield* makeIterator();
      settled = true;
      resolveResult(answer);
      return answer;
    } catch (error) {
      settled = true;
      rejectResult(error);
      throw error;
    } finally {
      if (!settled) rejectResult(new Error('the stream was closed before its result'));
    }
  })();
  Object.defineProperty(iterator, 'result', { value: result, enumerable: true });
  return iterator;
}

function turnStepForwarded(original, forwarded) {
  return forwarded && typeof forwarded === 'object' && !Array.isArray(forwarded)
    && forwarded.turnId === original.turnId && forwarded.index === original.index
    && forwarded.messageCount === original.messageCount
    && forwarded.agentId === original.agentId
    && typeof forwarded.model === 'string' && forwarded.model.trim() !== ''
    && (forwarded.effort === undefined || forwarded.effort === original.effort
      || ['low', 'medium', 'high', 'xhigh', 'max'].includes(forwarded.effort));
}

function restoreTurnStepInput(original, forwarded) {
  if (!forwarded || typeof forwarded !== 'object' || Array.isArray(forwarded)) return forwarded;
  return forwarded.agentId === undefined && original.agentId !== undefined
    ? { ...forwarded, agentId: original.agentId } : forwarded;
}

function turnStepResultValid(input, result) {
  return result && typeof result === 'object' && result.turnId === input.turnId
    && result.index === input.index && typeof result.answer === 'string'
    && Array.isArray(result.toolUses);
}

const turnStepStopReasons = new Set([
  'end_turn', 'max_tokens', 'stop_sequence', 'tool_use', 'pause_turn',
  'compaction', 'refusal', 'model_context_window_exceeded',
]);

function turnStepChunkProblem(chunk) {
  if (!chunk || typeof chunk !== 'object' || Array.isArray(chunk)) return 'no kind';
  const index = typeof chunk.index === 'number' && chunk.index >= 0;
  switch (chunk.kind) {
    case 'text':
    case 'thinking':
      return index && typeof chunk.text === 'string' ? undefined : 'no { index, text }';
    case 'tool':
      return index && typeof chunk.id === 'string' && /^[\w-]+$/.test(chunk.id)
        && typeof chunk.name === 'string' ? undefined : 'no { index, id, name }';
    case 'input':
      return index && typeof chunk.json === 'string' ? undefined : 'no { index, json }';
    case 'stop': {
      const reason = chunk.stopReason === null || turnStepStopReasons.has(chunk.stopReason);
      const usage = chunk.usage === null || (chunk.usage && typeof chunk.usage === 'object'
        && ['input_tokens', 'output_tokens', 'cache_read_input_tokens',
          'cache_creation_input_tokens'].every(key => Number.isFinite(chunk.usage[key])));
      return reason && usage ? undefined : 'no { stopReason, usage }';
    }
    case 'engine':
      return typeof chunk.ref === 'number' ? undefined : 'no engine ref';
    default:
      return 'unknown kind';
  }
}

function turnStepChunkGuard() {
  const pulledRefs = new Map();
  const passedEngine = new Set();
  const toolIds = new Set();
  const pulledObjects = new WeakSet();
  return {
    pulled(chunk) {
      if (chunk && typeof chunk === 'object') {
        pulledObjects.add(chunk);
        if (typeof chunk.ref === 'number' && typeof chunk.kind === 'string') {
          pulledRefs.set(chunk.ref, chunk.kind);
        }
      }
    },
    yielded(chunk) {
      const problem = pulledObjects.has(chunk) ? undefined : turnStepChunkProblem(chunk);
      if (problem) throw new Error(`turn.step yielded a chunk with ${problem}`);
      if (chunk.kind === 'engine') {
        if (pulledRefs.get(chunk.ref) !== 'engine') {
          throw new Error('turn.step yielded an engine ref this link never pulled as engine');
        }
        if (passedEngine.has(chunk.ref)) throw new Error('turn.step passed an engine ref twice');
        passedEngine.add(chunk.ref);
      }
      if (chunk.kind === 'tool') {
        if (toolIds.has(chunk.id)) throw new Error(`turn.step repeated tool id ${chunk.id}`);
        toolIds.add(chunk.id);
      }
    },
  };
}

function requestStreamCore(requestId, kind, extra) {
  return new Promise((accept, reject) => {
    const callId = nextCallId++;
    pendingStreamCore.set(callId, { requestId, kind, accept, reject });
    send({ id: requestId, kind, callId, ...extra });
  });
}

async function dispatchStream(message) {
  if (cancelledRequests.has(message.id)) throw new Error('stream dispatch cancelled');
  const controller = new AbortController();
  activeDispatch.set(message.id, controller);
  const handlers = registrations
    .filter(item => selectsEvent(item.event, 'turn.step') && item.hookId !== message.skipHookId)
    .sort((left, right) => tiers.indexOf(left.tier) - tiers.indexOf(right.tier)
      || left.tierOrder - right.tierOrder);
  function run(index, event, node) {
    return streamWithResult(async function*() {
      if (controller.signal.aborted) throw new Error('stream dispatch cancelled');
      if (index >= handlers.length) {
        const started = performance.now();
        let outcome = 'rejected';
        let returned;
        let chunks = 0;
        try {
          event = restoreTurnStepInput(message.input, event);
          if (!turnStepForwarded(message.input, event)) throw new Error('turn.step input identity is pinned');
          const { sourceId } = await requestStreamCore(message.id, 'source.open', { input: event });
          while (true) {
            const reply = await requestStreamCore(message.id, 'source.pull', { sourceId });
            if (reply.done) {
              returned = reply.result;
              outcome = 'returned';
              return returned;
            }
            chunks++;
            yield reply.chunk;
          }
        } finally {
          settleTrace(node, { index, plugin: 'engine', tier: 'core', event: 'turn.step',
            outcome, ms: performance.now() - started, chunks, received: event, returned });
        }
      }
      const entry = handlers[index];
      if (entry.matcher && !matches(entry.matcher, event)) return yield* run(index + 1, event, node);
      let outcome = 'rejected';
      let returned;
      let chunks = 0;
      const budget = makeBudget(hookBudgetMs);
      let activeBudget = budget;
      const hookController = new AbortController();
      let catchController;
      const forwardAbort = () => {
        hookController.abort(controller.signal.reason);
        catchController?.abort(controller.signal.reason);
      };
      if (controller.signal.aborted) forwardAbort();
      else controller.signal.addEventListener('abort', forwardAbort, { once: true });
      const guard = turnStepChunkGuard();
      const api = makeApi(entry.plugin, entry.storageId, entry.tier, entry.root,
        message.id, () => activeBudget, entry.hookId, 'turn.step', undefined, undefined,
        message.apiContextTicket, message.generationContextTicket,
        () => catchController?.signal ?? hookController.signal);
      let lastNext;
      let lastNextResult;
      let called = false;
      function nextFrom(nextEvent, resume, head = traceNode(), leaf = head) {
        called = true;
        nextEvent = restoreTurnStepInput(message.input, nextEvent);
        if (!turnStepForwarded(message.input, nextEvent)) {
          return streamWithResult(async function*() { throw new Error('turn.step input identity is pinned'); });
        }
        node.beneath = head;
        const below = run(resume, nextEvent, leaf);
        lastNext = below;
        return streamWithResult(async function*() {
          while (true) {
            const waitBudget = activeBudget;
            waitBudget.pause();
            let part;
            try { part = await below.next(); }
            finally { waitBudget.resume(); }
            if (part.done) {
              lastNextResult = part.value;
              return part.value;
            }
            guard.pulled(part.value);
            yield part.value;
          }
        });
      }
      const next = nextEvent => nextFrom(nextEvent, index + 1);
      next.to = (nextEvent, target) => {
        const allowed = nextToTargets[entry.tier];
        if (!allowed?.includes(target)) {
          return streamWithResult(async function*() { throw new Error('turn.step next.to tier is not allowed'); });
        }
        const rank = tiers.indexOf(target);
        let resume = index + 1;
        const first = traceNode();
        let skipped = first;
        while (resume < handlers.length && tiers.indexOf(handlers[resume].tier) < rank) {
          const bypassed = handlers[resume];
          settleTrace(skipped, { index: resume, plugin: bypassed.plugin, tier: bypassed.tier,
            event: 'turn.step', outcome: 'skipped', reason: `bypassed by ${entry.plugin}`,
            ms: 0, chunks: 0, received: nextEvent, returned: undefined });
          skipped.beneath = traceNode();
          skipped = skipped.beneath;
          resume++;
        }
        return nextFrom(nextEvent, resume, first, skipped);
      };
      next.event = 'turn.step';
      next.is = pattern => selectsEvent(pattern, 'turn.step');
      next.origin = freeze(message.origin || { plugin: 'engine', tier: 'core' });
      next.signal = hookController.signal;
      Object.defineProperty(next, 'budget', { get: () => activeBudget.read(), enumerable: true });
      Object.defineProperty(next, 'trace', { get: () => traceBelow(node), enumerable: true });
      async function* consume(handler, currentBudget) {
        const iterator = invokeHook(handler, [api, freeze(event), next], currentBudget);
        if (!iterator || typeof iterator.next !== 'function') {
          throw new Error(`${entry.plugin}: turn.step must return an async iterator`);
        }
        while (true) {
          const part = await Promise.race([iterator.next(), currentBudget.deadline]);
          if (part === budgetExpired || currentBudget.isExpired()) throw budgetExpired;
          if (part.done) return part.value;
          guard.yielded(part.value);
          chunks++;
          currentBudget.pause();
          try { yield part.value; }
          finally { currentBudget.resume(); }
        }
      }
      try {
        let answer = yield* consume(entry.handler, budget);
        if (answer === undefined && lastNext) answer = await lastNext.result;
        if (!turnStepResultValid(message.input, answer)) throw new Error('turn.step returned the wrong shape');
        returned = answer;
        outcome = lastNextResult === answer ? 'passed' : 'returned';
        return answer;
      } catch (error) {
        if (error === budgetExpired || budget.isExpired()) {
          hookController.abort(new Error(`${entry.plugin}: hook budget expired`));
        }
        budget.finish();
        if (entry.catchHandler) {
          const catchBudget = makeBudget(catchBudgetMs);
          catchController = new AbortController();
          if (controller.signal.aborted) catchController.abort(controller.signal.reason);
          next.signal = catchController.signal;
          next.error = freeze({ kind: error === budgetExpired ? 'timeout' : 'throw',
            ...error === budgetExpired ? {} : { message: String(error?.message || error) },
            budget: catchBudgetMs });
          next.called = called;
          try {
            activeBudget = catchBudget;
            const answer = yield* consume(entry.catchHandler, catchBudget);
            if (turnStepResultValid(message.input, answer)) {
              returned = answer;
              outcome = 'caught';
              return answer;
            }
          } catch (caughtError) {
            if (caughtError === budgetExpired || catchBudget.isExpired()) {
              catchController.abort(new Error(`${entry.plugin}: catch budget expired`));
            }
            // Keep the last forwarded stream or continue below.
          }
          finally { catchBudget.finish(); }
        }
        if (lastNext) {
          returned = yield* lastNext;
          outcome = error === budgetExpired ? 'expired' : 'kept';
          return returned;
        }
        if (message.event === 'turn.step') {
          // Native turn.step lets the outer query wrapper decide what to do
          // when a hook fails before it has forwarded next(e). In particular,
          // the request counter is still zero, so it drops buffered synthetic
          // assistant output and retries the original model request without
          // running the rest of this hook chain.
          throw error;
        }
        const child = traceNode();
        node.beneath = child;
        returned = yield* run(index + 1, event, child);
        outcome = error === budgetExpired ? 'expired' : 'skipped';
        return returned;
      } finally {
        const ms = Math.max(0, hookBudgetMs - budget.read().remainingMs);
        budget.finish();
        controller.signal.removeEventListener('abort', forwardAbort);
        settleTrace(node, { index, plugin: entry.plugin, tier: entry.tier,
          event: 'turn.step', outcome, ms, chunks, received: event, returned });
      }
    });
  }
  const iterator = run(0, message.input, traceNode());
  activeStreams.set(message.id, { iterator, controller, advancing: false });
  send({ id: message.id, kind: 'stream.ready' });
}

async function advanceStream(message) {
  const active = activeStreams.get(message.id);
  if (!active || active.advancing) {
    send({ id: message.id, kind: 'stream.error', message: 'unexpected stream.advance' });
    return;
  }
  active.advancing = true;
  try {
    const part = await active.iterator.next();
    if (part.done) {
      await drainVoidCalls(message.id);
      send({ id: message.id, kind: 'stream.result', result: part.value ?? null });
      activeStreams.delete(message.id);
      activeDispatch.delete(message.id);
    } else send({ id: message.id, kind: 'stream.chunk', chunk: part.value });
  } catch (error) {
    send({ id: message.id, kind: 'stream.error', message: String(error?.stack || error) });
    activeStreams.delete(message.id);
    activeDispatch.delete(message.id);
  } finally { active.advancing = false; }
}

async function processMessage(message) {
  try {
    if (message.kind === 'prepare') await prepareModule(message);
    else if (message.kind === 'discard') {
      preparedModules.delete(message.token);
      send({ id: message.id, kind: 'discarded' });
    }
    else if (message.kind === 'load') await load(message);
    else if (message.kind === 'dispatch') await dispatch(message);
    else if (message.kind === 'stream.dispatch') await dispatchStream(message);
    else if (message.kind === 'ui.listener') {
      if (!['ui.fault', 'ui.message', 'ui.press', 'ui.input', 'ui.select'].includes(message.event)
          || typeof message.plugin !== 'string' || message.plugin.length === 0) {
        send({ id: message.id, kind: 'ui.listener.result', matched: false });
      } else {
        const matched = registrations.some(item => item.plugin === message.plugin
          && selectsEvent(item.event, message.event)
          && (!item.matcher || matches(item.matcher, message.input)));
        send({ id: message.id, kind: 'ui.listener.result', matched });
      }
    }
    else if (message.kind === 'ui.press.preflight') {
      send({ id: message.id, kind: 'ui.press.preflight.result',
        admitted: parentPressHrefPreflight(message) });
    }
    else if (message.kind === 'ui.client.operation') {
      try {
        const result = await clientSurfaceManager.operation(message.operation);
        send({ id: message.id, kind: 'ui.client.result', result });
      } catch (error) {
        send({ id: message.id, kind: 'ui.client.result', error: {
          phase: ['load', 'render', 'run'].includes(error?.phase) ? error.phase : 'load',
          reason: String(error?.message || error),
        } });
      }
    }
    else if (message.kind === 'unload') {
      cancelTimers(message.storageId);
      revokeUiPressActionsForStorage(message.storageId);
      uiStorageEpochs.delete(message.storageId);
      uiPressCounters.delete(message.storageId);
      for (let i = registrations.length - 1; i >= 0; i--) {
        if (registrations[i].storageId === message.storageId) registrations.splice(i, 1);
      }
      for (const key of resolvedUiElements.keys()) {
        if (key.startsWith(`${message.storageId}\u0000`)) resolvedUiElements.delete(key);
      }
      moduleCache.clear();
      clientSurfaceManager.disposePlugin(message.plugin ?? message.storageId);
      send({ id: message.id, kind: 'unloaded' });
    }
    else throw new Error('unknown worker request: ' + message.kind);
  } catch (error) {
    send({ id: message.id, kind: 'error', message: String(error?.stack || error) });
  } finally {
    cancelledRequests.delete(message.id);
    pendingDispatch.delete(message.id);
  }
}

const input = createInterface({ input: process.stdin, crlfDelay: Infinity });
input.on('close', () => clientSurfaceManager.dispose());
input.on('line', line => {
  let message;
  try { message = JSON.parse(line); }
  catch (error) { send({ kind: 'error', message: String(error) }); return; }
  try { hydrateModUtf16Sidecars(message); }
  catch (error) {
    failModUtf16Hydration(message, error);
    return;
  }
  if (message.kind === 'next.result' || message.kind === 'next.error') {
    const reply = pendingCore.get(message.callId);
    if (!reply || reply.requestId !== message.id) {
      send({ id: message.id, kind: 'error', message: 'unexpected next reply' });
      return;
    }
    pendingCore.delete(message.callId);
    if (message.kind === 'next.result') {
      // JSON has no undefined. The host's null marker represents the void
      // value of a successful fs.write core result inside the Mod chain.
      if (['store.get', 'env.get', 'ui.selection', 'telemetry.log', 'telemetry.mark'].includes(reply.event) && !Object.hasOwn(message.result ?? {}, 'value')) {
        reply.accept({ value: undefined });
      } else if (['fs.write', 'ui.toast', 'ui.status', 'clock.sleep', 'clock.after', 'clock.every', 'store.set', 'store.delete', 'env.set'].includes(reply.event) && message.result?.value === null) {
        reply.accept({ value: undefined });
      } else reply.accept(message.result);
    }
    else reply.reject(new Error(message.message));
  } else if (message.kind === 'source.open.result' || message.kind === 'source.open.error'
      || message.kind === 'source.pull.result' || message.kind === 'source.pull.error') {
    const pending = pendingStreamCore.get(message.callId);
    if (!pending || pending.requestId !== message.id
        || !message.kind.startsWith(pending.kind + '.')) {
      send({ id: message.id, kind: 'stream.error', message: 'unexpected source reply' });
      return;
    }
    pendingStreamCore.delete(message.callId);
    if (message.kind.endsWith('.error')) pending.reject(new Error(message.message));
    else pending.accept(message);
  } else if (message.kind === 'api.result' || message.kind === 'api.error') {
    const reply = pendingApi.get(message.callId);
    if (!reply || reply.requestId !== message.id) {
      if (cancelledApiCalls.delete(message.callId)) return;
      send({ id: message.id, kind: 'error', message: 'unexpected api reply' });
      return;
    }
    pendingApi.delete(message.callId);
    if (message.kind === 'api.result') {
      reply.accept(reply.method === 'tool.call'
        ? projectToolCallApiResult(message.result) : message.result);
    }
    else reply.reject(new Error(message.message));
  } else if (message.kind === 'api.callers.result' || message.kind === 'api.callers.error') {
    const reply = pendingApiCallerRegistrations.get(message.callId);
    if (!reply || reply.requestId !== message.id) {
      send({ id: message.id, kind: 'error', message: 'unexpected API caller registration reply' });
      return;
    }
    pendingApiCallerRegistrations.delete(message.callId);
    if (message.kind === 'api.callers.result') reply.resolve();
    else reply.reject(new Error(message.message));
  } else if (message.kind === 'cancel') {
    for (const [callId, pending] of pendingApiCallerRegistrations) {
      if (pending.requestId === message.id) {
        pendingApiCallerRegistrations.delete(callId);
        pending.reject(new Error('request cancelled'));
      }
    }
    if (pendingDispatch.has(message.id) || activeDispatch.has(message.id)
        || activeStreams.has(message.id)) {
      cancelledRequests.add(message.id);
    }
    activeDispatch.get(message.id)?.abort();
    const active = activeStreams.get(message.id);
    if (active) {
      activeStreams.delete(message.id);
      void active.iterator.return().catch(() => {});
    }
    for (const [callId, pending] of pendingCore) {
      if (pending.requestId === message.id) {
        pendingCore.delete(callId);
        pending.reject(new Error('dispatch cancelled'));
      }
    }
    for (const [callId, pending] of pendingApi) {
      if (pending.requestId === message.id) {
        pendingApi.delete(callId);
        markCancelledApiCall(callId);
        pending.reject(new Error('dispatch cancelled'));
      }
    }
    for (const [callId, pending] of pendingStreamCore) {
      if (pending.requestId === message.id) {
        pendingStreamCore.delete(callId);
        pending.reject(new Error('stream dispatch cancelled'));
      }
    }
  } else if (message.kind === 'stream.advance') {
    void advanceStream(message);
  } else if (message.kind === 'dispatch' || message.kind === 'stream.dispatch') {
    // Dispatches may nest while an outer handler waits for its host-side next.
    pendingDispatch.add(message.id);
    void queue.then(() => processMessage(message));
  } else {
    queue = queue.then(() => processMessage(message));
  }
});

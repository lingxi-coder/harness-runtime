#!/usr/bin/env node
// Execute the hash-pinned native session/root instruction cache helpers.
// Usage: node scripts/tests/instruction_cache_286_oracle.mjs /path/to/2.1.286 [--check]
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

assert(process.argv[2], 'Pass the native 2.1.286 binary as the first argument');
const binary = readFileSync(process.argv[2]);
const sha256 = createHash('sha256').update(binary).digest('hex');
assert.equal(sha256, '75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433');
const sourceByteRanges = [];
function chunk(offset) {
  const marker = '// @bun @bytecode\n';
  assert.equal(binary.subarray(offset, offset + marker.length).toString(), marker);
  const end = binary.indexOf(marker, offset + marker.length);
  const bytes = binary.subarray(offset, end < 0 ? binary.length : end);
  const nul = bytes.indexOf(0, Math.max(bytes.lastIndexOf('export{'), 0));
  return bytes.subarray(0, nul < 0 ? bytes.length : nul);
}
function bounded(offset, startMarker, endMarker, label) {
  const bytes = chunk(offset);
  const start = bytes.indexOf(startMarker);
  const end = bytes.indexOf(endMarker, start + startMarker.length);
  assert(start >= 0 && end > start, `Missing native source: ${label}`);
  const selected = bytes.subarray(start, end);
  sourceByteRanges.push({ label, source_chunk_offset: offset,
    start_marker: startMarker, end_marker: endMarker,
    start: offset + start, end: offset + end,
    sha256: createHash('sha256').update(selected).digest('hex') });
  return selected.toString('utf8');
}

const helpers = [
  bounded(175995718, 'class At{', 'class je{', 'native root-keyed At cache'),
  bounded(183727495, 'var BDn=', 'var Fot=', 'native eager-file cache and R7'),
  bounded(183727495, 'function qb(', 'function sLn(', 'native qb eager-file single flight'),
  bounded(183727495, 'function lLn(', 'function YPt(', 'native eager-file invalidation and hook cause'),
  // This initializer is one clause of a native var declaration. Only the var
  // keyword is supplied; the initializer and DN function execute unchanged.
  'var ' + bounded(183727495, 'O7=[],dM=new At', 'function qvr(', 'native context state and DN'),
  bounded(183727495, 'function Gv(', 'async function yLn(', 'native Gv and descriptor projections'),
  bounded(183727495, 'function Ctn(', 'import{basename as sNn', 'native Ctn, pT, obo, sbo, nrt and ibo'),
].join('\n');
const helperSource = `(()=>{${helpers};return {At,BDn,R7,qb,lLn,WC,KPt,ZSo,dM,DN,Gv,u$n,rbo,xst,Pst,Ctn,obo,pT,sbo,nrt,ibo};})()`;
const clone = value => JSON.parse(JSON.stringify(value));
const normalize = value => value === undefined ? null : clone(value);
function deferred() {
  let resolve, reject;
  const promise = new Promise((accept, refuse) => { resolve = accept; reject = refuse; });
  // Register observation before an explicit rejection event. The observed
  // promise is still the original promise returned by the native helper.
  promise.catch(() => {});
  return { promise, resolve, reject };
}
async function flushEvents() {
  // Promise dependency chains settle without a timer or wall-clock assertion.
  for (let index = 0; index < 8; index++) await Promise.resolve();
}

function fixtureHost(test) {
  // Native root contexts also expose root === self; sbo passes that root to
  // nrt, whose WC/pT still use the unchanged At root-key lookup.
  const roots = new Map(test.roots.map(key => {
    const root = { key }; root.root = root;
    return [key, root];
  }));
  const sessions = new Map(test.sessions.map(({ key, root, id }) => [key, { root: roots.get(root), id }]));
  const disk = new Map(test.roots.map(key => [key, []]));
  const contexts = new Map(test.roots.map(key => [key, {}]));
  const promises = new Map(), values = new Map(), subscriptions = new Map();
  const fileLoads = [], contextLoads = [], classifierPublications = [], telemetry = [];
  const memorySubscribers = new Map(), autoMemorySubscribers = new Map();
  const projectPurges = [], accountPurges = [];
  let native, currentSession = sessions.get(test.sessions[0].key);
  function subscribe(table, callback, root) {
    let rows = table.get(root);
    if (!rows) { rows = new Set(); table.set(root, rows); }
    rows.add(callback);
    return () => rows.delete(callback);
  }
  function publish(table, root, value) {
    for (const callback of table.get(root) ?? []) callback(value);
  }
  function resolveContext(record) {
    if (!record.released || record.files === undefined || record.settled) return;
    record.settled = true;
    // yLn is the substituted asynchronous host/context loader. The joined
    // fixture file contents are opaque test blocks, not a native renderer.
    const claudeMd = record.files.map(file => file.content).join('\n');
    record.resolve({ blocks: { ...record.blocks, claudeMd }, claudeMd: claudeMd || null,
      memoryFiles: record.files, instructionFiles: record.files.map(file => ({ ...file })),
      pluginInstructionFiles: record.pluginFiles });
  }
  native = vm.runInNewContext(helperSource, {
    // Actual aLn traversal and actual yLn account/project/plugin acquisition
    // are outside this oracle. Both substitutes are explicit event-controlled
    // asynchronous loaders; native qb/Gv own all memoization and invalidation.
    aLn(root, eagerState, lazyState, includeExternal) {
      const pending = deferred();
      const record = { ...pending, number: fileLoads.length + 1, root: root.key,
        includeExternal, snapshot: clone(disk.get(root.key)), settled: false };
      fileLoads.push(record);
      return record.promise;
    },
    yLn(session) {
      const pending = deferred();
      const context = contexts.get(session.root.key);
      const record = { ...pending, number: contextLoads.length + 1,
        root: session.root.key, session: session.id, blocks: clone(context.blocks ?? {}),
        pluginFiles: context.plugin_instruction_files === undefined
          ? undefined : clone(context.plugin_instruction_files),
        released: false, settled: false, files: undefined };
      contextLoads.push(record);
      native.qb(session).then(files => { record.files = files; resolveContext(record); }, error => {
        if (!record.settled) { record.settled = true; record.reject(error); }
      });
      return record.promise;
    },
    Tae: { of: () => ({}) }, // opaque lazy traversal state unused by aLn substitute
    g$o: () => {}, // unrelated memory prefetch is not executed
    zGt: value => classifierPublications.push(value),
    i: (event, payload) => telemetry.push({ event, payload: clone(payload) }),
    ert: () => {}, // hook helper registration only; u$n executes unchanged
    ARe: files => files.map(file => file.content).join('\n'), // rbo not exercised
    ort: () => projectPurges.push(currentSession.root.key),
    gLn: () => accountPurges.push(currentSession.root.key),
    U_t: (callback, root) => subscribe(memorySubscribers, callback, root),
    B_t: (callback, root) => subscribe(autoMemorySubscribers, callback, root),
    j: () => currentSession,
    d: error => { throw error; },
  }, { timeout: 2_000 });

  function inspect(session) {
    const scope = native.dM.peek(session);
    const row = scope?.byId.get(session.id);
    const eager = native.BDn.peek(session);
    return {
      refresh_reason: native.ibo(session) ?? null,
      context_cached: row?.userContext !== undefined,
      memory_files_cached: row?.userContextMemoryFiles !== undefined,
      descriptors_cached: row?.userContextInstructionFiles !== undefined,
      scope_session_ids: scope ? [...scope.byId.keys()] : [],
      context_builds_started: scope?.userContextBuildsStarted ?? 0,
      classifier_build_ordinal: scope?.classifierClaudeMdBuildOrdinal ?? 0,
      logged_plugin_instruction_files: scope?.hasLoggedPluginInstructionFiles ?? false,
      eager_cache_keys: eager ? [...eager.files.keys()] : [],
      eager_next_load_reason: eager?.nextEagerLoadReason ?? null,
      eager_should_fire_hook: eager?.shouldFireHook ?? null,
      eager_walk_epoch: eager?.walkEpoch ?? 0,
      context_load_count: contextLoads.length,
      file_load_count: fileLoads.length,
      classifier_publications: clone(classifierPublications),
      telemetry: clone(telemetry),
      project_cache_purges: clone(projectPurges),
      account_project_detail_purges: clone(accountPurges),
    };
  }
  async function operation(input) {
    const session = sessions.get(input.session ?? test.sessions[0].key);
    assert(session, `Unknown fixture session: ${input.session}`);
    currentSession = session;
    switch (input.op) {
      case 'set_files': disk.set(input.root ?? session.root.key, clone(input.files)); break;
      case 'set_context': contexts.set(input.root ?? session.root.key, clone(input.context)); break;
      case 'load': {
        const promise = native.Gv(session);
        promise.catch(() => {});
        promises.set(input.handle, promise);
        break;
      }
      case 'file_load': promises.set(input.handle, native.qb(session, input.include_external ?? false)); break;
      case 'resolve_file_load': {
        const record = fileLoads[input.build - 1];
        assert(record && !record.settled, `Unavailable file load ${input.build}`);
        record.settled = true; record.resolve(record.snapshot); break;
      }
      case 'reject_file_load': {
        const record = fileLoads[input.build - 1];
        assert(record && !record.settled, `Unavailable file load ${input.build}`);
        record.settled = true; record.reject(new Error(input.error)); break;
      }
      case 'release_context_load': {
        const record = contextLoads[input.build - 1];
        assert(record && !record.settled, `Unavailable context load ${input.build}`);
        record.released = true; resolveContext(record); break;
      }
      case 'reject_context_load': {
        const record = contextLoads[input.build - 1];
        assert(record && !record.settled, `Unavailable context load ${input.build}`);
        record.settled = true; record.reject(new Error(input.error)); break;
      }
      case 'await_load':
      case 'await_file_load': {
        assert(promises.has(input.handle), `Unknown promise handle ${input.handle}`);
        try { return { status: 'fulfilled', value: normalize(await promises.get(input.handle)) }; }
        catch (error) { return { status: 'rejected', error: error.message }; }
      }
      case 'read_descriptors': {
        const value = await native.u$n(session);
        if (input.handle) values.set(input.handle, value);
        return { value: normalize(value) };
      }
      case 'read_memory_files': {
        const value = await native.DN(session).userContextMemoryFiles;
        if (input.handle) values.set(input.handle, value);
        return { value: normalize(value) };
      }
      case 'compare_handles': {
        assert(promises.has(input.left) && promises.has(input.right));
        return { same_promise: promises.get(input.left) === promises.get(input.right) };
      }
      case 'compare_values': {
        assert(values.has(input.left) && values.has(input.right));
        return { same_identity: values.get(input.left) === values.get(input.right) };
      }
      case 'ensure_entry': native.DN(session); break;
      case 'invalidate_user_context': native.pT(session, input.reason); break;
      case 'clear_files': native.WC(session); break;
      case 'reset_eager': native.KPt(session, input.reason); break;
      case 'consume_eager_load_reason': return { reason: native.lLn(native.R7(session)) ?? null };
      case 'clear_scope': native.Ctn(session); break;
      case 'reset_plugin_log': native.obo(session); break;
      case 'subscribe_memory_state': subscriptions.set(input.handle, native.sbo(session)); break;
      case 'unsubscribe_memory_state': {
        assert(subscriptions.has(input.handle));
        subscriptions.get(input.handle)(); subscriptions.delete(input.handle); break;
      }
      case 'memory_paused': publish(memorySubscribers, session.root, input.value); break;
      case 'auto_memory_disabled': publish(autoMemorySubscribers, session.root, input.value); break;
      case 'inspect': break;
      default: throw new Error(`Unknown fixture operation: ${input.op}`);
    }
    await flushEvents();
    return inspect(session);
  }
  return { operation };
}

// Gv treats descriptor paths as opaque values. Neutral fixture names keep the
// cache contract independent of either product's instruction filename.
const oldFiles = [{ path: '/repo/instructions.md', type: 'Project', content: 'old instructions' }];
const newFiles = [{ path: '/repo/instructions.md', type: 'Project', content: 'new instructions' },
  { path: '/managed/instructions.md', type: 'Managed', content: 'managed instructions' }];
const op = (name, fields = {}) => ({ op: name, ...fields });
const setFiles = (files, fields = {}) => op('set_files', { files, ...fields });
const load = (handle, session = 'main') => op('load', { session, handle });
const awaitLoad = handle => op('await_load', { handle });
const complete = (contextBuild, fileBuild) => [
  ...(fileBuild === undefined ? [] : [op('resolve_file_load', { build: fileBuild })]),
  op('release_context_load', { build: contextBuild }),
];
const invalidation = reason => op('invalidate_user_context', { reason });
const initial = () => [setFiles(oldFiles), op('set_context', { context: { blocks: { userEmail: 'first@example.test' } } })];
const established = () => [...initial(), load('initial'), ...complete(1, 1), awaitLoad('initial')];
const mainSession = { key: 'main', root: 'root-a', id: 'main' };
const standard = (name, operations) => ({ name, roots: ['root-a'], sessions: [mainSession], operations });
const cases = [
  standard('first-load-is-single-flight-and-preserves-descriptor-identity', [
    ...initial(), load('first'), load('second'), op('compare_handles', { left: 'first', right: 'second' }),
    ...complete(1, 1), awaitLoad('first'), awaitLoad('second'),
    op('read_descriptors', { handle: 'descriptors-1' }), op('read_descriptors', { handle: 'descriptors-2' }),
    op('compare_values', { left: 'descriptors-1', right: 'descriptors-2' }),
    op('read_memory_files', { handle: 'memory-1' }), op('read_memory_files', { handle: 'memory-2' }),
    op('compare_values', { left: 'memory-1', right: 'memory-2' }), op('inspect'),
  ]),
  standard('ordinary-file-and-context-edits-do-not-refresh-gv', [
    ...established(), op('read_descriptors', { handle: 'before' }), setFiles(newFiles),
    op('set_context', { context: { blocks: { userEmail: 'second@example.test' } } }),
    load('after'), op('compare_handles', { left: 'initial', right: 'after' }), awaitLoad('after'),
    op('read_descriptors', { handle: 'after' }), op('compare_values', { left: 'before', right: 'after' }), op('inspect'),
  ]),
  standard('rejected-context-build-is-sticky-until-explicit-invalidation', [
    ...initial(), load('failed'), op('resolve_file_load', { build: 1 }),
    op('reject_context_load', { build: 1, error: 'fixture context load failed' }), awaitLoad('failed'),
    load('retry'), op('compare_handles', { left: 'failed', right: 'retry' }), awaitLoad('retry'),
    op('read_descriptors'), op('read_memory_files'), invalidation('policy_refresh'),
    load('recovered'), ...complete(2), awaitLoad('recovered'), op('inspect'),
  ]),
  { name: 'invalidation-clears-every-session-id-under-the-same-root-only', roots: ['root-a', 'root-b'],
    sessions: [mainSession, { key: 'child', root: 'root-a', id: 'child' },
      { key: 'unbuilt', root: 'root-a', id: 'unbuilt' }, { key: 'other-root-main', root: 'root-b', id: 'main' }],
    operations: [...established(), load('child', 'child'), ...complete(2), op('await_load', { session: 'child', handle: 'child' }),
      op('ensure_entry', { session: 'unbuilt' }), setFiles(newFiles, { root: 'root-b' }),
      load('other', 'other-root-main'), ...complete(3, 2), op('await_load', { session: 'other-root-main', handle: 'other' }),
      invalidation('policy_refresh'), ...['main', 'child', 'unbuilt', 'other-root-main'].map(session => op('inspect', { session })),
      load('other-again', 'other-root-main'), op('compare_handles', { left: 'other', right: 'other-again' })],
  },
  standard('first-invalidation-reason-survives-a-second-unloaded-invalidation', [
    ...established(), invalidation('policy_refresh'), invalidation('settings_sync'), op('inspect'),
    load('rebuilt'), ...complete(2), awaitLoad('rebuilt'), invalidation('directory_added'), op('inspect'),
  ]),
  standard('pt-only-refreshes-context-with-the-old-eager-files', [
    ...established(), setFiles(newFiles), op('set_context', { context: { blocks: { userEmail: 'changed@example.test' } } }),
    invalidation('hooks_invalidate'), load('rebuilt'), ...complete(2), awaitLoad('rebuilt'),
    op('read_descriptors'), op('inspect'),
  ]),
  standard('wc-only-refreshes-qb-but-keeps-the-existing-gv-promise', [
    ...established(), setFiles(newFiles), op('clear_files'), op('file_load', { handle: 'new-files' }),
    op('resolve_file_load', { build: 2 }), op('await_file_load', { handle: 'new-files' }),
    load('unchanged'), op('compare_handles', { left: 'initial', right: 'unchanged' }),
    awaitLoad('unchanged'), op('read_descriptors'), op('inspect'),
  ]),
  standard('wc-and-pt-rebuild-both-file-and-context-views', [
    ...established(), setFiles(newFiles), op('clear_files'), invalidation('policy_refresh'),
    load('rebuilt'), ...complete(2, 2), awaitLoad('rebuilt'), op('read_descriptors'), op('inspect'),
  ]),
  standard('clear-scope-restores-session-start-and-fences-an-older-pending-build', [
    ...initial(), load('old-pending'), op('resolve_file_load', { build: 1 }), op('clear_scope'),
    op('inspect'), setFiles(newFiles), op('clear_files'), load('fresh'), ...complete(2, 2), awaitLoad('fresh'),
    op('release_context_load', { build: 1 }), awaitLoad('old-pending'), op('inspect'),
  ]),
  standard('newer-pt-build-fences-an-older-context-completion', [
    ...initial(), load('old-pending'), op('resolve_file_load', { build: 1 }),
    invalidation('hooks_invalidate'), setFiles(newFiles), op('clear_files'),
    op('set_context', { context: { blocks: { userEmail: 'new-build@example.test' } } }),
    load('new-pending'), ...complete(2, 2), awaitLoad('new-pending'),
    op('release_context_load', { build: 1 }), awaitLoad('old-pending'), op('inspect'),
  ]),
  standard('pt-alone-allows-an-invalidated-pending-classifier-publication', [
    ...initial(), load('pending'), op('resolve_file_load', { build: 1 }),
    invalidation('hooks_invalidate'), op('release_context_load', { build: 1 }),
    awaitLoad('pending'), op('inspect'),
  ]),
  standard('older-context-completion-publishes-before-the-newer-build-finishes', [
    ...initial(), load('old-pending'), op('resolve_file_load', { build: 1 }),
    invalidation('hooks_invalidate'), setFiles(newFiles), op('clear_files'), load('new-pending'),
    op('resolve_file_load', { build: 2 }), op('release_context_load', { build: 1 }),
    awaitLoad('old-pending'), op('release_context_load', { build: 2 }), awaitLoad('new-pending'), op('inspect'),
  ]),
  standard('clear-scope-fences-pending-classifier-publication-without-rebuilding', [
    ...initial(), load('pending'), op('resolve_file_load', { build: 1 }), op('clear_scope'),
    op('release_context_load', { build: 1 }), awaitLoad('pending'), op('inspect'),
  ]),
  standard('reset-eager-rearms-hook-reason-and-walk-without-invalidating-gv', [
    ...established(), op('consume_eager_load_reason'), op('consume_eager_load_reason'),
    op('reset_eager', { reason: 'compact' }), op('consume_eager_load_reason'), op('consume_eager_load_reason'),
    load('same-context'), op('compare_handles', { left: 'initial', right: 'same-context' }), op('inspect'),
  ]),
  standard('native-memory-pause-subscription-clears-both-caches-and-unsubscribes', [
    ...established(), op('subscribe_memory_state', { handle: 'subscription' }),
    setFiles(newFiles), op('memory_paused', { value: true }), load('paused'), ...complete(2, 2), awaitLoad('paused'),
    op('memory_paused', { value: false }), load('resumed'), ...complete(3, 3), awaitLoad('resumed'),
    op('unsubscribe_memory_state', { handle: 'subscription' }), op('memory_paused', { value: true }),
    load('after-unsubscribe'), op('compare_handles', { left: 'resumed', right: 'after-unsubscribe' }), op('inspect'),
  ]),
  standard('native-auto-memory-state-subscription-clears-both-caches', [
    ...established(), op('subscribe_memory_state', { handle: 'subscription' }),
    op('auto_memory_disabled', { value: true }), load('off'), ...complete(2, 2), awaitLoad('off'),
    op('auto_memory_disabled', { value: false }), load('back-on'), ...complete(3, 3), awaitLoad('back-on'), op('inspect'),
  ]),
  standard('account-change-forgets-project-detail-and-preserves-the-file-cache', [
    ...established(), invalidation('account_change'), load('new-account'), ...complete(2),
    awaitLoad('new-account'), op('inspect'),
  ]),
  standard('plugin-file-telemetry-is-once-per-root-until-explicitly-reset', [
    ...initial(), op('set_context', { context: { plugin_instruction_files: { fileCount: 2, totalContentLength: 12 } } }),
    load('plugin-first'), ...complete(1, 1), awaitLoad('plugin-first'), invalidation('hooks_invalidate'),
    load('plugin-again'), ...complete(2), awaitLoad('plugin-again'), op('reset_plugin_log'),
    invalidation('hooks_invalidate'), load('plugin-reset'), ...complete(3), awaitLoad('plugin-reset'), op('inspect'),
  ]),
  standard('eager-file-cache-keys-include-external-mode', [
    ...initial(), op('file_load', { handle: 'normal' }), op('file_load', { handle: 'normal-again' }),
    op('file_load', { handle: 'external', include_external: true }),
    op('compare_handles', { left: 'normal', right: 'normal-again' }),
    op('compare_handles', { left: 'normal', right: 'external' }),
    op('resolve_file_load', { build: 1 }), op('resolve_file_load', { build: 2 }),
    op('await_file_load', { handle: 'normal' }), op('await_file_load', { handle: 'external' }), op('inspect'),
  ]),
  standard('rejected-eager-file-promise-is-separate-from-gv-recovery', [
    ...initial(), load('failed'), op('reject_file_load', { build: 1, error: 'fixture file walk failed' }),
    awaitLoad('failed'), op('read_descriptors'), op('read_memory_files'), invalidation('policy_refresh'),
    load('still-failed'), awaitLoad('still-failed'), op('clear_files'), invalidation('policy_refresh'),
    load('recovered'), ...complete(3, 2), awaitLoad('recovered'), op('inspect'),
  ]),
];

const generated = [];
for (const test of cases) {
  const host = fixtureHost(test), expected = [];
  for (const [operationIndex, input] of test.operations.entries()) {
    expected.push({ operation_index: operationIndex, result: await host.operation(input) });
  }
  generated.push({ ...test, expected });
}

// Guard the fixture harness as well as recording native outputs. These checks
// make a missing subscription, accidental extra load or a dead completion
// observable immediately rather than writing a misleading new fixture.
const byName = name => generated.find(test => test.name === name);
const last = name => byName(name).expected.at(-1).result;
const resultsFor = (name, operation) => {
  const test = byName(name);
  return test.expected.filter(row => test.operations[row.operation_index].op === operation)
    .map(row => row.result);
};
assert.deepEqual(resultsFor('first-load-is-single-flight-and-preserves-descriptor-identity', 'compare_handles'), [{ same_promise: true }]);
assert.deepEqual(resultsFor('first-load-is-single-flight-and-preserves-descriptor-identity', 'compare_values'), [{ same_identity: true }, { same_identity: true }]);
assert.equal(last('first-load-is-single-flight-and-preserves-descriptor-identity').context_load_count, 1);
assert.equal(last('ordinary-file-and-context-edits-do-not-refresh-gv').context_load_count, 1);
assert.equal(last('ordinary-file-and-context-edits-do-not-refresh-gv').file_load_count, 1);
assert.deepEqual(resultsFor('rejected-context-build-is-sticky-until-explicit-invalidation', 'await_load').map(row => row.status), ['rejected', 'rejected', 'fulfilled']);
assert.equal(last('rejected-context-build-is-sticky-until-explicit-invalidation').context_load_count, 2);
const isolation = resultsFor('invalidation-clears-every-session-id-under-the-same-root-only', 'inspect');
assert.deepEqual(isolation.map(row => row.refresh_reason), ['policy_refresh', 'policy_refresh', 'session_start', 'session_start']);
assert.deepEqual(isolation.map(row => row.context_cached), [false, false, false, true]);
assert.deepEqual(resultsFor('first-invalidation-reason-survives-a-second-unloaded-invalidation', 'inspect').map(row => row.refresh_reason), ['policy_refresh', 'directory_added']);
assert.equal(last('pt-only-refreshes-context-with-the-old-eager-files').file_load_count, 1);
assert.deepEqual(resultsFor('pt-only-refreshes-context-with-the-old-eager-files', 'read_descriptors')[0].value, oldFiles);
assert.equal(last('wc-only-refreshes-qb-but-keeps-the-existing-gv-promise').context_load_count, 1);
assert.deepEqual(resultsFor('wc-only-refreshes-qb-but-keeps-the-existing-gv-promise', 'read_descriptors')[0].value, oldFiles);
assert.deepEqual(resultsFor('wc-and-pt-rebuild-both-file-and-context-views', 'read_descriptors')[0].value, newFiles);
const newClaudeMd = newFiles.map(file => file.content).join('\n');
assert.deepEqual(last('clear-scope-restores-session-start-and-fences-an-older-pending-build').classifier_publications, [newClaudeMd]);
assert.deepEqual(last('newer-pt-build-fences-an-older-context-completion').classifier_publications, [newClaudeMd]);
assert.deepEqual(last('pt-alone-allows-an-invalidated-pending-classifier-publication').classifier_publications, ['old instructions']);
assert.equal(last('pt-alone-allows-an-invalidated-pending-classifier-publication').context_cached, false);
assert.deepEqual(last('older-context-completion-publishes-before-the-newer-build-finishes').classifier_publications, ['old instructions', newClaudeMd]);
assert.deepEqual(last('clear-scope-fences-pending-classifier-publication-without-rebuilding').classifier_publications, []);
assert.deepEqual(resultsFor('reset-eager-rearms-hook-reason-and-walk-without-invalidating-gv', 'consume_eager_load_reason').map(row => row.reason), ['session_start', null, 'compact', null]);
assert.equal(last('native-memory-pause-subscription-clears-both-caches-and-unsubscribes').refresh_reason, 'memory_resumed');
assert.equal(last('native-memory-pause-subscription-clears-both-caches-and-unsubscribes').context_load_count, 3);
assert.equal(last('native-auto-memory-state-subscription-clears-both-caches').refresh_reason, 'auto_memory_back_on');
assert.equal(last('native-auto-memory-state-subscription-clears-both-caches').file_load_count, 3);
assert.deepEqual(last('account-change-forgets-project-detail-and-preserves-the-file-cache').account_project_detail_purges, ['root-a']);
assert.equal(last('plugin-file-telemetry-is-once-per-root-until-explicitly-reset').telemetry.length, 2);
assert.deepEqual(last('eager-file-cache-keys-include-external-mode').eager_cache_keys, [false, true]);
assert.deepEqual(resultsFor('rejected-eager-file-promise-is-separate-from-gv-recovery', 'await_load').map(row => row.status), ['rejected', 'rejected', 'fulfilled']);

const result = JSON.stringify({ version: '2.1.286', binary_sha256: sha256,
  source_byte_ranges: sourceByteRanges,
  execution: {
    actual_native_helpers: ['At', 'BDn', 'R7', 'qb', 'lLn', 'WC', 'KPt', 'DN', 'Gv', 'u$n', 'Ctn', 'obo', 'pT', 'sbo', 'nrt', 'ibo'],
    substitutions: {
      aLn: 'Event-controlled asynchronous file traversal; snapshots the supplied file array at invocation, with explicit completion or rejection.',
      yLn: 'Event-controlled asynchronous context loader using actual native qb; fixture opaque blocks and file descriptors substitute account/project/plugin acquisition and instruction rendering.',
      feature_state: 'U_t and B_t are deterministic per-root callback subscriptions; actual native sbo/nrt select causes and invalidate caches.',
      external_effects: 'Classifier publication, project cache purge, account project-detail purge and plugin telemetry are recorded callbacks; unrelated prefetch and helper registration are no-ops.',
      timing: 'Explicit completion events and Promise microtasks only; no sleeps, clocks, provider calls, subprocesses or filesystem traversal.',
    },
    native_yLn_executed: false, native_aLn_executed: false,
    native_instruction_renderer_executed: false,
    identity_observations: 'Promise and descriptor identity booleans are reference evidence. Portable cache tests can compare payloads, build counts, reasons, root isolation and recorded effects.',
  }, cases: generated }, null, 2) + '\n';
const fixture = new URL('../../crates/orchestrator/tests/fixtures/instruction_cache_2_1_286.json', import.meta.url);
if (process.argv.includes('--check')) assert.equal(readFileSync(fixture, 'utf8'), result);
else { mkdirSync(dirname(fileURLToPath(fixture)), { recursive: true }); writeFileSync(fixture, result); }
console.log(`Verified ${cases.length} native instruction-cache scenarios and ${generated.reduce((sum, test) => sum + test.expected.length, 0)} deterministic operations (${sha256})`);

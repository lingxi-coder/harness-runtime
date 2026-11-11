#!/usr/bin/env node
/** Real disk -> bridge engine -> Remote Control gateway -> encrypted RTDB tunnel.
 * Only the approved-viewer identity and tunnel keys are test fixtures. No engine,
 * transcript reader, catalog subprocess, gateway handler, or network is mocked.
 */
import assert from 'node:assert/strict';
import { createHash, randomUUID, webcrypto } from 'node:crypto';
import { mkdirSync, readFileSync, readdirSync, renameSync, writeFileSync, symlinkSync } from 'node:fs';
import { createServer } from 'node:http';
import childProcess from 'node:child_process';
import { syncBuiltinESMExports } from 'node:module';
import { resolve, join, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const flags = Object.fromEntries(Array.from({ length: (process.argv.length - 2) / 2 }, (_, i) => process.argv.slice(2 + i * 2, 4 + i * 2)));
const app = resolve(flags['--app']);
const binary = resolve(flags['--bridge-server']);
const output = resolve(flags['--output']);
const origin = flags['--emulator-origin'];
assert.match(origin, /^http:\/\/127\.0\.0\.1:\d+$/);
mkdirSync(output, { recursive: false, mode: 0o700 });
const ledgerPath = join(output, 'owned-bridge-processes.json');
const ownedGroups = [];
const ownedMetadataWorkers = [];
const metadataHandles = new Map();
const ledgerErrors = [];
let recordingStopped = false;
const saveLedger = () => {
  if (recordingStopped) return;
  writeFileSync(`${ledgerPath}.tmp`, `${JSON.stringify({ binary, runnerPid: process.pid, groups: ownedGroups, metadataWorkers: ownedMetadataWorkers }, null, 2)}\n`);
  renameSync(`${ledgerPath}.tmp`, ledgerPath);
};
saveLedger();
function observeGroup(group) {
  if (group.retired) return;
  try { process.kill(-group.pgid, 0); }
  catch (error) {
    if (error.code === 'ESRCH') group.retired = true;
    return;
  }
  let body;
  try { body = childProcess.execFileSync('/bin/ps', ['-ww', '-g', String(group.pgid), '-o', 'pid=,ppid=,pgid=,lstart=,command='], { encoding: 'utf8', timeout: 1000, maxBuffer: 1024 * 1024 }); }
  catch { return; }
  const rows = body.trim().split('\n').flatMap((line) => {
    const parts = line.trim().split(/\s+/);
    if (parts.length < 9 || !parts.slice(0, 3).every((value) => /^\d+$/.test(value)) || Number(parts[2]) !== group.pgid) return [];
    return [{ pid: Number(parts[0]), ppid: Number(parts[1]), birth: parts.slice(3, 8).join(' '), command: parts.slice(8).join(' ') }];
  });
  const known = new Map(group.members.map((member) => [member.pid, member]));
  const leader = rows.find((row) => row.pid === group.pid);
  if (leader && (leader.command.startsWith(`${group.execPath} `) || leader.command.startsWith(`${binary} `)) && leader.command.includes(group.bridgeDir) && leader.command.includes(group.workspace) && (!known.has(leader.pid) || known.get(leader.pid).birth === leader.birth)) {
    known.set(leader.pid, { pid: leader.pid, ppid: leader.ppid, birth: leader.birth, provedFromPid: process.pid });
  }
  // Capture descendant identity only while a current parent/ancestor is known.
  for (let pass = 0; pass < rows.length; pass++) {
    for (const row of rows) {
      const parent = rows.find((candidate) => candidate.pid === row.ppid);
      if (parent && known.get(parent.pid)?.birth === parent.birth && !known.has(row.pid)) known.set(row.pid, { pid: row.pid, ppid: row.ppid, birth: row.birth, provedFromPid: parent.pid });
    }
  }
  group.members = [...known.values()];
}
function observeMetadataWorker(worker) {
  if (worker.closed) return;
  let body;
  try { body = childProcess.execFileSync('/bin/ps', ['-ww', '-p', String(worker.pid), '-o', 'pid=,ppid=,pgid=,lstart=,command='], { encoding: 'utf8', timeout: 1000, maxBuffer: 1024 * 1024 }); }
  catch { return; }
  const parts = body.trim().split(/\s+/);
  if (parts.length < 9 || !parts.slice(0, 3).every((value) => /^\d+$/.test(value))) return;
  const row = { pid: Number(parts[0]), ppid: Number(parts[1]), pgid: Number(parts[2]), birth: parts.slice(3, 8).join(' '), command: parts.slice(8).join(' ') };
  if (row.pid !== worker.pid || row.pgid !== process.pid || !row.command.startsWith(`${worker.execPath} `) || !row.command.endsWith(' --session-catalog-worker-json')) return;
  if (worker.birth === undefined) {
    if (row.ppid !== worker.parentPid) return;
    worker.birth = row.birth;
  }
}
// Instrument the actual OS launch only. Engine execution and history remain
// production code. Metadata workers are owned children, never engine groups.
const originalSpawn = childProcess.spawn;
childProcess.spawn = function (file, args, options) {
  const child = originalSpawn(file, args, options);
  const at = Array.isArray(args) ? args.indexOf('--bridge-dir') : -1;
  const bridgeDir = at < 0 ? undefined : args[at + 1];
  if (options?.detached === true && options.cwd === join(output, 'workspace') && (resolve(file) === binary || resolve(file) === join(output, 'resources/bin/bridge-server')) && typeof bridgeDir === 'string' && bridgeDir.startsWith(`${join(output, 'bridges')}/`) && Number.isInteger(child.pid)) {
    const group = { pid: child.pid, pgid: child.pid, execPath: resolve(file), workspace: options.cwd, bridgeDir, startedAt: new Date().toISOString(), members: [], retired: false };
    ownedGroups.push(group);
    child.once('exit', (code, signal) => {
      group.leaderExit = { code, signal, at: new Date().toISOString() };
      try { observeGroup(group); saveLedger(); } catch (error) { ledgerErrors.push(String(error)); }
    });
    try { saveLedger(); observeGroup(group); saveLedger(); }
    catch (error) {
      // ENOSPC must not strand the brand-new detached process before its PID
      // can be persisted. The returned, unreaped child is still our launch.
      try { process.kill(-child.pid, 'SIGKILL'); } catch { /* It already exited. */ }
      throw error;
    }
  } else if (options?.detached !== true && options?.cwd === join(output, 'workspace') && (resolve(file) === binary || resolve(file) === join(output, 'resources/bin/bridge-server')) && Array.isArray(args) && args.length === 1 && args[0] === '--session-catalog-worker-json' && Number.isInteger(child.pid)) {
    const worker = { pid: child.pid, parentPid: process.pid, pgid: process.pid, execPath: resolve(file), argv: [...args], workspace: options.cwd, purpose: 'session-catalog-metadata', startedAt: new Date().toISOString() };
    ownedMetadataWorkers.push(worker);
    const closed = new Promise((resolveClose) => child.once('close', (code, signal) => {
      worker.closed = { code, signal, at: new Date().toISOString() };
      worker.retired = true;
      try { saveLedger(); } catch (error) { ledgerErrors.push(String(error)); }
      resolveClose();
    }));
    metadataHandles.set(worker, { child, closed });
    try { saveLedger(); observeMetadataWorker(worker); saveLedger(); }
    catch (error) {
      // This original, unreaped child handle proves ownership; it is not a group leader.
      child.kill('SIGKILL');
      throw error;
    }
  }
  return child;
};
syncBuiltinESMExports();
const processObserver = setInterval(() => { try { for (const group of ownedGroups) observeGroup(group); for (const worker of ownedMetadataWorkers) observeMetadataWorker(worker); saveLedger(); } catch (error) { ledgerErrors.push(String(error)); } }, 250);
processObserver.unref();
const load = (file) => import(pathToFileURL(join(app, file)).href);
const [{ SessionGateway }, { SessionRuntimeManager }, { RemotePolicy }, { RemoteCommands }, { SettingsStore }, { ProjectSessionCatalog }, { DiagnosticBuffer }, { resolveSavedConversationTranscript }, remote] = await Promise.all([
  load('apps/electron/src/main/remoteControl/sessionGateway.ts'),
  load('apps/electron/src/main/sessionRuntimeManager.ts'),
  load('apps/electron/src/main/remoteControl/policy.ts'),
  load('apps/electron/src/main/remoteControl/remoteCommands.ts'),
  load('apps/electron/src/main/settings.ts'),
  load('apps/electron/src/main/session-catalog.ts'),
  load('apps/electron/src/main/host-utils.ts'),
  load('apps/electron/src/main/host.ts'),
  load('packages/bridge-client/dist/remote.js'),
]);
const { RtdbClient, RtdbError, RtdbMirror, RpcPeer, MessageTransport, TunnelLink, deriveTunnelKeys, randomId, upPath, downPath, toHostPath, parseFrameKey, systemClock } = remote;
const sha = (path) => createHash('sha256').update(readFileSync(path)).digest('hex');
const dump = (name, value) => writeFileSync(join(output, name), `${JSON.stringify(value, null, 2)}\n`);
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
async function bounded(label, operation, milliseconds) {
  let timer;
  try {
    await Promise.race([Promise.resolve().then(operation), new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`${label} timed out`)), milliseconds); })]);
    return { label, passed: true };
  } catch (error) { return { label, passed: false, error: String(error) }; }
  finally { clearTimeout(timer); }
}
async function stopMetadataWorkers() {
  await Promise.all([...metadataHandles].map(async ([worker, { child, closed }]) => {
    if (!worker.closed) {
      child.kill('SIGTERM');
      const graceful = await bounded(`metadata worker ${worker.pid} close`, () => closed, 1500);
      if (!graceful.passed) {
        child.kill('SIGKILL');
        const forced = await bounded(`metadata worker ${worker.pid} reap`, () => closed, 1500);
        assert.ok(forced.passed, forced.error);
      }
    }
    assert.ok(worker.closed, `metadata worker ${worker.pid} must emit ChildProcess close`);
  }));
  saveLedger();
}
async function until(predicate, label) {
  const deadline = Date.now() + 20_000;
  while (!(await predicate())) {
    assert.ok(Date.now() < deadline, `timed out: ${label}`);
    await delay(20);
  }
}
const workspace = join(output, 'workspace');
const config = join(output, 'config');
const userData = join(output, 'desktop-settings');
const resources = join(output, 'resources');
for (const directory of [workspace, config, userData, join(output, 'tmp'), join(output, 'bridges'), join(resources, 'bin')]) mkdirSync(directory, { recursive: true, mode: 0o700 });
symlinkSync(binary, join(resources, 'bin', 'bridge-server'));
const globalConfigPath = join(config, '.claude.json');
writeFileSync(globalConfigPath, '{"hasCompletedOnboarding":true}\n');
writeFileSync(join(config, 'settings.json'), '{"model":"claude-sonnet-4-6"}\n');
const ids = [randomUUID(), randomUUID(), randomUUID()];
const title = 'REMOTE 321 same title \u4f1a\u8bdd \ud83c\udf31';
const sourcePaths = ids.map((id, index) => {
  const path = join(config, 'projects', workspace.replace(/[^a-zA-Z0-9]/g, '-'), `${id}.jsonl`);
  mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
  const userId = randomUUID();
  const common = { cwd: workspace, sessionId: id, isSidechain: false, version: '2.1.321', timestamp: '2026-11-09T10:00:00.000Z' };
  const rows = [
    { type: 'fixed-session-mode', sessionId: id, sessionMode: 'code', instructionEpoch: 0 },
    { type: 'custom-title', sessionId: id, customTitle: title },
    { ...common, type: 'user', uuid: userId, parentUuid: null, message: { role: 'user', content: `REMOTE_SOURCE_USER_${index}` } },
    { ...common, type: 'assistant', uuid: randomUUID(), parentUuid: userId, message: { id: `msg_remote_${index}`, type: 'message', role: 'assistant', model: 'claude-sonnet-4-6', content: [{ type: 'text', text: `REMOTE_SOURCE_ASSISTANT_${index}` }], stop_reason: 'end_turn', stop_sequence: null, usage: { input_tokens: 10, output_tokens: 4 } } },
  ];
  writeFileSync(path, `${rows.map((row) => JSON.stringify(row)).join('\n')}\n`);
  return path;
});
const initialHashes = sourcePaths.map(sha);
dump('fixture-inputs.json', sourcePaths.map((path) => ({ path, utf8: readFileSync(path, 'utf8') })));
const uid = `remote321-${randomId()}`;
const hostId = randomId();
const namespace = `demo-session-gateway-321-${randomUUID()}`;
const rules = readFileSync(join(app, 'infra/firebase/database.rules.json'), 'utf8');
const rulesResponse = await fetch(`${origin}/.settings/rules.json?ns=${namespace}`, { method: 'PUT', headers: { Authorization: 'Bearer owner' }, body: rules });
assert.equal(rulesResponse.status, 200, await rulesResponse.text());
function token(user) {
  const part = (value) => Buffer.from(JSON.stringify(value)).toString('base64url');
  const now = Math.floor(Date.now() / 1000);
  return `${part({ alg: 'none', typ: 'JWT' })}.${part({ iss: 'https://securetoken.google.com/demo-lingxi-backend', aud: 'demo-lingxi-backend', auth_time: now, user_id: user, sub: user, iat: now, exp: now + 3600, firebase: { identities: {}, sign_in_provider: 'password' } })}.`;
}
const db = (user = uid) => new RtdbClient({ databaseUrl: origin, namespace, getIdToken: async () => token(user) });
const rawFrames = [];
function capturedDb(inner, direction) {
  const capture = (key, value) => {
    if (typeof value === 'string' && /^f\d+$/.test(key)) rawFrames.push({ direction, key, value });
    else if (value && typeof value === 'object') for (const [child, data] of Object.entries(value)) capture(child, data);
  };
  return new Proxy(inner, { get(target, property) {
    if (property === 'listen') return (path, onEvent, onError) => target.listen(path, (event) => { capture(event.path.split('/').at(-1), event.data); onEvent(event); }, onError);
    const value = target[property];
    return typeof value === 'function' ? value.bind(target) : value;
  } });
}
const refusedNetwork = [];
const deny = createServer((request, response) => {
  refusedNetwork.push({ method: request.method, path: request.url });
  response.writeHead(503, { 'Content-Type': 'application/json' });
  response.end('{"error":{"type":"api_error","message":"isolated remote storage test has no model API"}}');
});
await new Promise((resolve) => deny.listen(0, '127.0.0.1', resolve));
const apiOrigin = `http://127.0.0.1:${deny.address().port}`;
// The production telemetry guard restores proxies from its host process, not
// workspace settings. Keep every optional engine HTTP request on this sink.
Object.assign(process.env, {
  HTTP_PROXY: apiOrigin, HTTPS_PROXY: apiOrigin, ALL_PROXY: apiOrigin,
  NO_PROXY: '127.0.0.1,localhost',
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: '1', DISABLE_TELEMETRY: '1',
});
const diagnostics = [];
let step = 'starting';
let desktop;
let tunnel;
const checks = {};
async function startDesktop(seed = false) {
  const settings = new SettingsStore(userData, join(output, 'managed-workspaces'));
  if (seed) {
    settings.addProject(workspace);
    settings.setTrust(workspace, true);
    settings.setRemoteControlAtStartup(true);
    for (const [index, sessionId] of ids.entries()) {
      const owner = { projectPath: workspace, projectId: workspace, sessionId, mode: 'code' };
      await settings.conversationSpaces.register(owner);
      settings.rememberSessionTranscriptSelection(owner, { catalogProjectPath: workspace, scope: 'current', transcriptPath: sourcePaths[index] });
    }
    for (const sessionId of ids.slice(0, 2)) settings.setRemoteSession({ projectPath: workspace, sessionId, mode: 'code' }, true);
  }
  assert.deepEqual(settings.getRemoteSessions().map((ref) => ref.sessionId).sort(), ids.slice(0, 2).sort(), 'sharing must be read back from disk without reseeding');
  assert.equal(settings.getTrust(workspace).trusted, true, 'only this owned fixture workspace is trusted, and trust must persist across reload');
  for (const [index, sessionId] of ids.entries()) {
    const ref = { projectPath: workspace, sessionId };
    assert.deepEqual(settings.conversationSpaces.find(ref), { ...ref, projectId: workspace, mode: 'code' });
    assert.equal(settings.getSessionTranscriptSelection(ref)?.transcriptPath, sourcePaths[index]);
  }
  const catalog = new ProjectSessionCatalog({ serverBin: binary, configHome: config, timeoutMs: 30_000, cacheTtlMs: 0 });
  const manager = new SessionRuntimeManager({
    isPackaged: true, resourcesPath: resources,
    bridgeRoot: join(output, 'bridges'), lockfileTimeoutMs: 45_000, sessionResumeTimeoutMs: 30_000, stopTimeoutMs: 2000,
    diagnostics: new DiagnosticBuffer(join(output, 'engine-diagnostics.jsonl')),
    // Never discover or adopt an engine outside this test's launch directories.
    listProcessCommands: () => [],
    resolveSavedTranscript: (ref) => resolveSavedConversationTranscript(settings, catalog, ref),
    resolveSessionMode: async (ref) => settings.conversationSpaces.find(ref)?.mode,
    accessState: () => ({ workspace, trusted: settings.getTrust(workspace).trusted }),
    launchConfig: (ref) => ({
      workspace, sessionId: ref.sessionId, configHome: config, globalConfigPath, trusted: settings.getTrust(workspace).trusted,
      model: 'claude-sonnet-4-6', apiBaseUrl: apiOrigin,
      // Empty process-local account registry: no secure store or Broker reads.
      providerAccounts: { storeScope: `remote321-${uid}`, configRevision: 0, accounts: [], health: [] },
      providerAccountCredentials: {},
      telemetry: { enabled: false, endpoint: apiOrigin, headers: {} },
      settingsEnvironment: {
        HTTP_PROXY: apiOrigin, HTTPS_PROXY: apiOrigin, ALL_PROXY: apiOrigin, NO_PROXY: '127.0.0.1,localhost',
        LINGXI_CONFIG_DIR: config, CLAUDE_CONFIG_DIR: config,
        CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: '1', CLAUDE_CODE_DISABLE_FEATURE_FLAGS: '1',
        CLAUDE_CODE_DISABLE_AUTO_MEMORY: '1', DISABLE_AUTOUPDATER: '1',
      },
    }),
  });
  const policy = new RemotePolicy({ settings, managedDirectory: join(output, 'managed-policy') });
  const gateway = new SessionGateway({
    runtimes: manager, policy,
    commands: new RemoteCommands({ allowAlwaysAllow: () => false, echo: () => {}, clock: systemClock }),
    titleOf: async (ref) => (await catalog.find(ref.projectPath, ref.sessionId))?.title,
    modelOf: () => 'claude-sonnet-4-6', clock: systemClock,
    diagnostics: (level, message) => diagnostics.push({ level, message }),
  });
  return { settings, catalog, manager, gateway, async close() {
    try { gateway.dispose(); }
    finally {
      try { await manager.dispose(); }
      finally {
        catalog.dispose();
        await settings.conversationSpaces.flush();
        await stopMetadataWorkers();
      }
    }
  } };
}
async function connectGateway(gateway) {
  const connId = randomId();
  const keys = await deriveTunnelKeys(webcrypto.getRandomValues(new Uint8Array(32)), connId);
  const viewerDb = capturedDb(db(), 'down'), hostDb = capturedDb(db(), 'up');
  const up = upPath(uid, hostId, connId), down = downPath(uid, hostId, connId);
  const viewerLink = new TunnelLink({ rtdb: viewerDb, connId, sendPath: up, sendDir: 'up', sendKey: keys.up, recvPath: down, recvDir: 'down', recvKey: keys.down, subscribe: true });
  const hostLink = new TunnelLink({ rtdb: hostDb, connId, sendPath: down, sendDir: 'down', sendKey: keys.down, recvPath: up, recvDir: 'up', recvKey: keys.up, subscribe: false });
  const mirror = new RtdbMirror();
  const inbox = hostDb.listen(toHostPath(uid, hostId), (event) => {
    for (const change of mirror.apply(event).at(3)) {
      const [connection, side, key] = change.path;
      if (connection !== connId || side !== 'up' || change.kind === 'removed') continue;
      const sequence = parseFrameKey(key);
      if (sequence !== null) hostLink.deliverFrame(sequence, change.value);
    }
  });
  const viewerRpc = new RpcPeer(new MessageTransport(viewerLink), { clock: systemClock, timeoutMs: 60_000 });
  const hostRpc = new RpcPeer(new MessageTransport(hostLink), { clock: systemClock, timeoutMs: 60_000 });
  const viewer = { account_uid: uid, host_id: hostId, device_id: randomId(), device_name: 'isolated remote321 viewer', platform: 'web', conn_id: connId };
  const notifications = [];
  viewerRpc.onNotification((method, params) => notifications.push({ method, params }));
  const channel = { notify(method, params) { void hostRpc.notify(method, params); }, request(method, params) { return hostRpc.request(method, params); } };
  hostRpc.onRequest((method, params) => gateway.handle(viewer, method, params, channel));
  return {
    rpc: viewerRpc, notifications, connId,
    async close() {
      gateway.viewerClosed(viewer);
      viewerRpc.close(); hostRpc.close(); inbox.close();
      await Promise.all([viewerLink.whenClosed(), hostLink.whenClosed()]);
    },
  };
}
let cleanupPromise;
function cleanupAll() {
  return cleanupPromise ??= (async () => {
    // A rejected/hung tunnel must never prevent manager disposal.
    const steps = await Promise.all([
      bounded('tunnel close', () => tunnel?.close(), 2000),
      bounded('desktop dispose', () => desktop?.close(), 4000),
    ]);
    // Run even after a failed desktop disposal so no metadata child is omitted.
    steps.push(await bounded('metadata worker close/reap', stopMetadataWorkers, 4000));
    clearInterval(processObserver);
    try { for (const group of ownedGroups) observeGroup(group); saveLedger(); }
    catch (error) { ledgerErrors.push(String(error)); }
    recordingStopped = true;
    let processes;
    try {
      const body = await new Promise((resolve, reject) => childProcess.execFile(process.env.LINGXI_REMOTE_TEST_PYTHON, [fileURLToPath(new URL('./session_remote_321_gateway_live.py', import.meta.url)), '--cleanup-ledger', ledgerPath], { encoding: 'utf8', timeout: 10_000 }, (error, stdout) => error ? reject(error) : resolve(stdout)));
      processes = JSON.parse(body);
    } catch (error) { processes = { verified: false, error: String(error) }; }
    steps.push(...await Promise.all([
      bounded('RTDB records cleanup', () => db().remove(`/remote-control/${uid}`), 2000),
      bounded('loopback deny server close', () => new Promise((resolve) => deny.close(resolve)), 2000),
    ]));
    dump('diagnostics.json', diagnostics);
    dump('raw-rtdb-frames.json', rawFrames);
    const result = { verified: processes.verified === true && steps.every((step) => step.passed) && ledgerErrors.length === 0, steps, processes, ledgerErrors };
    dump('cleanup.json', result);
    return result;
  })();
}
process.once('SIGTERM', async () => { try { await cleanupAll(); } finally { process.exit(143); } });
let outcome;
let failure;
let cleanup;
try {
  step = 'real catalog and persisted title/UUID';
  desktop = await startDesktop(true);
  const catalog = await desktop.catalog.listComplete(workspace);
  dump('catalog-first.json', catalog);
  assert.deepEqual(catalog.sessions.map((row) => row.uuid).sort(), [...ids].sort());
  assert.ok(catalog.sessions.every((row) => row.title === title && row.mode === 'code'));
  await assert.rejects(resolveSavedConversationTranscript(desktop.settings, desktop.catalog, { projectPath: join(output, 'foreign-workspace'), sessionId: ids[0] }), /owner does not match/);
  checks.savedTranscriptOwnerCannotBeBypassed = true;
  tunnel = await connectGateway(desktop.gateway);
  const list = await tunnel.rpc.request('sessions.list', {});
  dump('remote-list-first.json', list);
  assert.deepEqual(list.sessions.map((row) => row.session_id).sort(), ids.slice(0, 2).sort());
  assert.ok(list.sessions.every((row) => row.title === title && row.status === 'dormant'));
  checks.persistedTitleAndDistinctUuids = true;

  step = 'real engine cold resume and paged history';
  const attached = await tunnel.rpc.request('session.attach', { session_id: ids[0] });
  assert.ok(ownedGroups.length >= 1, 'real first hydration requires a recorded detached bridge launch');
  const firstBridgePids = new Set(ownedGroups.map((group) => group.pid));
  assert.ok(ownedMetadataWorkers.length >= 1, 'fresh catalog must launch its real metadata worker');
  const firstMetadataPids = new Set(ownedMetadataWorkers.map((worker) => worker.pid));
  assert.equal(attached.mode, 'snapshot');
  await until(() => tunnel.notifications.some((item) => item.method === 'session.snapshot' && item.params.phase === 'end'), 'snapshot end');
  const firstSnapshot = tunnel.notifications.filter((item) => item.method === 'session.snapshot').map((item) => item.params);
  const messages = firstSnapshot.filter((frame) => frame.phase === 'messages').flatMap((frame) => frame.messages);
  assert.equal(messages.length, 2);
  assert.match(JSON.stringify(messages), /REMOTE_SOURCE_USER_0/);
  assert.match(JSON.stringify(messages), /REMOTE_SOURCE_ASSISTANT_0/);
  assert.ok(!JSON.stringify(messages).includes('REMOTE_SOURCE_USER_1'));
  assert.equal(firstSnapshot[0].session_id, ids[0]);
  assert.equal(firstSnapshot[0].session_mode, 'code');
  const history = await tunnel.rpc.request('session.history', { session_id: ids[0], before_index: 2, limit: 1 });
  assert.equal(history.first_index, 1);
  assert.equal(history.has_more, true);
  assert.equal(history.messages.length, 1);
  assert.match(JSON.stringify(history.messages), /REMOTE_SOURCE_ASSISTANT_0/);
  dump('remote-snapshot-first.json', { attached, frames: firstSnapshot, history });
  checks.realEngineColdResumeAndHistory = true;

  step = 'remote fork rejection and source preservation';
  await assert.rejects(tunnel.rpc.request('prompt.send', { session_id: ids[0], text: '/fork', client_request_id: randomId() }), (error) => error?.code === 'forbidden');
  assert.equal(sha(sourcePaths[0]), initialHashes[0]);
  assert.equal((await desktop.catalog.listComplete(workspace)).sessions.length, 3);
  checks.remoteForkRejectedSourceUnchanged = true;
  const cursor = firstSnapshot.find((frame) => frame.phase === 'end').cursor;

  step = 'same-runtime reconnect cursor';
  await tunnel.close(); tunnel = await connectGateway(desktop.gateway);
  const delta = await tunnel.rpc.request('session.attach', { session_id: ids[0], resume: { epoch: cursor.epoch, after_seq: cursor.seq } });
  assert.equal(delta.mode, 'delta');
  checks.sameRuntimeCursorDelta = true;

  step = 'cold desktop reload';
  await tunnel.close(); tunnel = undefined;
  await desktop.close();
  assert.deepEqual(readdirSync(join(output, 'bridges'), { recursive: true }).filter((path) => path.endsWith('.lock')), [], 'cold reload must leave no previous owned bridge endpoint');
  checks.previousOwnedBridgeEndpointClosed = true;
  desktop = await startDesktop();
  checks.persistedOwnersPathsAndSharingReadBackWithoutSeeding = true;
  tunnel = await connectGateway(desktop.gateway);
  const reloadedList = await tunnel.rpc.request('sessions.list', {});
  assert.deepEqual(reloadedList.sessions.map((row) => row.session_id).sort(), ids.slice(0, 2).sort());
  assert.ok(reloadedList.sessions.every((row) => row.title === title));
  const cold = await tunnel.rpc.request('session.attach', { session_id: ids[0], resume: { epoch: cursor.epoch, after_seq: cursor.seq } });
  assert.ok(ownedGroups.length >= 2 && ownedGroups.some((group) => !firstBridgePids.has(group.pid)), 'cold reload requires a second, distinct recorded real bridge process');
  assert.ok(ownedMetadataWorkers.some((worker) => !firstMetadataPids.has(worker.pid)), 'cold reload must own a new metadata worker after the first worker was reaped');
  checks.expectedRealBridgeLaunchesRecorded = true;
  assert.equal(cold.mode, 'snapshot');
  assert.notEqual(cold.epoch, cursor.epoch);
  await until(() => tunnel.notifications.some((item) => item.method === 'session.snapshot' && item.params.phase === 'end'), 'cold snapshot end');
  const coldFrames = tunnel.notifications.filter((item) => item.method === 'session.snapshot').map((item) => item.params);
  const coldMessages = coldFrames.filter((frame) => frame.phase === 'messages').flatMap((frame) => frame.messages);
  assert.deepEqual(coldMessages, messages);
  dump('remote-cold-reload.json', { list: reloadedList, attached: cold, frames: coldFrames });
  checks.coldReloadIdentityHistoryAndCursorReset = true;

  step = 'unshared session and cross-account auth isolation';
  await assert.rejects(tunnel.rpc.request('session.attach', { session_id: ids[2] }), (error) => error?.code === 'unknown_session');
  await assert.rejects(tunnel.rpc.request('session.history', { session_id: ids[2], before_index: 2, limit: 1 }), (error) => error?.code === 'unknown_session' || error?.code === 'invalid_params');
  const other = db(`other321-${randomId()}`);
  await assert.rejects(other.get(`/remote-control/${uid}`), (error) => error instanceof RtdbError && error.status === 401);
  await assert.rejects(other.put(`/remote-control/${uid}/to_host/${hostId}/${tunnel.connId}/up/f0`, 'x'), (error) => error instanceof RtdbError && error.status === 401);
  assert.ok(rawFrames.some((frame) => frame.direction === 'up') && rawFrames.some((frame) => frame.direction === 'down'), 'observe nonempty actual RTDB SSE frames in both directions before acknowledgment deletion');
  assert.ok(rawFrames.length >= 18 && rawFrames.every((frame) => frame.value.length > 0), 'preserve at least 18 nonempty encrypted RTDB frame captures');
  const wire = rawFrames.map((frame) => frame.value).join('\n');
  assert.ok(!wire.includes(title) && !wire.includes('REMOTE_SOURCE_') && !wire.includes(ids[0]), 'actual stored RTDB frame values must not contain transcript markers');
  checks.unsharedHiddenAndCrossAccountDenied = true;
  checks.nonemptyRawRtdbFramesContainNoPlaintextMarkers = true;
  for (let index = 0; index < sourcePaths.length; index++) assert.equal(sha(sourcePaths[index]), initialHashes[index]);
  checks.allSourceBytesUnchanged = true;
  outcome = {
    passed: true, checks, bridgeServerSha256: sha(binary), repositoryRulesSha256: sha(join(app, 'infra/firebase/database.rules.json')),
    namespace, endpoint: origin, sessionIds: ids, sources: sourcePaths.map((path, index) => ({ path, beforeSha256: initialHashes[index], afterSha256: sha(path) })),
    refusedNonessentialRequests: refusedNetwork,
    capturedRawFrameCount: rawFrames.length,
    recordedBridgeGroupCount: ownedGroups.length,
    recordedBridgePids: ownedGroups.map((group) => group.pid),
    recordedMetadataWorkerCount: ownedMetadataWorkers.length,
    recordedMetadataWorkerPids: ownedMetadataWorkers.map((worker) => worker.pid),
    coverage: 'Production catalog subprocess, local JSONL parser, bridge engine WebSocket, SessionRuntimeManager, Remote Control gateway, snapshot/history/cursor handlers, encrypted RTDB HTTP/SSE tunnel, repository database rules, persisted desktop settings.',
    limitations: ['Local emulator only; no deployed cloud acceptance.', 'Approved viewer identity and tunnel keys are injected after the pairing seam; handshake approval is covered by the separate existing remote-e2e tests.', 'No native StorageV5 service was exercised: pinned public native factory has no backend.', 'Remote fork is host-only; this verifies refusal and unchanged bytes, not a new remote fork feature.'],
  };
} catch (error) {
  dump('failure.json', { passed: false, step, checks, error: String(error), stack: error?.stack, refusedNonessentialRequests: refusedNetwork });
  failure = error;
} finally {
  cleanup = await cleanupAll();
  if (outcome) {
    const finalHashes = sourcePaths.map(sha);
    const preserved = finalHashes.every((hash, index) => hash === initialHashes[index]);
    checks.allSourceBytesUnchangedAfterShutdown = preserved;
    outcome.sources = sourcePaths.map((path, index) => ({ path, beforeSha256: initialHashes[index], afterSha256: finalHashes[index] }));
    outcome.cleanup = cleanup;
    outcome.passed = cleanup.verified && preserved;
    dump('report.json', outcome);
  }
}
if (failure) throw failure;
assert.ok(cleanup.verified, 'cleanup must independently verify every owned bridge group exited');
assert.ok(outcome.passed, 'source bytes must also remain unchanged after final engine shutdown');
console.log(`PASS remote disk-backed gateway: ${Object.keys(checks).length} checks`);

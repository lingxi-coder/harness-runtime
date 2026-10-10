import {createHash} from 'node:crypto';
import {spawn} from 'node:child_process';
import {StringDecoder} from 'node:string_decoder';
import {mkdir, readdir, readFile, writeFile} from 'node:fs/promises';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {dummyApiKey} from './loopback-provider.mjs';

export const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
export function sessionIdFor(id) {
  const hex = sha256(id);
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-4${hex.slice(13, 16)}-8${hex.slice(17, 20)}-${hex.slice(20, 32)}`;
}
const jsonLine = row => Buffer.from(JSON.stringify(row) + '\n');
const replace = (text, values) => text.replace(/\{\{(state|workspace|provider)\}\}/g, (_, key) => values[key]);
export function environmentFor(engine, locations, provider) {
  const env = {
    PATH: '/usr/bin:/bin:/usr/sbin:/sbin', LANG: 'C.UTF-8', TMPDIR: locations.tmp, HOME: path.join(locations.state, 'isolated-home'),
    CLAUDE_CONFIG_DIR: locations.state, ANTHROPIC_API_KEY: dummyApiKey, ANTHROPIC_BASE_URL: provider.baseUrl,
    DISABLE_TELEMETRY: '1', DO_NOT_TRACK: '1', CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: '1', DISABLE_AUTOUPDATER: '1',
  };
  for (const [name, value] of Object.entries(engine.env ?? {})) {
    if (/KEY|TOKEN|SECRET|PASSWORD|AUTH/i.test(name) && value !== dummyApiKey) throw new Error(`Only the dummy credential is allowed: ${name}`);
    if (['HOME', 'CLAUDE_CONFIG_DIR', 'ANTHROPIC_BASE_URL', 'TMPDIR'].includes(name)) throw new Error(`Protected isolated environment variable ${name}`);
    if (typeof value !== 'string') throw new Error(`Environment ${name} must be a string`);
    const resolved = replace(value, {state: locations.state, workspace: locations.workspace, provider: provider.baseUrl});
    if (/PROXY/i.test(name) || /BASE_URL/i.test(name) && resolved !== provider.baseUrl) throw new Error(`Only the loopback provider is allowed: ${name}`);
    env[name] = resolved;
  }
  return env;
}

export async function snapshotSessions(stateDir, destination, sessionId) {
  const selectedIds = Array.isArray(sessionId) ? sessionId : [sessionId];
  if (selectedIds.some(value => typeof value !== 'string' || !/^[a-f0-9]{8}-[a-f0-9]{4}-[1-8][a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/i.test(value))) throw new Error('Session snapshots require explicit UUID identities');
  const files = [];
  async function walk(dir, relative = '') {
    let entries;
    try { entries = await readdir(dir, {withFileTypes: true}); } catch (error) { if (error.code === 'ENOENT') return; throw error; }
    for (const entry of entries.sort((a, b) => a.name.localeCompare(b.name))) {
      const child = path.join(dir, entry.name);
      const childRelative = path.join(relative, entry.name);
      if (entry.isDirectory()) await walk(child, childRelative);
      else if (entry.isFile() && entry.name.endsWith('.jsonl') && selectedIds.some(id => entry.name === `${id}.jsonl` || childRelative.split(path.sep).includes(id))) {
        const bytes = await readFile(child);
        const target = path.join(destination, childRelative);
        await mkdir(path.dirname(target), {recursive: true});
        await writeFile(target, bytes);
        files.push({path: childRelative, bytes: bytes.length, sha256: sha256(bytes)});
      }
    }
  }
  // Native and host stores may use different layout. Walk only this run's
  // isolated state tree and accept only this fixture's explicit session id.
  await walk(stateDir);
  return files;
}

export async function captureProcess({engine, fixture, captureDir, locations, provider, providerState, resume = false, timeoutMs}) {
  await mkdir(captureDir, {recursive: true});
  const sessionId = sessionIdFor(fixture.id);
  const common = ['--print', '--output-format', fixture.format, '--model', 'claude-sonnet-5-5', '--system-prompt', 'You are a local deterministic headless fixture.', '--tools', fixture.tools, '--permission-mode', fixture.permission ?? 'dontAsk', '--permission-prompts', fixture.prompts ?? 'none'];
  if (fixture.format === 'stream-json' || fixture.verbose) common.push('--verbose');
  common.push(resume ? '--resume' : '--session-id', sessionId);
  if (resume && fixture.resumeFork) common.push('--fork-session');
  if (fixture.input) common.push('--input-format', fixture.input);
  common.push(...fixture.flags ?? []);
  if (fixture.startupHooks) common.push('--settings', JSON.stringify({hooks: {SessionStart: [{matcher: 'startup', hooks: [{type: 'command', command: 'printf HEADLESS_STARTUP_HOOK'}]}]}}));
  if (fixture.endHooks) {
    const quote = text => "'" + text.replaceAll("'", "'\\''") + "'";
    const markerScript = fileURLToPath(new URL('./lifecycle-marker.mjs', import.meta.url));
    common.push('--settings', JSON.stringify({hooks: {SessionEnd: [{hooks: [{type: 'command', command: `${quote(process.execPath)} ${quote(markerScript)}`}]}]}}));
  }
  if (fixture.mcp) common.push('--strict-mcp-config', '--mcp-config', JSON.stringify({mcpServers: {fixture: {command: process.execPath, args: [fileURLToPath(new URL('./mcp-stdio-host.mjs', import.meta.url))]}}}));
  if (fixture.localPlugin) {
    const plugin = path.join(locations.state, 'local-plugin');
    await mkdir(path.join(plugin, '.claude-plugin'), {recursive: true});
    await writeFile(path.join(plugin, '.claude-plugin/plugin.json'), JSON.stringify({name: 'headless-local-plugin', version: '1.0.0', description: 'Owned local native catalog fixture', author: {name: 'Local fixture'}}) + '\n');
    common.push('--plugin-dir', plugin);
  }
  if (fixture.input !== 'stream-json' && !fixture.textStdin) common.push(fixture.turns[0]);
  const argv = [...engine.args ?? [], ...(engine.flags ?? []).filter(flag => fixture.bare !== false || flag !== '--bare'), ...common];
  const env = environmentFor({...engine, env: {...engine.env ?? {}, ...fixture.env ?? {}}}, locations, provider);
  await mkdir(env.HOME, {recursive: true});
  await writeFile(path.join(captureDir, 'invocation.json'), JSON.stringify({command: engine.command, argv, cwd: locations.workspace, environment: {...env, ANTHROPIC_API_KEY: '<fixed-dummy>'}, inheritedEnvironment: false}, null, 2) + '\n');
  const child = spawn(engine.command, argv, {cwd: locations.workspace, env, stdio: ['pipe', 'pipe', 'pipe'], detached: true});
  const stdout = [], stderr = [], stdin = [], events = [], results = [], controlResponses = [];
  const sessionIds = new Set([sessionId]); let primarySessionId = sessionId;
  const decoder = new StringDecoder('utf8');
  let lines = '', inputClosed = false, initialized = !fixture.initialize, sentTurns = 0, interruptSent = false, timedOut = false, forcedKill, closeError, startupTimer, startupSettled = false;
  const epoch = process.hrtime.bigint();
  const event = (kind, details = {}) => events.push({kind, wallMs: Date.now(), elapsedNs: String(process.hrtime.bigint() - epoch), ...details});
  const send = bytes => {
    if (inputClosed || child.stdin.destroyed) { event('stdin-write-rejected'); return; }
    stdin.push(bytes); event('stdin-write', {bytes: bytes.length}); child.stdin.write(bytes);
  };
  const endInput = () => { if (!inputClosed) { inputClosed = true; event('stdin-eof'); child.stdin.end(); } };
  const turnBytes = (turns, turnIndex) => jsonLine({type: 'user', session_id: sessionId, parent_tool_use_id: null, ...fixture.userUuids?.[turnIndex] ? {uuid: fixture.userUuids[turnIndex]} : {}, message: {role: 'user', content: turns[turnIndex]}});
  const submit = () => {
    if (!startupSettled && sentTurns === 0 && fixture.startupSettleMs > 0) {
      startupSettled = true; event('startup-settle', {milliseconds: fixture.startupSettleMs});
      startupTimer = setTimeout(submit, fixture.startupSettleMs); return;
    }
    const turns = resume ? ['Resume the saved local fixture.'] : fixture.turns;
    if (sentTurns >= turns.length) { endInput(); return; }
    if ((fixture.inputSchedule === 'burst' && sentTurns === 0) || (fixture.inputSchedule === 'after-tool-burst' && sentTurns === 1)) {
      const first = sentTurns;
      send(Buffer.concat(turns.slice(first).map((_, index) => turnBytes(turns, first + index))));
      sentTurns = turns.length; event('burst-submitted', {turns: turns.length - first});
    } else send(turnBytes(turns, sentTurns++));
  };
  let controlIndex = 0;
  const advanceControls = () => {
    const control = fixture.controls?.[controlIndex++];
    if (control) send(jsonLine({type: 'control_request', request_id: `fixture-${control.id}`, request: control.request}));
    else submit();
  };
  let afterControlIndex = 0;
  const advanceAfterControls = () => {
    const control = fixture.afterResultControls?.[afterControlIndex++];
    if (control) send(jsonLine({type: 'control_request', request_id: `fixture-${control.id}`, request: control.request}));
    else endInput();
  };
  const signal = value => { if (typeof child.pid !== 'number') return; try { process.kill(-child.pid, value); } catch { child.kill(value); } };
  providerState.onMessageRequest = () => {
    if (fixture.interrupt && !interruptSent) {
      interruptSent = true;
      send(jsonLine({type: 'control_request', request_id: 'fixture-interrupt', request: {subtype: 'interrupt'}}));
    }
  };
  child.stdin.on('error', error => { event('stdin-error', {code: error.code}); });
  child.stdout.on('data', chunk => {
    stdout.push(chunk); event('stdout', {bytes: chunk.length});
    lines += decoder.write(chunk);
    while (lines.includes('\n')) {
      const end = lines.indexOf('\n'); const line = lines.slice(0, end); lines = lines.slice(end + 1);
      let row; try { row = JSON.parse(line); } catch { continue; }
      if (row.type === 'system' && row.subtype === 'init' && typeof row.session_id === 'string' && /^[a-f0-9]{8}-[a-f0-9]{4}-[1-8][a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/i.test(row.session_id)) {
        primarySessionId = row.session_id; sessionIds.add(row.session_id);
      }
      if (row.type === 'control_response' && row.response?.request_id === 'fixture-initialize') {
        if (row.response.subtype === 'success') { initialized = true; event('initialize-response'); advanceControls(); }
        else { event('initialize-rejected'); endInput(); }
      }
      if (row.type === 'control_response' && fixture.controls?.some(control => row.response?.request_id === `fixture-${control.id}`)) {
        controlResponses.push(row.response); event('fixture-control-response', {requestId: row.response.request_id, subtype: row.response.subtype}); advanceControls();
      }
      if (row.type === 'control_response' && fixture.afterResultControls?.some(control => row.response?.request_id === `fixture-${control.id}`)) {
        controlResponses.push(row.response); event('fixture-control-response', {requestId: row.response.request_id, subtype: row.response.subtype}); advanceAfterControls();
      }
      if (row.type === 'control_request' && row.request?.subtype === 'can_use_tool') {
        event('permission-request', {tool: row.request.tool_name});
        const response = fixture.approval === 'allow' ? {behavior: 'allow', updatedInput: row.request.input} : {behavior: 'deny', message: 'The local fixture denied this tool.'};
        send(jsonLine({type: 'control_response', response: {subtype: 'success', request_id: row.request_id, response}}));
      }
      if (['after-tool-frame', 'after-tool-burst'].includes(fixture.inputSchedule) && sentTurns === 1 && row.type === 'assistant' && row.message?.content?.some(block => block.type === 'tool_use')) {
        event('midtool-submitted'); submit();
      }
      if (row.type === 'result') {
        results.push(row); event('result', {subtype: row.subtype});
        // Observe the native protocol result before EOF/process drain. No
        // synthetic flush, completion, or cleanup event is put on the wire.
        if (fixture.input === 'stream-json') {
          if (fixture.afterResultControls && sentTurns >= fixture.turns.length) advanceAfterControls();
          else submit();
        }
      }
    }
  });
  child.stderr.on('data', chunk => { stderr.push(chunk); event('stderr', {bytes: chunk.length}); });
  if (fixture.malformed) { send(Buffer.from('{malformed-json}\n')); endInput(); }
  else if (fixture.input === 'stream-json') {
    if (!initialized) send(jsonLine({type: 'control_request', request_id: 'fixture-initialize', request: {subtype: 'initialize', hooks: {}, sdkMcpServers: [], ...fixture.initializeRequest ?? {}}}));
    else submit();
  } else {
    const sendText = () => { if (fixture.stdinText !== undefined) send(Buffer.from(fixture.stdinText)); else if (fixture.textStdin) send(Buffer.from(fixture.turns[0] + '\n')); endInput(); };
    if (fixture.textStdin && fixture.startupSettleMs > 0) { event('startup-settle', {milliseconds: fixture.startupSettleMs}); startupTimer = setTimeout(sendText, fixture.startupSettleMs); }
    else sendText();
  }
  const timer = setTimeout(() => { timedOut = true; event('timeout'); signal('SIGTERM'); forcedKill = setTimeout(() => signal('SIGKILL'), 1500); }, timeoutMs);
  let exit;
  try {
    exit = await new Promise(resolve => {
      child.once('error', error => { closeError = String(error); resolve({code: null, signal: null, spawnError: closeError}); });
      child.once('close', (code, signalCode) => { event('close', {code, signal: signalCode}); resolve({code, signal: signalCode}); });
    });
  } finally {
    clearTimeout(timer); clearTimeout(forcedKill); clearTimeout(startupTimer);
    // This process group was created by this runner. Capture result and exit
    // first, then clear any fixture child left after its parent exited.
    if (typeof child.pid === 'number') { try { process.kill(-child.pid, 'SIGKILL'); event('own-process-group-cleanup'); } catch (error) { if (error.code !== 'ESRCH') event('cleanup-error', {code: error.code}); } }
    providerState.onMessageRequest = undefined;
  }
  const buffers = {stdin: Buffer.concat(stdin), stdout: Buffer.concat(stdout), stderr: Buffer.concat(stderr)};
  for (const [channel, bytes] of Object.entries(buffers)) await writeFile(path.join(captureDir, `${channel}.bin`), bytes);
  await writeFile(path.join(captureDir, 'exit.json'), JSON.stringify(exit) + '\n');
  await writeFile(path.join(captureDir, 'events.jsonl'), events.map(row => JSON.stringify(row) + '\n').join(''));
  const sessions = await snapshotSessions(locations.state, path.join(captureDir, 'sessions'), [...sessionIds]);
  if (fixture.format === 'json') {
    results.length = 0;
    try { const value = JSON.parse(buffers.stdout); results.push(...(Array.isArray(value) ? value.filter(row => row.type === 'result') : value.type === 'result' ? [value] : [])); } catch { /* Report invalid/absent JSON as a missing result. */ }
  }
  const resultCount = fixture.format === 'text' ? null : results.length;
  let lifecycleMarkers;
  if (fixture.endHooks) {
    try { lifecycleMarkers = JSON.parse(await readFile(path.join(locations.workspace, 'headless-session-end.json'))); }
    catch { lifecycleMarkers = null; }
    await writeFile(path.join(captureDir, 'lifecycle-markers.json'), JSON.stringify(lifecycleMarkers) + '\n');
  }
  const observation = {argv, sessionId, primarySessionId, sessionIds: [...sessionIds], pid: child.pid, exit, timedOut, resultCount, resultSubtypes: results.map(row => row.subtype), permissionRequests: events.filter(row => row.kind === 'permission-request').length, controlResponses, initialized, interruptSent, sessions, ...fixture.endHooks ? {lifecycleMarkers, firstStdoutWallMs: events.find(row => row.kind === 'stdout')?.wallMs, closeWallMs: events.find(row => row.kind === 'close')?.wallMs} : {}, channels: Object.fromEntries(Object.entries(buffers).map(([name, bytes]) => [name, {bytes: bytes.length, sha256: sha256(bytes)}]))};
  await writeFile(path.join(captureDir, 'observation.json'), JSON.stringify(observation, null, 2) + '\n');
  return observation;
}

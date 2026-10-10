#!/usr/bin/env node
// Explicit bounded transport research. During observation stdout stays open and
// unread. It is drained only after the exit/timeout verdict to preserve bytes.
import {spawn} from 'node:child_process';
import {mkdir, mkdtemp, readFile, writeFile, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {startProvider} from './loopback-provider.mjs';
import {environmentFor, sha256, sessionIdFor, snapshotSessions} from './capture.mjs';

const [binary, output] = process.argv.slice(2);
if (!binary || !path.isAbsolute(binary) || !output || !path.isAbsolute(output)) throw new Error('Use absolute native binary and NEW output directory');
const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../..');
const pin = JSON.parse(await readFile(path.join(repo, 'docs/parity/headless-native-baseline.json')));
if (pin.binary.sha256 !== sha256(await readFile(binary))) throw new Error('Pinned native checksum required');
await mkdir(output);
const provider = await startProvider();
const results = [];
try {
  for (const format of ['text', 'stream-json']) {
    const root = await mkdtemp(path.join(tmpdir(), 'headless-blocked-owned-'));
    const locations = {workspace: path.join(root, 'workspace'), state: path.join(root, 'state'), tmp: path.join(root, 'tmp')};
    for (const dir of Object.values(locations)) await mkdir(dir);
    const captureDir = path.join(output, format); await mkdir(captureDir);
    const sessionId = sessionIdFor(`blocked-${format}`);
    const fixture = {responses: [{text: 'HEADLESS_BLOCKED_' + 'x'.repeat(2 * 1024 * 1024)}]};
    const state = provider.begin(fixture, captureDir);
    const argv = ['--bare', '--print', '--output-format', format, '--model', 'claude-sonnet-5-5', '--system-prompt', 'Local controlled transport probe.', '--tools', '', '--permission-prompts', 'none', '--permission-mode', 'dontAsk', '--session-id', sessionId];
    if (format === 'stream-json') argv.push('--verbose', '--include-partial-messages');
    argv.push('Return the local controlled marker.');
    const env = environmentFor({}, locations, provider);
    await writeFile(path.join(captureDir, 'invocation.json'), JSON.stringify({binary, argv, cwd: locations.workspace, env: {...env, ANTHROPIC_API_KEY: '<fixed-dummy>'}, inheritedEnvironment: false}, null, 2) + '\n');
    const child = spawn(binary, argv, {cwd: locations.workspace, env, detached: true, stdio: ['pipe', 'pipe', 'pipe']});
    // Attach before pausing so Node's automatic post-exit stdio flush is captured.
    // No data flows during the live observation: the stream stays paused.
    const stderr = [], stdout = [], events = []; let exit, forcedCleanup = false, signalTimer, verdictTimer;
    const event = (kind, details = {}) => events.push({kind, wallMs: Date.now(), ...details});
    child.stdout.on('data', bytes => stdout.push(bytes)); child.stdout.pause();
    child.stdin.end(); event('stdin-eof');
    child.stderr.on('data', bytes => stderr.push(bytes));
    const exited = new Promise(resolve => child.once('exit', (code, signal) => { exit = {code, signal}; event('exit', exit); resolve('exit'); }));
    const deadline = new Promise(resolve => {
      verdictTimer = setTimeout(() => resolve('no-exit-within-overall-deadline'), 8500);
      state.onResponseWrite = record => {
        event('provider-response-write', {ordinal: record.ordinal, attempted: record.responseWriteAttempted});
        signalTimer = setTimeout(() => {
          event('sigint', {stdoutReadableLength: child.stdout.readableLength, stdoutDestroyed: child.stdout.destroyed, stdoutReadableEnded: child.stdout.readableEnded});
          if (!exit) child.kill('SIGINT');
          clearTimeout(verdictTimer); verdictTimer = setTimeout(() => resolve('still-running-4000ms-after-sigint'), 4000);
        }, 1000);
      };
    });
    const observation = await Promise.race([exited, deadline]);
    clearTimeout(signalTimer); clearTimeout(verdictTimer); state.onResponseWrite = undefined;
    event('observation-ended', {observation, stdoutReadableLength: child.stdout.readableLength, stdoutDestroyed: child.stdout.destroyed, stdoutReadableEnded: child.stdout.readableEnded});
    if (!exit) { forcedCleanup = true; event('forced-owned-process-cleanup'); try { process.kill(-child.pid, 'SIGKILL'); } catch { child.kill('SIGKILL'); } await exited; }
    // Reading now cannot affect the already-recorded SIGINT observation.
    child.stdout.resume();
    if (!child.stdout.readableEnded) await new Promise(resolve => child.stdout.once('end', resolve));
    await provider.settle();
    const sessions = await snapshotSessions(locations.state, path.join(captureDir, 'sessions'), sessionId);
    const buffers = {stdout: Buffer.concat(stdout), stderr: Buffer.concat(stderr), stdin: Buffer.alloc(0)};
    for (const [name, bytes] of Object.entries(buffers)) await writeFile(path.join(captureDir, `${name}.bin`), bytes);
    await writeFile(path.join(captureDir, 'exit.json'), JSON.stringify(exit) + '\n');
    await writeFile(path.join(captureDir, 'events.jsonl'), events.map(row => JSON.stringify(row) + '\n').join(''));
    results.push({format, observation, forcedCleanup, exit, intendedTextBytes: fixture.responses[0].text.length, events, channels: Object.fromEntries(Object.entries(buffers).map(([name, bytes]) => [name, {bytes: bytes.length, sha256: sha256(bytes)}])), sessions, providerUnexpected: state.unexpected});
    await rm(root, {recursive: true, force: true});
    console.error(`${format}: ${observation} exit=${JSON.stringify(exit)} forcedCleanup=${forcedCleanup}`);
  }
} finally { await provider.close(); }
await writeFile(path.join(output, 'report.json'), JSON.stringify({pin, researchOnly: true, harnessInvoked: false, parityEstablished: false, stdoutPolicy: 'Readable kept paused with a data collector installed: no live data flow; Node may automatically drain only after native exit, then remaining bytes explicitly drained after verdict. Descriptor remained open throughout SIGINT observation.', results}, null, 2) + '\n');

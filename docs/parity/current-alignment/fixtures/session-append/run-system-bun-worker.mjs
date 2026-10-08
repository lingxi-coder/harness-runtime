#!/usr/bin/env node
// Replay the pinned Native system append event through the production Bun
// Mods worker protocol. AgentTranscriptWriter remains covered by the Cargo
// integration test, not this worker-only gate.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync, spawn } from 'node:child_process';
import { mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { basename, dirname, join, relative, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import { decodeModsIpcFrame, encodeModsIpcFrame } from '../../../../../crates/hooks/src/mods_ipc_codec.mjs';

const fixtureDir = dirname(fileURLToPath(import.meta.url));
const repositoryRoot = resolve(fixtureDir, '../../../../../');
const workerPath = resolve(repositoryRoot, 'crates/hooks/src/mods_worker.mjs');
const modulePath = resolve(fixtureDir, 'native-plugin/hooks/register.js');
const nativeReceiptPath = resolve(fixtureDir, 'native-plugin/native-baseline/native-receipt.json');
const nativeEventPath = resolve(
  fixtureDir,
  'native-plugin/native-baseline/system-content-lone-surrogate/result-event.raw.json',
);
const nativeBinary = process.env.CLAUDE_NATIVE_BINARY;
const bunExecutable = process.env.LINGXI_MOD_BUN_EXECUTABLE;
if (!nativeBinary) throw new Error('Set CLAUDE_NATIVE_BINARY to the verified Native 2.1.293 binary');
if (!bunExecutable) throw new Error('Set LINGXI_MOD_BUN_EXECUTABLE to the tested production Bun executable');
if (!basename(bunExecutable).toLowerCase().startsWith('bun')) {
  throw new Error(`worker must run under Bun, got ${basename(bunExecutable)}`);
}

const nativeReceipt = JSON.parse(await readFile(nativeReceiptPath, 'utf8'));
const nativeRaw = await readFile(nativeEventPath, 'utf8');
const nativeCase = JSON.parse(nativeRaw);
const nativeInput = nativeCase.input;
const nativeResult = nativeCase.result;
const nativeInputText = nativeInput.message.content[0].text;
const expectedResultText = nativeResult.message.content[0].text;
assert.equal(nativeInput.message.type, 'system');
assert.equal(nativeInput.uuid, '00000000-0000-4000-8000-000000000015');
assert.equal(nativeInputText.charCodeAt(nativeInputText.indexOf('\ud800')), 0xd800);
assert.equal(expectedResultText.charCodeAt(expectedResultText.indexOf('\ud800')), 0xd800);

const workerRuntimeName = basename(bunExecutable).toLowerCase();
const workerEnv = Object.fromEntries(['PATH', 'TMPDIR', 'LANG'].flatMap(key =>
  process.env[key] === undefined ? [] : [[key, process.env[key]]],
));
const workerVersion = execFileSync(bunExecutable, ['--version'], {
  encoding: 'utf8',
  env: workerEnv,
}).trim();
const workerBytes = await readFile(workerPath);
const moduleBytes = await readFile(modulePath);
const outputParent = resolve(fixtureDir, 'run-output/system-bun-worker');
await mkdir(outputParent, { recursive: true });
const outputDir = await mkdtemp(join(outputParent, 'run-'));

const worker = spawn(bunExecutable, [workerPath], {
  cwd: fixtureDir,
  env: workerEnv,
  stdio: ['pipe', 'pipe', 'pipe'],
});
const stdoutChunks = [];
const stderrChunks = [];
const frames = [];
const pending = new Map();
let nextId = 1;
worker.stdout.on('data', chunk => stdoutChunks.push(Buffer.from(chunk)));
worker.stderr.on('data', chunk => stderrChunks.push(Buffer.from(chunk)));

function send(message) {
  worker.stdin.write(`${encodeModsIpcFrame(message)}\n`);
}

const lines = createInterface({ input: worker.stdout, crlfDelay: Infinity });
lines.on('line', line => {
  let frame;
  try {
    frame = decodeModsIpcFrame(line);
  } catch {
    return;
  }
  frames.push(frame);
  if (frame.kind === 'api.callers') {
    send({ id: frame.id, kind: 'api.callers.result', callId: frame.callId });
    return;
  }
  if (frame.kind === 'next') {
    send({
      id: frame.id,
      kind: 'next.result',
      callId: frame.callId,
      result: { message: frame.input.message, uuid: frame.input.uuid },
    });
    return;
  }
  if (frame.id !== undefined && frame.kind !== 'progress') {
    const finish = pending.get(frame.id);
    if (finish) {
      pending.delete(frame.id);
      finish(frame);
    }
  }
});

function request(message) {
  const id = nextId++;
  return new Promise((resolveRequest, reject) => {
    const timeout = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`worker request ${id} timed out`));
    }, 5_000);
    pending.set(id, frame => {
      clearTimeout(timeout);
      resolveRequest(frame);
    });
    send({ id, ...message });
  });
}

function rawFrame(buffer, requestId, kind) {
  let start = 0;
  for (let index = 0; index <= buffer.length; index++) {
    if (index !== buffer.length && buffer[index] !== 0x0a) continue;
    const end = index === buffer.length ? index : index + 1;
    const line = buffer.subarray(start, index);
    if (line.length > 0) {
      const frame = decodeModsIpcFrame(line.toString('utf8'));
      if (frame.id === requestId && frame.kind === kind) return buffer.subarray(start, end);
    }
    start = end;
  }
  return undefined;
}

const moduleSource = moduleBytes.toString('utf8');
let exitCode;
try {
  const prepared = await request({
    kind: 'prepare',
    root: fixtureDir,
    module: modulePath,
    sourceGraph: {
      entry: modulePath,
      compilerVersion: 'current-alignment system append Bun worker fixture',
      files: [{ file: modulePath, source: moduleSource }],
      links: [],
    },
  });
  assert.equal(prepared.kind, 'prepared');
  const loaded = await request({
    kind: 'load',
    root: fixtureDir,
    module: modulePath,
    preparedToken: prepared.token,
    plugin: 'current-alignment-session-append',
    storageId: 'current-alignment-session-append@user',
    tier: 'user',
  });
  assert.equal(loaded.kind, 'loaded');
  const response = await request({ kind: 'dispatch', event: 'session.append', input: nativeInput });
  assert.equal(response.kind, 'result');

  const next = frames.find(frame => frame.kind === 'next' && frame.id === response.id);
  assert.ok(next, 'system append rewrite reaches the actual core continuation');
  const workerText = response.result.message.content[0].text;
  const nextText = next.input.message.content[0].text;
  assert.equal(workerText, expectedResultText);
  assert.equal(nextText, expectedResultText);
  assert.equal(workerText.charCodeAt(workerText.indexOf('\ud800')), 0xd800);
  assert.equal(nextText.charCodeAt(nextText.indexOf('\ud800')), 0xd800);

  const workerStdout = Buffer.concat(stdoutChunks);
  const resultLine = rawFrame(workerStdout, response.id, 'result');
  const nextLine = rawFrame(workerStdout, response.id, 'next');
  assert.ok(resultLine);
  assert.ok(nextLine);
  await writeFile(join(outputDir, 'worker-result.raw.jsonl'), resultLine);
  await writeFile(join(outputDir, 'worker-next.raw.jsonl'), nextLine);
  await writeFile(join(outputDir, 'worker-stdout.raw.jsonl'), workerStdout);
  await writeFile(join(outputDir, 'worker-stderr.raw'), Buffer.concat(stderrChunks));
  exitCode = await new Promise(resolveExit => {
    worker.stdin.end();
    worker.once('exit', resolveExit);
  });
  const nativeBinaryBytes = await readFile(nativeBinary);
  const receipt = {
    native_version: nativeReceipt.native_version,
    native_sha256: nativeReceipt.native_sha256,
    native_event_sha256: createHash('sha256').update(nativeRaw).digest('hex'),
    bun_runtime: workerRuntimeName,
    bun_version: workerVersion,
    bun_sha256: createHash('sha256').update(await readFile(bunExecutable)).digest('hex'),
    production_worker_sha256: createHash('sha256').update(workerBytes).digest('hex'),
    fixture_module_sha256: createHash('sha256').update(moduleBytes).digest('hex'),
    input: {
      uuid: nativeInput.uuid,
      message_type: nativeInput.message.type,
      source_units: nativeInputText.length,
      source_lone_unit_index: nativeInputText.indexOf('\ud800'),
    },
    native_result_text_units: expectedResultText.length,
    native_result_lone_unit_index: expectedResultText.indexOf('\ud800'),
    worker_result_equals_native_by_utf16_string: workerText === expectedResultText,
    core_next_input_equals_native_by_utf16_string: nextText === expectedResultText,
    worker_result_lone_unit: `U+${workerText.charCodeAt(workerText.indexOf('\ud800')).toString(16).toUpperCase()}`,
    raw_frames: {
      result_path: relative(fixtureDir, join(outputDir, 'worker-result.raw.jsonl')),
      result_sha256: createHash('sha256').update(resultLine).digest('hex'),
      next_path: relative(fixtureDir, join(outputDir, 'worker-next.raw.jsonl')),
      next_sha256: createHash('sha256').update(nextLine).digest('hex'),
    },
    worker_exit_code: exitCode,
    scope: 'Direct production Bun worker protocol dispatch only; AgentTranscriptWriter/final JSONL host seam is covered by the queued Cargo integration test, not this receipt.',
  };
  await writeFile(join(outputDir, 'receipt.json'), `${JSON.stringify(receipt, null, 2)}\n`);
  console.log(JSON.stringify(receipt, null, 2));
} catch (error) {
  worker.stdin.destroy();
  worker.kill('SIGTERM');
  throw error;
}
if (exitCode !== 0) throw new Error(`Bun worker exited ${exitCode}`);

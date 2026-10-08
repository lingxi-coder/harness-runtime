#!/usr/bin/env node
// Drive the checked-in production Mods worker over its JSONL protocol. This
// keeps the middleware implementation in register.js shared with Native;
// only the core next() continuation is supplied by this narrow test driver.
import { createHash } from 'node:crypto';
import { execFileSync, spawn } from 'node:child_process';
import { createReadStream } from 'node:fs';
import { createInterface } from 'node:readline';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { basename, dirname, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { isDeepStrictEqual } from 'node:util';
import { decodeModsIpcFrame, encodeModsIpcFrame } from '../../../../../crates/hooks/src/mods_ipc_codec.mjs';

const fixtureDir = dirname(fileURLToPath(import.meta.url));
const repositoryRoot = resolve(fixtureDir, '../../../../../');
const workerPath = resolve(repositoryRoot, 'crates/hooks/src/mods_worker.mjs');
const modulePath = resolve(fixtureDir, 'hooks/register.js');
const nativeReceiptPath = resolve(fixtureDir, 'native-baseline/native-receipt.json');
const outputDir = resolve(process.argv[2] ?? resolve(fixtureDir, 'run-output/harness-worker'));
const cases = ['rewrite', 'direct', 'cancel', 'throw'];
const workerExecutable = process.env.LINGXI_MOD_BUN_EXECUTABLE;
if (!workerExecutable) {
  throw new Error('Set LINGXI_MOD_BUN_EXECUTABLE to the tested production Bun executable');
}
const workerRuntimeName = basename(workerExecutable).toLowerCase();
if (!workerRuntimeName.startsWith('bun')) {
  throw new Error(`worker must run under Bun, got ${workerRuntimeName}`);
}
const workerArgs = [workerPath];
const workerEnv = Object.fromEntries(['PATH', 'TMPDIR', 'LANG'].flatMap(key =>
  process.env[key] === undefined ? [] : [[key, process.env[key]]]));
const workerRuntimeVersion = execFileSync(workerExecutable, ['--version'], {
  encoding: 'utf8', env: workerEnv,
}).trim();

await mkdir(outputDir, { recursive: true });
const moduleSource = await readFile(modulePath, 'utf8');
const worker = spawn(workerExecutable, workerArgs, {
  cwd: fixtureDir,
  env: workerEnv,
  stdio: ['pipe', 'pipe', 'pipe'],
});

const stdoutChunks = [];
const stderrChunks = [];
const controlTrace = [];
const coreReplies = [];
const frames = [];
const rawLines = [];
const pending = new Map();
let nextRequestId = 1;
let nextHostEvent = 0;
worker.stdout.on('data', chunk => stdoutChunks.push(Buffer.from(chunk)));
worker.stderr.on('data', chunk => stderrChunks.push(Buffer.from(chunk)));

function send(message, direction = 'host-to-worker') {
  controlTrace.push({ direction, message });
  worker.stdin.write(`${encodeModsIpcFrame(message)}\n`);
}

const lines = createInterface({ input: worker.stdout, crlfDelay: Infinity });
lines.on('line', line => {
  rawLines.push(line);
  let frame;
  try { frame = decodeModsIpcFrame(line); }
  catch { frames.push({ parseError: line }); return; }
  frames.push(frame);
  if (frame.kind === 'api.callers') {
    send({ id: frame.id, kind: 'api.callers.result', callId: frame.callId });
    return;
  }
  if (frame.kind === 'next') {
    const event = frame.input;
    const reply = {
      id: frame.id,
      kind: 'next.result',
      callId: frame.callId,
      result: { message: event.message, uuid: event.uuid },
    };
    controlTrace.push({ direction: 'worker-to-core', message: frame });
    coreReplies.push(reply);
    send(reply, 'core-to-worker');
    return;
  }
  if (frame.id !== undefined && frame.kind !== 'progress') {
    const resolveResponse = pending.get(frame.id);
    if (resolveResponse) {
      pending.delete(frame.id);
      resolveResponse(frame);
    }
  }
});

function request(message) {
  const id = nextRequestId++;
  return new Promise((resolveResponse, reject) => {
    const timeout = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`worker request ${id} timed out`));
    }, 5_000);
    pending.set(id, frame => {
      clearTimeout(timeout);
      resolveResponse(frame);
    });
    send({ id, ...message });
  });
}

function sha256(buffer) {
  return createHash('sha256').update(buffer).digest('hex');
}

async function sha256File(path) {
  const hash = createHash('sha256');
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest('hex');
}

function rawObjectAfter(text, marker) {
  const markerAt = text.indexOf(marker);
  if (markerAt < 0) throw new Error(`raw JSON marker missing: ${marker}`);
  const start = text.indexOf('{', markerAt + marker.length);
  if (start < 0) throw new Error(`raw JSON object missing after: ${marker}`);
  let depth = 0;
  let inString = false;
  let escaped = false;
  for (let index = start; index < text.length; index++) {
    const char = text[index];
    if (inString) {
      if (escaped) escaped = false;
      else if (char === '\\') escaped = true;
      else if (char === '"') inString = false;
      continue;
    }
    if (char === '"') inString = true;
    else if (char === '{') depth++;
    else if (char === '}' && --depth === 0) return text.slice(start, index + 1);
  }
  throw new Error(`unterminated raw JSON object after: ${marker}`);
}

let exitCode;
try {
  const prepared = await request({
    kind: 'prepare',
    root: fixtureDir,
    module: modulePath,
    sourceGraph: {
      entry: modulePath,
      compilerVersion: 'current-alignment direct worker protocol fixture',
      files: [{ file: modulePath, source: moduleSource }],
      links: [],
    },
  });
  if (prepared.kind !== 'prepared') throw new Error(`prepare failed: ${JSON.stringify(prepared)}`);
  const loaded = await request({
    kind: 'load', root: fixtureDir, module: modulePath, preparedToken: prepared.token,
    plugin: 'current-alignment-middleware',
    storageId: 'current-alignment-middleware@user', tier: 'user',
  });
  if (loaded.kind !== 'loaded') throw new Error(`load failed: ${JSON.stringify(loaded)}`);

  const dispatchRows = [];
  for (const testCase of cases) {
    const input = {
      message: { type: 'user', content: [{ type: 'text', text: 'fixture-input' }] },
      door: 'user', origin: { kind: 'core' }, uuid: testCase,
    };
    const response = await request({ kind: 'dispatch', event: 'session.append', input });
    if (response.kind !== 'result') {
      throw new Error(`${testCase} dispatch failed: ${JSON.stringify(response)}`);
    }
    dispatchRows.push({ requestId: response.id, case: testCase, input, result: response.result });
  }
  const nativeReceipt = JSON.parse(await readFile(nativeReceiptPath, 'utf8'));

  const unloaded = await request({
    kind: 'unload',
    plugin: 'current-alignment-middleware',
    storageId: 'current-alignment-middleware@user',
  });
  if (unloaded.kind !== 'unloaded') throw new Error(`unload failed: ${JSON.stringify(unloaded)}`);
  const inputAfterUnload = {
    message: { type: 'user', content: [{ type: 'text', text: 'fixture-input' }] },
    door: 'user', origin: { kind: 'core' }, uuid: 'rewrite',
  };
  const afterUnload = await request({
    kind: 'dispatch', event: 'session.append', input: inputAfterUnload,
  });
  if (afterUnload.kind !== 'result') {
    throw new Error(`dispatch after unload failed: ${JSON.stringify(afterUnload)}`);
  }

  const preparedAgain = await request({
    kind: 'prepare',
    root: fixtureDir,
    module: modulePath,
    sourceGraph: {
      entry: modulePath,
      compilerVersion: 'current-alignment direct worker protocol fixture',
      files: [{ file: modulePath, source: moduleSource }],
      links: [],
    },
  });
  if (preparedAgain.kind !== 'prepared') {
    throw new Error(`re-prepare failed: ${JSON.stringify(preparedAgain)}`);
  }
  const reloaded = await request({
    kind: 'load', root: fixtureDir, module: modulePath, preparedToken: preparedAgain.token,
    plugin: 'current-alignment-middleware',
    storageId: 'current-alignment-middleware@user', tier: 'user',
  });
  if (reloaded.kind !== 'loaded') throw new Error(`reload failed: ${JSON.stringify(reloaded)}`);
  const afterReload = await request({
    kind: 'dispatch', event: 'session.append', input: inputAfterUnload,
  });
  if (afterReload.kind !== 'result') {
    throw new Error(`dispatch after reload failed: ${JSON.stringify(afterReload)}`);
  }
  const lifecycleRows = {
    unload: { requestId: unloaded.id, result: unloaded.kind },
    after_unload: { requestId: afterUnload.id, result: afterUnload.result },
    after_reload: { requestId: afterReload.id, result: afterReload.result },
    native_public_boundary: {
      test_process_exits_after_tests: nativeReceipt.native_plugin_test_process_exits_after_tests,
      explicit_module_unload_observed: false,
      note: 'Native plugin-test exposes no explicit unload trigger in this fixture; process exit is not an unload callback',
    },
  };

  const nativeRows = nativeReceipt.raw_event_lines_utf8.map(row => JSON.parse(row.json));
  const comparisons = dispatchRows.map(workerRow => {
    const native = nativeRows.find(row => row.case === workerRow.case);
    const nativeCapture = nativeReceipt.raw_event_lines_utf8.find(row =>
      JSON.parse(row.json).case === workerRow.case);
    const nativeResult = native?.result ?? native?.outcome?.returned;
    const nativeResultRaw = rawObjectAfter(
      nativeCapture.json,
      workerRow.case === 'cancel' || workerRow.case === 'throw' ? '"returned":' : '"result":',
    );
    const workerResultLine = rawLines.find(line => {
      const frame = decodeModsIpcFrame(line);
      return frame.id === workerRow.requestId && frame.kind === 'result';
    });
    if (workerResultLine === undefined) throw new Error(`worker result frame missing: ${workerRow.case}`);
    const workerResultRaw = rawObjectAfter(workerResultLine, '"result":');
    const nextInputs = frames
      .filter(frame => frame.kind === 'next' && frame.id === workerRow.requestId)
      .map(frame => frame.input);
    return {
      case: workerRow.case,
      native_result: nativeResult,
      worker_result: workerRow.result,
      worker_next_inputs: nextInputs,
      result_equal_at_worker_dispatch_seam: isDeepStrictEqual(nativeResult, workerRow.result),
      native_result_object_raw_json: nativeResultRaw,
      worker_result_object_raw_json: workerResultRaw,
      result_object_raw_bytes_equal: Buffer.from(nativeResultRaw).equals(Buffer.from(workerResultRaw)),
      note: 'worker dispatch result is intermediate; retained-row final result is covered by the Cargo test gate',
    };
  });

  const dispatchIds = new Set(dispatchRows.map(row => row.requestId));
  const dispatchFrames = rawLines.filter(line => {
    const frame = decodeModsIpcFrame(line);
    return dispatchIds.has(frame.id)
      && ['next', 'progress', 'result', 'error'].includes(frame.kind);
  });
  const lifecycleIds = new Set([unloaded.id, afterUnload.id, reloaded.id, afterReload.id]);
  const lifecycleFrames = rawLines.filter(line => {
    const frame = decodeModsIpcFrame(line);
    return lifecycleIds.has(frame.id)
      && ['unloaded', 'loaded', 'next', 'progress', 'result', 'error'].includes(frame.kind);
  });
  const rawLifecycleFrames = Buffer.from(lifecycleFrames.join('\n') + '\n');
  const rawDispatchFrames = Buffer.from(dispatchFrames.join('\n') + '\n');
  const rawCoreReplies = Buffer.from(coreReplies.map(reply => `${JSON.stringify(reply)}\n`).join(''));
  const rawStdout = Buffer.concat(stdoutChunks);
  const rawStderr = Buffer.concat(stderrChunks);
  const controlJsonl = Buffer.from(controlTrace.map(row => `${JSON.stringify(row)}\n`).join(''));
  const resultJson = Buffer.from(`${JSON.stringify({ comparisons, lifecycle_gate: lifecycleRows }, null, 2)}\n`);
  await Promise.all([
    writeFile(resolve(outputDir, 'harness.stdout'), rawStdout),
    writeFile(resolve(outputDir, 'harness.stderr'), rawStderr),
    writeFile(resolve(outputDir, 'driver-trace.jsonl'), controlJsonl),
    writeFile(resolve(outputDir, 'dispatch-frames.jsonl'), rawDispatchFrames),
    writeFile(resolve(outputDir, 'lifecycle-frames.jsonl'), rawLifecycleFrames),
    writeFile(resolve(outputDir, 'core-replies.jsonl'), rawCoreReplies),
    writeFile(resolve(outputDir, 'worker-comparison.json'), resultJson),
  ]);
  worker.stdin.end();
  const [status] = await new Promise((resolveExit, reject) => {
    worker.once('exit', (code, signal) => signal
      ? reject(new Error(`worker exited on ${signal}`))
      : resolveExit([code]));
    worker.once('error', reject);
  });
  exitCode = status;
  if (exitCode !== 0) throw new Error(`worker exited with ${exitCode}`);
  const afterUnloadText = afterUnload.result?.message?.content?.[0]?.text;
  const afterReloadText = afterReload.result?.message?.content?.[0]?.text;
  if (afterUnloadText !== 'fixture-input' || afterReloadText !== 'native-prefix:fixture-input') {
    throw new Error(`unload/reload lifecycle mismatch: ${JSON.stringify(lifecycleRows)}`);
  }

  const receipt = {
    worker_entry: 'crates/hooks/src/mods_worker.mjs (production source, direct JSONL entry)',
    runner_sha256: sha256(await readFile(fileURLToPath(import.meta.url))),
    worker_sha256: sha256(await readFile(workerPath)),
    fixture_module_sha256: sha256(Buffer.from(moduleSource)),
    native_receipt_sha256: sha256(await readFile(nativeReceiptPath)),
    driver_node_version: process.version,
    platform: process.platform,
    cwd_from_repository_root: relative(repositoryRoot, fixtureDir),
    native_version: nativeReceipt.native_version,
    native_sha256: nativeReceipt.native_sha256,
    worker_runtime: workerRuntimeName,
    worker_runtime_version: workerRuntimeVersion,
    worker_runtime_executable_sha256: await sha256File(workerExecutable),
    worker_runtime_source: process.env.LINGXI_MOD_BUN_EXECUTABLE
      ? 'explicit LINGXI_MOD_BUN_EXECUTABLE' : 'bun from PATH, matching ModHost default',
    output_directory_from_fixture: relative(fixtureDir, outputDir),
    worker_exit_code: exitCode,
    stdout_bytes: rawStdout.length,
    stdout_sha256: sha256(rawStdout),
    stderr_bytes: rawStderr.length,
    stderr_sha256: sha256(rawStderr),
    dispatch_frames_bytes: rawDispatchFrames.length,
    dispatch_frames_sha256: sha256(rawDispatchFrames),
    core_replies_bytes: rawCoreReplies.length,
    core_replies_sha256: sha256(rawCoreReplies),
    lifecycle_frames_bytes: rawLifecycleFrames.length,
    lifecycle_frames_sha256: sha256(rawLifecycleFrames),
    driver_trace_bytes: controlJsonl.length,
    driver_trace_sha256: sha256(controlJsonl),
    comparisons,
    lifecycle_gate: lifecycleRows,
    scope: 'actual worker dispatch and core continuation seam only; full raw stdout includes a machine-local absolute path in prepare frames and is kept under ignored run-output; checked dispatch fragments are verbatim protocol lines; not the Rust ModHost transcript consumer or whole Mods parity',
  };
  await writeFile(resolve(outputDir, 'worker-receipt.json'), `${JSON.stringify(receipt, null, 2)}\n`);
  process.stdout.write(`${JSON.stringify({ outputDir, receipt })}\n`);
} catch (error) {
  worker.kill();
  process.stderr.write(`${error?.stack || error}\n`);
  process.exitCode = 1;
}

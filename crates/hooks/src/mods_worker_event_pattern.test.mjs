import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { decodeModsIpcFrame, encodeModsIpcFrame } from './mods_ipc_codec.mjs';

test('Bun worker accepts Native 2.1.295 built-in and custom on() patterns and rejects invalid grammar', async t => {
  const root = await mkdtemp(join(tmpdir(), 'mods-event-patterns-'));
  const workerPath = fileURLToPath(new URL('./mods_worker.mjs', import.meta.url));
  const worker = spawn(process.execPath, [workerPath], { stdio: ['pipe', 'pipe', 'pipe'] });
  t.after(async () => {
    if (!worker.killed) worker.kill();
    await rm(root, { recursive: true, force: true });
  });

  const pending = new Map();
  const lines = createInterface({ input: worker.stdout, crlfDelay: Infinity });
  lines.on('line', line => {
    const frame = decodeModsIpcFrame(line);
    if (frame.kind === 'api.callers') {
      worker.stdin.write(`${encodeModsIpcFrame({
        id: frame.id, kind: 'api.callers.result', callId: frame.callId,
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

  let nextId = 1;
  const request = (message) => new Promise((resolve, reject) => {
    const id = nextId++;
    const timeout = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`worker request ${id} timed out`));
    }, 3_000);
    pending.set(id, frame => { clearTimeout(timeout); resolve(frame); });
    worker.stdin.write(`${encodeModsIpcFrame({ id, ...message })}\n`);
  });

  async function prepareAndLoad(fileName, source, plugin, linkedFiles = []) {
    const module = join(root, fileName);
    await writeFile(module, source);
    for (const linked of linkedFiles) {
      await writeFile(linked.file, linked.source);
    }
    const prepared = await request({
      kind: 'prepare',
      root,
      module,
      sourceGraph: {
        entry: module,
        compilerVersion: 'event-pattern test sourceGraph (plain JavaScript passthrough)',
        files: [{ file: module, source }, ...linkedFiles.map(({ file, source }) => ({ file, source }))],
        links: linkedFiles.map(({ file, spelled }) => ({ from: module, spelled, file })),
      },
    });
    assert.equal(prepared.kind, 'prepared', JSON.stringify(prepared));
    return request({
      kind: 'load',
      root,
      module,
      plugin,
      storageId: `${plugin}@user`,
      tier: 'user',
      preparedToken: prepared.token,
    });
  }

  const loaded = await prepareAndLoad('valid.mjs', [
    'export function register(on) {',
    "  const handler = () => ({ result: 'accepted' });",
    "  on('prompt.read', { variant: 'plain' }, handler);",
    "  on('\\x70rompt.read', { variant: 'single' }, handler);",
    '  on("\\x70rompt.read", { variant: "double" }, handler);',
    "  on(`tool.\\u0063all`, handler);",
    "  on('custom.event', handler);",
    "  on('!custom.event', handler);",
    "  on('custom.*', handler);",
    "  on('custom.e·vent', handler);",
    '}',
  ].join('\n'), 'event-patterns');
  assert.equal(loaded.kind, 'loaded', JSON.stringify(loaded));
  assert.deepEqual(loaded.hooks, [
    'prompt.read', 'prompt.read', 'prompt.read', 'tool.call',
    'custom.event', '!custom.event', 'custom.*', 'custom.e·vent',
  ]);

  const repeatedMatchedEvent = await prepareAndLoad('repeated-matched.mjs', `
    export function register(on) {
      const handler = () => {};
      on('tool.call', { to: 'Bash' }, handler);
      on('tool.call', { to: 'Bash' }, handler);
    }
  `, 'repeated-matched-event');
  assert.equal(repeatedMatchedEvent.kind, 'loaded', JSON.stringify(repeatedMatchedEvent));
  assert.deepEqual(repeatedMatchedEvent.hooks, ['tool.call', 'tool.call']);

  const repeatedUnmatchedEvent = await prepareAndLoad('repeated-unmatched.mjs', `
    export function register(on) {
      const handler = () => {};
      on('tool.call', handler);
      on('tool.call', handler);
    }
  `, 'repeated-unmatched-event');
  assert.equal(repeatedUnmatchedEvent.kind, 'error', JSON.stringify(repeatedUnmatchedEvent));
  assert.match(repeatedUnmatchedEvent.message, /on\("tool\.call"\) is registered twice without a matcher/);

  const repeatedPattern = await prepareAndLoad('repeated-pattern.mjs', `
    export function register(on) {
      const handler = () => {};
      on('tool.*', { to: 'Bash' }, handler);
      on('tool.*', { to: 'Read' }, handler);
    }
  `, 'repeated-pattern');
  assert.equal(repeatedPattern.kind, 'error', JSON.stringify(repeatedPattern));
  assert.match(repeatedPattern.message, /on\("tool\.\*"\) is registered twice/);

  const escapedImport = await prepareAndLoad('escaped-import.mjs', String.raw`import { "handler" as plainHandler, "hand\u006cer2" as callback } from './hand\u006cer.mjs';
    export function register(on) {
      on('prompt.read', callback);
      on('prompt.submit', plainHandler);
    }`, 'escaped-import', [{
    file: join(root, 'handler.mjs'),
    spelled: './handler.mjs',
    source: String.raw`function handler() { return { result: 'accepted' }; }
      export { handler as "handler", handler as "hand\u006cer2" };`,
  }]);
  assert.equal(escapedImport.kind, 'loaded', JSON.stringify(escapedImport));
  assert.deepEqual(escapedImport.hooks, ['prompt.read', 'prompt.submit']);

  const rejected = await prepareAndLoad('invalid.mjs', `
    export function register(on) {
      on('!*', () => ({ result: 'unreachable' }));
    }
  `, 'invalid-pattern');
  assert.equal(rejected.kind, 'error', JSON.stringify(rejected));
  assert.match(rejected.message, /"!\*" is not a pattern: it selects no event/);
});

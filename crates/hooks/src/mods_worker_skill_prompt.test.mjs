import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { decodeModsIpcFrame, encodeModsIpcFrame } from './mods_ipc_codec.mjs';

async function startWorker(t) {
  const root = await mkdtemp(join(tmpdir(), 'mods-skill-prompt-'));
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
    if (frame.kind === 'next') {
      worker.stdin.write(`${encodeModsIpcFrame({
        id: frame.id, kind: 'next.result', callId: frame.callId,
        result: { text: frame.input.text },
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
  const request = message => new Promise((resolve, reject) => {
    const id = nextId++;
    const timeout = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`worker request ${id} timed out`));
    }, 3_000);
    pending.set(id, frame => { clearTimeout(timeout); resolve(frame); });
    worker.stdin.write(`${encodeModsIpcFrame({ id, ...message })}\n`);
  });

  async function load(source, tier = 'user') {
    const module = join(root, 'skill-prompt.mjs');
    await writeFile(module, source);
    const prepared = await request({
      kind: 'prepare', root, module,
      sourceGraph: {
        entry: module,
        compilerVersion: 'skill.prompt worker test sourceGraph (plain JavaScript passthrough)',
        files: [{ file: module, source }],
        links: [],
      },
    });
    assert.equal(prepared.kind, 'prepared', JSON.stringify(prepared));
    const loaded = await request({
      kind: 'load', root, module, plugin: 'skill-prompt-test',
      storageId: 'skill-prompt-test@user', tier, preparedToken: prepared.token,
    });
    assert.equal(loaded.kind, 'loaded', JSON.stringify(loaded));
  }

  return { load, request };
}

test('skill.prompt uses exact skill matching and preserves UTF-16 source strings', async t => {
  const { load, request } = await startWorker(t);
  await load(String.raw`
    export function register(on) {
      on('skill.prompt', { skill: 'insights' }, (_api, event) => ({ text: 'seen:' + event.text }));
      on('skill.prompt', { skill: 'insights\uD800' }, (_api, event) => ({ text: event.text + '\uD800' }));
    }
  `);

  const matched = await request({
    kind: 'dispatch', event: 'skill.prompt',
    input: { skill: 'insights', text: 'hello' },
  });
  assert.equal(matched.kind, 'result', JSON.stringify(matched));
  assert.deepEqual(matched.result, { text: 'seen:hello' });
  assert.deepEqual(matched.hooked, ['skill-prompt-test']);

  const noHit = await request({
    kind: 'dispatch', event: 'skill.prompt',
    input: { skill: 'Insights', text: 'hello' },
  });
  assert.equal(noHit.kind, 'result', JSON.stringify(noHit));
  assert.deepEqual(noHit.result, { text: 'hello' });
  assert.equal(Object.hasOwn(noHit, 'hooked'), false);

  const skill = 'insights\uD800';
  const text = 'prompt\uD800tail';
  const utf16 = await request({
    kind: 'dispatch', event: 'skill.prompt', input: { skill, text },
  });
  assert.equal(utf16.kind, 'result', JSON.stringify(utf16));
  assert.deepEqual(utf16.result, { text: `${text}\uD800` });
  assert.deepEqual(utf16.hooked, ['skill-prompt-test']);
});

test('skill.prompt materializes inherited and getter text at the worker IPC boundary', async t => {
  const { load, request } = await startWorker(t);
  await load(String.raw`
    export function register(on) {
      on('skill.prompt', { skill: 'insights' }, (_api, event) => {
        if (event.text === 'inherited') {
          return Object.create({ text: 'from inherited text' });
        }
        if (event.text === 'getter') {
          return new (class { get text() { return 'from class getter'; } })();
        }
        if (event.text === 'getter-with-extra') {
          return new (class {
            constructor() { this.sourceMarker = 'preserved'; }
            get text() { return 'from getter with extra'; }
          })();
        }
        return { text: event.text };
      });
    }
  `);

  for (const [inputText, expected] of [
    ['inherited', { text: 'from inherited text' }],
    ['getter', { text: 'from class getter' }],
    ['getter-with-extra', { text: 'from getter with extra', sourceMarker: 'preserved' }],
  ]) {
    const frame = await request({
      kind: 'dispatch', event: 'skill.prompt', input: { skill: 'insights', text: inputText },
    });
    assert.equal(frame.kind, 'result', JSON.stringify(frame));
    assert.deepEqual(frame.result, expected);
  }
});

test('skill.prompt pins skill across next and next.to and rejects malformed result text', async t => {
  const { load, request } = await startWorker(t);
  await load(String.raw`
    export function register(on) {
      on('skill.prompt', { skill: 'insights' }, async (_api, event, next) => {
        if (event.text === 'next') {
          try { await next({ ...event, skill: 'other' }); }
          catch { return { text: 'next-pinned' }; }
        }
        if (event.text === 'next.to') {
          try { await next.to({ ...event, skill: 'other' }, 'core'); }
          catch { return { text: 'next.to-pinned' }; }
        }
        if (event.text === 'missing') return {};
        if (event.text === 'non-string') return { text: 42 };
        return { text: event.text };
      });
    }
  `, 'prepend');

  for (const [text, expected] of [['next', 'next-pinned'], ['next.to', 'next.to-pinned']]) {
    const pinned = await request({
      kind: 'dispatch', event: 'skill.prompt', input: { skill: 'insights', text },
    });
    assert.equal(pinned.kind, 'result', JSON.stringify(pinned));
    assert.deepEqual(pinned.result, { text: expected });
  }

  for (const text of ['missing', 'non-string']) {
    const malformed = await request({
      kind: 'dispatch', event: 'skill.prompt', input: { skill: 'insights', text },
    });
    assert.equal(malformed.kind, 'result', JSON.stringify(malformed));
    assert.deepEqual(malformed.result, { text },
      'a result without string text is skipped and cannot replace the original prompt');
  }
});

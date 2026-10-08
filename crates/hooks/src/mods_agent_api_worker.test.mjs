import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
import { tmpdir } from 'node:os';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

async function loadPrepared(request, id, message) {
  const prepared = await request(id, {
    kind: 'prepare', root: message.root, module: message.module,
    sourceGraph: {
      entry: message.module,
      compilerVersion: 'worker-test sourceGraph (plain JavaScript passthrough)',
      files: [{ file: message.module, source: await readFile(message.module, 'utf8') }],
      links: [],
    },
  });
  assert.equal(prepared.kind, 'prepared', JSON.stringify(prepared));
  return request(id, { ...message, kind: 'load', preparedToken: prepared.token });
}

test('direct agent list and spawn use the Native wrapper and current host API context', async t => {
  const root = await mkdtemp(join(tmpdir(), 'mods-agent-api-'));
  const modulePath = join(root, 'hooks.mjs');
  await writeFile(modulePath, `
    export function register(on) {
      on('agent.offer', async $ => {
        const rows = await $.agent.list({ ignored: true });
        if (JSON.stringify(rows) !== JSON.stringify([{
          id: 'listed-agent', status: 'running', sentinel: { preserved: true },
        }])) {
          throw new Error('agent.list did not return the host value unchanged');
        }

        const success = await $.agent.spawn({
          prompt: 'do it', model: 'requested-model', model_profile: 'openai-custom', name: 'worker', subagentType: 'Explore',
        });
        if (JSON.stringify(success) !== JSON.stringify({
          model: 'resolved-model', agentId: 'loop-id', teammateId: 'worker@team',
        }) || Object.keys(success).join(',') !== 'model,agentId,teammateId') {
          throw new Error('agent.spawn success projection changed');
        }

        const fallback = await $.agent.spawn({
          prompt: 'alpha beta gamma delta epsilon zeta', model: 'fallback-model', effort: 'ignored',
        });
        if (JSON.stringify(fallback) !== JSON.stringify({ model: 'fallback-model' })
            || Object.keys(fallback).join(',') !== 'model') {
          throw new Error('agent.spawn fallback projection changed');
        }

        const emptyStrings = await $.agent.spawn({
          prompt: 'open a panel', description: '', model: null,
          subagentType: null, name: null, cwd: null,
        });
        if (JSON.stringify(emptyStrings) !== JSON.stringify({
          model: '', agentId: 'stable', teammateId: '',
        })) {
          throw new Error('agent.spawn must preserve empty string results');
        }

        const denied = await $.agent.spawn({ prompt: 'deny me' });
        const hostError = await $.agent.spawn({ prompt: 'error me' });
        if (JSON.stringify(denied) !== JSON.stringify({ deny: 'denied' })
            || JSON.stringify(hostError) !== JSON.stringify({ deny: 'host error' })) {
          throw new Error('agent.spawn denial projection changed');
        }

        let promptError;
        try { await $.agent.spawn({ prompt: '  ' }); }
        catch (error) { promptError = error.message; }
        if (promptError !== 'agent-plugin: $.agent.spawn takes { prompt, ... } (a non-empty prompt)') {
          throw new Error('agent.spawn prompt validation changed');
        }
        return { isOffered: true };
      });
    }
  `);
  t.after(() => rm(root, { recursive: true, force: true }));

  const workerPath = fileURLToPath(new URL('./mods_worker.mjs', import.meta.url));
  const worker = spawn(process.execPath, ['--experimental-vm-modules', workerPath], {
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  t.after(() => { if (!worker.killed) worker.kill(); });

  const requests = [];
  const pending = new Map();
  const lines = createInterface({ input: worker.stdout, crlfDelay: Infinity });
  lines.on('line', line => {
    const frame = JSON.parse(line);
    if (frame.kind === 'api.callers') {
      worker.stdin.write(`${JSON.stringify({
        id: frame.id, kind: 'api.callers.result', callId: frame.callId,
      })}\n`);
      return;
    }
    if (frame.kind === 'api') {
      requests.push(frame);
      const spawnIndex = requests.filter(request => request.method === 'agent.spawn').length;
      const result = frame.method === 'agent.list'
        ? [{ id: 'listed-agent', status: 'running', sentinel: { preserved: true } }]
        : [
          { result: { resolvedModel: 'resolved-model', agentId: 'loop-id', teammate_id: 'worker@team' } },
          { result: { resolvedModel: null, agentId: 9, teammate_id: null } },
          { result: { resolvedModel: '', agentId: 'stable', teammate_id: '' } },
          { deny: 'denied' },
          { isError: true, text: 'host error' },
        ][spawnIndex - 1];
      worker.stdin.write(`${JSON.stringify({
        id: frame.id, kind: 'api.result', callId: frame.callId, result,
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
  const request = (id, message) => new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`worker request ${id} timed out`));
    }, 3_000);
    pending.set(id, frame => { clearTimeout(timeout); resolve(frame); });
    worker.stdin.write(`${JSON.stringify({ id, ...message })}\n`);
  });

  const loaded = await loadPrepared(request, 1, {
    plugin: 'agent-plugin', storageId: 'agent-plugin@user', tier: 'user',
    root, module: modulePath,
  });
  assert.equal(loaded.kind, 'loaded');

  const dispatched = await request(2, {
    kind: 'dispatch', event: 'agent.offer', input: {
      agent: 'worker', description: 'offer a task', source: 'test',
    }, apiContextTicket: 'host-opaque-origin-ticket',
  });
  assert.equal(dispatched.kind, 'result', JSON.stringify(dispatched));
  assert.deepEqual(dispatched.result, { isOffered: true });

  assert.deepEqual(requests.map(({ method, input }) => [method, input]), [
    ['agent.list', {}],
    ['agent.spawn', {
      tool: 'Agent', prompt: 'do it', description: 'do it', run_in_background: true,
      model: 'requested-model', model_profile: 'openai-custom', subagent_type: 'Explore', name: 'worker',
    }],
    ['agent.spawn', {
      tool: 'Agent', prompt: 'alpha beta gamma delta epsilon zeta',
      description: 'alpha beta gamma delta epsilon', run_in_background: true,
      model: 'fallback-model',
    }],
    ['agent.spawn', {
      tool: 'Agent', prompt: 'open a panel', description: '', run_in_background: true,
      model: null, subagent_type: null, name: null, cwd: null,
    }],
    ['agent.spawn', {
      tool: 'Agent', prompt: 'deny me', description: 'deny me', run_in_background: true,
    }],
    ['agent.spawn', {
      tool: 'Agent', prompt: 'error me', description: 'error me', run_in_background: true,
    }],
  ]);
  assert(requests.every(request => typeof request.apiContextTicket === 'string'
    && request.apiContextTicket.length > 0));
  assert(requests.every(request => request.id === 2), 'all direct APIs retain the active dispatch ID');
  assert.equal(new Set(requests.map(request => request.apiContextTicket)).size, 1,
    'all direct API calls inherit the dispatch origin ticket');
  assert(requests.every(request => request.plugin === 'agent-plugin'
    && request.storageId === 'agent-plugin@user' && Number.isSafeInteger(request.hookId)));
  assert(requests.every(request => !Object.hasOwn(request, 'hookOrigin')),
    'origin remains host-resolved from the opaque ticket');

  worker.stdin.end();
  const [exitCode] = await once(worker, 'exit');
  assert.equal(exitCode, 0);
});

test('agent.spawn hooks can clear the model and profile before reaching host core', async t => {
  const root = await mkdtemp(join(tmpdir(), 'mods-agent-clear-route-'));
  const modulePath = join(root, 'hooks.mjs');
  await writeFile(modulePath, `
    export function register(on) {
      on('agent.spawn', ($, e, next) => {
        const cleared = { ...e, model: null, model_profile: null };
        return e.name === 'with-tier' ? next.to(cleared, 'core') : next(cleared);
      });
    }
  `);
  t.after(() => rm(root, { recursive: true, force: true }));

  const workerPath = fileURLToPath(new URL('./mods_worker.mjs', import.meta.url));
  const worker = spawn(process.execPath, ['--experimental-vm-modules', workerPath], {
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  t.after(() => { if (!worker.killed) worker.kill(); });

  const coreInputs = [];
  const pending = new Map();
  const lines = createInterface({ input: worker.stdout, crlfDelay: Infinity });
  lines.on('line', line => {
    const frame = JSON.parse(line);
    if (frame.kind === 'api.callers') {
      worker.stdin.write(`${JSON.stringify({
        id: frame.id, kind: 'api.callers.result', callId: frame.callId,
      })}\n`);
      return;
    }
    if (frame.kind === 'next') {
      coreInputs.push(frame.input);
      worker.stdin.write(`${JSON.stringify({
        id: frame.id, kind: 'next.result', callId: frame.callId,
        result: { model: 'inherited-model', agentId: 'child-id' },
      })}\n`);
      return;
    }
    if (frame.kind !== 'progress' && pending.has(frame.id)) {
      pending.get(frame.id)(frame);
      pending.delete(frame.id);
    }
  });
  worker.once('exit', () => {
    for (const resolve of pending.values()) resolve({ kind: 'worker.exit' });
    pending.clear();
  });
  const request = (id, message) => new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`worker request ${id} timed out`));
    }, 3_000);
    pending.set(id, frame => { clearTimeout(timeout); resolve(frame); });
    worker.stdin.write(`${JSON.stringify({ id, ...message })}\n`);
  });

  const loaded = await loadPrepared(request, 1, {
    plugin: 'clear-route-plugin', storageId: 'clear-route-plugin@prepend', tier: 'prepend',
    root, module: modulePath,
  });
  assert.equal(loaded.kind, 'loaded', JSON.stringify(loaded));

  for (const [index, name] of ['plain', 'with-tier'].entries()) {
    const input = {
      tool_use_id: `tool-${index}`, prompt: 'inspect', description: 'inspect',
      subagentType: 'general-purpose', provider: { plugin: 'engine', tier: 'core' },
      parentModel: 'parent-model', permissionMode: 'default', background: true, fork: false,
      name, model: 'explicit-model', model_profile: 'explicit-profile',
    };
    const dispatched = await request(index + 2, { kind: 'dispatch', event: 'agent.spawn', input });
    assert.equal(dispatched.kind, 'result', JSON.stringify(dispatched));
    assert.deepEqual(dispatched.result, { model: 'inherited-model', agentId: 'child-id' });
    assert.equal(coreInputs.length, index + 1, 'the host core runs once per dispatch');
    assert.deepEqual(coreInputs[index], { ...input, model: null, model_profile: null },
      'the worker must forward the cleared route instead of falling back to the original route');
  }

  worker.stdin.end();
  const [exitCode] = await once(worker, 'exit');
  assert.equal(exitCode, 0);
});

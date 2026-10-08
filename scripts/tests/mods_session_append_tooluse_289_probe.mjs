#!/usr/bin/env node
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';
import path from 'node:path';

const require = createRequire(import.meta.url);
const scriptDirectory = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDirectory, '../..');
const fixturePath = path.join(
  repoRoot,
  'docs/parity/mods-session-append-tooluse-2.1.289.json',
);
const expectedBinarySha256 = '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69';
const defaultBinaryPath = '/tmp/harness-claude-code-2.1.289-darwin-arm64';

const windows = [
  {
    label: 'session.append response shape and immutable-message guard',
    start: 182727018,
    end_exclusive: 182728316,
    sha256: '481f4920b5e69666b7f31c884d9869b4b8bf0cc4f219a9c82dd3a6bdff4d13de',
  },
  {
    label: 'assistant content diff, source-block retention, and row merge helpers',
    start: 212339434,
    end_exclusive: 212344890,
    sha256: '44df3543c990b61bc0f06952210285cc98ff497b908235c94d2152888b7b91dc',
  },
  {
    label: 'native content block clone helper',
    start: 188334648,
    end_exclusive: 188334735,
    sha256: 'f55199628115b3aa3444ebef9eed0d76f5509c599107172e4a0eb3f1069f3e08',
  },
  {
    label: 'session.append row qualification and original-row through wrapper',
    start: 195330200,
    end_exclusive: 195331847,
    sha256: '90b6d38d60f49c1688b8bcbbe510db76bc3e5f155f33f1700a88f4d8537a65ed',
  },
  {
    label: 'query stores selected row and dispatches its tool_use blocks',
    start: 195466181,
    end_exclusive: 195469377,
    sha256: '380eb29894b93878ef7fc2d3b75c57ffc73b9d331a838520dff9b949e656570c',
  },
];

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

function sourceWindow(binary, start, end) {
  return binary.subarray(start, end).toString('utf8');
}

function runResponseGuard(binary) {
  const source = sourceWindow(binary, 182727239, 182728302);
  const program = `
    (() => {
    const N = value => value !== null && typeof value === 'object' && !Array.isArray(value);
    const Bn = value => JSON.stringify(value);
    ${source}
    const original = {
      uuid: 'outer-row-uuid',
      message: {
        type: 'assistant', role: 'assistant',
        content: [
          {type: 'text', text: 'raw assistant text'},
          {type: 'tool_use', id: 'toolu_original'},
        ],
      },
    };
    const candidate = {
      uuid: original.uuid,
      message: {
        type: 'assistant', role: 'assistant',
        content: [{type: 'text', text: 'Mod accepted text only'}],
      },
    };
    const accepted = Wi(candidate, original, [
      {deny: undefined, message: candidate.message},
    ]);
    const changedUuid = Wi({...candidate, uuid: 'different-row'}, original, [
      {deny: undefined, message: candidate.message},
    ]);
    return {
      contentArrayGuardPasses: Mt(candidate.message) === undefined,
      immutableMessageFieldsPass: dn(original.message, candidate.message) === undefined,
      matchingUuidAndNextResultPass: accepted === undefined,
      changedUuidRejected: changedUuid === 'a uuid other than the row it answers for',
    };
    })()
  `;
  return vm.runInNewContext(program, {}, {timeout: 5_000});
}

function runProjectionHelpers(binary) {
  let projectionSource = sourceWindow(binary, 212339434, 212342141);
  projectionSource = projectionSource.replace(
    'import{isDeepStrictEqual as Ne}from"util";',
    'const Ne = require("node:util").isDeepStrictEqual;',
  );
  assert.notEqual(projectionSource, sourceWindow(binary, 212339434, 212342141));

  let rowSource = sourceWindow(binary, 212343137, 212344890);
  rowSource = rowSource.replace(
    'import{isDeepStrictEqual as it}from"util";',
    'const it = require("node:util").isDeepStrictEqual;',
  );
  assert.notEqual(rowSource, sourceWindow(binary, 212343137, 212344890));

  const blockParserStub = `
    // The tested candidate blocks are valid plain Text/tool_use records. The
    // pinned Native projector and row helpers below are executed unchanged;
    // only their imported schema/parser boundary is supplied locally.
    K = () => ({
      safeParse(value) {
        return value && typeof value === 'object' && typeof value.type === 'string'
          ? {success: true, data: value}
          : {success: false};
      },
    });
    g = () => ({
      safeParse(value) {
        return value && value.type === 'text'
          && typeof value.text === 'string' && value.text.length > 0
          ? {success: true, data: value}
          : {success: false};
      },
    });
  `;
  const lznSource = sourceWindow(binary, 188334648, 188334735);
  const program = `
    const require = globalThis.require;
    const G = (items, predicate) => items.filter(predicate).length;
    const f = factory => {
      let value;
      return () => (value ??= factory());
    };
    ${lznSource}
    ${projectionSource}
    ${blockParserStub}
    ${rowSource}
    const ry = () => false;
    (async () => {
      const original = {
        type: 'assistant',
        uuid: 'outer-row-uuid',
        timestamp: '2026-10-04T00:00:00.321Z',
        message: {
          id: 'inner-message-id',
          type: 'message',
          role: 'assistant',
          model: 'served-model',
          content: [
            {type: 'text', text: 'raw assistant text'},
            {type: 'tool_use', id: 'toolu_original', name: 'Bash', input: {command: 'echo original'}},
          ],
          usage: {input_tokens: 17, output_tokens: 5},
          stop_reason: 'tool_use',
        },
      };
      const hookMessage = {
        type: 'assistant',
        role: 'assistant',
        content: [{type: 'text', text: 'Mod accepted text only'}],
      };
      let acceptedRow;
      const core = ne({fills: true, keep() {}}, original, row => { acceptedRow = row; });
      const returned = await core.run({message: hookMessage});
      return {
        hookContent: hookMessage.content,
        storedContent: acceptedRow.message.content,
        returnedContent: returned.message.content,
        storedToolUseCount: acceptedRow.message.content.filter(block => block.type === 'tool_use').length,
        originalToolUse: original.message.content.find(block => block.type === 'tool_use'),
        retainedToolUseUnchanged: JSON.stringify(acceptedRow.message.content.find(block => block.type === 'tool_use'))
          === JSON.stringify({type: 'tool_use', id: 'toolu_original', name: 'Bash', input: {command: 'echo original'}}),
        rowObjectMutatedInPlace: acceptedRow === original,
        outerUuidUnchanged: acceptedRow.uuid === 'outer-row-uuid',
        innerMessageIdUnchanged: acceptedRow.message.id === 'inner-message-id',
        timestampUnchanged: acceptedRow.timestamp === '2026-10-04T00:00:00.321Z',
        usageAndStopReasonUnchanged: acceptedRow.message.usage.input_tokens === 17
          && acceptedRow.message.stop_reason === 'tool_use',
      };
    })()
  `;
  return vm.runInNewContext(program, {require}, {timeout: 5_000});
}

async function main() {
  const args = process.argv.slice(2);
  const check = args.includes('--check');
  const binaryPath = args.find(arg => !arg.startsWith('--')) ?? defaultBinaryPath;
  const binary = await readFile(binaryPath);
  assert.equal(sha256(binary), expectedBinarySha256, 'Native binary SHA-256 changed');

  const measuredWindows = windows.map(({label, start, end_exclusive, sha256: expected}) => {
    const bytes = binary.subarray(start, end_exclusive);
    assert.equal(sha256(bytes), expected, `${label} source-window SHA-256 changed`);
    return {label, start, end_exclusive, sha256: expected};
  });
  const probes = {
    response_guard: JSON.parse(JSON.stringify(runResponseGuard(binary))),
    projection: JSON.parse(JSON.stringify(await runProjectionHelpers(binary))),
  };
  assert.deepEqual(probes.response_guard, {
    contentArrayGuardPasses: true,
    immutableMessageFieldsPass: true,
    matchingUuidAndNextResultPass: true,
    changedUuidRejected: true,
  });
  assert.deepEqual(probes.projection.storedContent, [
    {type: 'text', text: 'Mod accepted text only', citations: null},
    {
      type: 'tool_use',
      id: 'toolu_original',
      name: 'Bash',
      input: {command: 'echo original'},
    },
  ]);
  assert.equal(probes.projection.rowObjectMutatedInPlace, true);
  assert.equal(probes.projection.retainedToolUseUnchanged, true);
  assert.equal(probes.projection.outerUuidUnchanged, true);
  assert.equal(probes.projection.innerMessageIdUnchanged, true);
  assert.equal(probes.projection.timestampUnchanged, true);
  assert.equal(probes.projection.usageAndStopReasonUnchanged, true);
  const report = {
    native_binary_sha256: expectedBinarySha256,
    source_windows: measuredWindows,
    probes,
    execution_scope: {
      native_functions_executed: [
        'Mt', 'dn', 'Wi', 'D', 'L', 'b', 'z', 'q', 'C', 'ee', 'te', 'E', 'oe', 'ne', 'lzn',
      ],
      controlled_imports: ['block parser preserving supplied valid Text/tool_use values', 'Text block schema acceptance'],
      source_only_pipeline_gaps: [
        'full Native hook-worker dispatch and durable transcript writer were not launched',
        'the probe demonstrates the shared-row projection helper, not filesystem commit timing',
      ],
    },
  };

  if (check) {
    const fixture = JSON.parse(await readFile(fixturePath, 'utf8'));
    assert.deepEqual(report, fixture, 'source-executed Native probe differs from the pinned fixture');
    process.stdout.write(`PASS: ${path.relative(repoRoot, fixturePath)}\n`);
  } else {
    process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
  }
}

main().catch(error => {
  process.stderr.write(`${error.stack ?? error}\n`);
  process.exitCode = 1;
});

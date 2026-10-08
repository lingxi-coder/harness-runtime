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
  'docs/parity/mods-session-append-projection-2.1.289.json',
);
const expectedBinarySha256 = '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69';
const defaultBinaryPath = '/tmp/harness-claude-code-2.1.289-darwin-arm64';

const windows = [
  {
    label: 'content block identity keys',
    start: 212340271,
    end_exclusive: 212340573,
    sha256: '7fabf58dc79ef544ff29e40509423707055fd959ed6bea6a04aabf416fd0873b',
  },
  {
    label: 'content diff and ordered source picks',
    start: 212341259,
    end_exclusive: 212342141,
    sha256: 'ce98e760d98fe115068e0954fc23886d6c05b9e25113555f3d8a8a263abe4b24',
  },
  {
    label: 'tool_result content and optional error projection helpers',
    start: 212342141,
    end_exclusive: 212343137,
    sha256: 'a23fc7ceb62860cbf7e32c2f9a610159f203c545eddb38909d782d4b8a595333',
  },
  {
    label: 'plain text/image/document raise matching',
    start: 212340727,
    end_exclusive: 212340933,
    sha256: '4b988ddb9e089f24e59dd6e256e7469219aa765933f8394e455ad3736646794d',
  },
  {
    label: 'text pick materializes citations null',
    start: 212339434,
    end_exclusive: 212339575,
    sha256: '782cacfa8155f91b44654c68ac529fa1ba267889c2e4c36a140ab422bf30ee87',
  },
  {
    label: 'incoming block object requires a string type',
    start: 212339575,
    end_exclusive: 212339603,
    sha256: 'be7ffc37bc25f631095f75d5b39e91ba8e1cedf83a8bd277649be5d04d5ab024',
  },
  {
    label: 'new Text pick requires non-empty text',
    start: 212340677,
    end_exclusive: 212340727,
    sha256: 'f3cd2b2369e6d0dfffbe39e7aa56e5c61a688228c4c87da08f8f1ae0d03d2a0b',
  },
  {
    label: 'row projection and accepted assistant content merge',
    start: 212343598,
    end_exclusive: 212344521,
    sha256: '5a719052e59ebddb4218a3608ef959fe36e2b049b47d3323431f35ed20b2cac3',
  },
  {
    label: 'assistant merge mutates source row content in place',
    start: 212344297,
    end_exclusive: 212344521,
    sha256: '1fe19ca11f06dbcd79d4032d8ae27cb6dedb8d61e547280d11813b63d7f268f0',
  },
  {
    label: 'text value clone used by Native source projection',
    start: 188334648,
    end_exclusive: 188334735,
    sha256: 'f55199628115b3aa3444ebef9eed0d76f5509c599107172e4a0eb3f1069f3e08',
  },
  {
    label: 'block schema accepts a type string with unknown-field catchall',
    start: 179690862,
    end_exclusive: 179690942,
    sha256: 'baaa9d4a716af30b1a92ec1f3cfd6ddfdebd4c7e3f678c4ab7bd75cf58e05590',
  },
  {
    label: 'Native unknown catchall is a ZodUnknown schema',
    start: 179689383,
    end_exclusive: 179689411,
    sha256: '7721da81c579816a2c49b68106cafaa41ad0d914689f0fe50763dd5b3ca225ea',
  },
];

const vectors = [
  {
    id: 'text_replacement_citations',
    original: [{type: 'text', text: 'before'}],
    incoming: [{type: 'text', text: 'after'}],
  },
  {
    id: 'media_anchor_and_tool_order',
    original: [
      {type: 'text', text: 'old'},
      {type: 'image', source: {type: 'url', url: 'image:a'}},
      {type: 'tool_use', id: 'tool-a', name: 'Bash', input: {command: 'original-a'}},
      {type: 'document', source: {type: 'url', url: 'doc:d'}},
      {type: 'tool_use', id: 'tool-b', name: 'Read', input: {file: 'original-b'}},
    ],
    incoming: [
      {type: 'image', source: {type: 'url', url: 'image:a'}},
      {type: 'text', text: 'prefix'},
      {type: 'tool_use', id: 'tool-b', name: 'Read', input: {file: 'changed-b'}},
      {type: 'document', source: {type: 'url', url: 'doc:d'}},
      {type: 'tool_use', id: 'tool-a', name: 'Bash', input: {command: 'changed-a'}},
    ],
  },
  {
    id: 'duplicate_tool_use_id_anchor',
    original: [
      {type: 'tool_use', id: 'duplicate', name: 'Bash', input: {command: 'first'}},
      {type: 'text', text: 'middle'},
      {type: 'tool_use', id: 'duplicate', name: 'Read', input: {file: 'second'}},
    ],
    incoming: [
      {type: 'text', text: 'prefix'},
      {type: 'tool_use', id: 'duplicate', name: 'Changed', input: {value: 1}},
      {type: 'text', text: 'suffix'},
    ],
  },
  {
    id: 'unknown_block_payload_change',
    original: [
      {type: 'text', text: 'before'},
      {type: 'custom_block', payload: {version: 1}},
      {type: 'tool_use', id: 'tool', name: 'Bash', input: {command: 'original'}},
    ],
    incoming: [
      {type: 'text', text: 'after'},
      {type: 'custom_block', payload: {version: 2}},
      {type: 'tool_use', id: 'tool', name: 'Changed', input: {command: 'modified'}},
    ],
  },
  {
    id: 'omitted_unkeyed_media',
    original: [
      {type: 'text', text: 'before'},
      {type: 'image', source: {type: 'url', url: 'image:a'}},
      {type: 'document', source: {type: 'url', url: 'doc:d'}},
      {type: 'tool_use', id: 'tool', name: 'Bash', input: {command: 'original'}},
    ],
    incoming: [
      {type: 'text', text: 'after'},
      {type: 'tool_use', id: 'tool', name: 'Bash', input: {command: 'changed'}},
    ],
  },
  {
    id: 'duplicate_exact_images',
    original: [
      {type: 'text', text: 'before'},
      {type: 'image', source: {type: 'url', url: 'same'}},
      {type: 'image', source: {type: 'url', url: 'same'}},
      {type: 'document', source: {type: 'url', url: 'd'}},
    ],
    incoming: [
      {type: 'image', source: {type: 'url', url: 'same'}},
      {type: 'text', text: 'after'},
      {type: 'image', source: {type: 'url', url: 'same'}},
    ],
  },
  {
    id: 'tool_result_missing_is_error',
    row_type: 'user',
    original: [
      {type: 'tool_result', tool_use_id: 'toolu_result', content: 'original result'},
    ],
    incoming: [
      {type: 'tool_result', tool_use_id: 'toolu_result', content: 'rewritten result'},
    ],
  },
];

const rustSourceComparison = [
  {
    vector: 'text_replacement_citations',
    result: 'mismatch',
    detail: 'Native D creates citations:null for every text pick; the current Rust ContentBlock::Text carries only text and serializes no citations field.',
  },
  {
    vector: 'duplicate_tool_use_id_anchor',
    result: 'mismatch',
    detail: 'Native q replays overlays with a.get(key) for every repeated source key; Rust removes an overlay at its first occurrence, so the suffix appears once instead of after both duplicate-ID ToolUse blocks.',
  },
  {
    vector: 'tool_result_missing_is_error',
    result: 'presence_mismatch_when_source_omits_is_error',
    detail: 'Native tool_result schema makes is_error optional and V preserves the original absence when its logical value remains false; the Rust ContentBlock::ToolResult has a required bool and mod_append_content_block always emits is_error:false.',
  },
  {
    vector: 'media_anchor_and_tool_order',
    result: 'same_for_vector_by_source_trace',
    detail: 'Both match exact unkeyed media into the current anchor, keep source ToolUse order/content, and place the document after the last matching ToolUse anchor.',
  },
  {
    vector: 'unknown_block_payload_change',
    result: 'same_for_vector_by_source_trace',
    detail: 'Both key an unknown type by type plus ordinal and retain the source block instead of accepting its changed payload.',
  },
  {
    vector: 'omitted_unkeyed_media',
    result: 'same_for_vector_by_source_trace',
    detail: 'Both omit source Image/Document blocks unless an exactly equal incoming block raises them; keyed ToolUse remains from source.',
  },
  {
    vector: 'duplicate_exact_images',
    result: 'same_for_vector_by_source_trace',
    detail: 'Both consume exact unkeyed source media at most once per source index, retaining both identical image occurrences when both are supplied.',
  },
];

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

function sourceWindow(binary, start, end) {
  return binary.subarray(start, end).toString('utf8');
}

async function runNativeProjection(binary) {
  let identityAndDiff = sourceWindow(binary, 212339434, 212342141);
  identityAndDiff = identityAndDiff.replace(
    'import{isDeepStrictEqual as Ne}from"util";',
    'const Ne = require("node:util").isDeepStrictEqual;',
  );
  assert.notEqual(identityAndDiff, sourceWindow(binary, 212339434, 212342141));

  let rowProjection = sourceWindow(binary, 212343137, 212344890);
  rowProjection = rowProjection.replace(
    'import{isDeepStrictEqual as it}from"util";',
    'const it = require("node:util").isDeepStrictEqual;',
  );
  assert.notEqual(rowProjection, sourceWindow(binary, 212343137, 212344890));

  let toolResultProjection = sourceWindow(binary, 212342141, 212343137);
  toolResultProjection = toolResultProjection.replace(
    'import{isDeepStrictEqual as Xe}from"util";',
    'const Xe = require("node:util").isDeepStrictEqual;',
  );
  assert.notEqual(toolResultProjection, sourceWindow(binary, 212342141, 212343137));

  const lznSource = sourceWindow(binary, 188334648, 188334735);
  const serializedVectors = JSON.stringify(vectors);
  const program = `
    const G = (items, predicate) => items.filter(predicate).length;
    const f = factory => {
      let value;
      return () => (value ??= factory());
    };
    ${lznSource}
    ${identityAndDiff}
    ${toolResultProjection}
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
    Y = () => ({
      safeParse(value) {
        const contentValid = value && (value.content === undefined
          || typeof value.content === 'string'
          || Array.isArray(value.content) && value.content.every(block => block && typeof block.type === 'string'));
        return value && value.type === 'tool_result' && contentValid
          && (value.is_error === undefined || typeof value.is_error === 'boolean')
          ? {success: true, data: value}
          : {success: false};
      },
    });
    ${rowProjection}
    const ry = () => false;
    const vectors = JSON.parse(${JSON.stringify(serializedVectors)});
    (async () => {
      const output = {};
      for (const vector of vectors) {
        const original = {
          type: vector.row_type ?? 'assistant', uuid: 'outer-row', timestamp: '2026-10-04T00:00:00.321Z',
          message: {
            id: 'inner-message', type: 'message', role: vector.row_type === 'user' ? 'user' : 'assistant', model: 'served-model',
            content: vector.original, usage: {input_tokens: 10, output_tokens: 2},
            stop_reason: 'tool_use',
          },
        };
        const hookMessage = {type: vector.row_type ?? 'assistant', role: vector.row_type === 'user' ? 'user' : 'assistant', content: vector.incoming};
        let acceptedRow;
        const core = ne({fills: true, keep() {}}, original, row => { acceptedRow = row; });
        const returned = await core.run({message: hookMessage});
        output[vector.id] = {
          incoming: hookMessage.content,
          projected_content: acceptedRow.message.content,
          returned_content: returned.message.content,
          same_row_object: acceptedRow === original,
          outer_uuid: acceptedRow.uuid,
          inner_message_id: acceptedRow.message.id,
          timestamp: acceptedRow.timestamp,
        };
      }
      return output;
    })()
  `;
  const result = await vm.runInNewContext(program, {require}, {timeout: 5_000});
  return JSON.parse(JSON.stringify(result));
}

async function main() {
  const args = process.argv.slice(2);
  const check = args.includes('--check');
  const binaryPath = args.find(arg => !arg.startsWith('--')) ?? defaultBinaryPath;
  const binary = await readFile(binaryPath);
  assert.equal(sha256(binary), expectedBinarySha256, 'Native binary SHA-256 changed');

  const sourceWindows = windows.map(({label, start, end_exclusive, sha256: expected}) => {
    assert.equal(
      sha256(binary.subarray(start, end_exclusive)),
      expected,
      `${label} source-window SHA-256 changed`,
    );
    return {label, start, end_exclusive, sha256: expected};
  });
  const nativeProjection = await runNativeProjection(binary);

  assert.deepEqual(nativeProjection.text_replacement_citations.projected_content, [
    {type: 'text', text: 'after', citations: null},
  ]);
  assert.deepEqual(nativeProjection.duplicate_tool_use_id_anchor.projected_content, [
    {type: 'text', text: 'prefix', citations: null},
    {type: 'tool_use', id: 'duplicate', name: 'Bash', input: {command: 'first'}},
    {type: 'text', text: 'suffix', citations: null},
    {type: 'tool_use', id: 'duplicate', name: 'Read', input: {file: 'second'}},
    {type: 'text', text: 'suffix', citations: null},
  ], 'Native replays the same suffix overlay after each duplicate ToolUse identity');
  assert.equal(
    nativeProjection.media_anchor_and_tool_order.projected_content
      .filter(block => block.type === 'tool_use')
      .map(block => block.id)
      .join(','),
    'tool-a,tool-b',
  );

  const report = {
    native_binary_sha256: expectedBinarySha256,
    source_windows: sourceWindows,
    native_projection: nativeProjection,
    rust_source_comparison: rustSourceComparison,
    execution_scope: {
      native_functions_executed: ['D', 'b', 'z', 'q', 'C', 'te', 'E', 'oe', 'ne', 'lzn'],
      controlled_import_boundary: 'K/g parser outputs were supplied for valid JSON block values; their exact type/non-empty constraints and the Native Zod object catchall are separately source-pinned. The q/D/te/oe/ne functions under comparison were executed verbatim.',
      rust_comparison: 'Static comparison against conversation/transcript.rs::rewrite_mod_append_blocks and ContentBlock definitions; Rust code was not built or executed.',
      limitations: [
        'No full Native hook-worker or durable transcript writer was launched.',
        'The input vectors are valid JSON block shapes; no provider response generation was exercised.',
      ],
    },
  };

  if (check) {
    const fixture = JSON.parse(await readFile(fixturePath, 'utf8'));
    assert.deepEqual(report, fixture, 'source-executed Native projection differs from the pinned fixture');
    process.stdout.write(`PASS: ${path.relative(repoRoot, fixturePath)}\n`);
  } else {
    process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
  }
}

main().catch(error => {
  process.stderr.write(`${error.stack ?? error}\n`);
  process.exitCode = 1;
});

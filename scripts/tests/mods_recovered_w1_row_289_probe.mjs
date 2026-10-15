#!/usr/bin/env node
// Source-pinned Claude Code 2.1.289 nonstream fallback row and W1 sibling probe.
// It statically pins the Native request/row/query path and executes only the
// exact Native buildSameTurnToolUses method in a VM. It does not start the CLI
// or make a provider request.
//
// Usage:
//   node scripts/tests/mods_recovered_w1_row_289_probe.mjs <2.1.289-binary> --check
//   node scripts/tests/mods_recovered_w1_row_289_probe.mjs <2.1.289-binary> --write

import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';
import vm from 'node:vm';

const [binaryPath, mode] = process.argv.slice(2);
assert(binaryPath, 'provide the verified Claude Code 2.1.289 binary path');
assert(mode === '--check' || mode === '--write', 'mode must be --check or --write');

const binary = readFileSync(binaryPath);
const sha256 = value => createHash('sha256').update(value).digest('hex');
const binarySha256 = '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69';
assert.equal(sha256(binary), binarySha256, 'unexpected Claude Code 2.1.289 binary SHA-256');

const sourceWindows = [];
function take(label, start, end, expectedHash, startText) {
  assert(start >= 0 && end > start, label + ': invalid byte range');
  const bytes = binary.subarray(start, end);
  assert.equal(bytes.length, end - start, label + ': source range is short');
  if (startText) {
    assert.equal(
      bytes.subarray(0, Buffer.byteLength(startText)).toString(),
      startText,
      label + ': source start marker changed',
    );
  }
  assert.equal(sha256(bytes), expectedHash, label + ': source hash changed');
  sourceWindows.push({label, start, end_exclusive: end, sha256: expectedHash});
  return bytes.toString('utf8');
}

const nonstreamAdapter = take(
  'AGo full nonstream provider response',
  189272780, 189275792,
  '8c5767f055b2fe09e9da61a853edfb2d767cb845093b558c65da0de3a39da653',
  'async function*AGo(',
);
const nonstreamFallbackNormalizer = take(
  'l8e nonstream response normalization and assistant-row yield',
  189346377, 189349484,
  'b130facacfb390b3d95f6981cb4692a01bc2a33d034a193cd80659e471f5b06d',
  'async function*l8e(Wn,Go)',
);
const refusalAwareContentFilter = take(
  'dwn response content filtering',
  189341909, 189343263,
  '0a5e6c5014330afce1309b1d3c68e0aee3cec9f513faa18e37981c35e4aa18a3',
  'dwn=(Wn)=>{',
);
const contentBlockNormalizer = take(
  'eFt nonstream content block normalization',
  191356656, 191359199,
  'e4c70457d8c391da8ceb14bee3706b23783b09f6d511a2cc0f2d93dc95ca81e4',
  'function eFt(e,n,r,s,g)',
);
const batchToolContentTransform = take(
  'HUt ordered content and batch-tool transform',
  184985052, 184985387,
  '970be10d39bc63c462c134dbdd64437093d78544f248b0c37145aa7fe084f793',
  'function HUt(e,r,a)',
);
const streamingFallbackCaller = take(
  'stream error enters the shared nonstream fallback generator',
  189390373, 189391775,
  '82a9d3ff4707c3767cd7dab5db759d9d012f5adab086019fbe85477b773d98bf',
  'if(Ml==="fail")',
);
const notFoundFallbackCaller = take(
  '404 stream creation fallback enters the same nonstream generator',
  189391900, 189393148,
  'adea093332e10571ea048149bde06ff63e0c479c760487e882f728b14b838494',
);
const agentForwardedRowPredicate = take(
  'MC row predicate includes assistant and user rows',
  194774097, 194774244,
  '90a7c47a62578e657003eed20fbaf183f32b1dfd8211bf2a8cbba89c9ebedd01',
  'function El(e){return e.type==="assistant"',
);
const streamingPerBlockProducer = take(
  'stream content_block_stop produces one row for one completed block',
  189368685, 189370128,
  'aa8ce11178f40647830a76867eab1458645b378e24fbe5f6092a9f9042e4df50',
  'case"content_block_stop":{let ha=ng[ii.index]',
);
const agentRunQueryPassThrough = take(
  'MC forwards assistant and user query rows without content-block splitting',
  194793235, 194795722,
  '9d13f060e3966a8a65a6d4a0d78a9db9004493e8ec48b3c1c2a69724d3b61f5a',
  'for await(let E of Ke.options.runQuery(',
);
const queryToolUseConsumer = take(
  'query consumer enqueues tool blocks from one assistant row in content order',
  195466181, 195469377,
  '380eb29894b93878ef7fc2d3b75c57ffc73b9d331a838520dff9b949e656570c',
  'if(M.type==="assistant"&&!M.isApiErrorMessage&&!mr([M]))',
);
const executorAddTool = take(
  'executor stores each tool block with the assistant row object',
  195350179, 195351516,
  '92b720b6a34784e97521f5d290052a628d34aa365dc8b674d7e6fde78f810b2a',
  'addTool(e,o){',
);
const sameTurnBuilder = take(
  'buildSameTurnToolUses source method',
  195353804, 195354117,
  '813ea3c0b71b8e92bb9097ba2d7d23cc6a3de6f13c4a221af698db96f8bfd835',
  'buildSameTurnToolUses(e){',
);
const completedQueueEntries = take(
  'completed executor entries stay in the same tool array as yielded entries',
  195355929, 195356509,
  '39bd63336ce18174bef5b3867e0e6b721153fb64194a1ce144cd8418f2aa2bed',
  'getCompletedResults(){',
);
const executorW1Dispatch = take(
  'executor passes current row separately from earlier sibling context',
  195354700, 195355020,
  '49f7d225d78efab574e0afb853cfadacad167ff88b4777d39f8611d5c3df15b9',
  'W1(e.block,e.assistantMessage',
);
const w1CurrentRowAndPermissionContext = take(
  'W1 takes current assistant row and supplies sibling rows to permission helper',
  188831280, 188831680,
  '21cad4d61d3ecdc8f48224da1577da2f005c9eb0ae48f123510c21fd81c1c792',
  'let ao=WDe(Ft,Ze),wn=await AVn(',
);

assert.match(nonstreamAdapter, /V\.beta\.messages\.create\(\{\.\.\.Te,stream:!1\}/);
assert.match(nonstreamAdapter, /return\{message:qe,requestId:/);
assert.match(refusalAwareContentFilter, /for\(let\[rl,ac\]of\(Wn\.content\?\?\[\]\)\.entries\(\)\)/);
assert.match(refusalAwareContentFilter, /Ji\.push\(ac\)/);
assert.match(refusalAwareContentFilter, /return\{content:Ji,lastHop:ss,iterations:Aa\}/);
assert.match(contentBlockNormalizer, /return e\.flatMap\(\(h\)=>\{switch\(h\.type\)/);
assert.match(contentBlockNormalizer, /case"text":[\s\S]*?return h/);
assert.match(batchToolContentTransform, /for\(let u=0;u<e\.length;u\+\+\)/);
assert.match(batchToolContentTransform, /return\{content:i\?\?e,batchToolUses:l\}/);
assert.match(nonstreamFallbackNormalizer, /yield\*AGo\(/);
assert.match(nonstreamFallbackNormalizer, /let Ji=dwn\(lr\)/);
assert.match(nonstreamFallbackNormalizer, /eFt\(Ji\.content/);
assert.match(nonstreamFallbackNormalizer, /HUt\(eFt\(Ji\.content/);
assert.match(nonstreamFallbackNormalizer, /message:\{\.\.\.lr,content:Yl,/);
assert.match(nonstreamFallbackNormalizer, /apiBlockIndex:0/);
assert.match(nonstreamFallbackNormalizer, /uuid:t4\(\)/);
assert.match(nonstreamFallbackNormalizer, /wp\.push\(gh\),eI=gh,kT=!1,yield gh/);
assert.equal([...nonstreamFallbackNormalizer.matchAll(/yield gh/g)].length, 1);
assert.match(streamingFallbackCaller, /yield\*l8e\(/);
assert.match(notFoundFallbackCaller, /yield\*l8e\(/);
assert.match(streamingPerBlockProducer, /eFt\(\[ha\]/);
assert.match(streamingPerBlockProducer, /apiBlockIndex:ii\.index/);
assert.match(streamingPerBlockProducer, /uuid:t4\(\)/);
assert.match(streamingPerBlockProducer, /wp\.push\(wd\).*yield wd/);
assert.match(agentRunQueryPassThrough, /Ke\.options\.runQuery\(/);
assert.match(agentRunQueryPassThrough, /if\(El\(E\)\)\{[\s\S]*?yield E\}\}/);
assert.match(agentForwardedRowPredicate,
  /e\.type==="assistant"\|\|e\.type==="user"/);
assert.match(queryToolUseConsumer, /for\(let xe=0;xe<M\.message\.content\.length;xe\+\+\)/);
assert.match(queryToolUseConsumer, /let xe=X\.message\.content\.filter\(\(ve\)=>ve\.type==="tool_use"\)/);
assert.match(queryToolUseConsumer, /for\(let ve of xe\)ue\.streamingToolExecutor\.addTool\(ve,X\)/);
assert.match(executorAddTool,
  /this\.tools\.push\(\{id:e\.id,block:e,assistantMessage:o,status:"queued"/);
assert.match(completedQueueEntries, /for\(let e of this\.tools\)/);
assert.match(completedQueueEntries, /if\(e\.status==="completed"\)\{e\.status="yielded"/);
assert.doesNotMatch(completedQueueEntries, /this\.tools\.(?:splice|filter)/);
assert.match(executorW1Dispatch,
  /W1\(e\.block,e\.assistantMessage,this\.canUseTool,\{[\s\S]*?sameTurnToolUses:this\.buildSameTurnToolUses\(e\)/);
assert.match(w1CurrentRowAndPermissionContext, /LPt\(h,n,ao,ke\|\|tDe\(Qe,ao\),s\.sameTurnToolUses\),n/);

const nativeMethod = sameTurnBuilder;
assert.ok(nativeMethod.startsWith('buildSameTurnToolUses(e){'),
  'Native sibling-builder method start missing');
assert.match(nativeMethod, /for\(let n of this\.tools\)\{if\(n===e\)break/);
assert.match(nativeMethod, /o\.get\(n\.assistantMessage\)\?\?\[\]/);
assert.match(nativeMethod, /r\.push\(n\.block\)/);
assert.match(nativeMethod, /\(\[n,r\]\)=>\(\{\.\.\.n,message:\{\.\.\.n\.message,content:r\}\}\)/);

const context = vm.createContext({});
const NativeExecutor = vm.runInContext(
  '(class NativeExecutor {' + nativeMethod + '})',
  context,
);
const row = {
  uuid: 'outer-row-1',
  message: {
    id: 'provider-message-1',
    content: [
      {type: 'text', text: 'TextA'},
      {type: 'tool_use', id: 'tool-a', name: 'ToolA', input: {}},
      {type: 'text', text: 'TextB'},
      {type: 'tool_use', id: 'tool-b', name: 'ToolB', input: {}},
    ],
  },
};
const toolA = row.message.content[1];
const toolB = row.message.content[3];
const queuedA = {id: toolA.id, block: toolA, assistantMessage: row};
const queuedB = {id: toolB.id, block: toolB, assistantMessage: row};
const executor = new NativeExecutor();
executor.tools = [queuedA, queuedB];
const firstSiblings = executor.buildSameTurnToolUses(queuedA);
const secondSiblings = executor.buildSameTurnToolUses(queuedB);
const helperProbe = {
  input_content_order: row.message.content.map(block =>
    block.type === 'text' ? block.text : block.id),
  first_tool: {
    current_tool_id: toolA.id,
    same_turn_tool_uses: JSON.parse(JSON.stringify(firstSiblings)),
  },
  second_tool: {
    current_tool_id: toolB.id,
    same_turn_tool_uses: JSON.parse(JSON.stringify(secondSiblings)),
  },
  source_row_after_probe: JSON.parse(JSON.stringify(row)),
};
assert.deepEqual(helperProbe.first_tool.same_turn_tool_uses, []);
assert.equal(helperProbe.second_tool.same_turn_tool_uses.length, 1);
assert.equal(helperProbe.second_tool.same_turn_tool_uses[0].uuid, row.uuid);
assert.equal(helperProbe.second_tool.same_turn_tool_uses[0].message.id, row.message.id);
assert.deepEqual(
  helperProbe.second_tool.same_turn_tool_uses[0].message.content,
  [toolA],
);
assert.deepEqual(
  helperProbe.source_row_after_probe.message.content,
  row.message.content,
  'the helper returns a partial clone without mutating the current row',
);

const observation = {
  native_version: '2.1.289',
  binary: {
    path: '/tmp/harness-claude-code-2.1.289-darwin-arm64',
    size_bytes: binary.length,
    sha256: binarySha256,
  },
  source_windows: sourceWindows,
  findings: {
    provider_adapter: 'AGo requests stream:false and returns the complete provider message object.',
    nonstream_fallback_row: 'l8e normalizes the whole response content array and creates/pushes/yields one assistant row (apiBlockIndex 0, a fresh outer UUID, and the provider inner message id).',
    fallback_entrypoints: ['stream error fallback', '404 stream-creation fallback'],
    content_order_scope: 'For surviving ordinary text/tool blocks without batch decomposition or special server-refusal filtering, dwn/eFt/HUt preserve the response array order; special filters/decomposition can remove or expand blocks.',
    agent_query_wrapper: 'MC forwards a completed assistant row from runQuery as that same row; it does not split message.content into rows.',
    streaming_contrast: 'The streaming content_block_stop producer wraps one completed block in a row and stamps that block row with apiBlockIndex.',
    current_row_and_siblings: 'The current tool block and its assistant row are separate W1 arguments. sameTurnToolUses is built from executor queue entries before the current entry, grouped by assistantMessage object identity, with each sibling row cloned to contain only those earlier tool blocks.',
    example: 'With one row TextA/ToolA/TextB/ToolB, W1 for ToolA gets no prior sibling rows; W1 for ToolB gets a partial clone of the same row containing ToolA only.',
  },
  executable_native_helper_probe: helperProbe,
  evidence_boundary: 'The extracted Native sibling builder is executed in a VM. Provider request, l8e generator, query loop, and W1 are pinned and statically inspected, not executed; no provider or CLI session is started.',
};

const fixturePath = fileURLToPath(
  new URL('../../docs/parity/mods-recovered-w1-row-2.1.289.json', import.meta.url),
);
if (mode === '--write') {
  writeFileSync(fixturePath, JSON.stringify(observation, null, 2) + '\n');
  process.stdout.write('wrote ' + fixturePath + '\n');
} else {
  const expected = JSON.parse(readFileSync(fixturePath, 'utf8'));
  assert.deepEqual(observation, expected, 'captured Native evidence differs from fixture');
  process.stdout.write('Native 2.1.289 recovered-row/W1 probe passed (--check)\n');
}

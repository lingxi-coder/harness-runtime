#!/usr/bin/env node
// Replay pinned native Read helpers; persist inputs, outputs and range receipts only.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import vm from 'node:vm';

const args = process.argv.slice(2);
assert(args.every(arg => !arg.startsWith('--') || arg === '--check'));
const binaryPath = args.find(arg => !arg.startsWith('--'))
  ?? path.join(homedir(), '.local/share/claude/versions/2.1.322');
const binary = readFileSync(binaryPath);
const sha256 = value => createHash('sha256').update(value).digest('hex');
const nativeSha256 = sha256(binary);
assert.equal(nativeSha256, 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450');
const sourceByteRanges = [];
function take(name, start, end, digest, first, next) {
  const bytes = binary.subarray(start, end);
  assert(bytes.length > 0 && bytes.length < 5000);
  assert.equal(sha256(bytes), digest, `${name}: changed bytes`);
  assert(bytes.toString('utf8').startsWith(first));
  assert.equal(binary.subarray(end, end + Buffer.byteLength(next)).toString('utf8'), next);
  sourceByteRanges.push({ name, start, end, sha256: digest });
  return bytes.toString('utf8');
}
const estimate = take('UTF-16 token estimates', 270089174, 270089590,
  'edf93f4bb07cc165d37bda0ca9e0c708df3556f677e17d9560f99ba64661cc05',
  'function R(', 'function Hen(');
const numbering = take('line numbering', 261794014, 261794374,
  '4a70600099a3637da8664777392955d9c6ff0944c54bb746872d023aded608a5',
  'function _4e(', 'function ljr(');
const utf16Copy = take('UTF-16 identity copy', 260462017, 260462323,
  'bfb3e4e6e725909a3e13582d6543d1621c15793b92a29d303029b85ef1c5ed94',
  'function t1e(', 'var b=/');
const ranges = take('buffered text range selection', 270507280, 270508328,
  '27a9a0fad3acc466e59634b90f5c30ea74345830ce14d107141dbe18afd66d57',
  'function qyo(', 'function v(n)');
const prefix = take('partial view prefix', 265923500, 265923542,
  '50e844d87129a30d63809a320998b0568cb1e9320366567354d9e161e3b42356',
  'var yZe=', ',$on=');
const shrink = take('full-read token overflow shrink', 274321023, 274322396,
  'ba628f8588a1d50ebb431985201fb98b63efab9d776fb891a81dc935ffb225ea',
  'let Ro=sLe(', '}else throw DUn(Pn,tt)');
const mapper = take('text result mapper', 274306205, 274306973,
  'ca2c77e3bb1ca8a0c864c126409051d59acfe8b401667142c3901906665dac5f',
  'let r;if(e.file.content){', '}}}});async function h6r');
const reread = take('Read call always dispatches fresh I/O', 274306979, 274309041,
  'feb5c0d2f6596b7b8fc4a713fda43e6d2f7636bdc05335d737cea33106642dd2',
  'async function h6r(', 'var sjn=new WeakMap');
const room = take('large-read room and raised limits', 274311224, 274311772,
  '91974909516b5a573a0793b85d27a0d845333c3d114dad4c8c842b7d4371f1e7',
  'var E6r=', 'function cjn(');
const hint = take('large-read room guidance', 274311944, 274312156,
  '78536dc634dbf5c145511a124656408ae35a6bb4e9ea9251651dd6ca19fa07b1',
  'function sLe(', 'function DUn(');
const modelRatio = take('model bytes per token', 264401421, 264401792,
  '3dd66de3c2006f1b333c44bec9df221d9f3784908e8ea6694e867500fbd67e0f',
  'W$=new Set', 'function ERo(');
const hints = take('large-read guidance constants', 264884607, 264885037,
  '995fc7ff9d0bec66297a3eee866b729bf7436d5b5f9bbda55d0a4e4f13ad042d',
  'var HLs=', 'FLs=128;');
const error = take('token overflow error', 264885045, 264885406,
  'bbcf7ea631876f23b241a527e86237f47cc31a6625b3d03dfb0616c5c19751d0',
  'class qbe ', 'var n=new WeakMap');
const counter = take('Read token validation with offline fallback', 274312734, 274313018,
  'a9e74c56e607fd1bf2af6190031e6db778ec1bcac296fb1d09ef1de5d592e585',
  'async function Hco(', 'function oLe(');
const promptBody = take('Read prompt renderer', 265923950, 265926023,
  '30db86a214f3b2ff5ab022484bd714c51af530814148bf9eeb40c241acb6f9b5',
  'function D3i(', 'function rPs(');
const promptConstants = take('Read prompt constants', 265923500, 265923950,
  '66220555f91956c91c9ddd5dca76b402d8a51bd34783086cb325b64b0fad5696',
  'var yZe=', 'function D3i(');
const verification = take('Read verification guidance', 265922856, 265923017,
  'b07e1f44f0de219eaba6217f5850088ddd2c02767d3c608c01ee2d9f4d99d730',
  'var be=', ',Ewo=');
const rewrite = take('Read rewritten verification guidance', 265896443, 265896701,
  '9ae929df2cb1077b6c752e54247bbfae55b8b5a661845f8094a3fd2ca37da442',
  '_e=', ';function N4i(');
const defaultBytes = take('default Read byte limit', 261789877, 261789892,
  '3e3ec3ff73b63361271820d28cb935d02d7560b8b5c3e771f5644d5b4b62524d',
  'var aGt=', 'function UUe');
const schema = take('Read input schema', 274292273, 274293172,
  '2ab9e1dc6c6c6b5ef2814b55a247a89a0cc5c9e0f467aaa30e8e98fc9ac816e2',
  'a6r=p(', ',ZUn=');

const context = vm.createContext({
  Buffer,
  t1e: value => value,
  Ft: (value, separator) => value.split(separator).length - 1,
  Ge: 'Read', mr: 'Grep', Bae: 'allow_large',
  // Context-room hints are controlled absent here. This replays the native
  // ordinary/offline text path, not live account-specific token counting.
  Y: () => undefined,
  Sct: () => null, B_t: () => false, MUn: () => '', S6r: () => '', T6r: () => '',
  OUn: (_, content) => content,
  Bce: value => value, a0: value => value.input_tokens,
  Wt: value => value, it: value => value, Ht: value => value,
  uuo: async (_, fn) => fn(), d8t: async () => undefined,
  o1: () => ({maxTokens:25000,maxSizeBytes:262144}),
  X_: value => value, _ct: () => undefined,
  kua: (_, value) => ({spellings:[value]}), Ccn: () => undefined,
  o9: () => false, x: () => 'linux', i: {CLAUDE_CODE_SIMPLE: true},
  $b: path, JM: () => false, l: () => {}, S: value => value,
  $Un: async input => ({data:{type:'text',file:{filePath:input.file_path,content:'fresh'}}}),
  J0: ({leanPrompt}) => leanPrompt, u9: ({leanRewrite}) => leanRewrite,
  Fon: () => true,
});
vm.runInContext(estimate + numbering + utf16Copy + ranges + prefix + ';'
  + hints.slice(0, -1) + ';' + error + 'var FLs=128;'
  + 'var ' + modelRatio + room + hint + counter + reread
  + 'function fc(model) {return {bytesPerToken:$_(model)}};'
  + promptBody + promptConstants + verification + ';var ' + rewrite + ';' + defaultBytes
  + `function shrinkRead(input) {
    let {content: an, tokenCount, maxTokens: V, totalLines: wt, filePath: r} = input;
    let Pn = {tokenCount, maxTokens: V}, ke, mn, kn, so, Cn, xn = false;
    let originalHint = sLe; sLe = () => ''; ${shrink}; sLe = originalHint;
    return {content: mn, numLines: kn, note: Cn};
  }
  function mapReadResult(e,n) { ${mapper} }
  `, context);
const tokenCases = [];
for (const content of ['', 'a', 'ab', 'abc', 'abcde', '中文', '😀', '中😀文', 'éééé', '\u2028\u2029']) {
  for (const ext of ['txt', 'json', 'jsonl', 'jsonc']) {
    tokenCases.push({ input: {content, ext}, expected: context.Len(content, ext) });
  }
}
const rangeCases = [];
for (const content of ['', 'a', 'a\n', 'a\r\nb\r\n', 'a\r\r\n', '\r\r\n\r',
  '\n', 'a\rb\r', '\ufeffa\nb\n', '中\n😀\n']) {
  for (const [offset, limit] of [[1, undefined], [1, 1], [2, 1], [3, 2], [99, 3], [1, 0]]) {
    const input = {content, offset, ...(limit !== undefined && {limit})};
    const result = context.bvs(content, Buffer.byteLength(content), 42, offset - 1, limit);
    rangeCases.push({ input, expected: {
      content: result.content, numLines: result.lineCount, totalLines: result.totalLines,
      numberedContent: context._4e({content: result.content, startLine: offset}),
      modelContent: context.mapReadResult({file:{content:result.content,numLines:result.lineCount,
        totalLines:result.totalLines,startLine:offset}}, 'tool-1').content,
    }});
  }
}
const truncationCases = [];
for (const content of ['中'.repeat(80), '😀'.repeat(40), 'abc😀'.repeat(20),
  'first\n' + '中文😀\n'.repeat(20), '   \n' + '中'.repeat(100), '😀\n'.repeat(40),
  '\u0085\n' + '中'.repeat(100), '\ufeff\n' + '中'.repeat(100)]) {
  for (const maxTokens of [3, 7]) {
    const input = {content, tokenCount: context.Len(content, 'txt'), maxTokens,
      totalLines: content.split('\n').length, filePath: '/workspace/文件.txt'};
    truncationCases.push({ input, expected: context.shrinkRead(input) });
  }
}
const roomCases = [];
for (const [threshold, held, usage, claimed] of [[100000,10000,0,0],
  [100000,10000,20000,0],[100000,10000,20000,5000],[100000,10000,100000,0],
  [200000,25000,30000,40000],[1,0,0,0]]) {
  const input = {threshold,held,usage,claimed};
  const state = {threshold,held:()=>held,claimed:()=>claimed};
  roomCases.push({input,expected:context.Lco(state,usage)});
}
const allowanceCases = [];
for (const model of ['', 'claude-sonnet-4-6', 'claude-opus-5', 'other-model']) {
  for (const roomTokens of [undefined, 10000, 62999]) {
    const state = roomTokens === undefined ? undefined
      : {threshold:roomTokens / .7,held:()=>0,claimed:()=>0};
    const host = {contextRoom:()=>state,model:()=>model};
    const nativeRoom = context.C6r(host, undefined, {maxTokens:25000,maxSizeBytes:262144});
    for (const optedIn of [false,true]) {
      for (const tokens of [20000,50000,999999]) {
        allowanceCases.push({input:{model,roomTokens: nativeRoom.roomTokens ?? null,optedIn,tokens},
          expected:{modelBytesPerToken:context.$_(model),raised:nativeRoom.raised ?? null,
            hint:context.sLe(nativeRoom,optedIn,{tokens})}});
      }
    }
  }
}
const countCases = [];
for (const content of ['', '中文😀\n'.repeat(8), 'a'.repeat(41)]) {
  for (const raised of [false,true]) {
    const numbered = raised ? context._4e({content,startLine:1}) : content;
    const expected = await context.Hco(numbered,'txt',1000,undefined,true,raised);
    countCases.push({input:{content,raised,offset:1},expected:expected ?? 0});
  }
}
const rereadCases = [];
for (const entry of [undefined,{content:'prior',timestamp:42,offset:1},
  {content:'prior',timestamp:42,seededFromContext:true},
  {content:'prior',timestamp:42,isPartialView:true,offset:1,limit:3}]) {
  const host = {readFileState:new Map(entry?[['/workspace/file.txt',entry]]:[]),
    session:{writePermissionStash:{holds:()=>false,takeIdentity:()=>undefined}},
    contextRoom:()=>undefined, model:()=>'', nestedMemoryAttachmentTriggers:[]};
  rereadCases.push({input:{entry:entry??null},
    expected:await context.h6r({file_path:'/workspace/file.txt'},host)});
}
const promptCases = [];
for (const [lean,rewrite] of [[false,false],[true,false],[true,true]]) {
  const output = context.D3i('claude-opus-5', context.nPs, lean, rewrite)
    // Existing multi-provider image wording is the product's declared adapter.
    .replace('This tool allows Claude Code to read images (eg PNG, JPG, etc). When reading an image file the contents are presented visually as Claude Code is a multimodal LLM.',
      'This tool reads images (eg PNG, JPG, etc). When reading an image file, the contents are presented visually to the selected model.');
  const file = lean ? (rewrite ? 'read-lean-rewrite.md':'read-lean.md') : 'read-long.md';
  const template = new URL(`../../crates/tools/file/src/templates/claude-code-2.1.319/${file}`,import.meta.url);
  const existing = readFileSync(template,'utf8');
  if (existing !== output) {
    const index = Array.from({length:Math.max(existing.length,output.length)},(_,i)=>i)
      .find(i=>existing[i]!==output[i]);
    throw new Error(`${file} changed at ${index}: existing=${JSON.stringify(existing.slice(index,index+120))}; native=${JSON.stringify(output.slice(index,index+120))}`);
  }
  promptCases.push({input:{lean,rewrite},expected:output,sha256:sha256(output)});
}
// Schema library behavior is a controlled dependency; native descriptions and
// field inclusion come from the unchanged captured builder.
const scalar = () => ({int(){return this},nonnegative(){return this},positive(){return this},
  optional(){return this},describe(description){this.description=description;return this}});
Object.assign(context,{p:factory=>factory,Ne:fields=>fields,o:scalar,v:scalar,I:scalar,
  CU:value=>value,uv:value=>value,sl:(_,value)=>value,v7:()=>({}),eQ:20});
vm.runInContext('var '+schema+';',context);
const schemaCase = {maxSizeBytes:context.aGt,allowLargeDescription:context.a6r().allow_large.description};
const fixture = {schemaVersion: 1, claudeCodeVersion: '2.1.322', nativeSha256,
  sourceByteRanges, tokenCases, rangeCases, truncationCases, roomCases,allowanceCases,countCases,rereadCases,promptCases,schemaCase};
const destination = new URL('./fixtures/file_read_322.json', import.meta.url);
const serialized = JSON.stringify(fixture, null, 2) + '\n';
if (args.includes('--check')) assert.equal(readFileSync(destination, 'utf8'), serialized);
else writeFileSync(destination, serialized);
process.stdout.write(`Read 2.1.322: ${tokenCases.length + rangeCases.length + truncationCases.length + roomCases.length + allowanceCases.length + countCases.length + rereadCases.length + promptCases.length} native helper cases, ${sourceByteRanges.length} pinned ranges\n`);

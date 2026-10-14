import fs from 'node:fs';
import crypto from 'node:crypto';
import vm from 'node:vm';

const args=process.argv.slice(2);
const check=args.includes('--check');
const binaryPath=args.find(arg=>!arg.startsWith('--'))??'/tmp/harness-claude-code-2.1.289-darwin-arm64';
const bytes = fs.readFileSync(binaryPath);
const sha = crypto.createHash('sha256').update(bytes).digest('hex');
const expectedSha = '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69';
if (sha !== expectedSha) throw new Error(`pinned binary SHA mismatch: ${sha}`);

function cut(start, end) {
  const a = bytes.indexOf(Buffer.from(start));
  const z = bytes.indexOf(Buffer.from(end), a);
  if (a < 0 || z < 0) throw new Error(`source marker missing: ${start} / ${end}`);
  return bytes.subarray(a, z).toString('utf8');
}

const marker = '__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__';
const ctx = {};

// Execute exact embedded pure projection and boundary gate functions.
vm.runInNewContext(cut('function UDn(e,n){', 'function jDn()') + '\nglobalThis.__UDn = UDn;', ctx);
Object.assign(ctx, { $m: () => true, ni: () => true, He: () => 'firstParty' });
vm.runInNewContext(cut('function Ose(){', 'var TN=') + '\nglobalThis.__Ose = Ose;', ctx);
const markerEnabled = ctx.__Ose();
const projected = ctx.__UDn([
  { text: 'native shared base', scope: 'shared' },
  { text: 'native per-session context', scope: 'session' },
], markerEnabled ? marker : null);
const noMarkerProjection = ctx.__UDn([
  { text: 'native shared base', scope: 'shared' },
  { text: 'native per-session context', scope: 'session' },
], null);

// Qs is the pinned identity function. Mj/mho preserves the supplied block array.
vm.runInNewContext(cut('function Qs(e){return e}', 'function hSe(e){') + '\nglobalThis.__Qs = Qs;', ctx);
let promptComposer = cut('function Mj(e){', 'var Wie=');
// Node's script parser has no import.meta. These two unused coordinator-only
// expressions are stubbed; the tested Ta() === false branch does not call them.
promptComposer = promptComposer
  .replace('import.meta.require("/$bunfs/root/chunk-at6gymkh.js")', '({getCoordinatorSystemPrompt:()=>"coordinator"})')
  .replace('import.meta.require("/$bunfs/root/chunk-y4gf6qy2.js")', '({hasCommsRoledServer:()=>false})');
Object.assign(ctx, {
  Qs: ctx.__Qs,
  Ta: () => false,
  Ma: () => false,
  hSe: (value) => [value],
  fbn: () => undefined,
  $oe: () => {},
  i: () => {},
  d: (value) => value,
  _: (value) => value,
  y: () => {},
});
vm.runInNewContext(promptComposer + '\nglobalThis.__Mj = Mj;', ctx);
const queryPrompt = ctx.__Mj({
  defaultSystemPrompt: projected,
  toolUseContext: { options: { tools: [] } },
});
if (JSON.stringify(queryPrompt) !== JSON.stringify(projected)) throw new Error('Native Mj altered default prompt blocks');

// Execute exact fresh-writer function with only external collaborators stubbed.
const snapshotWriter = cut('function il(e,o,{resumeActive:', 'function Ts(e,o,{head:');
Object.assign(ctx, {
  l5e: () => true,
  Gye: () => undefined, // no pre-existing snapshot: take fresh live branch
  K6n: () => false,
  Bn: () => true,
  pdt: () => false,
  uDt: () => false,
  JMt: () => false,
  QMt: () => ({}),
  W0r: () => 'oracle-hash',
  Fan: () => [],
  ln: (attachment) => ({ type: 'attachment', attachment }),
  c: () => {},
});
vm.runInNewContext(snapshotWriter + '\nglobalThis.__Il = il;', ctx);
const writerResult = ctx.__Il([], queryPrompt, { options: { systemPromptSnapshot: true } });
const attachment = writerResult.record.attachment;
if (JSON.stringify(attachment.systemPrompt) !== JSON.stringify(projected)) throw new Error('fresh writer changed prompt blocks');
if (attachment.systemPrompt[1] !== marker) throw new Error('boundary marker not saved as its own block');
if (Object.hasOwn(attachment, 'sharedBoundary')) throw new Error('writer unexpectedly emitted sharedBoundary');

// Execute exact Native snapshot predicate/recovery function. LEn is represented
// by its visible contract systemPrompt: string[]; other schema collaborators are inert.
const snapshotPredicate = cut(
  'function WJe(e){return e.type==="attachment"&&e.attachment.type==="prompt_snapshot"}',
  'function FEn(e){',
);
const recovery = cut('function N1(e){', 'function q6n(');
Object.assign(ctx, {
  HJe: new WeakMap(),
  LEn: () => ({ safeParse: (value) => ({
    data: Array.isArray(value.systemPrompt) && value.systemPrompt.every((item) => typeof item === 'string')
      ? value : undefined,
  }) }),
  LFo: () => false,
  sfn: () => false,
});
vm.runInNewContext(snapshotPredicate + '\nglobalThis.__WJe = WJe;', ctx);
ctx.WJe = ctx.__WJe;
vm.runInNewContext(recovery + '\nglobalThis.__N1 = N1;', ctx);
const recovered = ctx.__N1([{ type: 'attachment', attachment }]);
if (JSON.stringify(recovered.systemPrompt) !== JSON.stringify(projected)) throw new Error('recovery changed prompt blocks');

// The exact message helper keeps the same attachment object as payload.
const attachmentWrapper = cut(
  'function ln(e,n={now:()=>new Date().toISOString(),uuid:()=>Rge()}){',
  'function Emr(e){',
);
Object.assign(ctx, { X6: () => ({}), Rge: () => 'oracle-uuid' });
vm.runInNewContext(attachmentWrapper + '\nglobalThis.__Ln = ln;', ctx);
const wrapped = ctx.__Ln(attachment, { now: () => 'oracle-time', uuid: () => 'oracle-uuid' });
if (wrapped.attachment !== attachment) throw new Error('message wrapper changed the snapshot payload');

const result = {
  binarySha256: sha,
  sourceOnlyOracle: true,
  externalDependenciesStubbed: ['feature/provider gate inputs', 'session transcript lookup', 'Zod safeParse contract', 'UUID/time/hash/telemetry'],
  markerEnabledForSyntheticFirstPartyConditions: markerEnabled,
  providerComposerOutput: projected,
  providerComposerWithoutMarker: noMarkerProjection,
  queryPrompt: queryPrompt,
  freshSnapshotAttachment: attachment,
  recoveredPrompt: recovered.systemPrompt,
  wrapperAttachmentSameReference: wrapped.attachment === attachment,
  sharedBoundaryFieldPresent: Object.hasOwn(attachment, 'sharedBoundary'),
};
const report=result;
if(check){
 const expected=JSON.parse(fs.readFileSync(new URL('../../docs/parity/mods-prompt-snapshot-native-2.1.289.json',import.meta.url),'utf8'));
 if(JSON.stringify(report)!==JSON.stringify(expected))throw new Error('Native snapshot fixture mismatch');
 console.log('Native prompt snapshot array/gate/writer/recovery fixture passed');
}else console.log(JSON.stringify(report,null,2));

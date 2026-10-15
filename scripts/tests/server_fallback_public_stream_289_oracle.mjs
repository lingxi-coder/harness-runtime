#!/usr/bin/env node
// Source-pinned 2.1.289 fallback QueryEvent/public stream boundary oracle.
// Usage: node scripts/tests/server_fallback_public_stream_289_oracle.mjs <2.1.288-binary> <2.1.289-binary> [--check]
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';

const args = process.argv.slice(2);
const check = args.includes('--check');
const binaries = args.filter((arg) => arg !== '--check');
assert.equal(binaries.length, 2, 'usage: node scripts/tests/server_fallback_public_stream_289_oracle.mjs <2.1.288-binary> <2.1.289-binary> [--check]');
const sha256 = (value) => createHash('sha256').update(value).digest('hex');
const specs = [
  {version: '2.1.288', path: binaries[0], sha256: 'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750'},
  {version: '2.1.289', path: binaries[1], sha256: '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69'},
];
const bytesByVersion = new Map(specs.map((spec) => {
  const bytes = readFileSync(spec.path);
  assert.equal(sha256(bytes), spec.sha256, `unexpected Claude Code ${spec.version} binary SHA-256`);
  return [spec.version, bytes];
}));
const sourceByteRanges = [];

function take(version, label, start, end, expectedHash, startText, endText) {
  const binary = bytesByVersion.get(version);
  assert(start >= 0 && end > start, `${label}: invalid source range`);
  if (startText) assert.equal(binary.subarray(start, start + Buffer.byteLength(startText)).toString(), startText, `${label}: start marker moved`);
  if (endText) assert.equal(binary.subarray(end, end + Buffer.byteLength(endText)).toString(), endText, `${label}: end marker moved`);
  const source = binary.subarray(start, end);
  const digest = sha256(source);
  assert.equal(digest, expectedHash, `${label}: pinned source hash changed`);
  sourceByteRanges.push({version, label, start, end, sha256: digest});
  return source.toString('utf8');
}

const source = {
  '2.1.288': {
    fallbackBranch: take('2.1.288', 'accepted server fallback internal QueryEvent sequence', 195142622, 195143001,
      '4caa8b706acf3d45acf13588fd6337e076c84fcbb98078db54833c90b5748140',
      'ne.push(...M.discardedMessages,...to),yield{type:"query_model_change",toModel:ve};', 'if(X.showBanner'),
    refusalSchema: take('2.1.288', 'internal refusal_continuation event schema', 180656306, 180656711,
      '13834ac216144f7adf654c2d23c895ea7c942741cee84e6a8957dfeafff01ab3',
      't8=f(()=>qo("phase",[u({type:R("refusal_continuation")', ',s8=f(()=>'),
    tombstoneSchema: take('2.1.288', 'internal tombstone event schema', 180628383, 180628846,
      '8465c3e5ae1ad7311102699cd56fb0fc4608e63f35e795c996c7ec070e90050e',
      'HJ=f(()=>u({type:R("tombstone")', 'tY=f(()=>'),
    headlessPassThrough: take('2.1.288', 'headless output frame passed to Q.write', 206106613, 206106654,
      '545f816299366fb00ca878c249249eb22629e65c2993b9dcdc4284f315ed6fdf',
      'let Lt=Ue.type==="result"?ke.stamp(Ue):Ue', ',or=Ys.has(Ue);'),
    writerTrack: take('2.1.288', 'stream-json writer trackWrite', 205245177, 205245817,
      'e56ded37f4480bebe1c02b5e637d69b865ab10a81ec8a4917a420b9eb395a58d',
      'trackWrite(s){', 'sampleForSdkSchemaCheck(s){'),
    writer: take('2.1.288', 'stream-json writer write method', 205247212, 205247398,
      'd1cec00c08b78b9613b64ad477769c717146c7807cad25d01aecca3281ad7272',
      'async write(s){let e=this.heldDueBefore(s);', 'writeAfterInit(s){'),
    writerCall: take('2.1.288', 'headless stream-json writer call', 206107200, 206107228,
      'e06379d7a063b2f79db0fa92aae653bbaa6aa2d75bd4c47a09fdb24e07ab1ffb',
      'if(await Q.write(Lt),At&&gs)', '{'),
  },
  '2.1.289': {
    fallbackBranch: take('2.1.289', 'accepted server fallback internal QueryEvent sequence', 195457859, 195458238,
      '4caa8b706acf3d45acf13588fd6337e076c84fcbb98078db54833c90b5748140',
      'ne.push(...M.discardedMessages,...to),yield{type:"query_model_change",toModel:ve};', 'if(X.showBanner'),
    refusalSchema: take('2.1.289', 'internal refusal_continuation event schema', 180919040, 180919445,
      '32a9f7a9958bc247cda96200e0f72f3b36e2303462eee7306e983273edf60260',
      'd8=f(()=>qo("phase",[u({type:R("refusal_continuation")', ',u8=f(()=>'),
    tombstoneSchema: take('2.1.289', 'internal tombstone event schema', 180891117, 180891580,
      'd906349e2b4fdf9b1f9c23b0511c800b1b5e8c320020ed01a90268e21b834e13',
      'ZJ=f(()=>u({type:R("tombstone")', 'dY=f(()=>'),
    headlessPassThrough: take('2.1.289', 'headless output frame passed to Q.write', 206442107, 206442148,
      '545f816299366fb00ca878c249249eb22629e65c2993b9dcdc4284f315ed6fdf',
      'let Lt=Ue.type==="result"?ke.stamp(Ue):Ue', ',or=Xs.has(Ue);'),
    writerTrack: take('2.1.289', 'stream-json writer trackWrite', 205579907, 205580547,
      '8ebb0488a7120a8fc041a453b32f7f30c94ff2dac4d0f0cc9af47b9c1e3d6122',
      'trackWrite(s){', 'sampleForSdkSchemaCheck(s){'),
    writer: take('2.1.289', 'stream-json writer write method', 205581942, 205582128,
      '99da2da7ffa1f9da8c36797f396b7f9b95a383de655509b22c1264acb8770140',
      'async write(s){let e=this.heldDueBefore(s);', 'writeAfterInit(s){'),
    writerCall: take('2.1.289', 'headless stream-json writer call', 206442694, 206442722,
      'bfee3435accd1e66eb223a616b4160d52fd7aec633d607af076ec5ba8dd3e339',
      'if(await Q.write(Lt),Rt&&gs)', '{'),
  },
};

const expectedEventExpressions = {
  queryModelChange: 'yield{type:"query_model_change",toModel:ve}',
  discardedTombstone: 'yield{type:"tombstone",message:ke,displayOnly:!0}',
  refusalBegin: 'yield{type:"refusal_continuation",phase:"begin",salvageText:J.text,join:"exact",replacesUuids:J.originals.map((ke)=>ke.uuid)}',
};
for (const [version, ranges] of Object.entries(source)) {
  const branch = ranges.fallbackBranch;
  const indices = Object.fromEntries(Object.entries(expectedEventExpressions).map(([name, expression]) => [name, branch.indexOf(expression)]));
  assert(Object.values(indices).every((index) => index >= 0), `${version}: accepted producer event expression missing`);
  assert(indices.queryModelChange < indices.discardedTombstone && indices.discardedTombstone < branch.indexOf('for(let ke of to)') && branch.indexOf('for(let ke of to)') < indices.refusalBegin, `${version}: fallback event order changed`);
  assert(ranges.tombstoneSchema.includes('@internal') && ranges.tombstoneSchema.includes('Wire shape pending a dedicated schema'), `${version}: tombstone internal-schema boundary changed`);
  assert(ranges.refusalSchema.includes('@internal') && ranges.refusalSchema.includes('salvage_text') && ranges.refusalSchema.includes('session_id'), `${version}: refusal continuation schema boundary changed`);
  assert(ranges.headlessPassThrough.includes('Ue.type==="result"?ke.stamp(Ue):Ue'), `${version}: headless frame pass-through changed`);
  assert(ranges.writer.includes('this.trackWrite(s)') && ranges.writer.includes('lr('), `${version}: direct writer behavior changed`);
  assert(ranges.writerTrack.includes('s.type==="result"&&this.resultDelivery') && !ranges.writerTrack.includes('switch(s.type)'), `${version}: writer filter assumption changed`);
}
const rawMarkerCounts = Object.fromEntries(specs.map(({version}) => {
  const binary = bytesByVersion.get(version);
  return [version, {
    assistantContinuationBegin: (() => { let count=0,at=0;const marker=Buffer.from('assistant_continuation_begin');while((at=binary.indexOf(marker,at))>=0){count++;at+=marker.length;}return count; })(),
    queryModelChangeSchema: (() => { let count=0,at=0;const marker=Buffer.from('type:R("query_model_change")');while((at=binary.indexOf(marker,at))>=0){count++;at+=marker.length;}return count; })(),
  }];
}));
assert.deepEqual(rawMarkerCounts, {'2.1.288': {assistantContinuationBegin: 0, queryModelChangeSchema: 0}, '2.1.289': {assistantContinuationBegin: 0, queryModelChangeSchema: 0}});

// Execute the exact local writer methods with a supplied internal object. This
// proves only that this writer method serializes a frame it is handed; it does
// not assert that the fallback QueryEvent reaches the headless writer.
async function probeWriter(ranges) {
  const context = vm.createContext({
    lr: (line) => context.output.push(line),
    xae: (frame) => JSON.stringify(frame),
    Lae: (frame) => JSON.stringify(frame),
    clearTimeout: () => undefined,
    setTimeout: () => ({unref() {}}),
    i: () => undefined,
    d: (value) => value,
    et: 1000,
    tt: 1000,
    output: [],
  });
  vm.runInContext(`this.writer={heldDueBefore:()=>[],heldDueAfter:()=>[],sampleForSdkSchemaCheck:()=>{},${ranges.writerTrack},${ranges.writer}};`, context);
  for (const frame of [
    {type: 'query_model_change', toModel: 'fallback-model'},
    {type: 'tombstone', message: {uuid: 'old'}, displayOnly: true},
    {type: 'refusal_continuation', phase: 'begin', salvageText: 'kept', join: 'exact', replacesUuids: ['old']},
  ]) await context.writer.write(frame);
  return context.output.map((line) => JSON.parse(line));
}
const writerProbe = await probeWriter(source['2.1.289']);
assert.deepEqual(writerProbe.map(({type}) => type), ['query_model_change', 'tombstone', 'refusal_continuation']);

const output = {
  versions: Object.fromEntries(specs.map(({version, sha256: digest}) => [version, digest])),
  sourceByteRanges,
  internalFallbackProducer: {
    eventNames: ['query_model_change', 'tombstone', 'refusal_continuation'],
    eventOrder: ['append discarded and compensation rows internally', 'query_model_change', 'tombstone discarded rows with displayOnly=true', 'tombstone compensation rows with displayOnly=true', 'refusal_continuation phase=begin'],
    rawBegin: expectedEventExpressions.refusalBegin,
    sameAcceptedBranchBytes288And289: source['2.1.288'].fallbackBranch === source['2.1.289'].fallbackBranch,
    sourceSha288: sourceByteRanges.find((entry) => entry.version === '2.1.288' && entry.label.includes('accepted server fallback')).sha256,
    sourceSha289: sourceByteRanges.find((entry) => entry.version === '2.1.289' && entry.label.includes('accepted server fallback')).sha256,
    markerCounts: rawMarkerCounts,
  },
  internalSchemas: {
    tombstone: {description: 'Internal QueryEvent; wire shape pending a dedicated schema; schema has message/uuid/session_id fields.', versions: ['2.1.288', '2.1.289']},
    refusalContinuation: {description: 'Internal QueryEvent schema uses salvage_text plus uuid/session_id; producer begin object uses salvageText/join/replacesUuids and does not include top-level uuid/session_id.', versions: ['2.1.288', '2.1.289']},
    queryModelChange: 'No type:R("query_model_change") schema in either pinned binary.',
  },
  publicStreamJsonBoundary: {
    headlessPassesFrameWithoutMapping: true,
    localWriterProbe: writerProbe,
    probeScope: 'Runs the pinned trackWrite/write methods with JSON.stringify supplied for imported Lae and a capturing lr callback. The probe proves no event-type filter in those methods; it is not an end-to-end CLI output capture.',
    conclusion: 'The output writer serializes the object it receives and does not type-filter these names. The source-pinned raw fallback producer emits internal QueryEvents; tombstone and refusal_continuation schemas are marked @internal, and query_model_change has no matching SDK schema (the raw refusal_continuation fields also differ from its schema). The complete lower-level QueryEvent-to-headless-generator path is not traced here, so this oracle does not claim that these internal events do or do not reach stdout. Do not add a new public server_fallback_* or assistant_continuation_begin frame based on this producer alone.',
  },
};
const json = JSON.stringify(output, null, 2) + '\n';
const fixture = new URL('../../crates/permission/tests/fixtures/server_fallback_public_stream_2_1_289.json', import.meta.url);
if (check) assert.equal(readFileSync(fixture, 'utf8'), json, 'server fallback public stream fixture differs');
else writeFileSync(fixture, json);
console.log(`${check ? 'checked' : 'wrote'} server-fallback 2.1.289 producer/schema/writer oracle (2 native versions, ${sourceByteRanges.length} ranges)`);

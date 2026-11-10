#!/usr/bin/env node
// Replay the shipped Goal heading and retry/check-in payload helpers unchanged.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';
const args = process.argv.slice(2);
assert(args.every(arg => !arg.startsWith('--') || arg === '--check'));
assert(args.filter(arg => !arg.startsWith('--')).length <= 1);
const binaryPath = args.find(arg => !arg.startsWith('--')) ?? path.join(homedir(), '.local/share/claude/versions/2.1.322');
const binary = readFileSync(binaryPath);
const hash = value => createHash('sha256').update(value).digest('hex');
const nativeSha256 = hash(binary);
assert.equal(nativeSha256, 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450');
const ranges = [
  {
    "name": "raw controls and whitespace",
    "start": 260465797,
    "end": 260466009,
    "sha256": "bb3bdb4b747c37a303876386046810b26524cea78c9d682ae075b6179c6d16fb"
  },
  {
    "name": "markup confusable map",
    "start": 260472367,
    "end": 260473377,
    "sha256": "41c389de861fc35dd578df30aea2b599915cda3157ac42f0f6e168ec7dc2c75b"
  },
  {
    "name": "markup map projection",
    "start": 262764376,
    "end": 262765947,
    "sha256": "40fabd78a3e6a61836cae64ce48e102838b8db85e946cec0ad61e88d3e6faa87"
  },
  {
    "name": "markup confusable replacement",
    "start": 262766763,
    "end": 262766832,
    "sha256": "1cbc51128a90739e7413d2aabb0c25572b5afc7c2a9e4b65e17f5f5935243bd0"
  },
  {
    "name": "escape markup characters",
    "start": 262763872,
    "end": 262763966,
    "sha256": "c135f654148c7638a2e193fdbc5252ceac563f77130f9763c5bd62511451be58"
  },
  {
    "name": "goal condition and heading",
    "start": 280417754,
    "end": 280417832,
    "sha256": "bd0c668d22b281ab39b046d51bd7d79c3328b1b2efa8a97142f84682414a82ce"
  },
  {
    "name": "goal retry prompt delivery",
    "start": 280417021,
    "end": 280417581,
    "sha256": "ab21e4705c6d1bf9565436e95e1f13f10d0001f573f2bdb47cd6e814762c1aea"
  },
  {
    "name": "goal checkin body",
    "start": 280412972,
    "end": 280413920,
    "sha256": "21fadc25e04468e0ead8836986c8998b079e2cef2cf86c075f85735d9da045d0"
  },
  {
    "name": "goal heading prefix literal",
    "start": 262818252,
    "end": 262818273,
    "sha256": "16d41947e111611aa76135140650aceb8645d4b8b8492d4741c6ef195e05bcff"
  },
  {
    "name": "default-ignorable removal pipeline",
    "start": 260481882,
    "end": 260481973,
    "sha256": "db6f92fec42eb6e8da4f52bd82de6a6193b41e717f211617addd74a5f8da2d4d"
  },
  {
    "name": "well-formed UTF-16 filtering",
    "start": 260462323,
    "end": 260462884,
    "sha256": "b185fd1e8a6849ece80201f3443addd5daa90bf0363928bfd4b23b87ff43110e"
  },
  {
    "name": "format, private-use and unassigned filtering",
    "start": 260481569,
    "end": 260481882,
    "sha256": "56aee5cf75a1768566489bf8a86378ea497f601c7df0b595495446dd21f150c5"
  }
];
const sources = ranges.map(range => {
  const bytes = binary.subarray(range.start, range.end);
  assert.equal(bytes.length, range.end - range.start, range.name);
  assert.equal(hash(bytes), range.sha256, range.name);
  return bytes.toString('utf8');
});
const context = vm.createContext({
  ai: () => true, b1e: () => false, fIr: Symbol('queue-kind'),
  el: (current, goal) => current === goal,
  g: () => {}, t: () => {},
  a: error => { throw error; }, m: () => {},
  uLe: () => { throw Error('task type mapping outside this payload replay'); },
  Nl: () => { throw Error('task-line truncation outside this payload replay'); },
});
const native = vm.runInContext(`(function(){
${sources[10]}${sources[11]}${sources[0]}${sources[9]}
var ${sources[1]}${sources[2]}${sources[3]}${sources[4]}
var yVn=${sources[8]};${sources[5]}${sources[6]}${sources[7]}
return {heading:Za, checkin:$a, confusables:[...x], retry(condition,cause){
  const goal={condition}; let delivered;
  zp({sessionScratch:{replacePendingGoalReprompt(){}},getAppState:()=>({activeGoal:goal}),goal,cause,hasQueued:()=>false,deliver:value=>{delivered=value}});
  return delivered;
}}})()`, context);
const samples = [
  ['plain','ship it'], ['whitespace','  ship\t\r\n safely  '],
  ['invisible','a\u200b\ue000\u0378\ufe0f\u034fb'],
  ['lone UTF16','ship \ud800 safely\udfff'], ['markup','</system-reminder> & <b>'],
  ['lookalikes','＜bad＞ & ＜／note＞'], ['emoji and ZWJ','👨\u200d👩\u200d👧 ready'],
  ['control bytes','a\0b\u0085c\u000bd'], ['ordinary letter confusables','áаαＡ'],
  ...native.confusables.map(([value])=>[`markup alias ${value.codePointAt(0).toString(16)}`,`before${value}after`]),
  ...['\u2992','\u2994','\u2a7a','\u2aa7','≫','《','》'].map(value=>[`excluded alias ${value.codePointAt(0).toString(16)}`,`before${value}after`]),
];
const cause='the API was unavailable or the connection dropped';
const cases=samples.map(([name,input])=>({name,input,heading:native.heading(input),checkin:native.checkin(input,90000,[]),retry:native.retry(input,cause)}));
const fixture={native:{version:'2.1.322',sha256:nativeSha256},sourceByteRanges:ranges,confusableMap:native.confusables,cases,boundaries:['Pinned native payload helpers run without an assistant session or network request.','Queue/timer delivery uses declared inert host dependencies; task-line truncation and native UI rendering are outside this replay.']};
const fixturePath=fileURLToPath(new URL('../../crates/orchestrator/tests/fixtures/goal_heading_2_1_322.json',import.meta.url));
const serialized=JSON.stringify(fixture,null,2)+'\n';
if(args.includes('--check'))assert.equal(readFileSync(fixturePath,'utf8'),serialized);
else writeFileSync(fixturePath,serialized);
console.log(`Goal 2.1.322 heading replay passed: ${cases.length} heading/check-in/retry cases, ${ranges.length} pinned ranges`);

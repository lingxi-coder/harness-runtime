// Run against extracted official 2.1.270 chunks; does not distribute oracle source.
// node permission/tests/oracle/loop_permission_270.cjs /path/to/chunks
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = process.argv[2];
if (!root) throw new Error('Pass the directory containing official 2.1.270 chunks');
const read = name => fs.readFileSync(path.join(root, name), 'utf8');
const between = (source, start, end) => {
  const first = source.indexOf(start);
  const last = source.indexOf(end, first);
  if (first < 0 || last < 0) throw new Error(`Missing oracle anchor: ${start}`);
  return source.slice(first, last);
};
const source = read('src_169588164.js');
if (!source.includes('// Version: 2.1.270')) throw new Error('Wrong oracle version');
const create = between(read('src_185261434.js'), 'async checkPermissions(r)', ',async validateInput');
const wakeup = between(source, 'async checkPermissions(n){if(e().mode==="auto")', ',async call');
const At = vm.runInNewContext(`(${between(read('src_167837625.js'), 'function At(', 'function YE(')})`, {
  C: {}, S: () => false, den: value => value,
});
// Ordinary local context: no remote surface, MCP ceilings, sandbox override,
// or permission hooks. Rules and the tool-owned baseline remain real inputs.
const context = {
  Ye: Error, be: r => r.permissions, vi: (p, e) => p.deny?.includes(e.name) ? e.name : null,
  yS: () => null, Cm: () => null, Klt: () => null, kft: () => null, cc: name => name,
  Kge: () => false, hx: (e, p) => p.mode, zj: () => false, K_: () => null,
  For: () => false, Z$n: () => false, IWe: (p, e) => p.allow?.includes(e.name) ? e.name : null,
  V$n: (r, n) => r.updatedInput ?? n, kQ: () => false, uh: e => e, Y$n: () => false,
  t: () => {}, b: JSON.stringify,
};
const outer = vm.runInNewContext(`(${between(source, 'async function tBn(', 'async function DKt(')})`, context);
(async () => {
  let cases = 0;
  for (const mode of ['default', 'acceptEdits', 'plan', 'dontAsk', 'bypassPermissions', 'auto']) {
    const local = {
      CronCreate: vm.runInNewContext(`({${create}})`, {e: {permissions: () => ({mode})}}),
      ScheduleWakeup: vm.runInNewContext(`({${wakeup}})`, {e: () => ({mode})}),
    };
    for (const name of ['CronCreate', 'CronDelete', 'CronList', 'ScheduleWakeup']) {
      const tool = At({name, inputSchema: {parse: n => n}, create: () => local[name] ?? {call: () => {}}});
      const result = await outer(tool, {prompt: 'check'}, {permissions: {mode}, abortController: {signal: {aborted: false}}});
      const expected = mode === 'auto' && ['CronCreate', 'ScheduleWakeup'].includes(name) ? 'ask' : 'allow';
      if (result.behavior !== expected) throw new Error(`${name} ${mode}: ${JSON.stringify(result)}`);
      cases++;
    }
  }
  const RYe = vm.runInNewContext(`(${between(read('src_177710667.js'), 'function RYe(', '\nexport{')})`, {
    $Gt: 4, e: 48, ne: (value, limit) => value.slice(0, limit),
  });
  const wsCheck = vm.runInNewContext(`(${between(read('src_177711820.js'), 'function Ee(', 'function F(')})`, {
    kYe: () => null, RYe,
  });
  for (const [protocols, suffix] of [
    [[], ''], [['v1', 'v2'], ' (subprotocols: "v1", "v2")'],
    [['a'.repeat(49), 'b', 'c', 'd', 'e'], ` (subprotocols: "${'a'.repeat(48)}…", "b", "c", "d" (+1 more))`],
  ]) {
    const result = wsCheck({url: 'wss://events.example.com/feed', protocols});
    if (result.message !== `Monitor will open a WebSocket to wss://events.example.com/feed${suffix}`) throw new Error(result.message);
    cases++;
  }
  console.log(JSON.stringify({oracle: '2.1.270', extractedFunctions: ['At', 'tBn', 'CronCreate.checkPermissions', 'ScheduleWakeup.checkPermissions', 'Ee', 'RYe'], cases, result: 'PASS', scope: 'local outer permission and WebSocket preview; downstream LLM classifier excluded'}));
})().catch(error => { console.error(error); process.exitCode = 1; });

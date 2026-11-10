import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import vm from 'node:vm';

const binaryPath = process.argv[2];
assert(binaryPath, 'Usage: node tool_prompts_319_oracle.mjs <2.1.319 binary> [--check]');
const binary = fs.readFileSync(binaryPath);
const hash = value => createHash('sha256').update(value).digest('hex');
const binarySha256 = hash(binary);
assert.equal(binarySha256, '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd');
const ranges = [];
function take(offset, first, last, label) {
  const start = binary.indexOf(first, offset);
  const end = binary.indexOf(last, start + first.length);
  assert(start >= offset && end > start && end - start < 40_000, label);
  const bytes = binary.subarray(start, end);
  ranges.push({ label, start, endExclusive: end, sha256: hash(bytes) });
  return bytes.toString('utf8');
}
function literal(offset, first, label) {
  const start = binary.indexOf(first, offset);
  assert(start >= offset, label);
  const quote = binary[start + first.length];
  assert([34, 39, 96].includes(quote), label);
  let end = start + first.length + 1;
  for (; end < binary.length; end++) {
    if (binary[end] === 92) end++;
    else if (binary[end] === quote) break;
  }
  end++;
  assert(end - start < 40_000, label);
  const bytes = binary.subarray(start, end);
  ranges.push({label, start, endExclusive:end, sha256:hash(bytes)});
  return bytes.toString('utf8');
}
const catalog = take(260741362, 'var kAo=', ';function M()', 'native baked model catalog');
const catalogWorld = vm.createContext({});
vm.runInContext(catalog, catalogWorld, { timeout: 1000 });
const models = vm.runInContext('kAo.models.map(({id,capabilities})=>({id,capabilities}))', catalogWorld);
const world = vm.createContext({
  models,
  i: {},
  uH: () => ({}),
  sd: id => models.find(model => model.id === id),
  rt: id => id,
  Pe: value => /^(1|true|yes|on)$/i.test(value ?? ''),
  rs: value => /^(0|false|no|off)$/i.test(value ?? ''),
  k: () => false,
  pe: () => false,
  tte: () => false,
});
vm.runInContext(take(260766296, 'function Rw(', 'function zbe(', 'native capability lookup with environment overrides'), world);
vm.runInContext(take(264447180, 'function _t(', 'var wt=', 'native standard/lean selectors'), world);
vm.runInContext('function C(){return {leanPrompt:ce,leanPromptCompiledOnly:ce};}', world);
const gateCases = [];
for (const model of [...models.map(model => model.id), 'claude-new-model', 'claude-sonnet-new-eap']) {
  for (const capability of [null, 'lean_prompt', '-lean_prompt']) {
    world.i.CLAUDE_CODE_MODEL_CAPABILITIES = capability ?? undefined;
    for (const simple of [null, '1', '0']) {
      world.i.CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT = simple ?? undefined;
      gateCases.push({ model, capability, simple, lean: vm.runInContext(`ce(${JSON.stringify(model)})`, world) });
    }
  }
}
world.i.CLAUDE_CODE_MODEL_CAPABILITIES = undefined;
world.i.CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT = undefined;
Object.assign(world, {
  Ge: 'Read', ct: 'Edit', Me: 'Bash', Sr: 'Grep', ft: 'Agent',
  wV: () => 'default',
  TZe: (model, ids) => ids.includes(model), at: () => 'claude-sonnet-4-6',
  F_e: () => false, Fgt: () => false,
});
vm.runInContext(take(264445443, 'function _5(', 'function ie(', 'native per-request lean-rewrite override'), world);
vm.runInContext(take(264487974 - 100, 'function oks(', 'var Ft=', 'native Glob text and default-style branch'), world);
vm.runInContext(take(264488156, 'var Ut=', 'function QLe(', 'native Read and Grep templates'), world);
vm.runInContext('var ' + take(264462543, 'ye=', ';function RWi(', 'native rewritten Read verification guidance'), world);
vm.runInContext('var ' + take(264481231, '_e=', ',o2i=', 'native rewritten Grep prompt'), world);
vm.runInContext(take(267639598, 'function o()', 'var p_=', 'native Write templates'), world);
vm.runInContext(take(274246064, 'function vjs()', 'var fwt=', 'native Edit templates'), world);
vm.runInContext(take(278629500, 'var Fi=', ';var ji=', 'native TodoWrite templates'), world);
Object.assign(world, { n: 'November 2026', a: '', P: (count, word) => count === 1 ? word : `${word}s` });
vm.runInContext(take(264516200, 'function oWi(', 'var FN=', 'native WebSearch templates'), world);
const webfetch = vm.createContext({
  b0: ({ leanPrompt }) => leanPrompt,
  W: class { constructor(factory) { this.value = factory(); } of() { return this.value; } },
  F: () => ({host: 'controlled-host'}), i: {},
  P: world.P, Vn: 'WebFetch', ff: 'WebSearch',
  Cp: () => false, uee: () => 'ArtifactRead', yGe: 'ArtifactRead',
});
vm.runInContext(take(268524459, 'var l=900000;', 'var zbs=', 'native WebFetch templates and default cache TTL'), webfetch);
const cases = [];
function add(tool, lean, output, extra = {}) {
  cases.push({ tool, lean, ...extra, output, bytes: Buffer.byteLength(output), sha256: hash(output) });
}
for (const lean of [false, true]) {
  world.lean = lean;
  const evaluate = text => vm.runInContext(text, world, { timeout: 1000 });
  add('Glob', lean, evaluate('oks("claude-opus-5-5",lean)'));
  add('Read', lean, evaluate('E2i("claude-opus-5-5",sks,lean,false)'));
  add('Grep', lean, evaluate('iks("claude-opus-5-5",lean,false)'));
  add('Write', lean, evaluate('VDi("claude-opus-5-5",lean)'));
  add('Edit', lean, evaluate('Cjs("claude-opus-5-5",lean)'));
  add('TodoWrite', lean, evaluate('mr("claude-opus-5-5",lean)'));
  add('WebSearch', lean, evaluate('oWi("claude-opus-5-5",lean)'), { monthYear: 'November 2026' });
  webfetch.lean = lean;
  add('WebFetch', lean, vm.runInContext('aBi("claude-opus-5-5","none",lean,false,false)', webfetch));
}
const agents = vm.createContext({
  Xge: () => true, _5: () => false, yPt: () => false, Ddt: () => false,
  Vat: () => false, q8e: () => true, pIe: () => false, k6: () => false,
  wx: () => false, Ll: () => false, wV: () => 'default',
  MV: () => false, Fc: () => false, N$n: () => false,
  cqi: () => true, H5: () => false, nUn: () => false, GQe: () => false,
  b0: ({ leanPrompt }) => leanPrompt,
  ft: 'Agent', Rn: 'SendMessage', Ge: 'Read', Sr: 'Grep', Vn: 'WebFetch', oT: 'fork', i: {},
});
vm.runInContext('var ' + take(264050305, 'HAr=', ',pVi=', 'native Agent introduction'), agents);
vm.runInContext('var ' + take(264051492, 'zAr=', ';\nexport', 'native required-agent-type message'), agents);
vm.runInContext('var ' + take(270843224, '$on=', ';function kde(', 'native Agent delegation guidance'), agents);
vm.runInContext(take(272816112, 'var yro=', 'function _ro(', 'native Agent prompt builder'), agents);
agents.yPt = () => agents.fork;
for (const lean of [false, true]) {
  for (const fork of [false, true]) {
    agents.lean = lean;
    agents.fork = fork;
    add('Agent', lean, await vm.runInContext('rvi({model:"claude-opus-5-5",isCoordinator:false,forkAvailable:fork,generalPurposeAvailable:true,continueAvailable:true,leanPrompt:lean,leanRewrite:false})', agents), {fork});
  }
}
const bash = vm.createContext({ Ncn: () => false });
vm.runInContext(take(270918500, 'function Vrn()', 'function cY()', 'native Bash working-directory guidance'), bash);
const bashCwdCases = [];
for (const maintainProjectDirectory of [false, true]) {
  bash.Ncn = () => maintainProjectDirectory;
  bashCwdCases.push({ maintainProjectDirectory, output: vm.runInContext('Vrn()', bash) });
}
const rewriteCases = [
  { tool: 'Read', output: vm.runInContext('E2i("claude-opus-5-5",sks,true,true)', world) },
  { tool: 'Grep', output: vm.runInContext('iks("claude-opus-5-5",true,true)', world) },
];
const task = vm.createContext({Zo: () => false});
vm.runInContext(take(278717919, 'function En()', 'var ol=', 'native TaskCreate description'), task);
vm.runInContext(take(278730609, 'function $n()', 'var ll=', 'native TaskList description'), task);
vm.runInContext('var ' + literal(278722100, 'xn=', 'native TaskGet description'), task);
vm.runInContext('var ' + literal(278724000, 'zn=', 'native TaskUpdate description'), task);
const taskCases = [
  {tool: 'TaskGet', output:vm.runInContext('xn',task)},
  {tool: 'TaskUpdate', output:vm.runInContext('zn',task)},
];
for (const swarms of [false,true]) {
  task.Zo = () => swarms;
  taskCases.push({tool:'TaskCreate',swarms,output:vm.runInContext('En()',task)});
  taskCases.push({tool:'TaskList',swarms,output:vm.runInContext('$n()',task)});
}
const cron = vm.createContext({
  Kf:'CronCreate',Uk:'CronDelete',DNe:'CronList',Ns:'Monitor',kDe:7,
  WZ:{recurringFrac:0.1,recurringCapMs:900000,oneShotMaxMs:90000},
});
vm.runInContext(take(274728905, 'function ems(', 'var tms=', 'native CronCreate description'),cron);
vm.runInContext(take(274733416, 'function nms(', 'var rms=', 'native CronDelete description'),cron);
vm.runInContext(take(274733704, 'function oms(', '\nexport{', 'native CronList description'),cron);
const cronCases=[];
for (const durable of [false,true]) {
  cron.durable=durable;
  for (const [tool,call] of [['CronCreate','ems(durable)'],['CronDelete','nms(durable)'],['CronList','oms(durable)']]) {
    cronCases.push({tool,durable,output:vm.runInContext(call,cron)});
  }
}
vm.runInContext('var ' + literal(267628300, 'jDi=', 'native TaskStop description'),world);
vm.runInContext('var ' + literal(274262900, 'Qso=', 'native NotebookEdit description'),world);
const otherToolCases = [
  {tool:'TaskStop',output:vm.runInContext('jDi',world)},
  {tool:'NotebookEdit',output:vm.runInContext('Qso',world)},
];
const monitor = vm.createContext({
  xze: () => false, q: () => false, Wy: () => false, $ce: () => false,
  GQe: () => false, h2e: () => false, Me: 'Bash', Ns: 'Monitor', xL: 'PushNotification',
});
vm.runInContext(take(267630245, 'function lms()', 'export{ya,bSr,jDi', 'native Monitor text, lifecycle branches and WebSocket guidance'), monitor);
const monitorCases = [];
for (const backgroundDisabled of [false, true]) {
  for (const pushNotifications of [false, true]) {
    monitor.xze = () => backgroundDisabled;
    monitor.h2e = () => pushNotifications;
    monitorCases.push({
      backgroundDisabled, pushNotifications,
      output: vm.runInContext('dms()+cms+lms()', monitor),
    });
  }
}
take(260195700, '"Deprecated: no longer has any effect (the TaskOutput tool was removed).', '),respondToBashCommands:', 'native explicit TaskOutput removal metadata');
const output = JSON.stringify({
  version: '2.1.319', binarySha256, ranges, gateCases, cases, bashCwdCases, rewriteCases, taskCases, cronCases, otherToolCases, monitorCases,
  controls: {
    modelNormalization: 'canonical IDs supplied; transport/profile normalization is tested by the host registry',
    featureFlags: 'absent/default false; style=default; leanRewrite=false (no baked temporal_hare capability)',
    toolCapabilities: 'PDF available; standard tab read prefix; artifact link integration absent; cache TTL=900000ms',
    upstreamOnlyDifferences: [
      'Read long image sentence is adapted to the selected multimodal model.',
      'WebSearch geographic and provider-support claims follow the actual selected search service.',
      'WebFetch GitHub guidance permits authenticated tools and the registered shell.',
      'TaskOutput is explicitly removed upstream; LingXi retains its implemented legacy tool as a product extension without a 2.1.319 template-equivalence claim.',
    ],
  },
}, null, 2) + '\n';
const target = new URL('./fixtures/tool_prompts_319.json', import.meta.url);
if (process.argv.includes('--check')) assert.equal(hash(fs.readFileSync(target)), hash(output), 'Pinned tool prompt oracle differs');
else fs.writeFileSync(target, output);
console.log(`Verified ${cases.length} tool templates, ${rewriteCases.length} rewrites, ${taskCases.length} task, ${cronCases.length} cron, ${otherToolCases.length} other tool and ${monitorCases.length} Monitor variants, and ${gateCases.length} selector cases from Claude Code 2.1.319 (${binarySha256})`);

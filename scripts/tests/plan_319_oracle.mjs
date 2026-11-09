// Regenerate only from the exact 2.1.319 artifact and an unmodified native API request.
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';

const root = path.dirname(fileURLToPath(import.meta.url));
if (!process.argv[2] || !process.argv[3]) throw Error('Usage: node plan_319_oracle.mjs <official binary> <captured request-01.json> [--check]');
const manifest = {
  "sha256": "1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd",
  "ranges": [
    {
      "start": 274604749,
      "endExclusive": 274618138,
      "bytes": 13389,
      "sha256": "ebc29ecf5a4944759eae3c0cd888b5f508a86cf062c1cca80e199807490288c5",
      "path": "reminders.js"
    },
    {
      "start": 278025425,
      "endExclusive": 278040859,
      "bytes": 15434,
      "sha256": "395c64d642710006dcce31903c980264dec9445ce8f7dedd13ce66242f25c744",
      "path": "exit.js"
    },
    {
      "start": 278486116,
      "endExclusive": 278492369,
      "bytes": 6253,
      "sha256": "52e1779022a5c64c3135b26391805ffcaee7e0dcbcade43256dadea5bf74ef6f",
      "path": "enter.js"
    },
    {
      "start": 274497311,
      "endExclusive": 274497628,
      "bytes": 317,
      "sha256": "47b6132fdee05ac871f989fa7ab56e36621c2c3960ef66dfe10d9e3fbf984a4d",
      "path": "counts.js"
    }
  ]
};
const binary = fs.readFileSync(process.argv[2]);
const hash = value => createHash('sha256').update(value).digest('hex');
if (hash(binary) !== manifest.sha256) throw Error('Official artifact changed');
const sources = {};
for (const range of manifest.ranges) {
  const bytes = binary.subarray(range.start, range.endExclusive);
  if (hash(bytes) !== range.sha256) throw Error('Hash-pinned source range changed');
  sources[path.basename(range.path)] = bytes.toString('utf8').replace(/export\{[^}]+\};\s*$/, '');
}

// These are explicit imported host capabilities, not replacements for the text builders.
// No complete application/bootstrap, real provider, account or filesystem function is executed.
const state = { outputStyle: 'default', agents: true, tier: 'none', profile: 'none', env: {}, inline: false, docs: false, planDoc: undefined };
const context = vm.createContext({
  state, Buffer, cs: 'AskUserQuestion', tc: 'ExitPlanMode', ct: 'Edit', Xt: 'Write',
  ft: 'Agent', $b: 'EnterPlanMode', Kg: 'ExitPlanMode', Zr: 'Glob', Sr: 'Grep', Ge: 'Read',
  Vx: { agentType: 'Explore' }, gge: { agentType: 'Plan' },
  wV: () => state.outputStyle, Cq: () => state.agents, ir: () => state.tier, XE: () => state.profile,
  i: state.env, mu: values => values, Ie: value => value, p: factory => factory, dn: value => value,
  wx: () => state.hybridSearch ?? false, Ll: () => state.hybridSearch ?? false, SMe: () => state.inline, xQt: () => state.docs,
  _pt: () => state.planDoc, O0n: doc => doc.title === undefined ? `the Claude Doc with the id ${doc.id}` : `the Claude Doc "${doc.title}"`,
  l: () => {}, f: value => value, Lt: value => value, iKn: () => false, iH: () => undefined, _: JSON.stringify,
  oT: 'fork', Ubr: () => undefined, tPe: () => {}, Fc: () => state.inProcessTeammate ?? false,
});
for (const source of Object.values(sources)) vm.runInContext(source, context, { timeout: 1000 });
const api = vm.runInContext('({reminder:wVs,where:IVs,enter:tJn,exit:R8,counts:()=>({plan:Ufo(),explore:jfo()})})', context);
const reminderCases = [];
for (const [name, options] of [
  ['full-default', {}], ['full-exists', { planExists: true }],
  ['full-no-agents', { agents: false }], ['full-custom-style', { outputStyle: 'custom' }],
  ['full-custom', { customInstructions: 'CUSTOM_PLAN_WORKFLOW' }],
  ['full-empty-custom', { customInstructions: '' }],
  ['full-multi', { planCount: 3, exploreCount: 5 }],
  ['sparse-default', { reminderType: 'sparse' }],
  ['sparse-custom', { reminderType: 'sparse', customInstructions: 'CUSTOM_PLAN_WORKFLOW' }],
  ['sparse-empty-custom', { reminderType: 'sparse', customInstructions: '' }],
  ['subagent-absent', { isSubAgent: true }], ['subagent-exists', { isSubAgent: true, planExists: true }],
]) {
  state.agents = options.agents ?? true;
  state.outputStyle = options.outputStyle ?? 'default';
  state.env.CLAUDE_CODE_PLAN_V2_AGENT_COUNT = options.planCount;
  state.env.CLAUDE_CODE_PLAN_V2_EXPLORE_AGENT_COUNT = options.exploreCount;
  const input = { planFilePath: '/tmp/plan.md', planExists: false, ...options };
  const body = api.reminder(input).map(value => value.content).join('\n');
  reminderCases.push({ name, input, body, bytes: Buffer.byteLength(body), sha256: hash(body) });
}
state.outputStyle = 'default'; state.agents = true;
state.env.CLAUDE_CODE_PLAN_V2_AGENT_COUNT = undefined;
state.env.CLAUDE_CODE_PLAN_V2_EXPLORE_AGENT_COUNT = undefined;
const exitCases = [];
for (const [name, input, style] of [
  ['empty', { plan: null, isAgent: false }],
  ['blank', { plan: ' \t\n', isAgent: false }],
  ['bom', { plan: '\ufeff', isAgent: false }],
  ['nel', { plan: '\u0085', isAgent: false }],
  ['normal', { plan: 'PLAN_BODY', isAgent: false, filePath: '/tmp/plan.md', hasTaskTool: true }],
  ['no-agent-tool', { plan: 'PLAN_BODY', isAgent: false, filePath: '/tmp/plan.md', hasTaskTool: false }],
  ['custom-style', { plan: 'PLAN_BODY', isAgent: false, filePath: '/tmp/plan.md', hasTaskTool: true }, 'custom'],
  ['edited', { plan: 'EDITED_BODY', isAgent: false, filePath: '/tmp/plan.md', hasTaskTool: true, planWasEdited: true }],
  ['agent', { plan: 'PLAN_BODY', isAgent: true, filePath: '/tmp/plan.md' }],
  ['utf16', { plan: 'A\ud800B', isAgent: false, filePath: '/tmp/plan.md', hasTaskTool: true }],
]) {
  state.outputStyle = style ?? 'default';
  const body = api.exit.mapToolResultToToolResultBlockParam(input, 'tool_probe').content;
  exitCases.push({ name, input, style: state.outputStyle, body, units: Array.from({ length: body.length }, (_, i) => body.charCodeAt(i)), bytes: Buffer.byteLength(body), sha256: hash(body) });
}
state.outputStyle = 'default';
const prompts = { enter: await api.enter.prompt(), exit: await api.exit.prompt() };
state.hybridSearch = true;
prompts.enterHybridSearch = await api.enter.prompt();
state.hybridSearch = false;
state.inline = true;
prompts.exitInline = await api.exit.prompt();
state.docs = true;
prompts.exitDoc = await api.exit.prompt();
state.docs = false; state.inline = false;
const enterPrompts = [];
for (const outputStyleIsDefault of [true, false]) for (const hybridSearch of [true, false]) {
  state.outputStyle = outputStyleIsDefault ? 'default' : 'custom';
  state.hybridSearch = hybridSearch;
  enterPrompts.push({ outputStyleIsDefault, hybridSearch, expected: await api.enter.prompt() });
}
state.outputStyle = 'default'; state.hybridSearch = false;
const enterConfirmation = (await api.enter.create({ permissions: () => ({ mode: 'default' }), setToolPermissionContext: () => {} }).call({})).data.message;
const enterResult = api.enter.mapToolResultToToolResultBlockParam({ message: 'Entered plan mode. You should now focus on exploring the codebase and designing an implementation approach.' }, 'tool_probe').content;
const validations = [];
for (const [name, inline, input, mode, agentId] of [
  ['file-empty', false, {}, 'plan'], ['file-outside', false, {}, 'default'],
  ['inline-empty', true, {}, 'plan'], ['inline-whitespace', true, { plan: ' \n' }, 'plan'],
  ['inline-extra-key', true, { plan: 'PLAN_BODY', other: true }, 'plan'],
  ['inline-valid', true, { plan: 'PLAN_BODY' }, 'plan'],
  ['inline-oversized', true, { plan: 'a'.repeat(32768) }, 'plan'],
  ['inline-child', true, { plan: 'PLAN_BODY' }, 'plan', 'child'],
]) {
  state.inline = inline;
  const host = { agentContext: {}, agentId, permissions: () => ({ mode }), mainLoopModel: () => 'probe', tools: [] };
  validations.push({ name, inline, result: await api.exit.create(host).validateInput(input) });
}
state.inline = false;
const counts = [];
for (const [tier, profile] of [['none','none'], ['max','default_claude_max_20x'], ['team','none'], ['enterprise','none']]) {
  state.tier = tier; state.profile = profile;
  counts.push({ tier, profile, ...api.counts() });
}
const metadata = Object.fromEntries(['enter','exit'].map(name => [name, {
  maxResultSizeChars: api[name].maxResultSizeChars,
  isConcurrencySafe: api[name].isConcurrencySafe(),
  isReadOnly: api[name].isReadOnly(),
  shouldDefer: api[name].shouldDefer,
  requiresUserInteraction: api[name].requiresUserInteraction?.() ?? false,
  description: name === 'enter' ? 'Requests permission to enter plan mode for complex tasks requiring exploration and design' : 'Prompts the user to exit plan mode and start coding',
}]));
state.inProcessTeammate = true;
metadata.exit.requiresUserInteractionAsTeammate = api.exit.requiresUserInteraction();
state.inProcessTeammate = false;
const captureBytes = fs.readFileSync(process.argv[3]);
const capture = JSON.parse(captureBytes);
const inputSchemas = Object.fromEntries(['EnterPlanMode', 'ExitPlanMode'].map(name => {
  const tool = capture.tools.find(tool => tool.name === name);
  if (!tool) throw Error(`Native capture lacks ${name}`);
  return [name, tool.input_schema];
}));
const result = {
  version: '2.1.319',
  provenance: {
    binarySha256: manifest.sha256,
    binaryBytes: binary.length,
    ranges: manifest.ranges,
    captureSha256: hash(captureBytes),
    scope: 'Text and pure metadata from hash-pinned native functions with explicit imported host predicates. Input schemas from a separately captured unmodified-executable request. No application bootstrap, real provider, account or filesystem function is evaluated.',
  },
  inputSchemas,
  enterPrompts,
  exitPrompt: prompts.exit,
  enterResult,
  enterConfirmation,
  metadata,
  reminders: reminderCases.map(({ name, input, body }) => ({ name, input, expected: body })),
  exitResults: exitCases.map(({ name, input: { plan, ...input }, style, units }) => ({ name, input: { ...input, planUnits: plan == null ? null : Array.from({length: plan.length}, (_, i) => plan.charCodeAt(i)) }, outputStyleIsDefault: style === 'default', expectedUnits: units })),
};
const target = path.join(root, 'fixtures/claude_2_1_319_plan.json');
const output = JSON.stringify(result, null, 2) + '\n';
if (process.argv.includes('--check')) {
  if (fs.readFileSync(target, 'utf8') !== output) throw Error('Claude Code 2.1.319 plan fixture differs');
  console.log('Claude Code 2.1.319 plan byte oracle matches ' + manifest.sha256);
} else {
  fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.writeFileSync(target, output);
  console.log(target);
}

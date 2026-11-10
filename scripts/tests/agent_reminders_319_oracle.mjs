import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import {fileURLToPath} from 'node:url';

const binaryPath = process.argv[2];
assert(binaryPath, 'Usage: node agent_reminders_319_oracle.mjs <official 2.1.319 binary> [--check]');
const binary = fs.readFileSync(binaryPath);
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const artifactSha256 = '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd';
assert.equal(hash(binary), artifactSha256, 'Use the pinned official darwin-arm64 artifact');
const ranges = [];
function take(label, first, after, from = 269336381) {
  const start = binary.indexOf(first, from);
  const endExclusive = binary.indexOf(after, start + first.length);
  assert(start >= from && endExclusive > start && endExclusive - start < 30000, label);
  const bytes = binary.subarray(start, endExclusive);
  ranges.push({label, start, endExclusive, sha256: hash(bytes)});
  return bytes.toString('utf8');
}

const state = {rewrite: false, posix: true, hybrid: false};
const context = vm.createContext({
  state,
  Buffer, TextDecoder, atob,
  Ll: () => state.posix,
  wx: () => state.hybrid,
  Me: 'Bash', Et: 'PowerShell', Zr: 'Glob', Sr: 'Grep', Ge: 'Read', Xt: 'Write',
  Vn: 'WebFetch', ft: 'Agent', Rn: 'SendMessage', mw: 'web-fetch',
  hwt: 'fetched-web-content', Jpe: 'tool-results', Hon: () => false,
  St: (...parts) => path.posix.join(...parts),
  F_e: () => state.rewrite,
  G_n: () => null,
  W_n: () => null,
  cje: text => text,
  Vho: 'Never push to main/master, force-push, or merge.',
  i: {}, k: (_key, fallback) => fallback,
  YEr: 5000000,
  xa: () => ({claim: () => false}), a: () => {},
  BS: row => row.virtual === true,
  _Ie: content => Array.isArray(content) && content.some(block => block.type === 'tool_result'),
  a9: () => false, MY: () => false,
  If: '(no content)', EG: 'No response requested.',
  cs: 'AskUserQuestion', z0n: 'Brief', ezr: 'BriefLegacy', Kg: 'ExitPlanMode', Su: 'SendUserFile',
  Er: () => state.cache,
  Bd: () => state.clientData,
  g: () => {},
  mu: messages => messages,
  Ie: message => message,
  nf: filename => filename,
  wce: snippet => snippet,
  as: 'ToolSearch',
  tf: 'command-name',
  _5: () => state.rewrite,
  Da: () => state.coordinator,
});
const sources = [
  take('general-purpose builder IRr', 'function IRr(){', 'var v3='),
  take('Explore builder ORo', 'function ORo(){', 'var MDt='),
  take('Plan builder NRo', 'function NRo(){', 'var gge='),
  `var ${take('WebFetch reporting constants', 'Who=', 'export{Vn', 264526000)}`,
  take('web-fetch system builder NRr', 'function NRr(){return', 'var SPt='),
  take('web-fetch agent definition SPt', 'var SPt=', 'function i8e('),
  `var ${take('rewritten subagent and background templates', 'HWi=', 'var GWi=', 264468400)}`,
  take('subagent appendix builder QAn', 'async function QAn(', 'function WCr('),
  take('background session builder GCr', 'function GCr(', 'var zCr='),
  take('total-token formatter NAn', 'function NAn(', 'var k_n='),
  take('silent-turn text and overrides', 'var B$n=', 'function G$n('),
  take('silent-turn thresholds', 'function z$n(', 'var Yat='),
  take('narration signature parser', 'var zMr=new TextDecoder(', 'function wwi('),
  take('turn companion predicate', 'function U3(e){return e.type==="user"', 'function ', 0),
  take('silent-turn scan and producer', 'function uzr(', 'function mzr('),
  take('task notification provenance headers', 'var f2e=', 'function iDi(', 268410340),
  `var changedFile=${take('changed-file attachment renderer', 'edited_text_file:(e)=>', ',at_mention_reference:').slice('edited_text_file:'.length)};`,
  `var audienceNote=${take('Bash output audience renderer', 'bash_output_audience_note:()=>', ',unknown_command_fallback:').slice('bash_output_audience_note:'.length)};`,
  `function toolSearchReminder(e){switch(e.type){${take('tool-search usage renderer', 'case"tool_search_usage_reminder":', 'case"relevant_memories":')}}}`,
  `var ${take('rewritten Skill tool template', 'o2i=', ',s2i=', 264480900)};`,
  take('Skill tool default template and selector Xwi', 'var rRr=', 'async function Q_n('),
];
for (const source of sources) vm.runInContext(source, context, {timeout: 1000});
const api = vm.runInContext('({general:IRr, explore:ORo, plan:NRo, webFetch:NRr, webFetchDescription:()=>SPt.whenToUse, reporting:bCr, appendix:QAn, background:GCr, total:NAn, silent:W$n, scan:fzr, producer:pzr, narration:VPe, changedFile, audienceNote, toolSearchReminder, skill:Xwi, notificationHeaders:{normal:DCe,inHumanTurn:Jdo}})', context);

function rebrand(text) {
  return text
    .replaceAll("Claude Code, Anthropic's official CLI for Claude", 'LingXi')
    .replaceAll('Claude Code', 'LingXi')
    .replaceAll('CLAUDE.md', 'LINGXI.md')
    .replaceAll('.claude/', '.lingxi/')
    .replaceAll('$CLAUDE_JOB_DIR', '$LINGXI_JOB_DIR');
}
function portableShell(text) {
  return text
    .replaceAll('Use Bash ONLY', 'Use the registered shell tool ONLY')
    .replaceAll('use Bash for:', 'use the registered shell tool for:')
    .replaceAll('between bash calls', 'between shell tool calls');
}
const agents = {general: rebrand(api.general()), explore: [], plan: []};
const webFetchNative = rebrand(api.webFetch());
const webFetchAdapted = webFetchNative
  .replace(/- The result for a fetched page opens with a "Fetched" line[^\n]*\n(?: [^\n]*\n){4}/, `- Reporting rules for fetched web content:\n${api.reporting}\n`)
  .replace(/- The "Fetched" line also says when a long page was cut\.[^\n]*\n/, '- If a tool result is truncated, say which part of the page is unknown to you; do not infer the omitted content.\n')
  .replace(/- When WebFetch says that a page came from a stored copy[^\n]*\n/, '');
assert(!webFetchAdapted.includes('"Fetched" line'), 'No unsupported native source-header claims');
agents.webFetch = {native: webFetchNative, body: webFetchAdapted, description: rebrand(api.webFetchDescription())};
for (const [posix, hybrid] of [[true, false], [true, true], [false, false]]) {
  Object.assign(state, {posix, hybrid});
  agents.explore.push({posix, hybrid, body: portableShell(rebrand(api.explore()))});
  agents.plan.push({posix, hybrid, body: portableShell(rebrand(api.plan()))});
}
const subagentAppendices = [];
const background = [];
for (const rewrite of [false, true]) {
  state.rewrite = rewrite;
  subagentAppendices.push({rewrite, parts: (await api.appendix([], 'claude-opus-5-5')).map(text => portableShell(rebrand(text)))});
  for (const isolation of ['in-place', 'worktree-first', 'worktree-before-edits']) {
    const input = {dir: '/jobs/example', tmpDir: '/jobs/example/tmp', isolation};
    background.push({rewrite, isolation, body: rebrand(api.background(input, rewrite))});
  }
  assert.equal(api.background(null, rewrite), null);
}
const totalTokens = ['infinite', 'fixed', 'countdown', 'padded-countdown'].flatMap(mode =>
  [-10, 0, 42, 15000000].map(remaining => ({mode, remaining, body: api.total(mode, remaining)})));
const bytesField = (field, value) => Buffer.concat([Buffer.from([field * 8 + 2, value.length]), value]);
const makeSignature = label => bytesField(2, bytesField(1, bytesField(8, Buffer.from(label)))).toString('base64');
const paddedSignature = Buffer.concat([Buffer.from([8, 1]), Buffer.from(makeSignature('narration'), 'base64')]).toString('base64');
const narrationSignatures = [
  makeSignature('narration'), makeSignature('summary'), '', 'not base64!', 'AA==',
  makeSignature('narration').replaceAll('=', ''),
  `${makeSignature('narration')}\n`,
  paddedSignature,
  paddedSignature.replaceAll('=', ''),
  `${paddedSignature}=`,
  `${paddedSignature}===`,
  `${makeSignature('narration')}=`,
  `${makeSignature('narration')}\v`,
  Buffer.concat([Buffer.from([8, ...Array(9).fill(0xff), 0x7f]), Buffer.from(makeSignature('narration'), 'base64')]).toString('base64'),
  Buffer.concat([bytesField(2, bytesField(1, bytesField(8, Buffer.from('narration')))), Buffer.from([0x80])]).toString('base64'),
].map(signature => ({signature, narration: api.narration({type: 'thinking', signature})}));
const atTime = seconds => new Date(Date.UTC(2026, 0, 1) + seconds * 1000).toISOString();
const user = {type: 'user', isMeta: false, timestamp: atTime(0), message: {content: 'task'}};
function assistant(id, second, content) { return {type: 'assistant', timestamp: atTime(second), message: {id, content}}; }
const silentAssistant = (id, second) => assistant(id, second, [{type: 'tool_use', name: 'Bash'}]);
const result = second => ({type: 'user', isMeta: false, timestamp: atTime(second), message: {content: [{type: 'tool_result'}]}});
const histories = [
  ['below-turn-threshold', [user, silentAssistant('1', 1), result(2)], {}, 10],
  ['five-silent-turns', [user, ...Array.from({length: 5}, (_, index) => [silentAssistant(String(index), index * 2 + 1), result(index * 2 + 2)]).flat()], {}, 20],
  ['time-threshold', [user, silentAssistant('1', 1), result(2)], {CLAUDE_CODE_SILENT_TURN_REMINDER_SECONDS: 5}, 6],
  ['time-companion-does-not-reset-clock', [user, silentAssistant('1', 1), {type: 'user', isMeta: true, turnCompanion: true, timestamp: atTime(9), message: {content: 'limit notice'}}, result(9.5)], {CLAUDE_CODE_SILENT_TURN_REMINDER_SECONDS: 5}, 10],
  ['time-ordinary-meta-resets-clock', [user, silentAssistant('1', 1), {type: 'user', isMeta: true, timestamp: atTime(9), message: {content: 'new instructions'}}, result(9.5)], {CLAUDE_CODE_SILENT_TURN_REMINDER_SECONDS: 5}, 10],
  ['time-too-short', [user, silentAssistant('1', 1), result(2)], {CLAUDE_CODE_SILENT_TURN_REMINDER_SECONDS: 5}, 4],
  ['time-mode-after-three-reminders', [user, ...Array.from({length: 3}, (_, index) => ({type: 'attachment', timestamp: atTime(index + 1), attachment: {type: 'silent_turn_reminder'}})), silentAssistant('1', 5), result(6)], {CLAUDE_CODE_SILENT_TURN_REMINDER_SECONDS: 5}, 10],
  ['narration-is-speaking', [user, assistant('1', 1, [{type: 'thinking', thinking: 'Working on it', signature: makeSignature('narration')}]), result(2), silentAssistant('2', 3), result(4)], {}, 10],
  ['ordinary-thinking-is-silent', [user, assistant('1', 1, [{type: 'thinking', thinking: 'internal', signature: makeSignature('summary')}]), result(2), silentAssistant('2', 3), result(4)], {}, 10],
];
const silentCases = histories.map(([name, messages, environment, nowSeconds]) => {
  context.i = environment;
  state.cache = {};
  state.clientData = {};
  const nowMs = Date.parse(atTime(nowSeconds));
  return {name, messages, environment, nowMs, stretch: api.scan(messages), attachments: api.producer(messages, 'claude-opus-5-5', nowMs)};
});
context.i = {};
const changedFiles = ['', '    1\tlet value = 2;'].map(snippet => {
  const input = {filename: '/tmp/example.rs', snippet};
  return {...input, body: api.changedFile(input)[0].content};
});
const toolSearchReminders = [[[], 0], [['mcp__example__find'], 1], [['mcp__example__find'], 3]].map(([names, count]) => {
  const messages = api.toolSearchReminder({type:'tool_search_usage_reminder', undiscoveredToolNames:names, undiscoveredCount:count});
  return {names, count, body: messages[0]?.content ?? null};
});
const skill = [];
for (const rewrite of [false, true]) {
  for (const coordinator of [false, true]) {
    Object.assign(state, {rewrite, coordinator});
    skill.push({rewrite, coordinator, body: rebrand(await api.skill())});
  }
}
const output = {
  version: '2.1.319', artifactSha256, ranges,
  normalization: [
    'Claude Code product name and CLAUDE.md/.claude/$CLAUDE_JOB_DIR use the LingXi equivalents.',
    'Builtin read-only prompts and subagent notes abstract Bash to the registered shell tool.',
    'WebFetch has no Native Fetched/source-approval/summary-pagination/stored-copy-date header: apply the upstream reporting limits to all fetched content, describe truncation plainly, and omit the unsupported stored-copy-date clause.',
  ],
  agents, subagentAppendices, background, totalTokens,
  silentTurn: {body: api.silent(), turns: vm.runInContext('B$n', context), maxReminders: vm.runInContext('j$n', context)},
  narrationSignatures, silentCases,
  changedFiles, toolSearchReminders,
  bashAudienceNote: api.audienceNote()[0].content,
  notificationHeaders: api.notificationHeaders,
  skill,
};
const fixture = path.join(path.dirname(fileURLToPath(import.meta.url)), 'fixtures/agent_reminders_319.json');
const serialized = `${JSON.stringify(output, null, 2)}\n`;
if (process.argv.includes('--check')) {
  assert.equal(fs.readFileSync(fixture, 'utf8'), serialized, 'The artifact-derived fixture changed');
  process.stdout.write(`Claude Code 2.1.319 agent/reminder oracle matches ${ranges.length} source ranges\n`);
} else {
  fs.writeFileSync(fixture, serialized);
  process.stdout.write(`Wrote ${fixture}\n`);
}

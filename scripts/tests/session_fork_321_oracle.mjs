#!/usr/bin/env node
// Execute hash-pinned native helpers with declared in-memory storage/host inputs.
// Only inputs, output bytes, and source range receipts are saved, never source.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import vm from 'node:vm';

const args = process.argv.slice(2);
assert(args.every(arg => !arg.startsWith('--') || arg === '--check'));
assert(args.filter(arg => !arg.startsWith('--')).length <= 1);
const binaryPath = args.find(arg => !arg.startsWith('--'))
  ?? path.join(homedir(), '.local/share/claude/versions/2.1.321');
const binary = readFileSync(binaryPath);
const sha256 = value => createHash('sha256').update(value).digest('hex');
const nativeSha256 = sha256(binary);
assert.equal(nativeSha256, 'abc9feeeca8091d51b623291e044cf0528c2f6b74e0c1d6ef671d52c73f7cd3c');
const sourceByteRanges = [];
function take(name, start, end, digest) {
  const bytes = binary.subarray(start, end);
  assert(bytes.length > 0 && bytes.length < 5000, `${name}: unbounded source range`);
  assert.equal(sha256(bytes), digest, `${name}: source range changed`);
  sourceByteRanges.push({ name, start, end, sha256: digest });
  return bytes.toString('utf8');
}
const source = {
  plainObject: take('plain object guard', 260439850, 260439920, '8a1ce3a61625f107943e485ff228f00c854ca71114a4b5ef6f9679392d78c7f7'),
  regexEscape: take('regex escape', 260453489, 260453551, 'c90074aa97369fa008d61f3c9a75424975f6bc4282f03ce852652405b6051d54'),
  safeSlice: take('first-prompt UTF-16 safe slice', 260453939, 260454082, '1381d416ee3a994ca70b86ec4281bdf4e1181e3cd6208848bf8751d5d98c1609'),
  sliceBuffer: take('UTF-16 slice carrier', 260454353, 260454454, '56c95c26fd8f72bd0880ba9adc916724e9471ff7cd4a28c4362e751094096dab'),
  pasted: take('pasted-content display reader', 260791428, 260792564, '134bf98fa32e938a60a665bf7e6ac67723f3dcb5be98e1b28b9326bbce17b7cd'),
  plugin: take('shown plugin prompt', 261795679, 261796369, 'd40d3b20488f792a3174ba626666d9f844132326076ed3953b878b1a173bc023'),
  promptRegex: take('first-prompt regexes', 261796369, 261796483, '061464ce02f3edb3592c7404f3960aa3b544999bd6dd320a573fb3ec68a6cebc'),
  firstPrompt: take('first meaningful shown prompt', 261796483, 261797234, 'e77742f984e832ac45ab0bf2de1592c57eb2795ba26c20818bf1a444a856dd81'),
  storedFirstPrompt: take('stored first-prompt caller', 261803826, 261804003, '4282f9672cdd9e4ff8fcd950202644d183c628d821e4f95818b52d22dc851b20'),
  suppress: take('fork suppression stamp', 261822396, 261822534, 'ec09c019db7d23b3711df381ffdf0a691d3e9b28b2b418dc66df67873bac6e6b'),
  transcript: take('transcript row guard', 268542034, 268542163, '9ac81c82c60305379eacc932307a441a062044f964a07fcc6f38ae0fd1823da3'),
  titleSaver: take('custom-title saver', 271380505, 271380921, 'f1e1ab5e0f1d80da0c93cfed86534cf7bb6d71ab6c102684adf4a2f91363334a'),
  nameSaver: take('agent-name saver', 271396093, 271396460, 'c70b9cc9dc9ad1092b04a96a175688b589147df229aee4e81deba184719f816c'),
  catalogScope: take('native repository-scoped title search caller', 271404314, 271404845, '26a21f0d675b0360bc7543acc0097f27e5630d8d437f1ecf06b7c909cd0c7363'),
  deriveName: take('fork name', 286399576, 286399781, '01f3c043f5f23d54e194ef761589a34936d4b606a533220448acda94e504ca11'),
  storageFork: take('storage fork', 286402691, 286405560, 'b08072301632d360ee344a8e23f2d83b022a967766583da10b825f811adc4fcc'),
  uniqueName: take('unique automatic fork name', 286405560, 286405916, '89f85eab8401e0fd03a51c3ec703ded2b53b6e3ac0b00ac332ba26f22bd38b3a'),
  commandFork: take('fork command and metadata', 286405916, 286407412, 'e9c6dae0b42ea7ad24018451503bfc2feec726c33a1d035ae9f7e04bcfdbd096'),
  restoreRepl: take('restore-valid REPL mode and typed events', 271434018, 271434319, '82a3f2dd7fd686092c2c749f628b8826ab05b32d1c991f8954bde39216d265cb'),
  restoreTools: take('restore-valid tools registry', 271435617, 271435764, '9fcc8e04aaba30dfb7d7b01ab8b5795f370564a063c42e9a7089d14fc12197f0'),
};

const units = value => Array.from({ length: value.length }, (_, index) => value.charCodeAt(index));
const SOURCE = '11111111-1111-4111-8111-111111111111';
const TARGET = '22222222-2222-4222-8222-222222222222';
const cwd = '/native-fixture';
const stamp = '2026-11-09T12:34:56.000Z';
const user = (content, extra = {}) => ({ type: 'user', uuid: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa', parentUuid: null, sessionId: SOURCE, timestamp: stamp, cwd, version: '2.1.321', message: { role: 'user', content }, ...extra });
const pluginText = 'Read <command-name>/review</command-name> now';
const plugin = `The demo plugin sent a message:\n${pluginText}\n\nThis is how Claude Code surfaces a prompt a plugin submits between turns — it starts this turn in the user's place. Address the message above.`;
const promptInputs = [
  ['empty', []], ['ordinary', [user('hello world')]],
  ['emoji-utf16-cap', [user('😀'.repeat(60))]],
  ['stored-title-utf16-cap', [user('😀'.repeat(120))]],
  ['stored-title-safe-surrogate-cap', [user('a'.repeat(199) + '😀 tail')]],
  ['split-surrogate-cap', [user('a'.repeat(99) + '😀 tail')]],
  ['raw-surrogates', [user('prefix \ud83d and \ude00')]],
  ['nested-text-array', [user([{ type: 'image' }, { type: 'text', text: 'chosen' }, { type: 'text', text: 'later' }])]],
  ['tool-result-aborts-message', [user([{ type: 'text', text: 'ignored' }, { type: 'tool_result' }]), user('next')]],
  ['commands-first-fallback', [user('<command-name>/one</command-name>'), user('<command-name>/two</command-name>')]],
  ['empty-command-is-skipped', [user('<command-name></command-name>'), user('next')]],
  ['command-unicode-line-separator', [user('<command-name>/one\u2028two</command-name>'), user('next')]],
  ['bash', [user('<bash-input>\ufeffecho hi\ufeff</bash-input>')]],
  ['meta-skipped', [user('ignored', { isMeta: true }), user('shown')]],
  ['nonboolean-meta-is-visible', [user('shown', { isMeta: 'true' })]],
  ['compact-skipped', [user('ignored', { isCompactSummary: true }), user('shown')]],
  ['xml-ascii-word', [user('<context-kind>hidden</context-kind>'), user('shown')]],
  ['xml-unicode-word-is-visible', [user('<café>visible</café>')]],
  ['interrupt-skipped', [user('[Request interrupted by user for tool use]'), user('shown')]],
  ['pasted-block', [user('<pasted_content id="abcd">\nhello there\n</pasted_content id="abcd">\n')]],
  ['pasted-inline', [user('before\n\n<pasted_content id="abcd">\ninside\n</pasted_content id="abcd">\n\nafter')]],
  ['pasted-empty', [user('<pasted_content id="abcd">\n</pasted_content id="abcd">\n')]],
  ['pasted-invalid-id', [user('<pasted_content id="ABCD">\ninside\n</pasted_content id="ABCD">\n'), user('shown')]],
  ['plugin-shown', [user(plugin, { pluginPrompt: { text: pluginText } })]],
  ['plugin-forged-metadata', [user(plugin, { pluginPrompt: { text: 'different' } }), user('shown')]],
];
for (const char of ['\t', '\n', '\r', '\v', '\f', ' ', '\u00a0', '\u1680', '\u2000', '\u200a', '\u2028', '\u2029', '\u202f', '\u205f', '\u3000', '\ufeff', '\u0085', '\u001c', '\u180e']) {
  promptInputs.push([`whitespace-${char.charCodeAt(0).toString(16)}`, [user(`${char}left${char}${char}right${char}`)]]);
}
const nameContext = vm.createContext({ Buffer });
vm.runInContext(source.safeSlice + source.sliceBuffer + source.pasted + source.plugin + source.promptRegex + source.firstPrompt + source.storedFirstPrompt + source.deriveName, nameContext);
const nameCases = promptInputs.map(([name, messages]) => {
  nameContext.INPUT = messages;
  return { name, messagesJson: JSON.stringify(messages), expectedUnits: units(vm.runInContext('Xha(INPUT)', nameContext)), expectedStoredTitleUnits: units(vm.runInContext('VIa(INPUT)||"(session)"', nameContext)) };
});
const truncateCases = ['  text\n here\ufeff', '\u0085text\u0085', '\ufefftext\ufeff', '😀'.repeat(120), 'a'.repeat(199) + '😀 tail', 'a'.repeat(199) + '  tail'].map(text => {
  nameContext.TEXT = text;
  return { textJson: JSON.stringify(text), expectedUnits: units(vm.runInContext('(()=>{const text=TEXT.replaceAll("\\n"," ").trim();return text.length>200?Q(text,200).trim()+"…":text})()', nameContext)) };
});

const baseMessage = user('fix the login bug', { sessionKind: 'daemon-worker' });
const second = { ...user('second'), uuid: 'bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb', parentUuid: baseMessage.uuid };
const refusal = { type: 'system', subtype: 'model_refusal_fallback', uuid: 'cccccccc-cccc-4ccc-8ccc-cccccccccccc', parentUuid: second.uuid, sessionId: SOURCE, timestamp: stamp, cwd, version: '2.1.321', neutralizedByFork: false };
const forkInputs = [
  ['basic', { records: [baseMessage, second], selected: [baseMessage, second] }],
  ['custom-title', { records: [baseMessage], selected: [baseMessage], customTitle: '  Named\t branch\ufeff ' }],
  ['custom-title-empty', { records: [baseMessage], selected: [baseMessage], customTitle: '' }],
  ['custom-title-feff-only', { records: [baseMessage], selected: [baseMessage], customTitle: '\ufeff\ufeff' }],
  ['custom-title-u0085', { records: [baseMessage], selected: [baseMessage], customTitle: '\u0085Named\u0085' }],
  ['unicode-derived', { records: [user('a'.repeat(99) + '😀')], selected: [baseMessage] }],
  ['refusal-neutralized', { records: [baseMessage, refusal], selected: [baseMessage, refusal] }],
  ['last-row-wins', { records: [baseMessage, { ...baseMessage, message: { role: 'user', content: 'last version' } }], selected: [baseMessage] }],
  ['later-sidechain-keeps-earlier-main', { records: [baseMessage, { ...baseMessage, isSidechain: true, message: { role: 'user', content: 'sidechain replacement' } }, second], selected: [baseMessage, second] }],
  ['newer-skill-epoch-with-null-sidechain', { records: [baseMessage, { ...baseMessage, isSidechain: null, entrypoint: 3, parentUuid: 3, origin: { type: 'skill_activation', context_epoch: 17, skill_name: 'history' }, message: { role: 'user', content: 'latest selected prompt' } }, second], selected: [baseMessage, second] }],
  ['newer-skill-epoch-with-zero-sidechain', { records: [baseMessage, { ...baseMessage, isSidechain: 0, entrypoint: 3, parentUuid: 3, origin: { type: 'skill_activation', context_epoch: 19, skill_name: 'history' }, message: { role: 'user', content: 'latest selected prompt' } }, second], selected: [baseMessage, second] }],
  ['sidechain-and-progress-ignored', { records: [baseMessage, { ...second, isSidechain: true }, { ...second, type: 'progress', uuid: 'dddddddd-dddd-4ddd-8ddd-dddddddddddd' }], selected: [baseMessage, second, { uuid: 'dddddddd-dddd-4ddd-8ddd-dddddddddddd' }] }],
  ['source-sidecars', { records: [baseMessage,
    { type: 'content-replacement', sessionId: SOURCE, replacements: [null, true, 3, 'text', [], { text: '\ud83d' }] },
    { type: 'content-replacement', agentId: 'agent', replacements: [{ text: 'wrong source' }] },
    { type: 'memory-mode', sessionId: SOURCE, mode: 'one', sequence: 1 },
    { type: 'memory-mode', sessionId: SOURCE, mode: 'two', sequence: 2 },
    { type: 'memory-mode', sessionId: TARGET, mode: 'foreign' },
    { type: 'relocated', sessionId: SOURCE, relocatedCwd: '/moved' },
    { type: 'atis-latch', sessionId: SOURCE, atis: 'latch' },
    { type: 'tools-used', sessionId: SOURCE, tools: ['Read', 'Bash'] },
    { type: 'repl-mode', sessionId: SOURCE, replMode: true, typedEvents: true },
  ], selected: [baseMessage], atis: 'latch' }],
  ['restore-valid-sidecars', { records: [baseMessage,
    { type: 'repl-mode', sessionId: SOURCE, replMode: true, typedEvents: true },
    { type: 'repl-mode', sessionId: SOURCE, replMode: { enabled: false }, typedEvents: false },
    { type: 'tools-used', sessionId: SOURCE, tools: ['Read', '', 4, null, ' ', 'Bash'] },
    { type: 'tools-used', sessionId: SOURCE, tools: 'invalid-later-value' },
  ], selected: [baseMessage] }],
  ['missing-version', { records: [{ ...baseMessage, version: undefined }], selected: [baseMessage] }],
  ['null-version', { records: [{ ...baseMessage, version: null }], selected: [baseMessage] }],
  ['null-timestamp', { records: [{ ...baseMessage, timestamp: null }], selected: [baseMessage] }],
  ['missing-timestamp', { records: [{ ...baseMessage, timestamp: undefined }], selected: [baseMessage] }],
  ['null-cwd', { records: [{ ...baseMessage, cwd: null }], selected: [baseMessage] }],
  ['missing-cwd', { records: [{ ...baseMessage, cwd: undefined }], selected: [baseMessage] }],
  ['missing-parent', { records: [{ ...baseMessage, parentUuid: undefined }], selected: [baseMessage] }],
  ['null-parent', { records: [{ ...baseMessage, parentUuid: null }], selected: [baseMessage] }],
];
for (const [name, catalog] of [
  ['collision-plain', [{ customTitle: 'fix the login bug (Branch)' }]],
  ['collision-gap', [1, 2, 4].map(n => ({ customTitle: `fix the login bug (Branch${n === 1 ? '' : ` ${n}`})` }))],
  ['collision-case-sensitive-number', [{ customTitle: 'FIX THE LOGIN BUG (BRANCH)' }, { customTitle: 'FIX THE LOGIN BUG (BRANCH 2)' }]],
  ['collision-ai-title', [{ aiTitle: 'fix the login bug (Branch)' }]],
  ['collision-summary-ignored', [{ summary: 'fix the login bug (Branch)' }]],
  ['collision-trim-query', [{ customTitle: '  fix the login bug (Branch)\ufeff' }]],
  ['collision-leading-zero', [{ customTitle: 'fix the login bug (Branch)' }, { customTitle: 'fix the login bug (Branch 02)' }]],
]) forkInputs.push([name, { records: [baseMessage], selected: [baseMessage], catalog }]);
const surrogateBase = 'a'.repeat(99) + '\ud83d';
forkInputs.push(['collision-surrogate-kept-distinct-from-replacement', {
  records: [user('a'.repeat(99) + '😀')], selected: [baseMessage],
  catalog: [{ customTitle: `${'a'.repeat(99)}\ufffd (Branch)` }],
}]);
forkInputs.push(['collision-surrogate-exact', {
  records: [user('a'.repeat(99) + '😀')], selected: [baseMessage],
  catalog: [{ customTitle: `${surrogateBase} (Branch)` }, { customTitle: `${surrogateBase} (Branch 2)` }],
}]);

class FixedDate extends Date { constructor(...args) { super(...args.length ? args : [stamp]); } }
const forkCases = [];
for (const [name, input] of forkInputs) {
  const emitted = [];
  const records = input.records.map(row => JSON.stringify(row));
  const catalog = input.catalog ?? [];
  const restoreContext = vm.createContext({ INPUT: input, SOURCE });
  vm.runInContext('var r={},i$=new WeakMap,PKe=new WeakMap,$t=new Map;for(const $o of INPUT.records){if(false){}' + source.restoreRepl + source.restoreTools + '}', restoreContext);
  const restored = vm.runInContext('({replMode:i$.get(r)?.get(SOURCE),typedEvents:PKe.get(r)?.has(SOURCE)??false,tools:$t.get(SOURCE)??[]})', restoreContext);
  const lookupCalls = [];
  const backend = {
    readRecords: async () => ({ ok: true, value: { items: records.map(data => ({ data: Buffer.from(data) })), nextSeq: undefined } }),
    append: async (_key, rows) => { emitted.push(...rows.map(row => row.data)); return { ok: true }; },
  };
  const host = {
    Buffer, Date: FixedDate, _: JSON.stringify, ee: JSON.parse, INPUT: input, SOURCE, TARGET,
    ne: 100000, te: 4194304, G: () => SOURCE, wy: () => '', Ki: x => x, x5: () => false, due: () => false,
    lde: () => input.atis, Yl: () => ({ names: () => restored.tools }), Gft: () => ({ replMode: restored.replMode }),
    cMn: () => [], K_r: () => {}, l: () => {}, mee: () => false, VJt: () => undefined, H0e: () => 'user',
    Qe: text => text.slice(0, 10), Ha: () => true, f: x => x,
    cO: async (_path, row) => { emitted.push(`${JSON.stringify(row)}\n`); },
    mse: async (query, options) => {
      lookupCalls.push({ queryUnits: units(query), exact: options?.exact ?? false });
      const needle = query.toLowerCase().trim();
      return catalog.filter(row => {
        const title = (row.customTitle ?? row.aiTitle)?.toLowerCase().trim();
        return title && (options?.exact ? title === needle : title.includes(needle));
      });
    },
    Jha: async (selected, customTitle, extraMessages) => forkContext.se({
      sourceV5: { backend, key: 'source' }, forkKey: 'target', forkSessionId: TARGET, originalSessionId: SOURCE,
      currentMessages: selected, customTitle, extraMessages, forkSessionPath: '/native/fork.jsonl',
      sourceTaintAtEntry: false, atisLatchAtEntry: input.atis, toolsUsedAtEntry: restored.tools,
    }),
  };
  const forkContext = vm.createContext(host);
  vm.runInContext(source.plainObject + source.transcript + 'function GI(e){return u_o(e)}' + source.regexEscape + source.suppress + source.storageFork + source.uniqueName + source.titleSaver + source.nameSaver + source.commandFork, forkContext);
  forkContext.se = vm.runInContext('se', forkContext);
  forkContext.Xha = messages => {
    nameContext.INPUT = messages;
    return vm.runInContext('Xha(INPUT)', nameContext);
  };
  const ok = await vm.runInContext('EXr({messages:INPUT.selected,storageV5:{}},()=>{},{customTitle:INPUT.customTitle})', forkContext);
  assert.equal(ok, true, name);
  forkCases.push({ name, sourceJsonl: `${records.join('\n')}\n`, selectedUuids: input.selected.map(row => row.uuid), customTitle: input.customTitle ?? null, catalogJson: JSON.stringify(catalog), restoredRuntime: restored, expectedJsonl: emitted.join(''), lookupCalls });
}

const output = `${JSON.stringify({ schemaVersion: 1, nativeVersion: '2.1.321', nativeSha256, sourceByteRanges, controlledDependencies: ['StorageV5 in-memory record reader and append collector', 'Catalog search results computed from declared title rows', 'Runtime globals for ATIS, ownership and memory stamps; tools and REPL restore execute native handlers', 'Fixed clock and path providers; no native filesystem fork or live resume', 'Display truncate cases wrap native UTF-16 Q with standard newline/trim/ellipsis operations'], sourceSessionId: SOURCE, targetSessionId: TARGET, cwd, nameCases, truncateCases, forkCases }, null, 2)}\n`;
const fixture = new URL('../../crates/session/tests/fixtures/session_fork_2_1_321.json', import.meta.url);
if (args.includes('--check')) assert.equal(readFileSync(fixture, 'utf8'), output, 'Native fork fixture changed');
else writeFileSync(fixture, output);
console.log(`Claude Code 2.1.321 fork oracle: ${nameCases.length} names/stored titles, ${truncateCases.length} display truncations, ${forkCases.length} forks, ${sourceByteRanges.length} verified source ranges`);

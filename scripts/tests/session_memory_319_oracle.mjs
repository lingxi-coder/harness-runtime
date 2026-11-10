#!/usr/bin/env node
// Source-evaluate the current file-memory extractor, independently of the
// retained LingXi session-document extension in session_memory.rs.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import vm from 'node:vm';

assert(process.argv[2], 'Usage: session_memory_319_oracle.mjs <2.1.319 binary> [--check]');
const binary = readFileSync(process.argv[2]);
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const nativeSha256 = '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd';
assert.equal(sha(binary), nativeSha256, 'The pinned native executable changed');
const ranges = [];
function take(first, last, from, label) {
  const start = binary.indexOf(first, from);
  const end = binary.indexOf(last, start + first.length);
  assert(start >= from && end > start, label);
  const bytes = binary.subarray(start, end);
  ranges.push({ label, start, endExclusive: end, sha256: sha(bytes) });
  return bytes.toString('utf8');
}
const context = vm.createContext({
  U: (values, predicate) => values.filter(predicate).length,
  Ll: () => context.posix,
  $en: () => context.skillUpkeep,
  Hq: path => path.startsWith('/memory/') && !path.includes('/../'),
  Me: 'Bash', Et: 'PowerShell', Ge: 'Read', Sr: 'Grep', Zr: 'Glob', ct: 'Edit', Xt: 'Write',
  Uf: 'memory_write', Rk: 'memory_list', yg: 'memory_read', X$t: new Set(['memory_write']),
  lm: 'MEMORY.md', nr: path => path.split('/').at(-1), P: (n, singular, plural) => n === 1 ? singular : plural,
  // Delimiter/BOM vectors stub only the external YAML parser. Its returned
  // marker preserves the exact source bytes passed to that parser.
  il: value => value,
  vHn: yaml => ({ ok: true, value: { yaml } }),
});
const sources = [
  take('function Pe(e)', 'function Tt(e)', 278815000, 'JavaScript whitespace word count'),
  take('function Ve(e)', 'var Ot=3;', 278815000, 'new message count'),
  take('function Nt(e,r,n,o=!1)', 'function Ut(e,r)', 278816000, 'main writes and own-user prose'),
  take('function $t(e,r)', 'function Pr(e,r)', 278818000, 'prose cursor scan'),
  take('function Ze(e)', 'var Tr=64,', 278819000, 'file-write path extraction'),
  take('function Vt(e,r,n,o,s)', 'var Te=null', 278822000, 'file extraction prompt'),
  take('function Qt(e)', 'function Dr(e,r)', 278833000, 'successful-write summary'),
  take('function GJr(e)', 'var WXo=150,', 270728000, 'existing memory inventory renderer'),
  take('function aE(e){return e.charCodeAt(0)', 'function _w(e)', 265395400, 'frontmatter BOM removal'),
  'var ' + take('mv=/^---', ',Gz=', 265481200, 'native frontmatter delimiter regex') + ';',
  take('function td(e,n,r)', 'function vHn(e)', 265481300, 'native frontmatter extraction'),
].join('\n');
vm.runInContext('var vt=3;\n' + sources, context);
const run = expression => vm.runInContext(expression, context, { timeout: 1000 });
const forkOptions = take('let K=await iR({promptMessages:', '}).finally(()=>mr?.())', 278842000, 'file-extraction fork options');
const cadenceSource = take('let Ne=w?.everyNTurns??k(', ';if(S===void 0)', 278837000, 'native cadence default');
const nativeGate = take('if(!I&&!k("tengu_passport_quail"', 'if(rr()!==null)', 278846000, 'file-extraction rollout gate');
const filePass = {
  query_source: forkOptions.match(/querySource:"([^"]+)"/)[1],
  fork_label: forkOptions.match(/forkLabel:"([^"]+)"/)[1],
  max_turns: Number(forkOptions.match(/maxTurns:(\d+)/)[1]),
  default_every_n_turns: Number(cadenceSource.match(/\?\?(\d+),it=/)[1]),
  rollout_flag: nativeGate.match(/k\("([^"]+)",!1\)/)[1],
  rollout_default: false,
};
const headLimits = take('var wTe=30,yse=65536,', 'mv=/', 265481000, 'file inventory head limits');
filePass.inventory_head_lines = Number(headLimits.match(/wTe=(\d+)/)[1]);
filePass.inventory_head_bytes = Number(headLimits.match(/yse=(\d+)/)[1]);
const shellDenied = take('`Only read-only shell commands and ${B?', ',"shell_not_allowed")', 278828000, 'memory shell denial text');
const toolDenied = take('`only ${Ge}, ${Sr}, ${Zr}, read-only ${C}, and ${ct}/${Xt} within ${e} are allowed`', ',"tool_not_allowed")', 278828000, 'memory tool denial text');
const denials = [];
for (const root of ['/memory', '/home/u/.lingxi/project memory']) for (const posix of [true, false]) {
  Object.assign(context, { e: root, B: posix, C: posix ? 'Bash' : 'PowerShell' });
  denials.push({ root, posix, expected_shell: run(shellDenied), expected_tool: run(toolDenied) });
}
const prompts = [];
for (const posix of [true, false]) for (const existing of ['', '- [feedback] test.md (2026-11-09T00:00:00.000Z): Use focused checks']) {
  for (const scope of [false, true]) for (const skillUpkeep of [false, true]) for (const sharedUnavailable of [false, true]) {
    Object.assign(context, { posix, skillUpkeep, existing, scope, sharedUnavailable });
    const messageCount = prompts.length % 3 === 0 ? 0 : prompts.length + 1;
    context.messageCount = messageCount;
    prompts.push({ input: { posix, existing_memory_files: existing, has_scope_guidance: scope,
      project_skill_upkeep: skillUpkeep, shared_memory_tools_unavailable: sharedUnavailable, message_count: messageCount },
      expected: run('Vt(messageCount,existing,scope,sharedUnavailable,false)') });
  }
}
const id = n => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
const user = (n, content, isMeta = false) => ({ type: 'user', uuid: id(n), isMeta, message: { content } });
const assistant = (n, content) => ({ type: 'assistant', uuid: id(n), message: { content } });
const text = value => ({ type: 'text', text: value });
const write = (path, name = 'Write', useId = 'w1') => ({ type: 'tool_use', name, id: useId, input: { file_path: path, content: 'fact' } });
const result = (useId, error) => ({ type: 'tool_result', tool_use_id: useId, content: '', ...(error === undefined ? {} : { is_error: error }) });
const histories = [
  [], [user(1, 'save this preference')], [user(1, 'two words')], [user(1, [text('one two'), text('three')])],
  [user(1, [text('one two three')])], [user(1, 'save this preference', true)],
  [user(1, 'one\uFEFFtwo\uFEFFthree')], [user(1, 'one\u0085two\u0085three')],
  [user(1, 'one\u2028two\u2029three')], [user(1, '一 二 三')],
  [assistant(1, [write('/memory/a.md')])],
  [assistant(1, [write('/outside/a.md')])],
  [assistant(1, [write('/memory/a.md', 'Edit')]), user(2, [result('w1', true)])],
  [assistant(1, [{ type: 'tool_use', id: 'shared', name: 'memory_write', input: {} }]), user(2, [result('shared')])],
  [assistant(1, [{ type: 'tool_use', id: 'shared', name: 'memory_write', input: {} }]), user(2, [result('shared', true)])],
  [user(1, 'save this preference'), { type: 'system', uuid: id(2), message: { content: 'not prose' } }, assistant(3, [text('done')])],
];
const triggers = [];
for (const messages of histories) for (const cursor of [null, id(1), id(99)]) {
  Object.assign(context, { messages, cursor: cursor ?? undefined });
  triggers.push({ messages, cursor, expected: { message_count: run('Ct(messages,cursor)'),
    has_user_prose: run('$t(messages,cursor)'), main_wrote_memory: run('Nt(messages,cursor,false)') } });
}
const summaries = [[], ['/memory/MEMORY.md'], ['/memory/a.md'], ['/memory/MEMORY.md', '/memory/a.md', '/memory/b.md']]
  .map(paths => { context.paths = paths; return { paths, expected: JSON.parse(JSON.stringify(run('Qt(paths)'))) }; });
const inventories = [[], [{ filename: 'a.md', mtimeMs: 0 }], [{ filename: 'user/name.md', mtimeMs: 1794182400123, type: 'user', description: 'Prefers focused checks' }],
  [{ filename: 'a.md', mtimeMs: 1, type: 'feedback', description: '' }, { filename: 'b.md', mtimeMs: 999, description: 'Two lines\nstay literal' }]]
  .map(entries => {
    context.entries = entries;
    return { entries: entries.map(entry => ({ ...entry, iso_timestamp: new Date(entry.mtimeMs).toISOString() })), expected: run('GJr(entries)') };
  });
const frontmatterHeads = [
  '---\nname: a\ndescription: Topic\n---\nBody', '---\nname: a\n---',
  '\uFEFF---\r\nname: a\r\n---  \r\nBody', '--- \n\ndescription: spaced\n---\n',
  '---\ndescription: early---suffix\n---\n', 'body without frontmatter',
  '---\nmissing: delimiter', '---\u0085\nname: not-js-space\n---\n',
  '---\uFEFF\nname: js-space\n---\n',
].map(head => {
  context.head = head;
  return { head, expected_yaml: run('td(head).frontmatterAsWritten.yaml') ?? null };
});
const output = JSON.stringify({ version: '2.1.319', native_sha256: nativeSha256, source_byte_ranges: ranges,
  scope: 'Native local-file extraction prompt, own-user prose/cursor/write-skip helpers, and write summaries. Host policy, tools, file scan, provider dispatch and automatic scheduling need independent production checks. Legacy agents/session-memory is a LingXi extension.',
  file_pass: filePass,
  prompts, triggers, summaries, inventories, frontmatter_heads: frontmatterHeads, denials }, null, 2) + '\n';
const target = new URL('../../crates/memory/tests/fixtures/session_memory_2_1_319.json', import.meta.url);
if (process.argv.includes('--check')) {
  assert.equal(readFileSync(target, 'utf8'), output);
  console.log(`Claude Code 2.1.319 extraction oracle matches: ${prompts.length} prompts, ${triggers.length} triggers, ${summaries.length} summaries, ${inventories.length} inventory renders, ${frontmatterHeads.length} frontmatter delimiters, ${denials.length} denials`);
} else {
  writeFileSync(target, output);
  console.log(`Wrote ${target.pathname}`);
}

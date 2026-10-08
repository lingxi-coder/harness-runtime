#!/usr/bin/env node
// Execute narrow instruction/context helpers extracted from the hash-pinned
// native reference. No reference implementation or full binary is executed.
// Usage: node scripts/tests/instruction_announcements_286_oracle.mjs /path/to/2.1.286 [--check]
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const sha256 = createHash('sha256').update(binary).digest('hex');
if (sha256 !== '75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433') {
  throw new Error(`Unexpected native oracle SHA-256: ${sha256}`);
}
const sourceByteRanges = [];
function chunk(offset) {
  const end = binary.indexOf('// @bun @bytecode\n', offset + 1);
  const contents = binary.subarray(offset, end < 0 ? binary.length : end);
  const exportStart = contents.lastIndexOf('export{');
  const nul = contents.indexOf(0, Math.max(exportStart, 0));
  return contents.subarray(0, nul < 0 ? contents.length : nul);
}
function range(offset, startMarker, endMarker) {
  const bytes = chunk(offset);
  const start = bytes.indexOf(startMarker);
  const end = bytes.indexOf(endMarker, start + startMarker.length);
  if (start < 0 || end < start) throw new Error(`Missing native source: ${startMarker}`);
  sourceByteRanges.push({ signature: startMarker, start: offset + start, end: offset + end });
  return bytes.subarray(start, end).toString('utf8');
}
// Evaluate the reference's pure schema library and lazy schema factory rather
// than approximating payload validation. These modules have no IO side effects.
function pureModule(offset, imports = {}, extraExports = []) {
  const bytes = chunk(offset);
  sourceByteRanges.push({ signature: 'pure helper dependency chunk', start: offset, end: offset + bytes.length });
  const source = bytes.toString('utf8')
    .replace(/import\{[^}]+\}from"[^"]+";/g, '')
    .replace(/export\{([^}]+)\};/, `return {$1${extraExports.length ? ',' + extraExports.join(',') : ''}};`);
  return vm.runInNewContext(`(()=>{${source}})()`, imports, { timeout: 2_000 });
}
const schemaCore = pureModule(176381936);
const schema = pureModule(176393579, schemaCore);
const lazy = pureModule(176310967);
const scrub = pureModule(177550468, {}, ['Au', 'iu']);
const sanitizer = vm.runInNewContext(`(()=>{
${range(176210667, 'var _Je=', 'function p_t(')}
${range(178463177, 'var H;function j()', 'function e6o(')}
${range(178479242, 'function fJ(', 'export{')}
return {fJ,J8e,tags:wQ,channel:E0};})()`, scrub, { timeout: 2_000 });
const helpers = [
  range(183727495, 'var c6=', 'var Ime='),
  range(183727495, 'var HPt=', 'function hx('),
  range(179372957, 'var us=', 'function Ge('),
  range(178667434, 'function as(', 'function KRe('),
  range(178667434, 'function Gp()', 'function Nm('),
  range(183727495, 'var i6e=', 'function BFn('),
  range(191662485, 'function xd(', 'async function Lr('),
  range(191662485, 'async function Jn(', 'function Xn('),
  range(191662485, 'function Xn(', 'async function Gr('),
  range(183727495, 'var Jx=', 'function gon('),
  range(183727495, 'function ol(e){', 'function JZt('),
  range(183727495, 'function Yi(', 'function Afr('),
  range(176210667, 'var kO=', ',EJe=') + ';',
  range(179754105, 'var TKn=', ',PNo=') + ';',
  range(183727495, 'function zbo(', 'async function Vbo('),
].join('\n');
const renderer = [
  range(183727495, 'session_context:(e)=>', 'coordinator_context:'),
  range(183727495, 'context_sections:(e)=>', 'prompt_snapshot:'),
  range(183727495, 'instructions:(e)=>', 'date:(e)=>'),
  range(183727495, 'date:(e)=>', 'fork_briefing:'),
].join('\n');
const oracle = vm.runInNewContext(`
${helpers}
let currentFiles,reason='session_start',localDate='2026-09-30';
let hostGit,prefetchedGit,canFetchGit=true,gitCalls=[];
const Ru={reader:undefined},F=()=>false,GFe=()=>false,aa=()=>false;
const u$n=async()=>currentFiles,ibo=()=>reason,K$r=()=>localDate;
const qvr=()=>canFetchGit,nbo=()=>{gitCalls.push('prefetch');return prefetchedGit===undefined?undefined:Promise.resolve(prefetchedGit)},d$n=async()=>{gitCalls.push('fetch');return hostGit};
// These snapshot materialization details do not affect schema validity or the
// routing hint. The native fmn, a$, xd, wd, CPt and Gp execute unchanged.
const li=value=>value,mvr=()=>'<fixture-hash>',STo=()=>true,Osn=()=>true;
const d=()=>{},Id=()=>{},i=()=>{},c=value=>value,me=value=>value,Ypt=()=>0;
const Re=({content,isMeta})=>({type:'user',isMeta,message:{content}});
const sn=attachment=>({type:'attachment',attachment});
const renderers={${renderer}};
const normalize=value=>JSON.parse(JSON.stringify(value));
const render=attachment=>renderers[attachment.type](attachment).map(row=>row.message.content);
async function run(test){
 currentFiles=test.current;reason=test.reason??'session_start';localDate=test.date??'2026-09-30';setGate(test);
 const history=test.prior??[],options=test.options??{};
 const routing=await Or(test.userContext??{},test.systemContext??{},{session:{},options},{messages:history,querySource:test.querySource??'agent:custom:fixture'});
 const attachments=yi(history,routing,{querySource:test.querySource??'agent:custom:fixture',resumeActive:test.resumeActive??false});
 return normalize({routing,expected:attachments.map(row=>({attachment:row.attachment,rendered:render(row.attachment)})),inlinePrefix:routing.bare?zbo([],routing.userContext).map(row=>row.message.content):[]});
}
function setGate(test){
 Ru.reader=test.readerThrows?()=>{throw Error('fixture feature reader failed')}:test.contextMetadata===undefined?undefined:()=>test.contextMetadata;
}
async function snapshot(test){
 setGate(test);currentFiles=[];
 const history=test.prior??[],latest=history.findLast(d6e)?.attachment;
 const parsed=fmn().safeParse(latest),projected=a$(history),snapshotHint=xd(history),historyHint=wd(history);
 const routing=await Or({claudeMd:'scalar instructions'},{},{session:{},options:test.options??{}},{messages:history,querySource:'agent:custom:fixture'});
 return normalize({schemaValid:parsed.success,parsedSnapshot:parsed.success?parsed.data:null,snapshotHint:projected?.contextRendering??null,hint:snapshotHint??null,metadataEnabled:CPt(),historyHint:historyHint??null,selectedRendering:routing.rendering});
}
async function gitSnapshot(test){
 hostGit=test.hostGit;prefetchedGit=test.prefetchedGit;canFetchGit=test.canFetchGit??true;gitCalls=[];
 const routing={announced:test.announced??{},bare:test.bare??false,carriesContext:test.carriesContext??true};
 const result=await Jn(test.prior??[],routing,{session:{},options:{omitGitStatus:test.omitGitStatus??false}},test.foreground??false);
 return normalize({routing:result,announced:result.announced,calls:gitCalls});
}
({run,snapshot,gitSnapshot,render:attachment=>normalize(render(attachment)),normalize});
`, { ...schema, ...lazy, ...sanitizer }, { timeout: 2_000 });

const managed = { path: '/managed/LINGXI.md', type: 'Managed', content: '  managed policy  \n' };
const project = { path: '/repo/LINGXI.md', type: 'Project', content: '\nproject policy\n' };
const user = { path: '/home/.lingxi/LINGXI.md', type: 'User', content: 'user policy' };
const local = { path: '/repo/LINGXI.local.md', type: 'Local', content: 'local policy' };
const initialFiles = [managed, user, project, local];
const filesInHistory = initialFiles.map(file => ({ ...file, content: file.content.trim() }));
const instructions = files => ({ type: 'attachment', attachment: { type: 'instructions', files } });
const initialHistory = [instructions(filesInHistory), { type: 'attachment', attachment: { type: 'date', date: '2026-09-30' } }];
const initialContext = { claudeMd: 'scalar replaced by typed files' };
const cases = [
  { name: 'initial-four-file-kinds', current: initialFiles, userContext: initialContext },
  { name: 'initial-js-trim-bom-preserves-nel', current: [{ path: '/仓库/规则.md', type: 'Project', content: '\ufeff\u0085 policy \u0085\ufeff' }], userContext: initialContext },
  { name: 'same-files-suppress-reannouncement', current: filesInHistory, userContext: initialContext, prior: initialHistory },
  { name: 'changed-body-and-type', current: [managed, user, { ...project, type: 'Local', content: 'changed project body' }, local], userContext: initialContext, prior: initialHistory, reason: 'policy_refresh' },
  { name: 'removed-files', current: [managed, project], userContext: initialContext, prior: initialHistory, reason: 'directory_added' },
  { name: 'successful-empty-initial', current: [], userContext: initialContext },
  { name: 'successful-empty-removes-prior', current: [], userContext: initialContext, prior: initialHistory, reason: 'hooks_invalidate' },
  { name: 'delta-chain-folds-current-baseline', current: [managed, { ...project, content: 'changed' }], userContext: initialContext, prior: [instructions(filesInHistory), { type: 'attachment', attachment: { type: 'instructions', files: [{ ...project, content: 'changed' }], removed: [user.path, local.path], changed: true } }, initialHistory[1]] },
  { name: 'cold-child-resume-rechecks-current-files', current: [managed, { ...project, content: 'new disk body' }], userContext: initialContext, prior: initialHistory, reason: 'session_start' },
  { name: 'only-incomplete-thinking-resume-suppresses', current: initialFiles, userContext: initialContext, resumeActive: true },
  { name: 'managed-only-selects-managed-files', current: initialFiles, userContext: initialContext, options: { managedInstructionsOnly: true } },
  { name: 'omitted-claude-md-does-not-announce-files', current: initialFiles, userContext: {} },
  { name: 'unavailable-file-descriptors-retain-context-section', userContext: { claudeMd: 'retained scalar context' } },
  { name: 'empty-scalar-with-unavailable-descriptors', userContext: { claudeMd: '' } },
  { name: 'ordinary-announcement-order', current: [project], userContext: { claudeMd: 'replaced', Environment: 'host environment', userEmail: 'user@example.test', attachedProject: 'repo' }, systemContext: { gitStatus: 'clean', perforceMode: 'disabled', extra: 'system section', cacheBreaker: 'stable suffix' } },
  { name: 'normal-announced-routing-even-metadata-gate-off', current: [project], userContext: initialContext, contextMetadata: false },
  { name: 'historical-inline-prefix-is-outgoing-only', current: [project], userContext: { claudeMd: 'inline scalar', userEmail: 'user@example.test' }, prior: [{ type: 'attachment', attachment: { type: 'prompt_snapshot', systemPrompt: ['fixture system'], contextRendering: 'inline' } }] },
  { name: 'historical-inline-empty-instruction-key-elided', current: [], userContext: { claudeMd: '', Environment: 'host environment' }, prior: [{ type: 'attachment', attachment: { type: 'prompt_snapshot', systemPrompt: ['fixture system'], contextRendering: 'inline' } }] },
  { name: 'explicit-bare-fork-prefix-is-outgoing-only', current: [project], userContext: initialContext, options: { bareFork: true } },
  { name: 'session-and-date-same-suppressed', current: [], userContext: { userEmail: 'user@example.test' }, prior: [{ type: 'attachment', attachment: { type: 'session_context', context: { userEmail: 'user@example.test' } } }, initialHistory[1]] },
  { name: 'session-cleared-and-local-date-changed', current: [], userContext: { Environment: 'current environment' }, prior: [{ type: 'attachment', attachment: { type: 'session_context', context: { userEmail: 'old@example.test', gitStatus: 'old status' } } }, initialHistory[1]], date: '2026-10-01', reason: 'account_change' },
];
const renderCases = [
  { name: 'instructions-first', attachment: { type: 'instructions', files: filesInHistory } },
  { name: 'instructions-empty-first', attachment: { type: 'instructions', files: [] } },
  { name: 'instructions-body-change-without-reason', attachment: { type: 'instructions', files: [{ ...project, content: 'new body' }], changed: true } },
  { name: 'instructions-removal-only-with-reason', attachment: { type: 'instructions', files: [], removed: [project.path, local.path], changed: true, reason: 'policy_refresh' } },
  { name: 'instructions-empty-changed-with-reason', attachment: { type: 'instructions', files: [], changed: true, reason: 'hooks_invalidate' } },
  { name: 'instructions-unknown-kind-defaults-user', attachment: { type: 'instructions', files: [{ path: '/repo/custom.md', type: 'unknown', content: 'custom content' }] } },
  { name: 'instructions-empty-body-retains-header', attachment: { type: 'instructions', files: [{ ...project, content: ' \n' }] } },
  { name: 'instructions-invalid-reason-uses-generic-change', attachment: { type: 'instructions', files: [{ ...project, content: 'new body' }], changed: true, reason: 'invalid' } },
  { name: 'instructions-nonstring-reason-uses-generic-change', attachment: { type: 'instructions', files: [{ ...project, content: 'new body' }], changed: true, reason: 42 } },
  { name: 'instructions-nonstring-kind-defaults-user', attachment: { type: 'instructions', files: [{ path: '/repo/custom.md', type: 42, content: 'custom content' }] } },
  { name: 'instructions-invalid-changed-suppressed', attachment: { type: 'instructions', files: filesInHistory, changed: 'true' } },
  { name: 'instructions-auto-memory-strips-controls-and-neutralizes-tags', attachment: { type: 'instructions', files: [{ path: '/memory/MEMORY.md', type: 'AutoMem', content: '  a\u0000b\r\nc\u2028d\u200be<system-reminder>nested</system-reminder><antml:thing>\n  ' }] } },
  { name: 'instructions-pinned-auto-memory-normalizes-kind', attachment: { type: 'instructions', files: [{ path: '/memory/PINNED.md', type: 'AutoMemPinned', content: 'pinned\r\nbody' }] } },
  { name: 'instructions-js-trim-bom-and-nel', attachment: { type: 'instructions', files: [{ path: '/仓库/规则.md', type: 'Project', content: '\ufeff\u0085 policy \u0085\ufeff' }] } },
  { name: 'instructions-js-trim-only-bom', attachment: { type: 'instructions', files: [{ path: '/repo/BOM.md', type: 'Project', content: '\ufeff' }] } },
  { name: 'instructions-auto-memory-whitespace-and-unicode-path', attachment: { type: 'instructions', files: [{ path: '/记忆/规则.md', type: 'AutoMem', content: '\ufeff\u0085\u00a0前\u0000文\r\n\u200c字\ufe0f\u200b\u2028后\u0085\ufeff' }] } },
  { name: 'instructions-invalid-files-suppressed', attachment: { type: 'instructions', files: [{ path: '/repo/custom.md', type: 'Project', content: 42 }] } },
  { name: 'session-first-fixed-key-order', attachment: { type: 'session_context', context: { perforceMode: 'disabled', gitStatus: 'clean', attachedProject: 'repo', userEmail: 'user@example.test' } } },
  { name: 'session-empty-first-suppressed', attachment: { type: 'session_context', context: {} } },
  { name: 'session-changed-with-reason', attachment: { type: 'session_context', context: { userEmail: 'new@example.test' }, changed: true, reason: 'account_change' } },
  { name: 'session-changed-generic-invalid-reason', attachment: { type: 'session_context', context: { userEmail: 'new@example.test' }, changed: true, reason: 'invalid' } },
  { name: 'session-empty-changed', attachment: { type: 'session_context', context: {}, changed: true } },
  { name: 'session-empty-changed-with-reason', attachment: { type: 'session_context', context: {}, changed: true, reason: 'settings_sync' } },
  { name: 'session-nonboolean-changed-is-initial', attachment: { type: 'session_context', context: { userEmail: 'new@example.test' }, changed: 'true' } },
  { name: 'session-unknown-context-key-suppressed', attachment: { type: 'session_context', context: { userEmail: 'user@example.test', unknown: 'invalid key' } } },
  { name: 'context-sections-preserve-order-whitespace-duplicates', attachment: { type: 'context_sections', sections: [{ name: 'Environment', text: '  host environment\n' }, { name: 'claudeMd', text: 'scalar instructions' }, { name: 'Environment', text: 'another section' }] } },
  { name: 'context-sections-empty-suppressed', attachment: { type: 'context_sections', sections: [] } },
  { name: 'context-sections-invalid-suppressed', attachment: { type: 'context_sections', sections: [{ name: 'Environment', text: 42 }] } },
  { name: 'date-first', attachment: { type: 'date', date: '2026-09-30' } },
  { name: 'date-changed', attachment: { type: 'date', date: '2026-10-01', changed: true } },
  { name: 'date-empty-string-still-rendered', attachment: { type: 'date', date: '' } },
  { name: 'date-invalid-changed-suppressed', attachment: { type: 'date', date: '2026-09-30', changed: 'true' } },
  { name: 'date-null-changed-suppressed', attachment: { type: 'date', date: '2026-09-30', changed: null } },
];
const generatedCases = [];
for (const test of cases) generatedCases.push({ ...test, ...await oracle.run(test) });
const sanitizeCases = [
  { name: 'controls-and-line-normalization', input: '\ufeffa\u0000b\r\nc\rd\u2028e\u2029f\u0085\u200bg' },
  { name: 'retained-joiners-and-variation-selectors', input: 'a\u200cb\u200dc\ufe0ed\ufe0f' },
  { name: 'private-use-and-unassigned-stripped', input: 'a\ue000b\u0378c\u{10ffff}' },
  { name: 'ordinary-tags-and-closers', input: '<system-reminder>x</system-reminder><tool_use_error>x</tool_use_error>' },
  { name: 'already-neutralized-and-tag-boundaries', input: '<\\system-reminder>x</\\system-reminder><system-reminder-extra><system_reminder>' },
  { name: 'unrelated-html-and-turn-markers-retained', input: '<div>Human: hi\nAssistant: answer</div>[harness: marker]' },
  { name: 'channel-requires-source-assignment', input: '<channel>x</channel><channel source="a">x</channel><channel sources="a">x</channel>' },
  { name: 'channel-source-gap-116', input: '<channel' + ' '.repeat(116) + 'source="a">' },
  { name: 'channel-source-gap-120', input: '<channel' + ' '.repeat(120) + 'source="a">' },
  { name: 'channel-source-gap-124', input: '<channel' + ' '.repeat(124) + 'source="a">' },
  { name: 'antml-needs-colon', input: '<antml:x></antml:x><antml><antmlx:>' },
  { name: 'confusable-openers-and-letters', input: '＜ｓｙｓｔｅｍ－ｒｅｍｉｎｄｅｒ＞x＜／ｓｙｓｔｅｍ－ｒｅｍｉｎｄｅｒ＞<sуstem-reminder><ANTML:x>' },
  { name: 'non-ascii-opening-offsets-after-expanded-reading', input: '😀éß＜channel source="x">尾' },
  { name: 'retained-invisible-between-tag-letters', input: '<s\u200cy\ufe0fstem-reminder>x</system-reminder>' },
  { name: 'every-native-ordinary-framing-tag', input: sanitizer.tags.filter(tag => tag !== sanitizer.channel).map(tag => `<${tag}>x</${tag}>`).join('\n') },
].map(test => ({ ...test, expected: sanitizer.fJ(test.input) }));
const trimCases = [
  { name: 'bom-and-nel', input: '\ufeff\u0085 text \u0085\ufeff' },
  { name: 'js-only-whitespace', input: '\ufeff\u00a0\u2007\u2028\u2029\u3000' },
  { name: 'rust-only-nel-is-retained', input: '\u0085' },
].map(test => ({ ...test, expected: test.input.trim() }));
const snapshotRow = payload => ({ type: 'attachment', attachment: { type: 'prompt_snapshot', ...payload } });
const inlineSnapshot = snapshotRow({ systemPrompt: ['base system'], contextRendering: 'inline' });
const sessionRow = context => ({ type: 'attachment', attachment: { type: 'session_context', context } });
const snapshotCases = [
  { name: 'valid-inline-empty-system-prompt', prior: [snapshotRow({ systemPrompt: [], contextRendering: 'inline' })] },
  { name: 'valid-announced-system-prompt', prior: [snapshotRow({ systemPrompt: ['base'], contextRendering: 'announced' })] },
  { name: 'missing-system-prompt-invalidates-latest', prior: [snapshotRow({ contextRendering: 'inline' })] },
  { name: 'null-system-prompt-invalidates-latest', prior: [snapshotRow({ systemPrompt: null, contextRendering: 'inline' })] },
  { name: 'non-array-system-prompt-invalidates-latest', prior: [snapshotRow({ systemPrompt: 'base', contextRendering: 'inline' })] },
  { name: 'mixed-system-prompt-invalidates-latest', prior: [snapshotRow({ systemPrompt: ['base', 42], contextRendering: 'inline' })] },
  { name: 'invalid-latest-does-not-fall-back-to-older-inline', prior: [inlineSnapshot, snapshotRow({ contextRendering: 'inline' })] },
  { name: 'valid-latest-without-hint-does-not-fall-back', prior: [inlineSnapshot, snapshotRow({ systemPrompt: ['new base'] })] },
  { name: 'invalid-context-rendering-is-caught-undefined', prior: [snapshotRow({ systemPrompt: ['base'], contextRendering: 'unknown' })] },
  { name: 'nonstring-context-rendering-is-caught-undefined', prior: [snapshotRow({ systemPrompt: ['base'], contextRendering: 42 })] },
  { name: 'valid-tools-object-schema-and-server', prior: [snapshotRow({ systemPrompt: ['base'], contextRendering: 'inline', tools: [{ name: 'Read', description: 'read file', schema: { type: 'object', custom: [1, 'x'] }, server: 'files' }], cliPrefix: 'claude' })] },
  { name: 'tool-name-required', prior: [snapshotRow({ systemPrompt: ['base'], contextRendering: 'inline', tools: [{ description: 'read file' }] })] },
  { name: 'tool-description-required', prior: [snapshotRow({ systemPrompt: ['base'], contextRendering: 'inline', tools: [{ name: 'Read' }] })] },
  { name: 'tool-schema-array-invalidates-latest', prior: [snapshotRow({ systemPrompt: ['base'], contextRendering: 'inline', tools: [{ name: 'Read', description: 'read file', schema: [] }] })] },
  { name: 'tool-schema-number-invalidates-latest', prior: [snapshotRow({ systemPrompt: ['base'], contextRendering: 'inline', tools: [{ name: 'Read', description: 'read file', schema: 42 }] })] },
  { name: 'tool-server-number-invalidates-latest', prior: [snapshotRow({ systemPrompt: ['base'], contextRendering: 'inline', tools: [{ name: 'Read', description: 'read file', server: 42 }] })] },
  { name: 'tools-null-invalidates-latest', prior: [snapshotRow({ systemPrompt: ['base'], contextRendering: 'inline', tools: null })] },
  { name: 'cli-prefix-number-invalidates-latest', prior: [snapshotRow({ systemPrompt: ['base'], contextRendering: 'inline', cliPrefix: 42 })] },
  { name: 'invalid-optional-booleans-are-caught-undefined', prior: [snapshotRow({ systemPrompt: ['base'], contextRendering: 'inline', reminderFold: 42, systemTurns: 'yes', toolChangeHeader: null, inlineTools: [], keptReminders: {}, echoWireToolInputs: 'no' })] },
  { name: 'metadata-flag-false-ignores-valid-inline-hint', prior: [inlineSnapshot], contextMetadata: false },
  { name: 'metadata-flag-nonfalse-value-retains-hint', prior: [inlineSnapshot], contextMetadata: 0 },
  { name: 'metadata-reader-error-defaults-enabled', prior: [inlineSnapshot], readerThrows: true },
  { name: 'recording-option-false-does-not-gate-reading-hint', prior: [inlineSnapshot], options: { systemPromptSnapshot: false } },
  { name: 'legacy-reminder-prose-does-not-establish-hint', prior: [{ type: 'user', isMeta: true, message: { content: '<system-reminder>\nAs you answer the user\'s questions, you can use the following context:\n# claudeMd\nold context\n</system-reminder>' } }] },
  { name: 'latest-invalid-snapshot-can-use-session-announced-hint', prior: [inlineSnapshot, sessionRow({ userEmail: 'user@example.test' }), snapshotRow({ contextRendering: 'inline' })] },
  { name: 'latest-invalid-session-does-not-fall-back-to-older-session', prior: [sessionRow({ userEmail: 'user@example.test' }), sessionRow({ unknown: 'invalid key' })] },
];
const generatedSnapshotCases = [];
for (const test of snapshotCases) generatedSnapshotCases.push({ ...test, expected: await oracle.snapshot(test) });
const gitSnapshotCases = [
  { name: 'valid-empty-session-context-freezes-missing-git', prior: [sessionRow({})], announced: { gitStatus: 'stale incoming git' }, hostGit: 'fresh host git' },
  { name: 'valid-email-context-freezes-missing-git', prior: [sessionRow({ userEmail: 'user@example.test' })], announced: { userEmail: 'user@example.test', gitStatus: 'stale incoming git' }, hostGit: 'fresh host git' },
  { name: 'valid-latest-missing-git-does-not-use-older-explicit-git', prior: [sessionRow({ gitStatus: 'old git' }), sessionRow({ userEmail: 'user@example.test' })], announced: { userEmail: 'user@example.test' }, hostGit: 'fresh host git' },
  { name: 'invalid-latest-context-fetches-host-without-older-fallback', prior: [sessionRow({ gitStatus: 'old git' }), sessionRow({ unknown: 'invalid key' })], announced: { userEmail: 'user@example.test', gitStatus: 'stale incoming git' }, hostGit: 'fresh host git' },
  { name: 'invalid-latest-git-value-fetches-host', prior: [sessionRow({ gitStatus: 42 })], announced: { userEmail: 'user@example.test' }, hostGit: 'fresh host git' },
  { name: 'valid-explicit-git-is-frozen', prior: [sessionRow({ gitStatus: 'frozen git', userEmail: 'user@example.test' })], announced: { userEmail: 'user@example.test', gitStatus: 'stale incoming git' }, hostGit: 'fresh host git' },
  { name: 'new-history-fetches-host-git', announced: { userEmail: 'user@example.test', gitStatus: 'stale incoming git' }, hostGit: 'fresh host git' },
  { name: 'new-history-omit-git-does-not-fetch', announced: { userEmail: 'user@example.test', gitStatus: 'stale incoming git' }, hostGit: 'fresh host git', omitGitStatus: true },
  { name: 'new-history-ineligible-git-does-not-fetch', announced: { userEmail: 'user@example.test' }, hostGit: 'fresh host git', canFetchGit: false },
  { name: 'foreground-consumes-prefetched-git', announced: { userEmail: 'user@example.test' }, hostGit: 'fresh host git', prefetchedGit: 'prefetched git', foreground: true },
  { name: 'foreground-null-prefetch-does-not-fall-back-to-host', announced: { userEmail: 'user@example.test' }, hostGit: 'fresh host git', prefetchedGit: null, foreground: true },
  { name: 'host-null-removes-incoming-git', announced: { userEmail: 'user@example.test', gitStatus: 'stale incoming git' }, hostGit: null },
  { name: 'bare-routing-keeps-incoming-context', announced: { userEmail: 'user@example.test', gitStatus: 'stale incoming git' }, hostGit: 'fresh host git', bare: true },
  { name: 'no-context-keeps-incoming-context-without-fetch', announced: { gitStatus: 'stale incoming git' }, hostGit: 'fresh host git', carriesContext: false },
];
const generatedGitSnapshotCases = [];
for (const test of gitSnapshotCases) generatedGitSnapshotCases.push({ ...test, expected: await oracle.gitSnapshot(test) });
const generated = {
  version: '2.1.286', binary_sha256: sha256, sourceByteRanges,
  environment: { querySource: 'agent:custom:fixture', defaultDate: '2026-09-30', coordinatorEnabled: false, instructionFileAvailability: 'current absent means native undefined', historyEnvelope: 'UUID and timestamps omitted; payload and rendered bytes preserved' },
  cases: generatedCases,
  renderCases: renderCases.map(test => ({ ...test, rendered: oracle.render(test.attachment) })),
  sanitizeCases, trimCases,
  snapshotCases: generatedSnapshotCases,
  gitSnapshotCases: generatedGitSnapshotCases,
};
const content = JSON.stringify(generated, null, 2) + '\n';
const path = fileURLToPath(new URL('../../crates/core/tests/fixtures/instruction_announcements_2_1_286.json', import.meta.url));
const exclusions = [], reading = [];
let start;
for (let codepoint = 0; codepoint <= 0x10ffff; codepoint++) {
  const character = String.fromCodePoint(codepoint), excluded = sanitizer.J8e(character);
  if (excluded && start === undefined) start = codepoint;
  if (!excluded && start !== undefined) { exclusions.push([start, codepoint - 1]); start = undefined; }
  if (!excluded && codepoint >= 128) {
    const normalized = scrub.iu(character);
    if (normalized !== character) reading.push([codepoint, normalized]);
  }
}
if (start !== undefined) exclusions.push([start, 0x10ffff]);
const patterns = [
  scrub.Au({ tags: sanitizer.tags.filter(tag => tag !== sanitizer.channel) }).source,
  scrub.Au({ tags: [sanitizer.channel], tail: scrub.dVr }).source,
  scrub.Au({ tags: ['antml'], tail: ({ colon }) => `[${colon}]` }).source,
];
const tables = JSON.stringify({ version: '2.1.286', binary_sha256: sha256, tags: sanitizer.tags, exclusions, reading, patterns }, null, 2) + '\n';
for (const [output, bytes] of [
  [path, content],
  [fileURLToPath(new URL('../../crates/core/src/host/instruction_memory_sanitize_286.json', import.meta.url)), tables],
]) {
  if (process.argv.includes('--check')) {
    if (readFileSync(output, 'utf8') !== bytes) throw new Error(`Instruction oracle output differs: ${output}`);
  } else {
    mkdirSync(dirname(output), { recursive: true });
    writeFileSync(output, bytes);
  }
}
console.log(`Verified ${cases.length} native routing/announcement, ${renderCases.length} native renderer, ${snapshotCases.length} native snapshot, ${gitSnapshotCases.length} native git snapshot cases and ${reading.length} sanitizer mappings (${sha256})`);

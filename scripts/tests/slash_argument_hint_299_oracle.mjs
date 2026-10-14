// Run pure functions recovered from the pinned installed Claude Code artifact.
// The native application is never started: no credentials, network, or model.
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const args = process.argv.slice(2);
const binaryPath = args.find(arg => !arg.startsWith('--'))
  ?? `${process.env.HOME}/.local/share/claude/versions/2.1.299`;
const bytes = fs.readFileSync(binaryPath);
const hash = value => crypto.createHash('sha256').update(value).digest('hex');
const binarySha256 = hash(bytes);
assert.equal(binarySha256, 'ba5ab7b8c195fa03ab8d5bf3915c76153b576a2c40e79a5c46e265e7ed9744b3', 'Unrecognized oracle binary');
const sources = [];
function moduleAt(name, offset) {
  const start = bytes.lastIndexOf('// @bun @bytecode\n', offset);
  const end = bytes.indexOf(0, offset);
  assert(start >= 0 && end > start);
  const source = bytes.subarray(start, end);
  sources.push({ name, start, end, sha256: hash(source) });
  return { source: source.toString('utf8'), offset: start };
}
function cut(module, name, startText, endText) {
  const start = module.source.indexOf(startText);
  const end = module.source.indexOf(endText, start);
  assert(start >= 0 && end > start, `Missing native source ${name}`);
  const source = module.source.slice(start, end);
  const byteStart = module.offset + Buffer.byteLength(module.source.slice(0, start));
  sources.push({ name, start: byteStart, end: byteStart + Buffer.byteLength(source), sha256: hash(source) });
  return source;
}
const shell = moduleAt('shell-parser-module', 198367046);
const ast = moduleAt('shell-ast-module', 198430294);
const wrapper = moduleAt('shell-wrapper-module', 201451545);
const metadata = moduleAt('argument-helper-module', 202704014);
const lookup = moduleAt('command-lookup-module', 200503071);
const identity = moduleAt('command-identity-module', 198814618);
const autocomplete = moduleAt('autocomplete-module', 225732404);
const review = moduleAt('review-module', 211575492);
const input = moduleAt('input-render-module', 220400319);
const context = { performance, TextEncoder };
vm.createContext(context);
vm.runInContext(`globalThis.$_=(()=>{${shell.source.replace(/export\{[^}]+\};\s*$/, '')};return $_})()`, context);
vm.runInContext(`globalThis.iv=(()=>{${
  cut(ast, 'shell-ast-constants', 'var Te=', 'async function vEr(')
}${cut(ast, 'shell-argument-traversal', 'function que(', 'var be=')}
var vP=1e4;${cut(wrapper, 'shell-argument-entry', 'function iv(', 'var x=')};return iv})()`, context);
vm.runInContext(cut(metadata, 'argument-parse-and-remaining', 'function fJr(', 'function RDe('), context);
vm.runInContext(cut(identity, 'display-name', 'function Ro(e){return e.userFacingName', 'function lhr(')
  + cut(lookup, 'command-lookup', 'function OC(', 'function ehs(')
  + cut(autocomplete, 'slash-token-grammar', 'function lse(h)', 'function BK(')
  + cut(autocomplete, 'argument-entry-gate', 'function aBe(', 'function Mse('), context);

// Exact native hint-producing branch. Async completion providers and other menu
// branches are intentionally outside this controlled pure-function replay.
const selector = cut(autocomplete, 'hint-selector', 'if(Le==="prompt"&&dR(yn)&&Ps>0&&!aBe(Is,yn)){', 'let Xs=j0(Go,yn,C)');
vm.runInContext(`globalThis.hint=(yn,C,Ps=yn.length,Le='prompt')=>{
let result;const Jt=callback=>{result=callback().commandArgumentHint};
const Is=Ps===yn.length&&Ps>0&&yn.length>0&&yn[Ps-1]===' ';
(()=>{${selector}}})();return result??null;
};globalThis.parse=fJr;globalThis.argumentNames=Kae;`, context);
const reviewHint = cut(review, 'code-review-hint', 'function ts()', 'async function os(');
vm.runInContext(`globalThis.reviewHint=(enabled)=>{const yPt=()=>enabled,me=['low','medium','high','xhigh','max'];${reviewHint};return ts()}`, context);
const renderGate = cut(input, 'hint-render-gate', 'U=o.value&&o.value.trim()', ',ie=o.showCursor');
vm.runInContext(`globalThis.renderedHint=(value,argumentHint)=>{const o={value,argumentHint};let ${renderGate};return E?(value.endsWith(' ')?'':' ')+argumentHint:null}`, context);

const commands = [
  { name: 'review', aliases: ['rv'], argumentHint: '[level] [target]', type: 'prompt' },
  { name: 'named', aliases: ['n'], argumentHint: '[custom hint]', argNames: ['first', 'second', 'third'], type: 'prompt' },
  { name: 'names-only', argNames: ['first', 'second', 'third'], type: 'prompt' },
  { name: 'empty-hint', argumentHint: '', argNames: ['first', 'second'], type: 'prompt' },
  { name: 'local', argumentHint: '[path]', argNames: ['ignored'], type: 'local' },
  { name: 'local-names', argNames: ['ignored'], type: 'local' },
  { name: 'no-hint', type: 'prompt' },
  { name: 'hidden', argumentHint: '[still resolvable]', type: 'prompt', isHidden: true },
  { name: 'plugin:skill', displayName: 'display.name', aliases: ['alias_name'], argumentHint: '[value]', type: 'prompt' },
  { name: 'early', aliases: ['winner'], argumentHint: '[alias]', type: 'prompt' },
  { name: 'winner', argumentHint: '[primary]', type: 'prompt' },
];
const nativeCommands = commands.map(command => ({ ...command,
  ...(command.displayName ? { userFacingName: () => command.displayName } : {}) }));
const selectorInputs = [
  '', '/', '/review', '/rev ', '/review ', '/review  ', '/review arg', '/review arg ', '/review\t',
  '/rv ', '/REVIEW ', ' /review ', '\n/review ', '/review\n ', '/review \n', '/review \n ',
  '/review\u00a0', '/review\u00a0 ', '/review\t ', '/review \t', '/review \t ',
  '/named ', '/named  ', '/named a ', '/named a b ', '/named a b c ', '/named a b c d ',
  '/n a ', '/names-only ', '/empty-hint ', '/local ', '/local a ', '/local-names ', '/no-hint ',
  '/hidden ', '/plugin:skill ', '/display.name ', '/alias_name ', '/winner ', '/unknown ',
  '/unknown a ', '/foo/bar ', '/中文 ', '/name+extra ',
];
const cases = selectorInputs.map(input => ({ input, cursor: input.length, mode: 'prompt',
  expected: context.hint(input, nativeCommands) }));
for (const cursor of [0, 1, 3, 6, 7, 8]) cases.push({ input: '/review ', cursor, mode: 'prompt',
  expected: context.hint('/review ', nativeCommands, cursor) });
cases.push({ input: '/review ', cursor: 8, mode: 'bash', expected: context.hint('/review ', nativeCommands, 8, 'bash') });

const argumentInputs = [
  '', ' ', 'a ', 'a b ', 'a b c ', 'a  b ', '"a b" ', "'a b' ", '"" ', "'' ", 'a\\ b ',
  'a"b c" ', '"a"b ', '"a\\"b" ', '"unterminated ', "'unterminated ", 'foo "bar ',
  'a; b ', 'a;b c ', 'a | b ', 'a && b ', 'a || b ', 'a & b ', 'a\nb ', 'a\tb ', 'a\rb ',
  'a # comment ', '#comment ', '# comment ', 'a#comment b ', 'a\\ ', '\\ ',
  'A=b foo ', 'A="b c" foo bar ', 'A=b ', 'A=b B=c ', 'export A=b C=d ', 'unset a b ',
  '$(echo hi) ', '`echo hi` ', 'a $(echo hi) b ', 'a `echo hi` b ', 'a pre$(echo hi)post b ',
  '$VAR b ', '${x} b ', '$((1 + 2)) b ', 'a >out b ', '>out a b ', 'a 2>out b ',
  'a >>out b ', 'a <input b ', '(a b) ', '{ a b; } ', 'a (b c) ', 'if true; then a b; fi ',
  '*.ts ', '[abc] foo ', '{a,b} c ', '~root a ', 'a\u00a0b ', 'a\u3000b ', '\u00a0', '\ufeff',
  'a\u2028b ', 'a\u0085b ', '😀 "二 三" ', 'a\\\nb ', '--flag=value foo ', '--flag "value with spaces" ',
  "a $'b c' d ", 'a $"b c" d ', 'a <(b c) d ', '<(a b) ', '((1+2)) ', '[[ a == b ]] ',
  '! a b ', 'time a b ', 'a\\;b c ', 'a \\# b ', 'a "# b" ', 'a "b;c" ',
  'a; b ' + ' '.repeat(9994), 'a; b ' + ' '.repeat(9995),
];
const argumentCases = argumentInputs.map(input => ({ input, parsed: [...context.parse(input)],
  count: context.parse(input).length,
  hint: context.hint(`/names-only ${input.endsWith(' ') ? input : input + ' '}`, nativeCommands) }));
const argumentNameCases = [undefined, null, '', 'one two', ' one  two ', ['one', '2', '', 'two'],
  '0 1 02 a2', [' a ', '3', '003', 'x'], 'first\u00a0second', ['first', null, 5, 'second']]
  .map(input => ({ ...(input !== undefined ? { input } : {}), expected: [...context.argumentNames(input)] }));
const renderCases = [['/review', '[level]'], ['/review ', '[level]'], ['/review arg', '[level]'],
  ['/review arg ', '[next]'], [' /review ', '[level]'], ['/review ', ''], ['', '[level]']]
  .map(([input, hint]) => ({ input, hint, expected: context.renderedHint(input, hint) }));
const parserSource = `// Generated by scripts/tests/slash_argument_hint_299_oracle.mjs.
// Claude Code 2.1.299 native arm64 SHA-256: ${binarySha256}
// Extracted native parser bodies are unchanged. Host wrapper supplies the clock.
// Exposes slashCommandParseArguments(string) and slashCommandArgumentCount(string).
(() => {
const performance = globalThis.performance ?? { now: () => Date.now() };
const $_ = (() => {
${shell.source.replace(/export\{[^}]+\};\s*$/, '')}
return $_;
})();
const iv = (() => {
${ast.source.slice(ast.source.indexOf('var Te='), ast.source.indexOf('async function vEr('))}
${ast.source.slice(ast.source.indexOf('function que('), ast.source.indexOf('var be='))}
var vP=1e4;
${wrapper.source.slice(wrapper.source.indexOf('function iv('), wrapper.source.indexOf('var x='))}
return iv;
})();
${metadata.source.slice(metadata.source.indexOf('function fJr('), metadata.source.indexOf('function Kae('))}
globalThis.slashCommandParseArguments = fJr;
globalThis.slashCommandArgumentCount = input => fJr(input).length;
})();
`;
const generatedContext = {};
vm.createContext(generatedContext);
vm.runInContext(parserSource, generatedContext);
for (const { input, parsed, count } of argumentCases) {
  assert.deepEqual([...generatedContext.slashCommandParseArguments(input)], parsed);
  assert.equal(generatedContext.slashCommandArgumentCount(input), count);
}
const fixture = { version: '2.1.299', binary_sha256: binarySha256, sources,
  generated_parser_sha256: hash(parserSource),
  commands, cases, argumentCases, argumentNameCases, renderCases,
  codeReviewHints: { ultraEnabled: context.reviewHint(true), ultraDisabled: context.reviewHint(false) } };
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const fixturePath = path.join(root, 'docs/parity/slash-argument-hints-2.1.299.json');
const parserPath = path.join(root, 'docs/parity/slash-argument-parser-2.1.299.js');
const runtimeParserPath = path.join(root, 'crates/client/src/slash_argument_parser_299.js');
const json = JSON.stringify(fixture, null, 2) + '\n';
if (args.includes('--check')) {
  assert.equal(fs.readFileSync(fixturePath, 'utf8'), json, 'Native oracle fixture drift');
  assert.equal(fs.readFileSync(parserPath, 'utf8'), parserSource, 'Extracted native parser drift');
} else {
  fs.writeFileSync(fixturePath, json);
  fs.writeFileSync(parserPath, parserSource);
}
if (fs.existsSync(runtimeParserPath)) {
  assert.equal(fs.readFileSync(runtimeParserPath, 'utf8'), parserSource, 'Runtime native parser drift');
}
console.log(`Native ${fixture.version}: ${cases.length} selector, ${argumentCases.length} shell, ${argumentNameCases.length} metadata, ${renderCases.length} render cases verified`);

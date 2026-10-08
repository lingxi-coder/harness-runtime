// Execute the timeout helpers and public wording from the pinned native oracle.
// Usage: node background-timeout-2.1.286.mjs /tmp/claude-code-oracle-2.1.286
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import crypto from 'node:crypto';
const root = process.argv[2];
const sha = text => crypto.createHash('sha256').update(text).digest('hex');
const capture = JSON.parse(fs.readFileSync(path.join(root, 'capture.json'), 'utf8'));
const expectedBinarySha = '75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433';
if (capture.version !== '2.1.286' || sha(fs.readFileSync(capture.binary)) !== expectedBinarySha) throw Error('Wrong oracle binary');
const sources = {};
function read(name) { const source = fs.readFileSync(path.join(root, name), 'utf8'); sources[name] = sha(source); return source; }
function between(source, start, end) { const a = source.indexOf(start), b = source.indexOf(end, a); if (a < 0 || b < 0) throw Error(start); return source.slice(a, b); }
const timeout = read('src_181936610.js');
const lifecycle = read('src_182118681.js');
const monitor = read('src_183356562.js');
const main = read('src_183727495.js');
const ctx = vm.createContext({process: {env: {}}, rc: value => parseInt(value, 10), YH: () => ({}), R: (_, fallback) => fallback, vo: (_, fallback) => fallback, bpe: () => false});
vm.runInContext(between(timeout, 'var Me=120000,Ne=600000;', 'var Ce=2000;'), ctx);
vm.runInContext(between(lifecycle, 'function Iin(){', 'import{isDeepStrictEqual'), ctx);
vm.runInContext(between(monitor, 'var Ldt=300000', 'function pdn('), ctx);
const cases = [];
for (const env of [{}, {BASH_DEFAULT_TIMEOUT_MS:'3600000'}, {BASH_MAX_TIMEOUT_MS:'9000000'}, {BASH_MAX_TIMEOUT_MS:'9999999999'}]) {
  ctx.process.env = env;
  for (const requested of [null, 1, 2000, 600000, 1800000, 7200000, 9999999999]) {
    ctx.requested = requested ?? undefined;
    cases.push({env, requested, max: vm.runInContext('tne()', ctx), background: vm.runInContext('UWn(requested)', ctx)});
  }
}
ctx.process.env = {};
ctx.AM = () => vm.runInContext('Tye()', ctx);
const timeoutDescription = vm.runInContext(between(main, 'describe(`Optional timeout in milliseconds', '),description:o().optional()').slice(9), ctx);
const commandDescription = vm.runInContext(between(main, 'description:o().optional().describe(`Clear, concise description', '),run_in_background:').slice('description:o().optional().describe('.length), ctx);
const parameterDescription = vm.runInContext('Hin()', ctx);
const verboseTimeoutSuffix = vm.runInContext('Oin()', ctx);
const monitorDefault = vm.runInContext('l7()', ctx);
const monitorCap = vm.runInContext('VFe()', ctx);
ctx.bpe = () => true;
const singlePromptMonitorCap = vm.runInContext('VFe()', ctx);
ctx.Fbe = 'Background command ';
ctx.H6 = vm.runInContext('(' + between(main, 'H6={', ',W6=').slice(3) + ')', ctx);
vm.runInContext(between(main, 'function tGe(', 'function nGe('), ctx);
const deadlineSummary = vm.runInContext('tGe("bash", "watch build", "killed", undefined, undefined, undefined, "deadline")', ctx);
const output = path.resolve(import.meta.dirname, '../fixtures/background_timeout_2_1_286.json');
fs.writeFileSync(output, JSON.stringify({version:'2.1.286', binarySha256:expectedBinarySha, sources, timeoutDescription, commandDescription, parameterDescription, verboseTimeoutSuffix, deadlineSummary, monitorDefault, monitorCap, singlePromptMonitorCap, cases}, null, 2) + '\n');
console.log(`Generated ${cases.length} native background timeout cases`);

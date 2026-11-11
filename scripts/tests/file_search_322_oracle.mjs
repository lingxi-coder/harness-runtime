#!/usr/bin/env node
// Replay native search renderers with explicit process/filesystem dependencies.
// Persist only inputs, outputs and byte-range receipts, never extracted source.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { homedir, tmpdir } from 'node:os';
import path from 'node:path';
import vm from 'node:vm';

const args = process.argv.slice(2);
assert(args.every(arg => !arg.startsWith('--') || arg === '--check'));
assert(args.filter(arg => !arg.startsWith('--')).length <= 1);
const binaryPath = args.find(arg => !arg.startsWith('--'))
  ?? path.join(homedir(), '.local/share/claude/versions/2.1.322');
const binary = readFileSync(binaryPath);
const sha256 = value => createHash('sha256').update(value).digest('hex');
const nativeSha256 = sha256(binary);
assert.equal(nativeSha256, 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450');
const sourceByteRanges = [];
function take(name, start, end, digest, first, next) {
  const bytes = binary.subarray(start, end);
  assert(bytes.length > 0 && bytes.length < 6000, `${name}: unbounded extraction`);
  assert.equal(sha256(bytes), digest, `${name}: source changed`);
  assert(bytes.toString('utf8').startsWith(first), `${name}: start changed`);
  assert.equal(binary.subarray(end, end + Buffer.byteLength(next)).toString('utf8'), next);
  sourceByteRanges.push({ name, start, end, sha256: digest });
  return bytes.toString('utf8');
}
const timeoutNotice = take('incomplete search notice', 272417437, 272417614,
  '21928496d807844d29e6eb75c3b1e411e24bdb21b458cab5499edbf428438c45',
  'var bRe=', 'function TRe(');
const globNotice = take('glob truncation notice', 272420656, 272421111,
  '7f763065ff25a1205a32bbe3b05039be74a0d99d6da22e6925d2c37dd230535d',
  'function $ar(', 'async function Har(');
const globMapper = take('glob result mapper', 272423426, 272423699,
  'aacfd2f167da79aecbdc83e665d8059fa99831ec9db6c7e84479cd46309493ac',
  'mapToolResultToToolResultBlockParam(e,n)', '});var cdn=');
const pagination = take('head limit and pagination', 272427250, 272427522,
  '0337a4210ad22846f5c46af9f41ce9d3ec777503a9900e12b422055c2ede252b',
  'function AJe(', 'var qar=');
const grepMapper = take('grep result mapper', 272428949, 272429990,
  '2c63bf54f1d41f3e58fde102d9737f726528c0471db6d6a4c249d56fcba9d52c',
  'mapToolResultToToolResultBlockParam({mode:e=', ',create(e){');
const contentParser = take('ripgrep JSON event parser and line offset', 272437708, 272439143,
  '065088479b8a096cd28db5a129e287697c9ac6ada984f48a1b310065e745c40b',
  'function Zar(', 'var tlr=');
const contentSearch = take('grep call result and search arguments', 272431723, 272437053,
  '9439a554d7f647124784fff88f1a0d46fe12b83d7c7b2041d5c96cd352a042ad',
  'async function Xar(', 'function Qar(');
const lexicalPaths = take('grep lexical result paths', 272437053, 272437415,
  'a4693203413cda874aba360e9752a0391524fab185749c920d3987e2abdb2b35', 'function Qar(', 'function Jar(');
const suffix = take('one suffix removal', 260463926, 260463995,
  'ebc95b8712d8d2875533b7d0885bc7cf4386ef983718267912d7e3fc42955774',
  'function Hh(', 'var f=');
const repeatedRoot = take('glob repeated-root prefix and real-directory probe', 272416874, 272417437,
  '5af63c94a9d336bb08b1a45e5f8c7335de213b65dd908050efb0a4e94eb10fb7',
  'function Mar(', 'var bRe=');

const root = '/workspace/project';
const context = vm.createContext({
  Buffer, JSON, ps: JSON.parse,
  P: (n, word) => n === 1 ? word : `${word}s`, zar: 250,
  Ft: (value, separator) => value.split(separator).length - 1,
  Rar: path.posix.join,
  ae: () => ({ lstat: async name => {
    const entry = context.DIRECTORIES?.[name];
    if (!entry) throw new Error('Controlled missing directory');
    return { isDirectory: () => entry === 'directory', isSymbolicLink: () => entry === 'symlink' };
  } }),
});
vm.runInContext(timeoutNotice + globNotice + pagination + suffix + contentParser + repeatedRoot
  + `;var glob = {${globMapper}}; var grep = {${grepMapper}};`, context);

const projections = [];
function project(tool, name, input) {
  context.INPUT_JSON = JSON.stringify(input);
  const expected = vm.runInContext(`${tool}.mapToolResultToToolResultBlockParam(JSON.parse(INPUT_JSON), 'tool-1')`, context);
  projections.push({ tool, name, input, expected });
}
project('glob', 'empty', { filenames: [], truncated: false });
project('glob', 'empty timeout', { filenames: [], truncated: false, timedOut: true });
project('glob', 'paths with newline and Unicode', { filenames: ['a\nb.txt', '中文😀.txt'], truncated: false });
project('glob', 'complete truncated', { filenames: ['a', 'b'], truncated: true, totalMatches: 5, countIsComplete: true });
project('glob', 'incomplete truncated timeout', { filenames: ['a', 'b'], truncated: true, totalMatches: 5, countIsComplete: false, timedOut: true });
project('glob', 'legacy truncation', { filenames: ['a'], truncated: true });
for (const mode of ['content', 'count', 'files_with_matches']) {
  const base = { mode, numFiles: 0, filenames: [] };
  project('grep', `${mode} empty`, base);
  project('grep', `${mode} empty timeout`, { ...base, timedOut: true });
  project('grep', `${mode} past end timeout`, { ...base, appliedOffset: 9, numMatches: 1, totalFiles: 1, totalLines: 1, timedOut: true });
}
project('grep', 'content timeout with pagination', { mode: 'content', numFiles: 0, filenames: [], content: 'a:1:hit', numLines: 1, totalLines: 8, appliedLimit: 1, timedOut: true });
project('grep', 'count singular timeout', { mode: 'count', numFiles: 1, filenames: [], content: 'a:1', numMatches: 1, timedOut: true });
project('grep', 'files timeout', { numFiles: 2, filenames: ['a', 'b'], timedOut: true });

const headLimits = [];
for (const [limit, offset] of [[undefined, 0], [0, 2], [2, 0], [2, 4], [9, 1], [undefined, 300]]) {
  const input = { items: [1, 2, 3, 4, 5], ...(limit === undefined ? {} : { limit }), offset };
  context.INPUT = input;
  headLimits.push({ input, expected: JSON.parse(JSON.stringify(vm.runInContext('AJe(INPUT.items, INPUT.limit, INPUT.offset)', context))) });
}

// Execute only the binary's bundled ripgrep applet over disposable fixture
// files. The ordinary Claude entry point, account and model APIs are unused.
// Xar/Zar remain unchanged; the qX dependency supplies actual rg JSON events.
// Only each temporary root is mapped to the fixed fixture root in path fields.
const searches = [];
async function addSearch(name, contents, options, singleFile = false) {
  const directory = mkdtempSync(path.join(tmpdir(), 'file-search-322-'));
  const file = path.join(directory, 'a.txt');
  writeFileSync(file, contents);
  const context = vm.createContext({
    Buffer, JSON, ps: JSON.parse, zar: 250, Gar: ['.git', '.svn', '.hg', '.bzr', '.jj', '.sl'],
    gon: () => new Map(), cTr: () => [], vyo: () => false, HUn: () => false,
    iRe: async () => [], x$n: () => false, FAs: () => [], WW: '/',
    qX: async argv => {
      const processResult = spawnSync(binaryPath, [...argv, singleFile ? file : directory], {
        cwd: directory, env: { ...process.env, ARGV0: 'rg' }, encoding: 'utf8', timeout: 15000,
        maxBuffer: 1024 * 1024,
      });
      assert.ifError(processResult.error);
      assert([0, 1].includes(processResult.status), processResult.stderr);
      return processResult.stdout.split('\n').filter(Boolean).map(line => {
        const event = JSON.parse(line);
        if (event.data?.path?.text?.startsWith(directory + path.sep)) {
          event.data.path.text = root + '/' + event.data.path.text.slice(directory.length + 1);
        }
        return JSON.stringify(event);
      });
    },
    wpn: name => name.startsWith(root + '/') ? name.slice(root.length + 1) : name,
    P: (n, word) => n === 1 ? word : `${word}s`, NUn: () => false,
  });
  try {
    vm.runInContext(timeoutNotice + pagination + suffix + contentParser + contentSearch + lexicalPaths
      + `;var grep = {${grepMapper}};`, context);
    context.ROOT = singleFile ? root + '/a.txt' : root;
    context.SINGLE_FILE = singleFile;
    context.OPTIONS = { pattern: 'hit', output_mode: 'content', ...options };
    let result;
    try { result = await vm.runInContext(`Xar({canonical:ROOT,lexical:ROOT,target:ROOT,isDirectory:!SINGLE_FILE,
      relativeOutput:false,spawnCwd:ROOT,recheckBeforeSpawn(){}},OPTIONS,{permissions:()=>({})})`, context);
    } catch (error) { throw new Error(`Native search replay ${name}: ${error.message}`); }
    context.RESULT = result;
    const mapped = vm.runInContext(`grep.mapToolResultToToolResultBlockParam(RESULT.data,'tool-1')`, context);
    searches.push({ name, contents, options: { pattern: 'hit', output_mode: 'content', ...options }, singleFile,
      expected: JSON.parse(JSON.stringify(result.data)), modelText: mapped.content });
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}
for (const [name, text] of [['exactly 500 ASCII', 'a'.repeat(500)], ['501 ASCII', 'a'.repeat(501)],
  ['250 astral characters', '😀'.repeat(250)], ['251 astral characters', '😀'.repeat(251)],
  ['400 three-byte characters', '中'.repeat(400)]]) {
  await addSearch(name, text + '\n', { pattern: '.' });
}
await addSearch('context gaps', 'before\nhit\nafter\ngap\ngap\ngap\nlead\nhit\ntail\n', { '-C': 1 });
await addSearch('single file without filename', 'hit\n', {}, true);
await addSearch('single file context without line numbers', 'before\nhit\nafter\n', { '-C': 1, '-n': false }, true);
await addSearch('multiline normal output', 'first\nhit\nlast\n', { pattern: 'first\\nhit', multiline: true });
await addSearch('multiline only matching line offsets', 'first\nhit\nlast\n', { pattern: 'first\\nhit', multiline: true, '-o': true });
await addSearch('context pagination includes group break', 'before\nhit\nafter\ngap\ngap\ngap\nlead\nhit\ntail\n', { '-C': 1, head_limit: 2, offset: 3 });
await addSearch('unterminated double carriage return', 'hit\r\r', {});
await addSearch('empty matching line', '\n', { pattern: '^$' });
await addSearch('context override retains raw grouping gate', 'hit\ngap\nhit\n', { context: 0, '-B': 1 });

await addSearch('NUL separates matching lines and adds binary notice', 'hit\nnone\0hit\nhit\n', {});
await addSearch('single file binary notice', 'hit\0rest\n', {}, true);
await addSearch('NUL without matches stays empty', 'none\0rest\n', {});

const repeatedRoots = [];
for (const input of [
  { pattern: 'project/**/*.rs', root },
  { pattern: 'workspace/project/**/*.rs', root },
  { pattern: 'project/*.rs', root, directories: { [`${root}/project`]: 'directory' } },
  { pattern: 'project/*.rs', root, directories: { [`${root}/project`]: 'symlink' } },
  { pattern: '*.rs', root },
  { pattern: 'other/**/*.rs', root },
  { pattern: 'project/file.rs', root },
]) {
  context.INPUT = input;
  context.DIRECTORIES = input.directories ?? {};
  const expected = await vm.runInContext(`(async()=>{let candidate=Mar(INPUT.pattern,INPUT.root);
    return candidate!==INPUT.pattern&&!await Iar(INPUT.root,Oar(INPUT.pattern,candidate))?candidate:INPUT.pattern})()`, context);
  repeatedRoots.push({ input, expected });
}

const fixture = { schemaVersion: 1, claudeCodeVersion: '2.1.322', nativeSha256, sourceByteRanges, nativeProcessMode: 'ARGV0=rg',
  projections, headLimits, searches, repeatedRoots };
const fixturePath = new URL('./fixtures/file_search_322.json', import.meta.url);
const output = JSON.stringify(fixture, null, 2) + '\n';
if (args.includes('--check')) assert.equal(readFileSync(fixturePath, 'utf8'), output);
else writeFileSync(fixturePath, output);
console.log(`Claude Code 2.1.322 search oracle: ${projections.length} result projections, ${headLimits.length} pagination cases, ${searches.length} bundled-rg searches, ${repeatedRoots.length} root-prefix cases; ${sourceByteRanges.length} pinned ranges`);

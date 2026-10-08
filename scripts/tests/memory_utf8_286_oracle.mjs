// Execute actual 2.1.286 memory readers from the hash-pinned native binary.
// Usage: node scripts/tests/memory_utf8_286_oracle.mjs /path/to/2.1.286 [--check]
import { createHash } from 'node:crypto';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import * as fs from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const sha256 = createHash('sha256').update(binary).digest('hex');
if (sha256 !== '75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433') {
  throw new Error(`Unexpected native oracle SHA-256: ${sha256}`);
}
const sourceByteRanges = [];
function range(module, start, end, signature) {
  const bytes = binary.subarray(module + start, module + end);
  if (!bytes.toString('utf8').startsWith(signature)) throw new Error(`Native range moved: ${signature}`);
  sourceByteRanges.push({ signature, start: module + start, end: module + end,
    sha256: createHash('sha256').update(bytes).digest('hex') });
  return bytes.toString('utf8');
}
const l0 = range(177772099, 1238, 1376, 'async function l0(');
const QDn = range(183727495, 662368, 662907, 'async function QDn(');
const Hot = range(183727495, 662907, 663534, 'async function Hot(');
// Source-only evidence for rawContent and the absence of a native size field;
// the downstream markdown parser is deliberately not evaluated below.
range(183727495, 660924, 661413, 'function $ot(');
const constantSignature = 'var HDn=0.05,T7=4194304,jDn=40000;';
const constantStart = binary.indexOf(constantSignature, 183727495);
if (constantStart < 0 || constantStart > 184390402) throw new Error('Missing current memory byte limit');
const constants = range(constantStart, 0, constantSignature.length, constantSignature);
const slot = { skip: false };
let adapter, logEvents = [], healthEvents = [], readErrors = [];
const native = vm.runInNewContext(`${constants}\n${l0}\n${QDn}\n${Hot}\n({l0,QDn,Hot,byteLimit:T7})`, {
  Buffer,
  ae: () => adapter,
  // Hot's downstream markdown parser is outside this byte-decoding fixture.
  $ot: (body) => ({ info: { content: body }, includePaths: [] }),
  t: (message) => logEvents.push(message), Rwe: () => slot,
  p: (...args) => healthEvents.push(args), Uot: (code) => readErrors.push(code),
  XDn: (error) => readErrors.push(error.code),
});
const byteCases = [
  ['empty', []], ['ascii', Buffer.from('# rules\nhello\n')],
  ['valid-unicode', Buffer.from('中文😀\n')], ['bom-and-nul', [0xef,0xbb,0xbf,0x61,0,0x62]],
  ['isolated-continuations', [0x80,0xbf]], ['invalid-leading-bytes', [0xc0,0xc1,0xf5,0xff]],
  ['truncated-two-byte', [0xc2]], ['truncated-three-byte', [0xe2,0x82]],
  ['truncated-four-byte', [0xf0,0x9f,0x92]], ['overlong-two-byte', [0xc0,0xaf]],
  ['overlong-three-byte', [0xe0,0x80,0xaf]], ['overlong-four-byte', [0xf0,0x80,0x80,0xaf]],
  ['encoded-high-surrogate', [0xed,0xa0,0x80]], ['encoded-low-surrogate', [0xed,0xbf,0xbf]],
  ['out-of-range-scalar', [0xf4,0x90,0x80,0x80]], ['wrong-continuation', [0xe2,0x28,0xa1]],
  ['truncated-prefix-before-ascii', [0xf0,0x9f,0x41]],
  ['valid-scalar-boundary-and-replacement', [0xf4,0x8f,0xbf,0xbf,0xef,0xbf,0xbd]],
  ['invalid-parent-with-import', [...Buffer.from('parent '),0xff,...Buffer.from('\n@./child.md\n')]],
  ['invalid-child-with-import', [...Buffer.from('child '),0xed,0xa0,0x80,...Buffer.from('\n@./grandchild.md\n')]],
];
const dir = mkdtempSync(join(tmpdir(), 'memory-utf8-286-'));
const fixture = { native_version: '2.1.286', native_sha256: sha256, source_byte_ranges: sourceByteRanges,
  coverage: {
    actual_native_functions: ['l0', 'QDn', 'Hot'],
    controlled_seams: ['Node fs.stat/readFile utf8 and Buffer UTF-8 decoder replace native Bun builtins',
      'QDn backend supplies a padded Uint8Array view with real raw bytes and totalBytes',
      'Hot downstream $ot markdown parser returns captured decoded text; logging and health are recorded callbacks'],
    size_bytes: 'Rust local LoadedFile metadata uses raw bytes read; native $ot info has no size_bytes field',
  }, byte_limit: native.byteLimit, byte_cases: [], stat_cases: [], backend_cases: [] };
try {
  for (const [name, values] of byteCases) {
    const bytes = Buffer.from(values), path = join(dir, `${name}.md`);
    writeFileSync(path, bytes);
    adapter = { stat: fs.stat, readFile: fs.readFile };
    const l0Body = await native.l0(adapter, path, native.byteLimit);
    const padded = Buffer.concat([Buffer.from([0x61,0x62]), bytes, Buffer.from([0x63])]);
    const view = new Uint8Array(padded.buffer, padded.byteOffset + 2, bytes.length);
    const backend = { read: async () => ({ ok: true, value: { items: [{ found: true, totalBytes: bytes.length, value: view }] } }) };
    const backendBody = (await native.QDn({ backend, key: name })).content;
    const hotBody = (await native.Hot(path, 'Project', 'cwd')).info.content;
    const hotBackendBody = (await native.Hot(path, 'Project', 'cwd', { backend, key: name })).info.content;
    if (l0Body !== backendBody || l0Body !== hotBody || l0Body !== hotBackendBody) throw new Error(`Native readers disagree: ${name}`);
    fixture.byte_cases.push({ name, bytes: [...bytes], raw_size_bytes: bytes.length,
      decoded_utf8_bytes: Buffer.byteLength(l0Body), l0_body: l0Body, backend_body: backendBody,
      hot_body: hotBody, hot_backend_body: hotBackendBody });
  }
  for (const [name, size, directory] of [
    ['exact-limit', native.byteLimit, false], ['over-limit', native.byteLimit + 1, false], ['directory', 0, true],
  ]) {
    const path = join(dir, name);
    if (directory) await fs.mkdir(path); else writeFileSync(path, Buffer.alloc(size, 0x78));
    let reads = 0, skippedDirectory = null;
    adapter = { stat: fs.stat, readFile: async (...args) => { reads++; return fs.readFile(...args); } };
    const body = await native.l0(adapter, path, native.byteLimit, (stat) => { skippedDirectory = stat.isDirectory(); });
    fixture.stat_cases.push({ name, skipped: body === null, read_calls: reads,
      decoded_length: body?.length ?? null, callback_is_directory: skippedDirectory });
  }
  for (const [name, result] of [
    ['absent', { ok: true, value: { items: [{ found: false }] } }],
    ['over-limit', { ok: true, value: { items: [{ found: true, totalBytes: native.byteLimit + 1, value: new Uint8Array() }] } }],
    ['directory', { ok: false, error: { code: 'Io', telemetryCode: 'EISDIR' } }],
    ['read-error', { ok: false, error: { code: 'Io', telemetryCode: 'EACCES' } }],
  ]) fixture.backend_cases.push({ name, expected: await native.QDn({ key: name, backend: { read: async () => result } }) });
} finally { rmSync(dir, { recursive: true, force: true }); }
const output = `${JSON.stringify(fixture, null, 2)}\n`;
const destination = fileURLToPath(new URL('../../crates/memory/tests/fixtures/memory_utf8_2_1_286.json', import.meta.url));
if (process.argv.includes('--check')) {
  if (readFileSync(destination, 'utf8') !== output) throw new Error('Memory UTF-8 native fixture is stale');
} else writeFileSync(destination, output);
console.log(`Native memory UTF-8: ${fixture.byte_cases.length} byte + ${fixture.stat_cases.length} stat + ${fixture.backend_cases.length} backend cases`);

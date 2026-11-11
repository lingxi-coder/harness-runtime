#!/usr/bin/env node
// Execute pinned native helpers with controlled file I/O. No model or account calls.
import assert from 'node:assert/strict';
import { isUtf8 } from 'node:buffer';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import vm from 'node:vm';

const args = process.argv.slice(2);
assert(args.every(arg => !arg.startsWith('--') || arg === '--check'));
assert(args.filter(arg => !arg.startsWith('--')).length <= 1);
const binaryPath = args.find(arg => !arg.startsWith('--'))
  ?? path.join(homedir(), '.local/share/claude/versions/2.1.322');
const binary = readFileSync(binaryPath);
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
const nativeSha256 = sha256(binary);
assert.equal(nativeSha256, 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450');
const sourceByteRanges = [];
function take(name, start, end, digest, first, next) {
  const bytes = binary.subarray(start, end);
  assert(bytes.length > 0 && bytes.length < 5000, `${name}: bounded range`);
  assert.equal(sha256(bytes), digest, `${name}: digest`);
  assert(bytes.toString('utf8').startsWith(first), `${name}: first`);
  assert.equal(binary.subarray(end, end + Buffer.byteLength(next)).toString('utf8'), next);
  sourceByteRanges.push({ name, start, end, sha256: digest });
  return bytes.toString('utf8');
}
const classification = take('encoding classification', 261217236, 261217753,
  '08c1125b0d0337b040eb448b74986be39728a041dd25f6c04333cdcb769a4245', 'function KEe(n)', 'function gjr(n,e)');
const preserveBom = take('retain leading BOM', 261217753, 261217851,
  'b502cd543233551cd672cc79b43a3ca6808f16b408dbc96b2f7600f4b87efee0', 'function gjr(n,e)', 'function mW(n)');
const lineEndings = take('line ending classification', 261218080, 261218207,
  '5e716ff4858b4f8fb56ddb7baf6e1f317e79cefa3c88914bdeff5e4a827a9eec', 'function cGt(n)', 'function AKs(n,e,r)');
const metadata = take('decode and normalize file metadata', 261218686, 261218968,
  '226f9515fc671c95b4e123af6d1781b9cbff3dd67580d026d96da2a352f091e1', 'async function VUe(n,e,r)', '\nexport{k4e');
const messages = take('encoding refusal and read warning', 263771837, 263772888,
  '5ab6e7980cc55bfc1e88189b99e94588fffd213879d5a9c1fc8a3b2658437156', 'e={not_utf8:', 'class FT extends Error');
const quotes = take('quote and Unicode escape matching and replacement', 270518028, 270520911,
  'beafe26f990083c755ce6ecd197a2ab3ca4ab09d54f2ba3a01bb3667f2aa3bf6', 'var R="\\u2018"', 'function n0n(');
const encodeLineEndings = take('encode line endings', 261791593, 261791688,
  '14ea0ad5f724493d9a36e3bc99a24a81d547401e8430d54ca21122a1cbbd024d', 'function BUe(e,n)', 'async function JEt(');
const writeBytes = take('write and verify encoded bytes', 261791688, 261792557,
  '2489b09253a679c81cee508b1cc1a8ec3c64a1010e6274f0928f9bc37c08f349', 'async function JEt(', 'function Tua(');
const typeGuard = take('regular file guard', 261216306, 261216737,
  'd301dfe5fa2b50e094788c4cb0a447b07774dfe2f8748fda9de722ad1d7a514f', 'function k4e(', 'function Iua(');
const sizeGuard = take('metadata byte bound', 261216886, 261217045,
  '079766c037bdb4008743d964a42eea3f0ac527c68e53d854a6581bb63d93fb4a', 'function hLo(', 'function Ahe(');
const writeTargetValidation = take('Write nonregular target validation', 272394765, 272395116,
  'fc73796c7a1f0b054c17e0e9f21bee0bd15d981444ba6cc8310a9e84766b78f1', 'if(He=st.mtimeMs,st.isDirectory())', 'if(XEt(st.mode))');

// Dependencies are explicit: buffer.isUtf8, literal regex quoting, and a
// successful regular-file host. JEt writes into `written`, not the filesystem.
let written;
const context = vm.createContext({
  Buffer, Map, l: isUtf8, t: () => {},
  ts: value => value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'),
  UP: async (_file, content, { encoding }) => {
    written = Buffer.from(content, encoding); return 'atomic';
  },
  ae: () => ({ stat: async () => ({ size: written.length, mtimeMs: 123 }) }),
});
vm.runInContext(classification + preserveBom + lineEndings + typeGuard + sizeGuard + metadata
  + '\nvar ' + messages + quotes + encodeLineEndings + writeBytes, context);
const classifications = [];
const metadataCases = [];
async function addBytes(name, bytes) {
  const inputHex = bytes.toString('hex');
  const classified = context.got(bytes);
  const kind = classified.notText ?? null;
  classifications.push({ name, inputHex, expected: {
    encoding: classified.encoding, notText: kind,
    refusal: kind ? context.YBt(kind) : null,
    warning: kind ? context.Vra(kind) : null,
  } });
  const expected = await context.VUe({
    stat: async () => ({ isDirectory: () => false, isFile: () => true, size: bytes.length }),
    readFile: async () => bytes,
  }, '/oracle/file');
  metadataCases.push({ name, inputHex, expected: JSON.parse(JSON.stringify(expected)) });
}
await addBytes('empty', Buffer.alloc(0));
await addBytes('UTF8', Buffer.from('café 😀\n'));
await addBytes('UTF8 BOM CRLF', Buffer.from('\ufeffa\r\nb\r\n'));
await addBytes('UTF16LE BOM CRLF', Buffer.from('\ufeffa\r\nb', 'utf16le'));
await addBytes('UTF16LE keeps NUL and odd trailing byte', Buffer.concat([Buffer.from('\ufeffa\0', 'utf16le'), Buffer.from([255])]));
await addBytes('UTF16BE takes priority over NUL', Buffer.from([254, 255, 0, 97]));
await addBytes('invalid UTF8', Buffer.from([99, 97, 102, 233]));
await addBytes('NUL takes priority over invalid UTF8', Buffer.from([0, 255]));
await addBytes('bare CR remains literal', Buffer.from('a\rb\rc\r'));
await addBytes('CRLF tie prefers LF', Buffer.from('a\r\nb\nc'));
await addBytes('NUL last classified byte', Buffer.concat([Buffer.alloc(7999, 97), Buffer.from([0])]));
await addBytes('NUL outside classification window', Buffer.concat([Buffer.alloc(8000, 97), Buffer.from([0])]));
await addBytes('invalid UTF8 outside NUL window', Buffer.concat([Buffer.alloc(8000, 97), Buffer.from([255])]));
await addBytes('line ending sample ignores later majority', Buffer.from('a\r\n' + 'x'.repeat(4093) + '\n'.repeat(10)));
await addBytes('UTF16 sample counts astral units', Buffer.from('😀'.repeat(2047) + '\r\n' + '\n'.repeat(10)));

const writes = [];
for (const [name, original, replacement] of [
  ['UTF8 ordinary', Buffer.from('a\r\nb'), 'c\r\nd'],
  ['UTF8 BOM retained', Buffer.from('\ufeffold'), 'new'],
  ['UTF8 BOM not doubled', Buffer.from('\ufeffold'), '\ufeffnew'],
  ['UTF8 empty replacement removes BOM', Buffer.from('\ufeffold'), ''],
  ['UTF16LE replacement retains encoding and BOM', Buffer.from('\ufeffold', 'utf16le'), 'café 😀\n'],
  ['UTF16LE empty replacement', Buffer.from('\ufeffold', 'utf16le'), ''],
]) {
  const encoding = context.got(original).encoding;
  const originalFile = original.toString(encoding).replaceAll('\r\n', '\n');
  const content = context.gjr(originalFile, replacement);
  await context.JEt('/oracle/file', content, encoding, 'LF');
  writes.push({ name, originalHex: original.toString('hex'), replacement,
    expected: { content, originalFile, bytesHex: written.toString('hex') } });
}
const edits = [];
for (const [name, file, old, replacement] of [
  ['literal', 'a "old" b', '"old"', '"new"'],
  ['curly double quotes', 'a “old” b', '"old"', 'say "new"'],
  ['literal to escaped style', 'x = \\u00E9;', 'é', 'é Ω'],
  ['escape to literal style', 'x = é;', '\\u00e9', '\\u03a9'],
  ['astral to escaped style', 'x = \\uD83D\\uDE00;', '😀', '😀😃'],
  ['surrogate pair escapes to literal', 'x = 😀;', '\\ud83d\\ude00', '\\ud83d\\ude03'],
  ['escaped backslash boundary is not a match', 'x = \\\\u00e9;', 'é', 'Ω'],
  ['trailing backslash cannot consume escape prefix', 'x = \\u00e9\\u0000;', 'é\\', 'Ω'],
]) {
  const actual = context.sGe(file, old);
  const transformed = actual === null ? null : context.qSr(old, actual, context.J0t(old, actual, replacement));
  edits.push({ name, file, old, replacement, expected: {
    actual, replacement: transformed,
    updated: actual === null ? null : context.q(file, actual, transformed),
  } });
}
const writeTargets = ['directory', 'socket', 'regular'].map(kind => {
  const expected = vm.runInNewContext(`(() => {let He;${writeTargetValidation}return {result:true}})()`, {
    h: '/oracle/target', Uc: value => value,
    st: { mtimeMs: 123, isDirectory: () => kind === 'directory', isFile: () => kind === 'regular' },
  });
  return { kind, path: '/oracle/target', expected };
});
const fixture = { schemaVersion: 1, claudeCodeVersion: '2.1.322', nativeSha256,
  sourceByteRanges, classifications, metadata: metadataCases, writes, edits, writeTargets };
const output = new URL('./fixtures/file_mutation_322.json', import.meta.url);
const bytes = JSON.stringify(fixture, null, 2) + '\n';
if (args.includes('--check')) assert.equal(readFileSync(output, 'utf8'), bytes);
else writeFileSync(output, bytes);
process.stdout.write(`${classifications.length} classifications, ${metadataCases.length} metadata, ${writes.length} writes, ${edits.length} edits, ${writeTargets.length} target validations; ${sourceByteRanges.length} pinned ranges verified\n`);

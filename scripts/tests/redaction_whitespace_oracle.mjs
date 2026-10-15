// Hash-pinned ud whitespace oracle. No upstream source is vendored.
// Usage: node scripts/tests/redaction_whitespace_oracle.mjs /path/to/2.1.286 [--check]
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const sha256 = createHash('sha256').update(binary).digest('hex');
const expectedSha = '75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433';
if (sha256 !== expectedSha) throw new Error(`Unexpected oracle SHA-256: ${sha256}`);

function extract(signature, startMarker, endMarker) {
  const anchor = binary.indexOf(signature);
  const start = binary.lastIndexOf(startMarker, anchor);
  const end = binary.indexOf(endMarker, anchor);
  if (anchor < 0 || start < 0 || end < start || end - start > 20_000) throw new Error(`Missing native region: ${signature}`);
  const bytes = binary.subarray(start, end);
  return { signature, start, end, sourceSha256: createHash('sha256').update(bytes).digest('hex'), source: bytes.toString('utf8') };
}
const unicode = extract('function zbe(e){return e.replace', 'var u=', 'var p=');
const redactor = extract('function ud(t){let a=_(t,o);return u.test(t)?_(a,p):a}', 'var E=', '\nexport{ud};');
const dependencies = vm.runInNewContext(`(()=>{${unicode.source};return {fpe,q2e,zbe};})()`, Object.create(null), { timeout: 1_000 });
const oracle = vm.runInNewContext(`${redactor.source};({redact:ud,firstPass:input=>_(input,o),invisiblePass:input=>u.test(input)})`, dependencies, { timeout: 1_000 });

const whitespace = [0x9, 0xa, 0xb, 0xc, 0xd, 0x20, 0xa0, 0x1680, ...Array.from({ length: 11 }, (_, index) => 0x2000 + index), 0x2028, 0x2029, 0x202f, 0x205f, 0x3000, 0xfeff];
const forms = {
  bearer_value: c => `Bearer one${c}two`,
  basic_value: c => `Authorization: Basic one${c}two`,
  key_value: c => `password=one${c}two`,
  bearer_separator: c => `Bearer${c}secret`,
  basic_separator: c => `Authorization: Basic${c}secret`,
  basic_colon: c => `Authorization:${c}Basic secret`,
  key_separator: c => `password=${c}secret`,
  key_colon: c => `password${c}=secret`,
  url_password: c => `https://u:one${c}two@host.example/path`,
  url_credential_after: c => `https://u:p@host.example/path${c}other`,
  url_password_after_at: c => `https://u:p@ss${c}two@host.example/path`,
};
const cases = [];
function add(id, input, strongerExpected, reason) {
  const nativeExpected = oracle.redact(input);
  const rustExpected = strongerExpected ?? nativeExpected;
  if (reason === undefined && rustExpected !== nativeExpected) throw new Error(`Undocumented divergence: ${id}`);
  if (reason !== undefined && rustExpected === nativeExpected) throw new Error(`Stale divergence: ${id}`);
  cases.push({ id, input, nativeExpected, firstPassExpected: oracle.firstPass(input), invisiblePass: oracle.invisiblePass(input), rustExpected, alignment: reason === undefined ? 'exact' : 'strengthened', ...(reason === undefined ? {} : { reason }) });
}
for (const codePoint of [...whitespace, 0x85]) {
  const c = String.fromCodePoint(codePoint);
  for (const [form, make] of Object.entries(forms)) {
    const id = `U+${codePoint.toString(16).toUpperCase().padStart(4, '0')}/${form}`;
    // Keep the runtime's existing stricter Basic header handling. Upstream
    // leaves these NEL-separated credentials visible; copying that is unsafe.
    if (codePoint === 0x85 && form === 'basic_separator') {
      add(id, make(c), `Authorization: Basic${c}[REDACTED]`, 'Existing runtime accepts U+0085 as a Basic separator and masks its credential; native ud leaves it visible.');
    } else if (codePoint === 0x85 && form === 'basic_colon') {
      add(id, make(c), `Authorization:${c}Basic [REDACTED]`, 'Existing runtime accepts U+0085 before Basic and masks its credential; native ud leaves it visible.');
    } else add(id, make(c));
  }
}

for (const [id, input] of [
  ['leading-nel-basic', 'Authorization: Basic \u0085secret'],
  ['leading-nel-bearer', 'Bearer \u0085secret'],
  ['invisible-basic-bom', 'Authorization: B\u200Basic\uFEFFsecret'],
  ['invisible-bearer-nel', 'B\u200Bearer one\u0085two'],
  ['invisible-basic-nel', 'Authorization: B\u200Basic one\u0085two'],
  ['leading-nel-bom-key', 'password=\u0085\uFEFFsecret'],
]) add(id, input);

for (const [id, input, rustExpected] of [
  ['quoted-key', 'password="hello world"', 'password=[REDACTED]'],
  ['bearer-key', 'password=Bearer secret', 'password=[REDACTED]'],
  ['basic-key', 'password=Basic secret', 'password=[REDACTED]'],
  ['bearer-key-nel', 'password=Bearer one\u0085two', 'password=[REDACTED]'],
  ['basic-key-nel', 'password=Basic one\u0085two', 'password=[REDACTED]'],
  ['leading-nel-bearer-key', 'password=\u0085Bearer secret', 'password=[REDACTED]'],
  ['leading-nel-quoted-key', 'password=\u0085"hello world"', 'password=[REDACTED]'],
  ['leading-bom-bearer-key', 'password=\uFEFFBearer secret', 'password=\uFEFF[REDACTED]'],
  ['ssh-slash-password', 'ssh://user:p/ss@[::1]/path', 'ssh://***:***@[::1]/path'],
]) add(id, input, rustExpected, 'Preserve existing runtime security strengthening: consume a complete quoted/scheme-prefixed assignment or slash-bearing SSH credentials rather than expose the remainder.');

const fixture = {
  version: '2.1.286', sha256,
  sourceByteRanges: [unicode, redactor].map(({ source, ...provenance }) => provenance),
  scope: 'ud error-text surface only. Includes the complete ECMAScript whitespace table plus U+0085 across 11 forms (286 cases), invisible second-pass evidence, and documented stronger runtime cases. This is source evaluation, not a live CLI/model run; Rust execution is a separate test.',
  whitespaceCodePoints: whitespace.map(codePoint => `U+${codePoint.toString(16).toUpperCase().padStart(4, '0')}`),
  exactCases: cases.filter(testCase => testCase.alignment === 'exact').length,
  strengthenedCases: cases.filter(testCase => testCase.alignment === 'strengthened').length,
  cases,
};
const output = `${JSON.stringify(fixture, null, 2)}\n`;
const path = fileURLToPath(new URL('../../crates/llm-runtime/tests/fixtures/claude_2_1_286_redaction_whitespace.json', import.meta.url));
if (process.argv.includes('--check')) {
  if (readFileSync(path, 'utf8') !== output) throw new Error('Redaction whitespace fixture differs');
  console.log(`Redaction whitespace oracle ${fixture.version}: ${fixture.exactCases} exact, ${fixture.strengthenedCases} explicitly strengthened expectations (${sha256})`);
} else {
  writeFileSync(path, output);
  console.log(path);
}

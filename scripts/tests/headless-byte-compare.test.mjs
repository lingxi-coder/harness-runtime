import test from 'node:test';
import assert from 'node:assert/strict';
import { compareBuffers, createComparisonContext, firstByteDifference, NormalizationError } from './headless-byte-compare.mjs';

const b = (text) => Buffer.from(text);
const idA = '11111111-1111-4111-8111-111111111111';
const idB = '22222222-2222-4222-8222-222222222222';
const idC = '33333333-3333-4333-8333-333333333333';
const idD = '44444444-4444-4444-8444-444444444444';
const identity = (path = '/session_id', extra = {}) => ({ id: 'session', path, type: 'string', mode: 'identity', group: 'session', count: 1, validate: 'uuid', ...extra });
const compareJson = (left, right, rules = [], extra = {}) => compareBuffers(b(left), b(right), { format: 'json', rules, ...extra });
const rejected = (fn, text) => assert.throws(fn, (error) => error instanceof NormalizationError && error.message.includes(text));

test('raw comparison distinguishes equality from normalized equality', () => {
  const raw = compareBuffers(b('hello\n'), b('hello\n'));
  assert.equal(raw.rawEqual, true);
  assert.equal(raw.normalizedEqual, true);
  assert.equal(raw.difference, null);
  const normalized = compareJson(`{"session_id":"${idA}"}`, `{"session_id":"${idB}"}`, [identity()]);
  assert.equal(normalized.rawEqual, false);
  assert.equal(normalized.normalizedEqual, true);
  assert.equal(normalized.equal, true);
  assert.notEqual(normalized.rawDifference, null);
  assert.equal(normalized.difference, null);
  assert.equal(normalized.transformations.left.length, 1);
  assert.equal(normalized.transformations.right[0].originalHex, b(`"${idB}"`).toString('hex'));
});

test('object key order remains significant', () => {
  assert.equal(compareJson('{"a":1,"b":2}', '{"b":2,"a":1}').equal, false);
  assert.equal(compareJson(`{"session_id":"${idA}","a":1}`, `{"a":1,"session_id":"${idB}"}`, [identity()]).equal, false);
});

test('duplicate keys and their order remain visible', () => {
  assert.equal(compareJson('{"a":1,"a":2}', '{"a":2,"a":1}').equal, false);
  assert.equal(compareJson('{"a":1,"a":2}', '{"a":2}').equal, false);
  assert.equal(compareJson('{"a":1,"a":2}', '{"a":1,"a":2}').equal, true);
});

test('null, omitted, false, zero and empty string remain distinct', () => {
  for (const value of ['null', 'false', '0', '""']) assert.equal(compareJson(`{"a":${value}}`, '{}').equal, false);
  assert.equal(compareJson('{"a":null}', '{"a":""}').equal, false);
});

test('ordinary numeric spelling and negative zero are byte significant', () => {
  for (const [left, right] of [['1', '1.0'], ['1e2', '100'], ['-0', '0'], ['1E+2', '1e2']]) assert.equal(compareJson(`{"n":${left}}`, `{"n":${right}}`).equal, false);
  assert.equal(compareJson('{"n":1e400}', '{"n":1e400}').equal, true);
});

test('escaped spelling, solidus and UTF-16 surrogate leaves survive unchanged', () => {
  const left = '{"body":"\\ud800|\\u0061|\\/|\\ud83d\\ude00"}';
  assert.equal(compareJson(left, left).equal, true);
  assert.equal(compareJson(left, '{"body":"\\ud800|a|/|😀"}').equal, false);
  const input = `{"session_id":"${idA}","\\ud800":"\\udfff","body":"\\u0061"}\r\n`;
  const result = compareJson(input, input.replace(idA, idB), [identity()]);
  assert.equal(result.equal, true);
  assert.equal(result.normalized.left.toString(), input.replace(`"${idA}"`, '"@identity:session:0"'));
  assert.equal(result.transformations.left[0].path, '/session_id');
});

test('invalid UTF-8 is rejected rather than converted to replacement characters', () => {
  const invalid = Buffer.concat([b('{"body":"'), Buffer.from([0xff]), b('"}')]);
  rejected(() => compareBuffers(invalid, invalid, { format: 'json' }), 'invalid string or UTF-8');
  assert.equal(compareBuffers(invalid, invalid).equal, true);
  const invalidLiteral = Buffer.from([0x74, 0xf2, 0xf5, 0xe5]);
  rejected(() => compareBuffers(invalidLiteral, invalidLiteral, { format: 'json' }), 'invalid literal');
});

test('a UUID in user text is never scrubbed by an identity path rule', () => {
  const result = compareJson(`{"session_id":"${idA}","body":"user ${idA}"}`, `{"session_id":"${idB}","body":"user ${idB}"}`, [identity()]);
  assert.equal(result.equal, false);
  assert.ok(result.normalized.left.includes(b(`user ${idA}`)));
  assert.ok(result.normalized.right.includes(b(`user ${idB}`)));
});

test('unknown dynamic fields remain significant', () => {
  const result = compareJson(`{"session_id":"${idA}","new_timestamp":1}`, `{"session_id":"${idB}","new_timestamp":2}`, [identity()]);
  assert.equal(result.equal, false);
  assert.equal(result.transformations.left.length, 1);
});

test('identity mapping is shared across event, stdout and session channels', () => {
  const context = createComparisonContext();
  for (const channel of ['event', 'stdout', 'session']) {
    const result = compareJson(`{"session_id":"${idA}"}`, `{"session_id":"${idB}"}`, [identity()], { context, channel });
    assert.equal(result.equal, true);
    assert.equal(result.normalized.left.toString(), '{"session_id":"@identity:session:0"}');
  }
  assert.equal(context.identities.get('session').left.size, 1);
});

test('one left identity cannot correlate to two right identities', () => {
  const context = createComparisonContext();
  compareJson(`{"session_id":"${idA}"}`, `{"session_id":"${idB}"}`, [identity()], { context, channel: 'stdout' });
  rejected(() => compareJson(`{"session_id":"${idA}"}`, `{"session_id":"${idC}"}`, [identity()], { context, channel: 'session' }), 'not a bijection');
});

test('two left identities cannot collapse into one right identity', () => {
  const context = createComparisonContext();
  compareJson(`{"session_id":"${idA}"}`, `{"session_id":"${idB}"}`, [identity()], { context });
  rejected(() => compareJson(`{"session_id":"${idC}"}`, `{"session_id":"${idB}"}`, [identity()], { context }), 'not a bijection');
});

test('repeated identities are accepted only with matching correlation', () => {
  const rows = (a, c) => `{"session_id":"${a}"}\n{"session_id":"${c}"}\n`;
  const options = { format: 'ndjson', rules: [identity('/session_id', { count: 2 })] };
  assert.equal(compareBuffers(b(rows(idA, idA)), b(rows(idB, idB)), options).equal, true);
  rejected(() => compareBuffers(b(rows(idA, idA)), b(rows(idB, idC)), options), 'not a bijection');
});

test('different identity groups do not accidentally share a map', () => {
  const result = compareJson(`{"session":"${idA}","message":"${idA}"}`, `{"session":"${idB}","message":"${idC}"}`, [identity('/session'), identity('/message', { id: 'message', group: 'message' })]);
  assert.equal(result.equal, true);
});

test('invalid UUIDs, empty identities and numeric identities fail validation', () => {
  rejected(() => compareJson('{"session_id":"not-a-uuid"}', `{"session_id":"${idB}"}`, [identity()]), 'Invalid UUID');
  rejected(() => compareJson('{"session_id":""}', '{"session_id":"x"}', [identity('/session_id', { validate: undefined })]), 'nonempty');
  rejected(() => compareJson('{"session_id":1}', `{"session_id":"${idB}"}`, [identity()]), 'type mismatch');
});

test('missing, extra and duplicate replacement hits fail closed', () => {
  rejected(() => compareJson('{}', `{"session_id":"${idB}"}`, [identity()]), 'hit count');
  rejected(() => compareJson(`{"session_id":"${idA}","session_id":"${idA}"}`, `{"session_id":"${idB}"}`, [identity()]), 'hit count');
  rejected(() => compareJson(`{"session_id":"${idA}"}`, `{"session_id":"${idB}"}`, [identity('/session_id', { count: 2 })]), 'hit count');
});

test('type mismatch fails even when raw bytes are identical', () => {
  rejected(() => compareJson('{"duration":null}', '{"duration":null}', [{ id: 'duration', path: '/duration', count: 1, type: 'number', mode: 'value' }]), 'type mismatch');
});

test('JSON pointer escapes and array indices select only their exact paths', () => {
  const left = `{"a/b":{"~x":[{"id":"${idA}"}]},"id":"literal"}`;
  const right = left.replace(idA, idB);
  assert.equal(compareJson(left, right, [identity('/a~1b/~0x/0/id')]).equal, true);
  assert.equal(compareJson(left, right, [identity(['a/b', '~x', 0, 'id'])]).equal, true);
  rejected(() => compareJson(left, right, [identity('/a~2b/~0x/0/id')]), 'pointer escape');
  rejected(() => compareJson(left, right, [identity(['a/b', -1])]), 'nonnegative');
  rejected(() => compareJson(left, right, [identity('/a~1b/~0x/*/id')]), 'hit count');
});

test('NDJSON record selection and type selector scope normalization', () => {
  const left = `{"type":"system","session_id":"fixed"}\n{"type":"result","session_id":"${idA}"}\n`;
  const right = left.replace(idA, idB);
  for (const selector of [{ records: [1] }, { match: { path: '/type', equals: 'result' } }]) {
    const result = compareBuffers(b(left), b(right), { format: 'ndjson', rules: [identity('/session_id', selector)] });
    assert.equal(result.equal, true);
    assert.equal(result.transformations.left[0].record, 1);
  }
  rejected(() => compareBuffers(b(left), b(right), { format: 'ndjson', rules: [identity('/session_id', { records: [2] })] }), 'does not exist');
});

test('ambiguous duplicate key record selectors fail closed', () => {
  const row = `{"type":"system","type":"result","session_id":"${idA}"}`;
  rejected(() => compareJson(row, row.replace(idA, idB), [identity('/session_id', { match: { path: '/type', equals: 'result' } })]), 'ambiguous');
});

test('newline, CRLF, whitespace and final newline differences remain visible', () => {
  for (const [left, right, format] of [['{"a":1}\n', '{"a":1}', 'ndjson'], ['{"a":1}\r\n', '{"a":1}\n', 'ndjson'], ['{"a":1}', '{ "a":1 }', 'json'], ['answer\n', 'answer', 'raw']]) {
    assert.equal(compareBuffers(b(left), b(right), { format }).equal, false);
  }
  const row = `{"session_id":"${idA}"}\n`;
  assert.equal(compareBuffers(b(row), b(row.replace(idA, idB).slice(0, -1)), { format: 'ndjson', rules: [identity()] }).equal, false);
});

test('JSON validation rejects malformed or incomplete records without dropping them', () => {
  for (const invalid of ['{"a":1,}', '[1,]', '{"a":01}', '{"a":"\\q"}', '{"a":"\\u12xz"}', '{"a":"unterminated}', '{}extra', '']) {
    rejected(() => compareJson(invalid, invalid), 'Invalid JSON');
  }
  rejected(() => compareBuffers(b('{}\n\n'), b('{}\n\n'), { format: 'ndjson' }), 'Invalid JSON');
  assert.equal(compareBuffers(b(''), b(''), { format: 'ndjson' }).equal, true);
});

test('explicit dynamic number rule records the original lexeme and validates it', () => {
  const rule = { id: 'duration', path: '/duration', mode: 'value', type: 'number', count: 1, validate: 'nonnegative-number' };
  const result = compareJson('{"duration":1.00e2,"cost":1.0}', '{"duration":12,"cost":1.0}', [rule]);
  assert.equal(result.equal, true);
  assert.equal(result.normalized.left.toString(), '{"duration":0,"cost":1.0}');
  assert.equal(result.transformations.left[0].originalHex, b('1.00e2').toString('hex'));
  rejected(() => compareJson('{"duration":-1}', '{"duration":2}', [rule]), 'failed validation');
  rejected(() => compareJson('{"duration":1e400}', '{"duration":2}', [rule]), 'finite');
});

test('exact JSON fixture replacement requires expected values', () => {
  const rule = { id: 'cwd', path: '/cwd', mode: 'exact', type: 'string', count: 1, left: '/tmp/native', right: '/tmp/harness' };
  assert.equal(compareJson('{"cwd":"/tmp/native","body":"/tmp/native"}', '{"cwd":"/tmp/harness","body":"/tmp/native"}', [rule]).equal, true);
  rejected(() => compareJson('{"cwd":"/tmp/unexpected"}', '{"cwd":"/tmp/harness"}', [rule]), 'value mismatch');
});

test('overlapping rules fail and leave identity context unchanged', () => {
  const context = createComparisonContext();
  const left = `{"session_id":"${idA}"}`;
  const right = `{"session_id":"${idB}"}`;
  rejected(() => compareJson(left, right, [identity(), identity('/session_id', { id: 'duplicate' })], { context }), 'overlap');
  assert.equal(context.identities.size, 0);
  assert.equal(compareJson(left, right.replace(idB, idC), [identity()], { context }).equal, true);
});

test('a later failed rule rolls back earlier newly discovered identities', () => {
  const context = createComparisonContext();
  const rules = [identity(), { id: 'missing', path: '/missing', mode: 'value', type: 'number', count: 1 }];
  rejected(() => compareJson(`{"session_id":"${idA}"}`, `{"session_id":"${idB}"}`, rules, { context }), 'hit count');
  assert.equal(context.identities.size, 0);
  assert.equal(compareJson(`{"session_id":"${idA}"}`, `{"session_id":"${idC}"}`, [identity()], { context }).equal, true);
});

test('previous identity mappings also survive a failed transaction', () => {
  const context = createComparisonContext();
  compareJson(`{"session_id":"${idA}"}`, `{"session_id":"${idB}"}`, [identity()], { context });
  rejected(() => compareJson(`{"session_id":"${idC}","other":0}`, `{"session_id":"${idD}","other":0}`, [identity(), { id: 'other', path: '/other', mode: 'value', type: 'string', count: 1 }], { context }), 'type mismatch');
  assert.equal(context.identities.get('session').left.size, 1);
  rejected(() => compareJson(`{"session_id":"${idA}"}`, `{"session_id":"${idD}"}`, [identity()], { context }), 'not a bijection');
});

test('raw normalization allows only explicit exact fixture roots and loopback ports', () => {
  const rules = [{ id: 'root', kind: 'text', category: 'fixture-root', left: '/tmp/native', right: '/tmp/harness', count: 1 }, { id: 'port', kind: 'text', category: 'port', left: '127.0.0.1:1234', right: '127.0.0.1:5678', count: 1 }];
  const result = compareBuffers(b(`path=/tmp/native/file host=127.0.0.1:1234 UUID=${idA}\n`), b(`path=/tmp/harness/file host=127.0.0.1:5678 UUID=${idA}\n`), { rules });
  assert.equal(result.equal, true);
  assert.equal(result.rawEqual, false);
  assert.equal(result.transformations.left.length, 2);
  assert.ok(result.normalized.left.includes(b(idA)));
  rejected(() => compareBuffers(b(idA), b(idB), { rules: [{ id: 'uuid', kind: 'text', category: 'identity', left: idA, right: idB, count: 1 }] }), 'only explicit');
});

test('text token boundaries prevent directory and port prefix replacement', () => {
  const root = { id: 'root', kind: 'text', category: 'fixture-root', left: '/tmp/a', right: '/tmp/b', count: 1 };
  const port = { id: 'port', kind: 'text', category: 'port', left: ':1234', right: ':5678', count: 1 };
  rejected(() => compareBuffers(b('/tmp/abc'), b('/tmp/bcd'), { rules: [root] }), 'hit count');
  rejected(() => compareBuffers(b('/tmp/a汉字'), b('/tmp/b汉字'), { rules: [root] }), 'hit count');
  rejected(() => compareBuffers(b('host:12345'), b('host:56789'), { rules: [port] }), 'hit count');
  assert.equal(compareBuffers(b('host:1234/path'), b('host:5678/path'), { rules: [port] }).equal, true);
});

test('raw UUID user text differences remain visible with fixture rules', () => {
  const rule = { id: 'root', kind: 'text', category: 'fixture-root', left: '/tmp/a', right: '/tmp/b', count: 1 };
  const result = compareBuffers(b(`/tmp/a ${idA}`), b(`/tmp/b ${idB}`), { rules: [rule] });
  assert.equal(result.equal, false);
});

test('text rules cannot modify JSON even if the token is a fixture root', () => {
  rejected(() => compareJson('{"cwd":"/tmp/a"}', '{"cwd":"/tmp/b"}', [{ id: 'root', kind: 'text', category: 'fixture-root', left: '/tmp/a', right: '/tmp/b', count: 1 }]), 'cannot modify JSON');
});

test('channel-scoped rules do not hide a value in another channel', () => {
  const rule = identity('/session_id', { channel: 'stdout' });
  assert.equal(compareJson(`{"session_id":"${idA}"}`, `{"session_id":"${idB}"}`, [rule], { channel: 'session' }).equal, false);
  assert.equal(compareJson(`{"session_id":"${idA}"}`, `{"session_id":"${idB}"}`, [rule], { channel: 'stdout' }).equal, true);
});

test('rule schema rejects omitted counts, duplicate IDs, invalid paths and types', () => {
  const row = `{"session_id":"${idA}"}`;
  for (const extra of [{ count: undefined }, { count: -1 }, { path: undefined }, { type: 'object' }, { mode: 'regex' }, { validate: 'anything' }, { records: [0, 0] }, { validate: 'integer' }]) assert.throws(() => compareJson(row, row, [identity('/session_id', extra)]), NormalizationError);
  rejected(() => compareJson(row, row, [identity(), identity()]), 'Duplicate rule id');
  rejected(() => compareJson(row, row, [identity('/session_id', { channel: 123 })]), 'Invalid rule channel');
  rejected(() => compareJson(row, row, [identity('/session_id', { pathh: '/missing' })]), 'Unknown normalization rule property');
  rejected(() => compareJson(row, row, [identity('/session_id', { channel: 'another-channel', count: undefined })]), 'hit count');
});

test('ISO timestamp and integer validators reject invalid dates and fractional PIDs', () => {
  const timestamp = { id: 'timestamp', path: '/timestamp', mode: 'value', type: 'string', count: 1, validate: 'iso-timestamp' };
  assert.equal(compareJson('{"timestamp":"2026-10-07T12:34:56.123456Z"}', '{"timestamp":"2026-10-07T12:35:56Z"}', [timestamp]).equal, true);
  rejected(() => compareJson('{"timestamp":"2026-02-30T00:00:00Z"}', '{"timestamp":"2026-10-07T00:00:00Z"}', [timestamp]), 'Invalid ISO timestamp');
  const pid = { id: 'pid', path: '/pid', mode: 'value', type: 'number', count: 1, validate: 'integer' };
  rejected(() => compareJson('{"pid":1.5}', '{"pid":123}', [pid]), 'failed validation');
});

test('first difference reports byte offset, lengths, hex/text context and EOF', () => {
  const diff = firstByteDifference(b('éx\n'), b('éy\n'), 2);
  assert.equal(diff.offset, 2);
  assert.equal(diff.left.byte, 0x78);
  assert.equal(diff.right.byte, 0x79);
  assert.equal(diff.left.hex, 'c3a9780a');
  assert.equal(diff.left.text, 'éx\n');
  const eof = firstByteDifference(b('x'), b('x\n'));
  assert.equal(eof.offset, 1);
  assert.equal(eof.left.byte, null);
  assert.equal(eof.right.byte, 0x0a);
  assert.equal(firstByteDifference(b(''), b('')), null);
});

test('comparison accepts only Buffers and created contexts', () => {
  assert.throws(() => compareBuffers('{}', b('{}')), TypeError);
  rejected(() => compareBuffers(b('{}'), b('{}'), { context: {} }), 'createComparisonContext');
});

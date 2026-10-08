import test from 'node:test';
import assert from 'node:assert/strict';
import {compareBuffers, createComparisonContext, NormalizationError} from '../headless-byte-compare.mjs';
import {jsonScalarSpans, compareMetadataWrapped, compareHttpWrapped, splitHttpRequest, resolvePidSocketRules, mapForkSessionFiles, compareForkSessionHeaders} from './normalization-adapter.mjs';
const first = 'a'.repeat(64), second = 'b'.repeat(64), third = 'c'.repeat(64);
const session = '11111111-1111-4111-8111-111111111111';
const metadata = {id: 'metadata-device', outerPath: '/metadata/user_id', innerPath: '/device_id', count: 1, group: 'device'};
const options = () => ({context: createComparisonContext(), channel: 'provider/request-body', metadata});
const body = (device, account = '', extra = '') => Buffer.from(`{"metadata":{"user_id":${JSON.stringify(`{"device_id":"${device}","account_uuid":"${account}","session_id":"${session}"}`)}},"text":"keep ${first}","escaped":"\\ud800|\\u0061|\\/"${extra}}\r\n`);
const http = bytes => Buffer.concat([Buffer.from(`POST /v1/messages?beta=true HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nContent-Length: ${bytes.length}\r\n\r\n`), bytes]);

test('independent lexer retains byte spans duplicate keys and NDJSON record indices', () => {
  const bytes = Buffer.from('{"a":"\\ud800","a":"😀","n":-0}\r\n{"a":null}\n');
  const spans = jsonScalarSpans(bytes, 'ndjson');
  assert.deepEqual(spans.map(row => [row.path, row.record, row.type]), [['/a', 0, 'string'], ['/a', 0, 'string'], ['/n', 0, 'number'], ['/a', 1, 'null']]);
  assert.equal(bytes.subarray(spans[0].start, spans[0].end).toString(), '"\\ud800"');
  assert.ok(Object.is(spans[2].value, -0));
  for (const invalid of ['{"a":}', '{"a":"\\x"}', '[1,]', '{"a":1}junk', '']) assert.throws(() => jsonScalarSpans(Buffer.from(invalid)), NormalizationError);
});

test('nested device leaf normalization preserves all surrounding original spellings', () => {
  const left = body(first), right = body(second);
  const result = compareMetadataWrapped(left, right, options());
  assert.equal(result.rawEqual, false); assert.equal(result.equal, true);
  assert.equal(result.transformations.left.length, 1);
  const row = result.transformations.left[0];
  assert.equal(row.path, '/metadata/user_id::/device_id');
  assert.equal(left.subarray(row.start, row.end).toString('hex'), row.originalHex);
  assert.equal(result.normalized.left.subarray(0, row.start).toString(), left.subarray(0, row.start).toString());
  assert.ok(result.normalized.left.includes(Buffer.from(`"text":"keep ${first}"`)));
  assert.ok(result.normalized.left.includes(Buffer.from('"escaped":"\\ud800|\\u0061|\\/"')));
  assert.ok(result.normalized.left.toString().endsWith('\r\n'));
});

test('escaped outer quote spelling outside the device scalar remains significant', () => {
  const left = body(first);
  // An outer escaped quote in an inner property name has the same decoded
  // meaning but differs lexically. It is not within the selected scalar span.
  const right = Buffer.from(body(second).toString().replace('\\"account_uuid\\"', '\\u0022account_uuid\\u0022'));
  const result = compareMetadataWrapped(left, right, options());
  assert.equal(result.equal, false);
  assert.ok(result.normalized.right.includes(Buffer.from('\\u0022account_uuid\\u0022')));
});

test('metadata account session structure order and ordinary text are never hidden', () => {
  assert.equal(compareMetadataWrapped(body(first), body(second, 'other-account'), options()).equal, false);
  const changedSession = Buffer.from(body(second).toString().replace(session, '22222222-2222-4222-8222-222222222222'));
  assert.equal(compareMetadataWrapped(body(first), changedSession, options()).equal, false);
  const reordered = Buffer.from(body(second).toString().replace('\\"device_id\\":\\"' + second + '\\",\\"account_uuid\\":\\"\\"', '\\"account_uuid\\":\\"\\",\\"device_id\\":\\"' + second + '\\"'));
  assert.equal(compareMetadataWrapped(body(first), reordered, options()).equal, false);
  assert.equal(compareMetadataWrapped(body(first), body(second, '', ',"new":1'), options()).equal, false);
});

test('duplicate metadata inner device IDs unknown rules invalid types and nonASCII fail closed', () => {
  for (const right of [
    Buffer.from(body(second).toString().replace('\\"device_id\\":', '\\"device_id\\":\\"' + second + '\\",\\"device_id\\":')),
    Buffer.from(body(second).toString().replace('"user_id":', '"user_id":"duplicate","user_id":')),
    body('x'.repeat(64)), body(second, 'Ω'),
  ]) assert.throws(() => compareMetadataWrapped(body(first), right, options()), NormalizationError);
  assert.throws(() => compareMetadataWrapped(body(first), body(second), {...options(), metadata: {...metadata, ignored: true}}), NormalizationError);
  assert.throws(() => compareMetadataWrapped(body(first), body(second), {...options(), rules: [{id: 'hide', path: '/metadata/user_id'}]}), NormalizationError);
});

test('inner identity correlates across wrapped body and HTTP and rejected call rolls back', () => {
  const shared = options();
  compareMetadataWrapped(body(first), body(second), shared);
  const result = compareHttpWrapped(http(body(first)), http(body(second)), shared);
  assert.equal(result.equal, true);
  assert.equal(shared.context.identities.get('device').left.size, 1);
  assert.throws(() => compareMetadataWrapped(body(first), body(third), shared), /bijection/);
  assert.equal(shared.context.identities.get('device').right.size, 1);
  assert.equal(compareMetadataWrapped(body(first), body(second), shared).equal, true);
});

test('HTTP framing validates lengths preserves exact headers and maps raw offsets', () => {
  const left = http(body(first)), right = http(body(second));
  const result = compareHttpWrapped(left, right, options());
  assert.equal(result.equal, true);
  const row = result.transformations.left[0];
  assert.equal(left.subarray(row.start, row.end).toString('hex'), row.originalHex);
  assert.ok(result.normalized.left.subarray(0, splitHttpRequest(left).bodyStart).equals(splitHttpRequest(left).headers));
  assert.equal(compareHttpWrapped(left, Buffer.from(right.toString().replace('Host:', 'host:')), options()).equal, false);
  for (const invalid of [Buffer.from('garbage'), Buffer.from(left.toString().replace('Content-Length: ', 'Transfer-Encoding: chunked\r\nContent-Length: ')), Buffer.concat([left, Buffer.from('x')]), Buffer.from(left.toString().replace('Content-Length:', 'Content-Length: 1\r\nContent-Length:'))]) assert.throws(() => splitHttpRequest(invalid), NormalizationError);
});

test('no wrapper rule changes comparator default raw behavior', () => {
  assert.equal(compareBuffers(body(first), body(second)).equal, false);
});

test('socket path leaf normalization requires exact correlation to each process PID', () => {
  const left = Buffer.from('{"type":"system","subtype":"init","messaging_socket_path":"/tmp/cc-socks/123.sock","text":"/tmp/cc-socks/123.sock"}\n');
  const right = Buffer.from('{"type":"system","subtype":"init","messaging_socket_path":"/tmp/cc-socks/456.sock","text":"/tmp/cc-socks/123.sock"}\n');
  const declared = [{id: 'socket', kind: 'native-pid-socket', path: '/messaging_socket_path', count: 1}];
  const rules = resolvePidSocketRules(declared, {left, right, format: 'ndjson', leftPid: 123, rightPid: 456});
  assert.equal(compareBuffers(left, right, {format: 'ndjson', rules}).equal, true);
  assert.throws(() => resolvePidSocketRules(declared, {left, right, format: 'ndjson', leftPid: 123, rightPid: 789}), /correlate/);
  assert.throws(() => resolvePidSocketRules(declared, {left, right, format: 'ndjson', leftPid: undefined, rightPid: 456}), /Missing/);
  assert.throws(() => resolvePidSocketRules([{...declared[0], path: '/text'}], {left, right, format: 'ndjson', leftPid: 123, rightPid: 456}), /Unsupported/);
  assert.throws(() => resolvePidSocketRules(declared, {left: Buffer.from(left.toString().replace('"init"', '"user"')), right, format: 'ndjson', leftPid: 123, rightPid: 456}), /init record/);
});

test('fork filenames map only an exact bijection established by real init scalars', () => {
  const leftId = session, rightId = '22222222-2222-4222-8222-222222222222';
  const files = id => new Map([['processes/1/stdout.bin', Buffer.from(JSON.stringify({type: 'system', subtype: 'init', session_id: id}) + '\n')], [`processes/1/sessions/projects/owned/${id}.jsonl`, Buffer.from('original')], ['processes/1/sessions/projects/owned/other.jsonl', Buffer.from('other')]]);
  const declaration = {kind: 'fork-init', process: 1, count: 1, group: 'session'};
  const context = createComparisonContext();
  const result = mapForkSessionFiles(files(leftId), files(rightId), {context, declaration, leftId, rightId});
  assert.equal(result.left.files.size, 3); assert.equal(result.right.files.size, 3);
  assert.equal(result.left.files.get('processes/1/sessions/projects/owned/@forked-session.jsonl').toString(), 'original');
  assert.equal(result.left.files.get('processes/1/sessions/projects/owned/other.jsonl').toString(), 'other');
  assert.equal(result.mapping.leftPath, `processes/1/sessions/projects/owned/${leftId}.jsonl`);
  assert.throws(() => mapForkSessionFiles(files(leftId), files(rightId), {context: createComparisonContext(), declaration, leftId, rightId: '33333333-3333-4333-8333-333333333333'}), /real init scalar/);
  const duplicate = files(leftId); duplicate.set(`processes/1/sessions/projects/second/${leftId}.jsonl`, Buffer.from('duplicate'));
  assert.throws(() => mapForkSessionFiles(duplicate, files(rightId), {context: createComparisonContext(), declaration, leftId, rightId}), /cardinality/);
});

test('fork metadata session normalization requires the already established init identity', () => {
  const rightId = '22222222-2222-4222-8222-222222222222';
  const right = Buffer.from(body(second).toString().replace(session, rightId));
  const declared = {...metadata, normalizeForkSession: true};
  assert.throws(() => compareMetadataWrapped(body(first), right, {...options(), metadata: declared}), /observed init identity/);
  const context = createComparisonContext();
  compareBuffers(Buffer.from(`{"session_id":"${session}"}`), Buffer.from(`{"session_id":"${rightId}"}`), {context, format: 'json', rules: [{id: 'fork', path: '/session_id', type: 'string', mode: 'identity', group: 'session', count: 1, validate: 'uuid'}]});
  const compared = compareMetadataWrapped(body(first), right, {context, channel: 'fork-metadata', metadata: declared});
  assert.equal(compared.equal, true); assert.equal(compared.transformations.left.length, 2);
  assert.deepEqual(compared.transformations.left.map(row => row.path), ['/metadata/user_id::/device_id', '/metadata/user_id::/session_id']);
});

test('one named fork header UUID correlates to init and preserves every other header byte', () => {
  const rightId = '22222222-2222-4222-8222-222222222222', context = createComparisonContext();
  compareBuffers(Buffer.from(`{"session_id":"${session}"}`), Buffer.from(`{"session_id":"${rightId}"}`), {context, format: 'json', rules: [{id: 'fork', path: '/session_id', type: 'string', mode: 'identity', group: 'session', count: 1, validate: 'uuid'}]});
  const header = (id, userAgent = 'same') => Buffer.from(`POST /v1/messages HTTP/1.1\r\nX-Claude-Code-Session-Id:  ${id} \r\nUser-Agent: ${userAgent}\r\n\r\n`);
  const declaration = {name: 'x-claude-code-session-id', count: 1, group: 'session'};
  const result = compareForkSessionHeaders(header(session), header(rightId), {context, declaration});
  assert.equal(result.equal, true); assert.ok(result.normalized.left.toString().includes('Session-Id:  @identity:session:0 \r\n'));
  assert.equal(compareForkSessionHeaders(header(session), header(rightId, 'changed'), {context, declaration}).equal, false);
  assert.throws(() => compareForkSessionHeaders(header(session), header('33333333-3333-4333-8333-333333333333'), {context, declaration}), /correlate/);
  const duplicate = Buffer.from(header(session).toString().replace('User-Agent:', `X-Claude-Code-Session-Id: ${session}\r\nUser-Agent:`));
  assert.throws(() => compareForkSessionHeaders(duplicate, header(rightId), {context, declaration}), /cardinality/);
});

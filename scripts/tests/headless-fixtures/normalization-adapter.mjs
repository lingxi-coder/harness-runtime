// Explicit wrapper normalization for a JSON document encoded inside one JSON
// string and for an HTTP request envelope. Every unselected source byte survives.
// This does not reorder, omit, parse/reencode, or replace a whole metadata string.
import {TextDecoder} from 'node:util';
import {compareBuffers, firstByteDifference, NormalizationError} from '../headless-byte-compare.mjs';
import path from 'node:path';
const decoder = new TextDecoder('utf8', {fatal: true});
const fail = (message, details) => { throw new NormalizationError(message, details); };
const pointer = path => path.length ? '/' + path.map(token => token.replaceAll('~', '~0').replaceAll('/', '~1')).join('/') : '';

// Kept independent of the byte comparator lexer so wrapper semantics can be
// tested without changing the common JSON rule contract.
export function jsonScalarSpans(bytes, format = 'json') {
  if (!Buffer.isBuffer(bytes)) fail('Wrapper requires Buffer input');
  const spans = [];
  const documents = format === 'ndjson' ? (() => {
    const rows = []; let from = 0;
    while (from < bytes.length) { const nl = bytes.indexOf(10, from); const end = nl === -1 ? bytes.length : nl; rows.push([from, end]); from = end + 1; }
    return rows;
  })() : [[0, bytes.length]];
  for (let record = 0; record < documents.length; record++) {
    const [from, end] = documents[record]; let at = from;
    const whitespace = () => { while (at < end && [9, 10, 13, 32].includes(bytes[at])) at++; };
    function string() {
      const start = at++;
      if (bytes[start] !== 34) fail('Wrapper JSON string expected', {offset: start});
      while (at < end) {
        const byte = bytes[at++];
        if (byte === 34) {
          try { return {start, end: at, value: JSON.parse(decoder.decode(bytes.subarray(start, at)))}; }
          catch { fail('Wrapper invalid string/UTF8', {offset: start}); }
        }
        if (byte < 32) fail('Wrapper unescaped control', {offset: at - 1});
        if (byte === 92) {
          const escape = bytes[at++];
          if (escape === 117) {
            for (let digit = 0; digit < 4; digit++) if (at >= end || !/[0-9a-fA-F]/.test(String.fromCharCode(bytes[at++]))) fail('Wrapper invalid unicode escape');
          } else if (![34, 92, 47, 98, 102, 110, 114, 116].includes(escape)) fail('Wrapper invalid escape');
        }
      }
      fail('Wrapper unterminated string');
    }
    function value(path, depth = 0) {
      if (depth > 128) fail('Wrapper nesting limit');
      whitespace(); const start = at; const byte = bytes[at];
      if (byte === 123 || byte === 91) {
        const object = byte === 123; const close = object ? 125 : 93; at++; whitespace();
        if (bytes[at] === close) { at++; return; }
        let index = 0;
        for (;;) {
          whitespace(); let key;
          if (object) { key = string().value; whitespace(); if (bytes[at++] !== 58) fail('Wrapper missing colon'); }
          else key = String(index++);
          value([...path, key], depth + 1); whitespace();
          if (bytes[at] === close) { at++; return; }
          if (bytes[at++] !== 44) fail('Wrapper missing delimiter');
        }
      }
      if (byte === 34) { spans.push({...string(), path: pointer(path), type: 'string', record}); return; }
      while (at < end && ![9, 10, 13, 32, 44, 93, 125].includes(bytes[at])) at++;
      const raw = bytes.subarray(start, at);
      let decoded;
      try { decoded = JSON.parse(decoder.decode(raw)); } catch { fail('Wrapper invalid scalar', {offset: start}); }
      if (decoded !== null && !['number', 'boolean'].includes(typeof decoded)) fail('Wrapper invalid scalar type');
      spans.push({start, end: at, value: decoded, path: pointer(path), type: decoded === null ? 'null' : typeof decoded, record});
    }
    value([]); whitespace(); if (at !== end) fail('Wrapper trailing JSON bytes', {offset: at});
  }
  return spans;
}

// An ASCII inner document is an intentional narrow contract for Native's
// metadata.user_id: hex device id, UUID/empty account, UUID session. Unicode in
// a new inner field is rejected rather than normalized by UTF8 reencoding.
function asciiStringMap(bytes, token) {
  if (!/^[\x00-\x7f]*$/.test(token.value)) fail('Metadata inner JSON must be ASCII');
  const map = []; let at = token.start + 1;
  while (at < token.end - 1) {
    const start = at;
    if (bytes[at] === 92) {
      at += bytes[at + 1] === 117 ? 6 : 2;
      let value;
      try { value = JSON.parse('"' + bytes.subarray(start, at).toString('ascii') + '"'); } catch { fail('Invalid wrapper string escape map'); }
      if (value.length !== 1 || value.charCodeAt(0) > 127) fail('Unsupported non-ASCII metadata escape');
    } else { if (bytes[at] > 127) fail('Unsupported metadata UTF8'); at++; }
    map.push({start, end: at});
  }
  if (map.length !== token.value.length) fail('Metadata decoded span map length mismatch');
  return map;
}
function apply(bytes, transformations) {
  const sorted = [...transformations].sort((a, b) => a.start - b.start); let last = 0; const chunks = [];
  for (const row of sorted) {
    if (row.start < last || row.end < row.start || row.end > bytes.length) fail('Overlapping/outside wrapper transformations');
    chunks.push(bytes.subarray(last, row.start), Buffer.from(row.replacementHex, 'hex')); last = row.end;
  }
  chunks.push(bytes.subarray(last)); return Buffer.concat(chunks);
}
function selectMetadata(bytes, format, expectedCount) {
  const selected = jsonScalarSpans(bytes, format).filter(row => row.path === '/metadata/user_id');
  if (selected.length !== expectedCount) fail('Metadata wrapper hit count mismatch', {expected: expectedCount, actual: selected.length});
  for (const row of selected) if (row.type !== 'string') fail('Metadata user_id must be a string');
  return selected;
}

function metadataComparison(left, right, {context, channel, metadata, rules = [], format = 'json'}) {
  if (rules.length !== 0) fail('Metadata wrapper cannot combine ordinary scalar rules');
  if (!metadata || Object.keys(metadata).some(key => !['id', 'outerPath', 'innerPath', 'count', 'group', 'normalizeForkSession'].includes(key)) || metadata.outerPath !== '/metadata/user_id' || metadata.innerPath !== '/device_id' || metadata.count !== 1 || metadata.group !== 'device' || typeof metadata.id !== 'string' || metadata.normalizeForkSession !== undefined && metadata.normalizeForkSession !== true) fail('Unsupported metadata wrapper rule');
  const leftTokens = selectMetadata(left, format, metadata.count); const rightTokens = selectMetadata(right, format, metadata.count);
  const transformations = {left: [], right: []};
  for (let index = 0; index < leftTokens.length; index++) {
    const inner = {left: Buffer.from(leftTokens[index].value, 'ascii'), right: Buffer.from(rightTokens[index].value, 'ascii')};
    for (const side of ['left', 'right']) {
      const device = jsonScalarSpans(inner[side]).filter(row => row.path === '/device_id');
      if (device.length !== 1 || device[0].type !== 'string' || !/^[a-f0-9]{64}$/.test(device[0].value)) fail('Metadata device id requires exactly one hex64 string');
    }
    const innerRules = [{id: metadata.id, path: metadata.innerPath, type: 'string', mode: 'identity', group: metadata.group, count: 1}];
    if (metadata.normalizeForkSession) {
      const ids = {};
      for (const side of ['left', 'right']) {
        const selected = jsonScalarSpans(inner[side]).filter(row => row.path === '/session_id');
        if (selected.length !== 1 || selected[0].type !== 'string') fail('Fork metadata session cardinality/type mismatch');
        ids[side] = selected[0].value;
      }
      const known = context.identities.get('session');
      if (known?.left.get(ids.left)?.peer !== ids.right || known?.right.get(ids.right)?.peer !== ids.left) fail('Fork metadata session does not correlate to observed init identity');
      innerRules.push({id: `${metadata.id}-fork-session`, path: '/session_id', type: 'string', mode: 'identity', group: 'session', count: 1, validate: 'uuid'});
    }
    const result = compareBuffers(inner.left, inner.right, {context, channel: `${channel}:inner-metadata`, format: 'json', rules: innerRules});
    for (const side of ['left', 'right']) {
      const original = side === 'left' ? left : right; const token = side === 'left' ? leftTokens[index] : rightTokens[index]; const map = asciiStringMap(original, token);
      for (const innerTransform of result.transformations[side]) {
        const start = map[innerTransform.start]?.start; const end = map[innerTransform.end - 1]?.end;
        if (start === undefined || end === undefined) fail('Inner transformation outside decoded metadata');
        const replacement = JSON.stringify(Buffer.from(innerTransform.replacementHex, 'hex').toString('ascii')).slice(1, -1);
        transformations[side].push({rule: innerTransform.rule, kind: 'inner-string-scalar', path: `/metadata/user_id::${innerTransform.path}`, start, end, originalHex: original.subarray(start, end).toString('hex'), replacementHex: Buffer.from(replacement).toString('hex')});
      }
    }
  }
  const normalized = {left: apply(left, transformations.left), right: apply(right, transformations.right)};
  const compared = compareBuffers(normalized.left, normalized.right, {format, context, channel, rules});
  return {...compared, rawEqual: left.equals(right), rawDifference: firstByteDifference(left, right), transformations: {left: [...transformations.left, ...compared.transformations.left], right: [...transformations.right, ...compared.transformations.right]}};
}

export function compareMetadataWrapped(left, right, options) {
  const identities = options.context.identities;
  try { return metadataComparison(left, right, options); }
  catch (error) { options.context.identities = identities; throw error; }
}

export function splitHttpRequest(bytes) {
  const headerEnd = bytes.indexOf('\r\n\r\n'); if (headerEnd < 0) fail('HTTP request header delimiter missing');
  const bodyStart = headerEnd + 4; const headers = bytes.subarray(0, bodyStart); const body = bytes.subarray(bodyStart);
  const text = headers.toString('latin1');
  if (!/^(?:POST|HEAD) [^\r\n ]+ HTTP\/1\.1\r\n/.test(text)) fail('Unexpected HTTP request line');
  const lengths = [...text.matchAll(/^content-length:\s*(\d+)\s*$/gim)];
  if (/^transfer-encoding:/im.test(text) || lengths.length !== 1 || Number(lengths[0][1]) !== body.length) fail('HTTP request content-length/framing mismatch');
  return {headers, body, bodyStart};
}

export function compareHttpWrapped(left, right, {context, channel, metadata, header}) {
  const a = splitHttpRequest(left); const b = splitHttpRequest(right);
  const headers = header ? compareForkSessionHeaders(a.headers, b.headers, {context, channel: `${channel}:headers`, declaration: header}) : compareBuffers(a.headers, b.headers, {format: 'raw', channel: `${channel}:headers`, context});
  const body = compareMetadataWrapped(a.body, b.body, {format: 'json', context, channel: `${channel}:body`, metadata});
  const normalized = {left: Buffer.concat([headers.normalized.left, body.normalized.left]), right: Buffer.concat([headers.normalized.right, body.normalized.right])};
  const transformations = {left: [...headers.transformations.left, ...body.transformations.left.map(row => ({...row, start: row.start + a.bodyStart, end: row.end + a.bodyStart}))], right: [...headers.transformations.right, ...body.transformations.right.map(row => ({...row, start: row.start + b.bodyStart, end: row.end + b.bodyStart}))]};
  return {equal: headers.equal && body.equal, rawEqual: left.equals(right), normalizedEqual: normalized.left.equals(normalized.right), normalized, transformations, difference: firstByteDifference(normalized.left, normalized.right), rawDifference: firstByteDifference(left, right)};
}

export function compareForkSessionHeaders(left, right, {context, declaration}) {
  if (!declaration || Object.keys(declaration).some(key => !['name', 'count', 'group'].includes(key)) || declaration.name !== 'x-claude-code-session-id' || declaration.count !== 1 || declaration.group !== 'session') fail('Unsupported fork session header declaration');
  const selected = {};
  for (const [side, bytes] of [['left', left], ['right', right]]) {
    if (!bytes.subarray(-4).equals(Buffer.from('\r\n\r\n'))) fail('Header-only capture delimiter mismatch');
    const matches = [...bytes.toString('latin1').matchAll(/^x-claude-code-session-id:([ \t]*)([^\r\n]*?)\r$/gim)];
    if (matches.length !== 1) fail('Fork session header cardinality mismatch', {side, actual: matches.length});
    const match = matches[0], value = match[2].trimEnd();
    if (!/^[a-f0-9]{8}-[a-f0-9]{4}-[1-8][a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/i.test(value)) fail('Fork session header must be a UUID');
    const start = match.index + match[0].indexOf(':') + 1 + match[1].length;
    selected[side] = {value, start, end: start + value.length};
  }
  const mappings = context.identities.get('session');
  const a = mappings?.left.get(selected.left.value), b = mappings?.right.get(selected.right.value);
  if (!a || !b || a.peer !== selected.right.value || b.peer !== selected.left.value || a.canonical !== b.canonical) fail('Fork header does not correlate to observed init identity');
  const transformations = {left: [], right: []}; const normalized = {};
  for (const [side, bytes] of [['left', left], ['right', right]]) {
    const span = selected[side];
    transformations[side].push({rule: 'fork-session-header', path: '/headers/x-claude-code-session-id', type: 'string', start: span.start, end: span.end, originalHex: bytes.subarray(span.start, span.end).toString('hex'), replacementHex: Buffer.from(a.canonical).toString('hex')});
    normalized[side] = apply(bytes, transformations[side]);
  }
  return {equal: normalized.left.equals(normalized.right), rawEqual: left.equals(right), normalizedEqual: normalized.left.equals(normalized.right), transformations, normalized, difference: firstByteDifference(normalized.left, normalized.right), rawDifference: firstByteDifference(left, right)};
}

export function resolvePidSocketRules(rules, {left, right, format, leftPid, rightPid}) {
  return rules.map(rule => {
    if (rule.kind !== 'native-pid-socket') return rule;
    if (Object.keys(rule).some(key => !['kind', 'id', 'path', 'count'].includes(key)) || !/^\/(?:\d+\/)?messaging_socket_path$/.test(rule.path) || rule.count !== 1 || typeof rule.id !== 'string') fail('Unsupported PID socket rule');
    for (const [side, bytes, pid] of [['left', left, leftPid], ['right', right, rightPid]]) {
      if (!Number.isSafeInteger(pid) || pid <= 0) fail('Missing process PID for socket rule', {side});
      const selected = jsonScalarSpans(bytes, format).filter(span => span.path === rule.path);
      if (selected.length !== 1 || selected[0].type !== 'string' || selected[0].value !== `/tmp/cc-socks/${pid}.sock`) fail('Native socket path does not correlate to process PID', {side});
      const documents = format === 'ndjson' ? bytes.toString().trim().split('\n').map(JSON.parse) : [JSON.parse(bytes)];
      const index = /^\/(\d+)\//.exec(rule.path)?.[1];
      const row = index === undefined ? documents[selected[0].record] : documents[0][Number(index)];
      if (row?.type !== 'system' || row.subtype !== 'init') fail('Socket path must belong to native init record', {side});
    }
    return {id: rule.id, path: rule.path, type: 'string', mode: 'exact', count: rule.count, left: `/tmp/cc-socks/${leftPid}.sock`, right: `/tmp/cc-socks/${rightPid}.sock`};
  });
}

export function mapForkSessionFiles(leftFiles, rightFiles, {context, declaration, leftId, rightId}) {
  const identities = context.identities;
  try {
    if (!declaration || Object.keys(declaration).some(key => !['kind', 'process', 'count', 'group'].includes(key)) || declaration.kind !== 'fork-init' || declaration.process !== 1 || declaration.count !== 1 || declaration.group !== 'session') fail('Unsupported fork filename identity declaration');
    const stdout = 'processes/1/stdout.bin';
    if (!leftFiles.has(stdout) || !rightFiles.has(stdout)) fail('Fork filename identity requires both real stdout captures');
    // Seed the bijection from actual native init scalar tokens, not a synthetic
    // guessed identifier carrier. Unknown events remain in the ordinary stdout
    // comparison; this call only proves the file identity correspondence.
    compareBuffers(leftFiles.get(stdout), rightFiles.get(stdout), {format: 'ndjson', channel: 'fork-file-init', context, rules: [{id: 'fork-session-file', path: '/session_id', type: 'string', mode: 'identity', group: 'session', count: 1, validate: 'uuid', match: {path: '/subtype', equals: 'init'}}]});
    const mapped = context.identities.get('session');
    if (mapped?.left.get(leftId)?.peer !== rightId || mapped?.right.get(rightId)?.peer !== leftId) fail('Fork capture identity does not match real init scalar');
    const remap = (files, id) => {
      const hits = [...files.keys()].filter(name => name.startsWith('processes/1/sessions/') && path.basename(name) === `${id}.jsonl`);
      if (hits.length !== 1) fail('Fork session filename cardinality mismatch', {id, actual: hits.length});
      const original = hits[0]; const canonical = path.join(path.dirname(original), '@forked-session.jsonl');
      if (files.has(canonical)) fail('Fork filename canonical identity collision');
      const result = new Map(files); const bytes = result.get(original); result.delete(original); result.set(canonical, bytes);
      return {files: result, paths: new Map([[canonical, original]]), original, canonical};
    };
    const left = remap(leftFiles, leftId), right = remap(rightFiles, rightId);
    return {left, right, mapping: {group: 'session', leftId, rightId, leftPath: left.original, rightPath: right.original, leftCanonical: left.canonical, rightCanonical: right.canonical}};
  } catch (error) { context.identities = identities; throw error; }
}

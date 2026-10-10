/**
 * Byte comparator for headless oracle captures. No default normalization rules.
 * A rule author must enumerate the exact channel, path, scalar type and hit count.
 * JSON is lexed into byte spans; only a selected scalar token is replaced. Object
 * order, duplicate keys, whitespace, string spelling and line endings survive.
 */
import { TextDecoder } from 'node:util';

const utf8 = new TextDecoder('utf-8', { fatal: true });
const contextBrand = Symbol('headless byte comparison context');
const scalarTypes = new Set(['string', 'number', 'boolean', 'null']);

export class NormalizationError extends Error {
  constructor(message, details = {}) {
    super(message);
    this.name = 'NormalizationError';
    this.details = details;
  }
}

function fail(message, details) {
  throw new NormalizationError(message, details);
}

/** Create one context per pair of runs, shared by all of their channels. */
export function createComparisonContext() {
  return { [contextBrand]: true, identities: new Map() };
}

function copyIdentities(identities) {
  return new Map([...identities].map(([group, maps]) => [group, {
    left: new Map(maps.left), right: new Map(maps.right),
  }]));
}

function pathTokens(path) {
  if (Array.isArray(path)) {
    if (!path.every((p) => typeof p === 'string' || Number.isSafeInteger(p) && p >= 0)) {
      fail('Path must contain only string keys and nonnegative integer indices', { path });
    }
    return path.map(String);
  }
  if (typeof path !== 'string' || path !== '' && !path.startsWith('/')) {
    fail('Path must be an RFC 6901 JSON pointer or token array', { path });
  }
  if (path === '') return [];
  return path.slice(1).split('/').map((token) => {
    if (/~(?:[^01]|$)/u.test(token)) fail('Invalid JSON pointer escape', { path });
    return token.replace(/~1/gu, '/').replace(/~0/gu, '~');
  });
}

const samePath = (a, b) => a.length === b.length && a.every((p, i) => p === b[i]);
const pointer = (path) => path.length === 0 ? '' : '/' + path.map((p) => p.replace(/~/gu, '~0').replace(/\//gu, '~1')).join('/');

/** Lex a single JSON document, preserving byte offsets and duplicate properties. */
function lexDocument(buffer, start, end, record) {
  let cursor = start;
  const tokens = [];
  const whitespace = () => {
    while (cursor < end && [0x20, 0x09, 0x0a, 0x0d].includes(buffer[cursor])) cursor++;
  };
  const syntax = (message) => fail(`Invalid JSON: ${message}`, { record, offset: cursor });
  function string() {
    const from = cursor++;
    let closed = false;
    while (cursor < end) {
      const byte = buffer[cursor++];
      if (byte === 0x22) { closed = true; break; }
      if (byte < 0x20) syntax('unescaped control character');
      if (byte === 0x5c) {
        if (cursor >= end) syntax('unterminated escape');
        const escape = buffer[cursor++];
        if (escape === 0x75) {
          for (let i = 0; i < 4; i++) {
            if (cursor >= end || !/[0-9a-fA-F]/u.test(String.fromCharCode(buffer[cursor++]))) syntax('invalid Unicode escape');
          }
        } else if (![0x22, 0x5c, 0x2f, 0x62, 0x66, 0x6e, 0x72, 0x74].includes(escape)) syntax('invalid escape');
      }
    }
    if (!closed) syntax('unterminated string');
    // Decoding a single string token validates UTF-8 and obtains identity/key
    // semantics; the token's original bytes are never reencoded for comparison.
    try { return JSON.parse(utf8.decode(buffer.subarray(from, cursor))); }
    catch { syntax('invalid string or UTF-8'); }
  }
  function value(path, depth) {
    if (depth > 512) syntax('nesting limit exceeded');
    whitespace();
    const from = cursor;
    const byte = buffer[cursor];
    let type;
    let decoded;
    if (byte === 0x7b || byte === 0x5b) {
      const object = byte === 0x7b;
      const close = object ? 0x7d : 0x5d;
      type = object ? 'object' : 'array';
      cursor++;
      whitespace();
      if (buffer[cursor] === close) cursor++;
      else {
        let index = 0;
        while (cursor < end) {
          let key;
          if (object) {
            if (buffer[cursor] !== 0x22) syntax('expected property name');
            key = string();
            whitespace();
            if (buffer[cursor++] !== 0x3a) syntax('expected colon');
          } else key = String(index++);
          value([...path, key], depth + 1);
          whitespace();
          if (buffer[cursor] === close) { cursor++; break; }
          if (buffer[cursor++] !== 0x2c) syntax('expected comma or closing delimiter');
          whitespace();
        }
        if (buffer[cursor - 1] !== close) syntax('unterminated container');
      }
    } else if (byte === 0x22) {
      type = 'string';
      decoded = string();
    } else if (byte === 0x74 || byte === 0x66 || byte === 0x6e) {
      const literal = byte === 0x74 ? 'true' : byte === 0x66 ? 'false' : 'null';
      if (!buffer.subarray(cursor, cursor + literal.length).equals(Buffer.from(literal))) syntax('invalid literal');
      cursor += literal.length;
      type = literal === 'null' ? 'null' : 'boolean';
      decoded = literal === 'null' ? null : literal === 'true';
    } else {
      // Scan ASCII number bytes without interpreting neighboring JSON tokens.
      while (cursor < end && ([0x2d, 0x2b, 0x2e, 0x65, 0x45].includes(buffer[cursor]) || buffer[cursor] >= 0x30 && buffer[cursor] <= 0x39)) cursor++;
      const raw = buffer.subarray(from, cursor).toString('ascii');
      if (!/^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?$/u.test(raw)) syntax('invalid value or number');
      type = 'number';
      decoded = Number(raw);
    }
    tokens.push({ path, record, start: from, end: cursor, type, value: decoded });
  }
  value([], 0);
  whitespace();
  if (cursor !== end) syntax('trailing bytes');
  return tokens;
}

function lex(buffer, format) {
  if (format === 'json') return lexDocument(buffer, 0, buffer.length, 0);
  const tokens = [];
  let start = 0;
  let record = 0;
  while (start < buffer.length) {
    const newline = buffer.indexOf(0x0a, start);
    const end = newline === -1 ? buffer.length : newline;
    // Blank NDJSON records are rejected, rather than silently being dropped.
    tokens.push(...lexDocument(buffer, start, end, record++));
    start = end + 1;
  }
  return tokens;
}

function validateCommonRule(rule, ids) {
  if (!rule || typeof rule !== 'object' || typeof rule.id !== 'string' || rule.id.length === 0) fail('Rule requires a nonempty id');
  if (ids.has(rule.id)) fail('Duplicate rule id', { rule: rule.id });
  ids.add(rule.id);
  if (!Number.isSafeInteger(rule.count) || rule.count < 0) fail('Rule requires an exact nonnegative hit count', { rule: rule.id });
  if (rule.channel !== undefined && typeof rule.channel !== 'string') fail('Invalid rule channel', { rule: rule.id });
  const allowed = rule.kind === 'text' ? ['id', 'channel', 'kind', 'category', 'left', 'right', 'count'] : ['id', 'channel', 'path', 'type', 'mode', 'group', 'count', 'records', 'match', 'validate', 'left', 'right'];
  for (const key of Object.keys(rule)) if (!allowed.includes(key)) fail('Unknown normalization rule property', { rule: rule.id, property: key });
}

function validateRule(rule, format) {
  if (format === 'raw') {
    if (rule.kind !== 'text' || !['fixture-root', 'port'].includes(rule.category)) fail('Raw replacement allows only explicit fixture-root or port tokens', { rule: rule.id });
    for (const side of ['left', 'right']) {
      if (typeof rule[side] !== 'string' || rule[side].length === 0) fail('Text replacement requires nonempty exact tokens', { rule: rule.id, side });
      if (rule.category === 'fixture-root' && (!rule[side].startsWith('/') || rule[side] === '/' || rule[side].endsWith('/'))) fail('Fixture root must be a complete absolute directory without trailing slash', { rule: rule.id, side });
      if (rule.category === 'port' && !/^(?:127\.0\.0\.1|localhost)?:[1-9]\d{0,4}$/u.test(rule[side])) fail('Port token must include a colon and an explicit loopback host if present', { rule: rule.id, side });
      if (rule.category === 'port' && Number(rule[side].slice(rule[side].lastIndexOf(':') + 1)) > 65535) fail('Port outside valid range', { rule: rule.id, side });
    }
    return rule;
  }
  if (rule.kind === 'text') fail('Text rules cannot modify JSON documents', { rule: rule.id });
  if (!scalarTypes.has(rule.type)) fail('JSON rule requires an explicit scalar type', { rule: rule.id });
  if (!['identity', 'value', 'exact'].includes(rule.mode)) fail('Unknown JSON normalization mode', { rule: rule.id });
  if (rule.mode === 'identity' && (rule.type !== 'string' || typeof rule.group !== 'string' || rule.group.length === 0)) fail('Identity rule requires string type and nonempty group', { rule: rule.id });
  if (rule.records !== undefined && (!Array.isArray(rule.records) || !rule.records.every((r) => Number.isSafeInteger(r) && r >= 0) || new Set(rule.records).size !== rule.records.length)) fail('Invalid record indices', { rule: rule.id });
  if (rule.validate !== undefined && !['uuid', 'iso-timestamp', 'nonnegative-number', 'integer'].includes(rule.validate)) fail('Unknown value validator', { rule: rule.id });
  if (rule.validate === 'uuid' && rule.type !== 'string' || rule.validate === 'iso-timestamp' && rule.type !== 'string' || ['nonnegative-number', 'integer'].includes(rule.validate) && rule.type !== 'number') fail('Validator and declared type disagree', { rule: rule.id });
  if (rule.mode === 'exact') {
    for (const side of ['left', 'right']) {
      const value = rule[side];
      const type = value === null ? 'null' : typeof value;
      if (type !== rule.type || type === 'number' && !Number.isFinite(value)) fail('Exact rule requires scalar expected values of the declared type', { rule: rule.id, side });
    }
  }
  const path = pathTokens(rule.path);
  let match;
  if (rule.match !== undefined) {
    if (!rule.match || Object.keys(rule.match).some((key) => !['path', 'equals'].includes(key)) || !Object.hasOwn(rule.match, 'equals') || !scalarTypes.has(rule.match.equals === null ? 'null' : typeof rule.match.equals) || typeof rule.match.equals === 'number' && !Number.isFinite(rule.match.equals)) fail('Record match requires a scalar equals value', { rule: rule.id });
    match = { path: pathTokens(rule.match.path), equals: rule.match.equals };
  }
  return { ...rule, path, match };
}

function select(tokens, rule, side) {
  const records = new Set(tokens.map((t) => t.record));
  if (rule.records) for (const record of rule.records) if (!records.has(record)) fail('Selected record does not exist', { rule: rule.id, side, record });
  const eligible = (record) => {
    if (rule.records && !rule.records.includes(record)) return false;
    if (!rule.match) return true;
    const matches = tokens.filter((t) => t.record === record && samePath(t.path, rule.match.path));
    if (matches.length > 1) fail('Record selector is ambiguous due to duplicate keys', { rule: rule.id, side, record });
    return matches.length === 1 && Object.is(matches[0].value, rule.match.equals);
  };
  const selected = tokens.filter((t) => samePath(t.path, rule.path) && eligible(t.record));
  if (selected.length !== rule.count) fail('Replacement hit count mismatch', { rule: rule.id, side, expected: rule.count, actual: selected.length, path: pointer(rule.path) });
  for (const token of selected) {
    if (token.type !== rule.type) fail('Replacement token type mismatch', { rule: rule.id, side, expected: rule.type, actual: token.type, record: token.record, path: pointer(token.path) });
    const value = token.value;
    if (rule.mode === 'identity' && value.length === 0) fail('Identity must be nonempty', { rule: rule.id, side, record: token.record });
    if (rule.type === 'number' && !Number.isFinite(value)) fail('Dynamic numeric value must be finite', { rule: rule.id, side });
    if (rule.mode === 'exact' && !Object.is(value, rule[side])) fail('Exact replacement value mismatch', { rule: rule.id, side, record: token.record });
    if (rule.validate === 'uuid' && !/^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[1-8][0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}$/u.test(value)) fail('Invalid UUID identity', { rule: rule.id, side, record: token.record });
    if (rule.validate === 'iso-timestamp' && (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?Z$/u.test(value) || !Number.isFinite(Date.parse(value)) || new Date(value).toISOString().slice(0, 19) !== value.slice(0, 19))) fail('Invalid ISO timestamp', { rule: rule.id, side, record: token.record });
    if (rule.validate === 'nonnegative-number' && value < 0 || rule.validate === 'integer' && !Number.isSafeInteger(value)) fail('Dynamic number failed validation', { rule: rule.id, side, record: token.record });
  }
  return selected;
}

function identityReplacement(identities, group, left, right, rule) {
  let maps = identities.get(group);
  if (!maps) { maps = { left: new Map(), right: new Map() }; identities.set(group, maps); }
  const previousLeft = maps.left.get(left);
  const previousRight = maps.right.get(right);
  if (previousLeft && previousLeft.peer !== right || previousRight && previousRight.peer !== left) fail('Identity correlation is not a bijection', { rule, group, left, right });
  const canonical = previousLeft?.canonical ?? previousRight?.canonical ?? `@identity:${group}:${maps.left.size}`;
  maps.left.set(left, { peer: right, canonical });
  maps.right.set(right, { peer: left, canonical });
  return Buffer.from(JSON.stringify(canonical));
}

function scalarReplacement(rule) {
  return Buffer.from(rule.type === 'string' ? JSON.stringify(`@dynamic:${rule.id}`) : rule.type === 'number' ? '0' : rule.type === 'boolean' ? 'false' : 'null');
}

function textSpans(buffer, rule, side) {
  const needle = Buffer.from(rule[side]);
  const spans = [];
  let from = 0;
  while (from < buffer.length) {
    const start = buffer.indexOf(needle, from);
    if (start === -1) break;
    const end = start + needle.length;
    // Exact directory/port tokens must not be prefixes of another token.
    const after = buffer[end];
    const before = buffer[start - 1];
    const word = (byte) => byte !== undefined && (byte >= 0x30 && byte <= 0x39 || byte >= 0x41 && byte <= 0x5a || byte >= 0x61 && byte <= 0x7a || byte >= 0x80 || byte === 0x5f || byte === 0x2d || byte === 0x2e);
    const boundary = rule.category === 'port' ? !word(after) : !word(after) && (before === undefined || !word(before) && before !== 0x2f);
    if (boundary) spans.push({ start, end, type: 'text' });
    from = end;
  }
  if (spans.length !== rule.count) fail('Replacement hit count mismatch', { rule: rule.id, side, expected: rule.count, actual: spans.length });
  return spans;
}

function apply(buffer, spans, side, channel) {
  spans.sort((a, b) => a.start - b.start);
  let cursor = 0;
  const pieces = [];
  const transformations = [];
  for (const span of spans) {
    if (span.start < cursor) fail('Normalization rules overlap', { rule: span.rule, side, offset: span.start });
    pieces.push(buffer.subarray(cursor, span.start), span.replacement);
    transformations.push({ rule: span.rule, channel, record: span.record ?? null, path: span.path ? pointer(span.path) : null, type: span.type, start: span.start, end: span.end, originalHex: buffer.subarray(span.start, span.end).toString('hex'), replacementHex: span.replacement.toString('hex') });
    cursor = span.end;
  }
  pieces.push(buffer.subarray(cursor));
  return { buffer: Buffer.concat(pieces), transformations };
}

/** First differing byte with bounded, original byte context (including EOF). */
export function firstByteDifference(left, right, radius = 24) {
  if (!Buffer.isBuffer(left) || !Buffer.isBuffer(right)) throw new TypeError('Comparison inputs must be Buffers');
  if (!Number.isSafeInteger(radius) || radius < 0) throw new TypeError('Context radius must be a nonnegative integer');
  let offset = 0;
  while (offset < Math.min(left.length, right.length) && left[offset] === right[offset]) offset++;
  if (offset === left.length && offset === right.length) return null;
  const describe = (buffer) => {
    const start = Math.max(0, offset - radius);
    const end = Math.min(buffer.length, offset + radius + 1);
    return { length: buffer.length, byte: offset < buffer.length ? buffer[offset] : null, contextStart: start, contextEnd: end, hex: buffer.subarray(start, end).toString('hex'), text: buffer.subarray(start, end).toString('utf8') };
  };
  return { offset, left: describe(left), right: describe(right) };
}

/**
 * compareBuffers(left, right, { format, channel, rules, context })
 *
 * Rules default to []; no unknown fields are normalized. JSON rules select an
 * RFC 6901 path (or key/index array), exact count, scalar type, and one of:
 * - identity: group names a bijection shared across channels; validate may be uuid.
 * - value: an explicitly approved dynamic scalar (e.g. duration).
 * - exact: left/right are the fixture's expected scalar values.
 * records restrict NDJSON indices; match selects records by an unambiguous scalar.
 * Raw text rules require kind:text, category:fixture-root|port, left/right exact
 * tokens and exact count. They are intentionally unavailable inside JSON.
 *
 * Context updates are transactional: a rejected rule does not poison later calls.
 * Callers must not reuse a context for a different pair of captured runs.
 */
export function compareBuffers(left, right, options = {}) {
  if (!Buffer.isBuffer(left) || !Buffer.isBuffer(right)) throw new TypeError('Comparison inputs must be Buffers');
  const { format = 'raw', channel = 'unnamed', rules = [], context = createComparisonContext() } = options;
  if (!['raw', 'json', 'ndjson'].includes(format)) fail('Unknown artifact format', { format });
  if (typeof channel !== 'string' || channel.length === 0 || !Array.isArray(rules)) fail('Channel must be nonempty and rules must be an array');
  if (context?.[contextBrand] !== true || !(context.identities instanceof Map)) fail('Use createComparisonContext() for a shared comparison context');
  const ids = new Set();
  for (const rule of rules) validateCommonRule(rule, ids);
  const checked = rules.filter((r) => r.channel === undefined || r.channel === channel).map((r) => validateRule(r, format));
  const leftTokens = format === 'raw' ? null : lex(left, format);
  const rightTokens = format === 'raw' ? null : lex(right, format);
  const identities = copyIdentities(context.identities);
  const leftSpans = [];
  const rightSpans = [];
  for (const rule of checked) {
    const leftSelected = format === 'raw' ? textSpans(left, rule, 'left') : select(leftTokens, rule, 'left');
    const rightSelected = format === 'raw' ? textSpans(right, rule, 'right') : select(rightTokens, rule, 'right');
    for (let i = 0; i < leftSelected.length; i++) {
      const replacement = format === 'raw' ? Buffer.from(`@fixture:${rule.id}`) : rule.mode === 'identity' ? identityReplacement(identities, rule.group, leftSelected[i].value, rightSelected[i].value, rule.id) : scalarReplacement(rule);
      leftSpans.push({ ...leftSelected[i], replacement, rule: rule.id });
      rightSpans.push({ ...rightSelected[i], replacement, rule: rule.id });
    }
  }
  const normalizedLeft = apply(left, leftSpans, 'left', channel);
  const normalizedRight = apply(right, rightSpans, 'right', channel);
  context.identities = identities;
  const rawEqual = left.equals(right);
  const normalizedEqual = normalizedLeft.buffer.equals(normalizedRight.buffer);
  return {
    equal: normalizedEqual, rawEqual, normalizedEqual,
    transformations: { left: normalizedLeft.transformations, right: normalizedRight.transformations },
    normalized: { left: normalizedLeft.buffer, right: normalizedRight.buffer },
    difference: firstByteDifference(normalizedLeft.buffer, normalizedRight.buffer),
    rawDifference: firstByteDifference(left, right),
  };
}

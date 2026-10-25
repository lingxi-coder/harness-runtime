const MiB = 1024 * 1024;

export const DEFAULT_JSON_PREFLIGHT_LIMITS = Object.freeze({
  maxBytes: 116 * MiB,
  maxDepth: 64,
  maxNodes: 250_000,
  maxArrayItems: 10_000,
  maxObjectProperties: 10_000,
  maxStringBytes: 116 * MiB,
});

export const EVENT_PAGE_PREFLIGHT_LIMITS = Object.freeze({
  maxBytes: 8 * MiB,
  maxDepth: 64,
  maxNodes: 100_000,
  maxArrayItems: 10_000,
  maxObjectProperties: 10_000,
  maxStringBytes: 8 * MiB,
});

function reject(code, message) {
  const error = new RangeError(message);
  error.code = code;
  throw error;
}

function jsonStringByteLength(value) {
  let bytes = 2; // JSON quotes
  for (let index = 0; index < value.length; index++) {
    const code = value.charCodeAt(index);
    if (code === 0x22 || code === 0x5c || code === 0x08 || code === 0x09
        || code === 0x0a || code === 0x0c || code === 0x0d) {
      bytes += 2;
    } else if (code < 0x20) {
      bytes += 6;
    } else if (code >= 0xd800 && code <= 0xdbff) {
      const next = value.charCodeAt(index + 1);
      if (next >= 0xdc00 && next <= 0xdfff) {
        bytes += 4;
        index++;
      } else {
        bytes += 6;
      }
    } else if (code >= 0xdc00 && code <= 0xdfff) {
      bytes += 6;
    } else if (code <= 0x7f) {
      bytes++;
    } else if (code <= 0x7ff) {
      bytes += 2;
    } else {
      bytes += 3;
    }
  }
  return bytes;
}

/**
 * Bound untrusted JSON-like input before calling Zod. Zod 3's issue collector
 * can allocate one error per invalid array element, so size limits must be
 * checked before the official schemas walk the value.
 */
export function preflightJson(value, overrides = {}) {
  const limits = {...DEFAULT_JSON_PREFLIGHT_LIMITS, ...overrides};
  const ancestors = new WeakSet();
  let bytes = 0;
  let nodes = 0;

  const consume = amount => {
    if (amount > limits.maxBytes - bytes) reject('agui_input_too_large', 'The AG-UI value exceeds the supported JSON size limit.');
    bytes += amount;
  };

  const visit = (current, depth, inArray = false) => {
    if (depth > limits.maxDepth) reject('agui_input_too_deep', 'The AG-UI value exceeds the supported nesting depth.');
    if (++nodes > limits.maxNodes) reject('agui_input_too_complex', 'The AG-UI value contains too many JSON values.');
    if (current === null) { consume(4); return; }
    if (current === undefined) { if (inArray) consume(4); return; }
    if (typeof current === 'boolean') { consume(current ? 4 : 5); return; }
    if (typeof current === 'number') { consume(Number.isFinite(current) ? String(current).length : 4); return; }
    if (typeof current === 'string') {
      const stringBytes = jsonStringByteLength(current);
      if (stringBytes > limits.maxStringBytes) reject('agui_input_string_too_large', 'An AG-UI string exceeds the supported size limit.');
      consume(stringBytes);
      return;
    }
    if (typeof current !== 'object') reject('agui_input_not_json', 'The AG-UI value must contain only JSON data.');

    if (ancestors.has(current)) reject('agui_input_not_json', 'The AG-UI value must not contain cycles.');
    ancestors.add(current);
    try {
      if (Array.isArray(current)) {
        if (current.length > limits.maxArrayItems) reject('agui_input_array_too_large', 'An AG-UI array exceeds the supported item limit.');
        consume(2 + Math.max(0, current.length - 1));
        for (let index = 0; index < current.length; index++) visit(current[index], depth + 1, true);
        return;
      }
      const prototype = Object.getPrototypeOf(current);
      if (prototype !== Object.prototype && prototype !== null) {
        const prototypeName = prototype && prototype.constructor && prototype.constructor.name || 'unknown';
        reject('agui_input_not_json', `The AG-UI value must contain only plain JSON objects (found ${prototypeName}).`);
      }
      consume(2);
      let propertyCount = 0;
      let firstProperty = true;
      for (const key in current) {
        if (!Object.prototype.hasOwnProperty.call(current, key)) continue;
        const descriptor = Object.getOwnPropertyDescriptor(current, key);
        if (!descriptor || !Object.prototype.hasOwnProperty.call(descriptor, 'value')) {
          reject('agui_input_not_json', 'The AG-UI value must not contain accessor properties.');
        }
        if (descriptor.value === undefined) continue;
        if (++propertyCount > limits.maxObjectProperties) reject('agui_input_object_too_large', 'An AG-UI object exceeds the supported property limit.');
        if (!firstProperty) consume(1);
        firstProperty = false;
        consume(jsonStringByteLength(key) + 1); // key and colon
        visit(descriptor.value, depth + 1);
      }
    } finally {
      ancestors.delete(current);
    }
  };

  visit(value, 0);
  if (bytes > limits.maxBytes) reject('agui_input_too_large', 'The AG-UI value exceeds the supported JSON size limit.');
  return {bytes, nodes};
}

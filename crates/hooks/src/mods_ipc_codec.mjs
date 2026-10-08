const PROTOCOL = 'lingxi.mods.ipc';
const VERSION = 1;
const numberView = new DataView(new ArrayBuffer(8));

function float64Hex(value) {
  numberView.setFloat64(0, value, false);
  let bits = '';
  for (let index = 0; index < 8; index += 1) {
    bits += numberView.getUint8(index).toString(16).padStart(2, '0');
  }
  return bits;
}

function float64FromHex(bits) {
  if (typeof bits !== 'string' || !/^[0-9a-f]{16}$/.test(bits)) {
    throw new TypeError('invalid Mods IPC number bits');
  }
  for (let index = 0; index < 8; index += 1) {
    numberView.setUint8(index, Number.parseInt(bits.slice(index * 2, index * 2 + 2), 16));
  }
  return numberView.getFloat64(0, false);
}

function pointerUnits(path) {
  const pointer = path.map(segment => `/${segment.replaceAll('~', '~0').replaceAll('/', '~1')}`).join('');
  const units = [];
  for (let index = 0; index < pointer.length; index += 1) units.push(pointer.charCodeAt(index));
  return units;
}

function pointerTokens(pointer) {
  if (pointer === '') return [];
  if (pointer[0] !== '/') throw new TypeError('invalid Mods IPC number pointer');
  return pointer.slice(1).split('/').map(segment => {
    let decoded = '';
    for (let index = 0; index < segment.length; index += 1) {
      const unit = segment[index];
      if (unit !== '~') {
        decoded += unit;
        continue;
      }
      const escaped = segment[index + 1];
      if (escaped === '0') decoded += '~';
      else if (escaped === '1') decoded += '/';
      else throw new TypeError('invalid Mods IPC number pointer escape');
      index += 1;
    }
    return decoded;
  });
}

function stringFromUnits(units) {
  let result = '';
  const chunkSize = 4096;
  for (let start = 0; start < units.length; start += chunkSize) {
    result += String.fromCharCode(...units.slice(start, start + chunkSize));
  }
  return result;
}

function restoreNumber(message, pointer, bits) {
  const value = float64FromHex(bits);
  const tokens = pointerTokens(pointer);
  if (tokens.length === 0) {
    if (message !== null) throw new TypeError('Mods IPC root number placeholder is missing');
    return value;
  }

  let parent = message;
  for (const token of tokens.slice(0, -1)) {
    if (parent === null || typeof parent !== 'object' || !Object.hasOwn(parent, token)) {
      throw new TypeError('Mods IPC number pointer does not identify a value');
    }
    parent = parent[token];
  }

  if (parent === null || typeof parent !== 'object') {
    throw new TypeError('Mods IPC number pointer does not identify an object');
  }
  const key = tokens[tokens.length - 1];
  if (!Object.hasOwn(parent, key) || parent[key] !== null) {
    throw new TypeError('Mods IPC number placeholder is missing');
  }
  Object.defineProperty(parent, key, {
    value,
    writable: true,
    enumerable: true,
    configurable: true,
  });
  return message;
}

/** Encode one line-protocol message while keeping exceptional JS numbers exact. */
export function encodeModsIpcFrame(message) {
  const paths = new WeakMap();
  const numberBits = [];
  const serializedMessage = JSON.stringify(message, function encodeNumber(key, value) {
    const parentPath = paths.get(this);
    const path = parentPath === undefined ? [] : [...parentPath, key];

    if (typeof value === 'number' && (!Number.isFinite(value) || Object.is(value, -0))) {
      numberBits.push({ pointerUnits: pointerUnits(path), bits: float64Hex(value) });
      return null;
    }
    if (value !== null && typeof value === 'object') paths.set(value, path);
    return value;
  });

  if (serializedMessage === undefined) {
    throw new TypeError('Mods IPC message must be JSON serializable');
  }
  return `{"protocol":${JSON.stringify(PROTOCOL)},"version":${VERSION},"message":${serializedMessage},"numberBits":${JSON.stringify(numberBits)}}`;
}

/** Decode one protocol frame into the ordinary JS data shape used by the worker. */
export function decodeModsIpcFrame(line) {
  const frame = JSON.parse(line);
  if (frame === null || typeof frame !== 'object' || Array.isArray(frame)
      || frame.protocol !== PROTOCOL || frame.version !== VERSION
      || !Object.hasOwn(frame, 'message') || !Array.isArray(frame.numberBits)) {
    throw new TypeError('invalid Mods IPC frame');
  }

  let message = frame.message;
  for (const entry of frame.numberBits) {
    if (entry === null || typeof entry !== 'object' || !Array.isArray(entry.pointerUnits)
        || !entry.pointerUnits.every(unit => Number.isInteger(unit) && unit >= 0 && unit <= 0xffff)) {
      throw new TypeError('invalid Mods IPC number entry');
    }
    const pointer = stringFromUnits(entry.pointerUnits);
    message = restoreNumber(message, pointer, entry.bits);
  }
  return message;
}

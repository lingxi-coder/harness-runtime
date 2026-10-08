// Functional port of the five runtime exports from Claude Code 2.1.289's
// `claude:hooks-types` module. Keep this source small and independent from the
// embedded BunFS asset; host source graphs provide this exact local module as
// the builtin binding.

const ATOM = Symbol.for('claude-code.state.atom');
const DERIVED = Symbol.for('claude-code.state.derived');

function deepFreeze(value) {
  if (!value || typeof value !== 'object' || Object.isFrozen(value)) return value;
  for (const child of Object.values(value)) deepFreeze(child);
  return Object.freeze(value);
}

function frozenRef(ref) {
  const normalized = { plugin: ref?.plugin, key: ref?.key };
  if (ref?.id !== undefined) normalized.id = ref.id;
  return Object.freeze(normalized);
}

function isAtom(source) {
  return source !== null && typeof source === 'object' && source[ATOM] === true;
}

function isDerived(source) {
  return source !== null && typeof source === 'object' && source[DERIVED] !== undefined;
}

function atomRef(source) {
  return isAtom(source) ? source.ref : source;
}

export function atom(ref, initial, options) {
  const result = {
    ref: frozenRef(ref),
    initial: deepFreeze(initial),
  };
  if (options?.shape !== undefined) result.shape = options.shape;
  result[ATOM] = true;
  return Object.freeze(result);
}

export function derive(sources, compute) {
  const normalized = Object.freeze(sources.map(source => {
    if (isAtom(source) || isDerived(source)) return source;
    return frozenRef(source);
  }));
  const result = { sources: normalized, compute };
  result[DERIVED] = { version: undefined, value: undefined };
  return Object.freeze(result);
}

export function memberOf(family, event) {
  const base = atomRef(family);
  const requestId = event?.requestId;
  const ref = frozenRef({
    plugin: base.plugin,
    key: base.key,
    id: requestId === undefined ? base.id : requestId,
  });
  return isAtom(family) ? Object.freeze({ ...family, ref }) : ref;
}

function readAtomValue(record, source) {
  const stored = record?.value;
  if (source.shape !== undefined) {
    return stored?.shape === source.shape && stored.value !== undefined
      ? stored.value : source.initial;
  }
  return stored === undefined ? source.initial : stored;
}

async function readWithVersions($, source) {
  if (isDerived(source)) {
    const dependencies = await Promise.all(source.sources.map(item => readWithVersions($, item)));
    const versions = dependencies.flatMap(item => item.versions);
    const versionKey = versions.join(',');
    const memo = source[DERIVED];
    const previous = memo.version;
    if (previous === versionKey) {
      return { value: memo.value, versions };
    }
    const value = source.compute(...dependencies.map(item => item.value));
    memo.version = versionKey;
    memo.value = value;
    return { value, versions };
  }

  const ref = atomRef(source);
  const record = await $.state.get(ref);
  return {
    value: isAtom(source) ? readAtomValue(record, source) : record?.value,
    versions: [record?.version],
  };
}

export async function read($, source) {
  return (await readWithVersions($, source)).value;
}

function valueForUpdate(record, target) {
  return isAtom(target) ? readAtomValue(record, target) : record?.value;
}

function valueToStore(value, target) {
  return isAtom(target) && target.shape !== undefined
    ? { shape: target.shape, value } : value;
}

export async function update($, target, change) {
  const ref = atomRef(target);
  for (let attempt = 0; attempt < 64; attempt += 1) {
    const current = await $.state.get(ref);
    const value = change(valueForUpdate(current, target));
    const stored = valueToStore(value, target);
    const result = await $.state.set(ref, stored, { ifVersion: current?.version });
    if (result.isSet) return value;
  }
  throw new Error('update: the value was written by another every time it was read, up to the bound on tries; nothing was written');
}

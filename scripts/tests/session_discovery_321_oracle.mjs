#!/usr/bin/env node
// Controlled replay of unchanged helpers from a hash-pinned installed artifact.
// The earlier fixture supplies inputs only; every expected value is recomputed.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import { createInterface } from 'node:readline';
import { Readable } from 'node:stream';
import vm from 'node:vm';

const args = process.argv.slice(2);
assert(args.filter(arg => !arg.startsWith('--')).length <= 1);
assert(args.every(arg => !arg.startsWith('--') || arg === '--check'));
const binary = readFileSync(args.find(arg => !arg.startsWith('--'))
  ?? path.join(homedir(), '.local/share/claude/versions/2.1.321'));
const sha256 = value => createHash('sha256').update(value).digest('hex');
const native_sha256 = sha256(binary);
assert.equal(native_sha256, 'abc9feeeca8091d51b623291e044cf0528c2f6b74e0c1d6ef671d52c73f7cd3c');
const sourceByteRanges = [];
const ranges = [
  [
    "nfc",
    260566907,
    260566948,
    "9fcd04e31340b326cfb10c35bcf77fa79183f51464cca2488de5349155ef0a04"
  ],
  [
    "hash",
    260791086,
    260791175,
    "23b3f3a43f7958210e3b8195cfdc643aeb0bc8aebafae672d13248a7a5889dc5"
  ],
  [
    "slug",
    261811222,
    261811425,
    "03094f0422f6c3105c0f9488632e826bd6c37e94e6f502f8b3212da9f9198f63"
  ],
  [
    "portablePaths",
    261812104,
    261812333,
    "35201b1380aa27d8e39144823b4d60555d2898aaeff0155a230b1f304df9b97d"
  ],
  [
    "projectsRootCache",
    265962118,
    265962158,
    "2142114c2d16cb2b299743809f0c500f79140d1c91aaaeeb1c9d5776b1fb4676"
  ],
  [
    "projectCachePaths",
    265962672,
    265962919,
    "5e16189168cef9a669445b93bee9e5e503d46d43bed5f67e04e126c151edb095"
  ],
  [
    "unique",
    260587032,
    260587068,
    "56c292081c355b9b9ce0c1c7a450ade80bb47b984070274a9c00b90db1c0cbf9"
  ],
  [
    "diskPath",
    270839546,
    270839602,
    "546dcb77b816463b022fefcdf5ba3d032fcae0b19528ea26eac9f450b87b7fad"
  ],
  [
    "persistence",
    271263286,
    271263547,
    "6418016c495d13d666b51081cbe67e329d379fe8e29f5e7f67c7e5bb7462e57a"
  ],
  [
    "errorCode",
    260447632,
    260447731,
    "a84ab0a2488aed5689416393c679dde4e7f4b29aa980e121bd56b25ab3e7b5e4"
  ],
  [
    "missing",
    260450388,
    260450425,
    "110ed8b0d9326aec9929ad29724cf147279a2d5f02afd8301025542c6c968b9b"
  ],
  [
    "expectedErrors",
    260451138,
    260451296,
    "c07482569fa4c2180544b06c16be942722cd90b7e6e855e0fd4c822890ecac29"
  ],
  [
    "aliases",
    271264078,
    271266374,
    "c49231a5e12232c17fbc2c12b1390c69e29f6f108f430e79d0dbe829988bd3e8"
  ],
  [
    "sort",
    260804468,
    260804621,
    "42c3d61564f766f52901de6761df388dfc2ad2305ea8677686aab6628d1b1ef8"
  ],
  [
    "dedup",
    271479612,
    271479834,
    "6559ee6a8e8e4339204e97ce6d6a721044cca813e1b74f9788dd0eff2e2e0549"
  ],
  [
    "projectListing",
    271479834,
    271480295,
    "33e16e6c8a76dbcd3c36dbce4b145203987b48a7120d4eb67a1882ce1ef9da30"
  ],
  [
    "currentDiscovery",
    271377090,
    271377660,
    "10fca13c4077b08b182ec20cb59b0bd34870001925e4ca5f9c11eff044c6b124"
  ],
  [
    "repositoryDiscovery",
    271456512,
    271459320,
    "16269a025fabd47c2a5fab45cb24618add8e8607cdbba326de97aae0138fe1bf"
  ],
  [
    "collisionBatch",
    271459381,
    271459490,
    "3523818835295af80c87909a6802cc54a6e4d514aab34a99b23a01a50bb163ad"
  ],
  [
    "repositoryCompare",
    271459320,
    271459381,
    "fa73eaa59177d6b200e0f59d67f0d7bea85434134065b97de652a324653bac95"
  ],
  [
    "uuidSyntax",
    261800620,
    261800768,
    "a700b9c2f66c386d764797ada09b72412cc7203533c8b52f6ddb3c847ae9bf67"
  ],
  [
    "messagePresence",
    261805147,
    261805642,
    "319291b5a694577f7a71ac16828ef2e776aacb46e63831e09fe83d35fcd1a076"
  ],
  [
    "uuidMissing",
    261808910,
    261808992,
    "29a91b0a81c0f07876d3772fd605d542835efb078cf889d18a61794188cc7108"
  ],
  [
    "uuidResolution",
    261807247,
    261808432,
    "aa8ecee6169bed0c9acbfd49dfa8eafa62a617797ba2d4bfa45921c973336142"
  ],
  [
    "uuidPresenceProbe",
    261808594,
    261808910,
    "81ac1c568a50ddfab8879d567870cd84c0ac6c9b02456e28d441067bcb075bf5"
  ],
  [
    "uuidGlobal",
    261809635,
    261810010,
    "845f3456c30468ba206f4c630934ed131c1f37093ab18ef59d53eeb8dac7ae3b"
  ],
  [
    "uuidRealpath",
    261812396,
    261812595,
    "a6f27f7c6509366ec01639feb270c0df7d61f7611afc40d1c43c70c0e01d5aa7"
  ],
  [
    "uuidProjectCandidates",
    261813736,
    261814243,
    "a5bbf5bccaa41fb5b708e45d93a0eeaccfef6d90d92dae6c1499b6135055727b"
  ],
  [
    "uuidWorktreeCandidates",
    261815216,
    261815455,
    "2ae546232fdedd862e609609c0f00327f5fef7cb44f8a520d9a35ddfc1cb1bea"
  ],
  [
    "worktreePath",
    261811425,
    261811499,
    "b04a68cca583c66c81359e5bbd152dd6b89e251b38ead72bb26f54c311472a5b"
  ],
  [
    "aliasKey",
    261142566,
    261142629,
    "ffe97361d521ca4e5a81481b22034411a206bc08687231a8d48b619cd893c3b9"
  ]
];
const src = Object.fromEntries(ranges.map(([name, start, end, digest]) => {
  const bytes = binary.subarray(start, end);
  assert(bytes.length > 0 && bytes.length <= 6500, `${name}: unbounded extraction`);
  assert.equal(sha256(bytes), digest, `${name}: source range changed`);
  sourceByteRanges.push({ name, start, end, sha256: digest });
  return [name, bytes.toString('utf8')];
}));

const earlier = JSON.parse(readFileSync(new URL('../../crates/session/tests/fixtures/workspace_session_2_1_307.json', import.meta.url)));
const configDir = '/oracle/.claude';
const clean = value => JSON.parse(JSON.stringify(value));
const error = (code, operation) => Object.assign(new Error(`${code}: controlled ${operation}`), { code });

function context(input = {}) {
  const calls = [], logs = [], errors = [];
  const files = new Map((input.initial_files ?? []).map(file => [file.path, file.utf8]));
  const cache = { projectDirCache: new Map(), cacheProjectDir(key, value) { this.projectDirCache.set(key, value); } };
  const record = (operation, fields = {}) => calls.push({ operation, ...fields });
  const fail = operation => { if (input.failures?.[operation]) throw error(input.failures[operation], operation); };
  const ctx = vm.createContext({
    Buffer, S: path.posix.join, rm: path.posix.join, zS: path.posix.dirname, pA: path.posix.basename,
    c: { of: () => cache }, F: () => ({ host: 'controlled-host' }), Be: () => configDir,
    u3n: () => input.project_key, aOe: () => input.source_project_dir,
    Bs: () => input.pinned_project_path ?? null, ve: () => input.current_dir ?? '/workspace/A',
    X: () => input.storage_v5 === true, q: () => input.diskless === true,
    i_a: () => input.persistence?.runtime_environment ?? 'production',
    i: Object.assign(path.posix.join, { TEST_ENABLE_SESSION_PERSISTENCE: input.persistence?.test_enable === true,
      CLAUDE_CODE_SKIP_PROMPT_HISTORY: input.persistence?.skip_prompt_history === true }),
    dL: () => input.persistence?.explicit_disable === true, Pbt: () => input.persistence?.nested_marker === true,
    t: (message, options) => logs.push({ message, ...(options ? { level: options.level } : {}) }),
    a: value => errors.push({ code: value.code ?? null, message: value.message ?? String(value) }),
    oI: (kind, file) => record('diskless_write_suppressed', { kind, path: file }),
    nGt: async value => { record('realpath', { path: value }); fail('realpath'); return input.realpath_result ?? value; },
    c$: async (file, encoding) => {
      record('read', { path: file, encoding }); fail('read');
      if (!files.has(file)) throw error('ENOENT', 'read');
      return files.get(file);
    },
    d$: async (directory, options) => { record('mkdir', { path: directory, ...options }); fail('mkdir'); },
    SKe: async (file, text, options) => {
      record('append', { path: file, utf8: text, ...options }); fail('append');
      files.set(file, (files.get(file) ?? '') + text);
    },
    ag: () => input.invalid_storage_key ? 'controlled invalid key' : undefined, F2t: () => undefined,

    Bt: value => value.code + (value.telemetryCode ? ` ${value.telemetryCode}` : ''),
    LGt: value => value.code === 'Failed' && value.telemetryCode === 'NotFound',
    Rkt: value => value.code === 'Failed' && value.telemetryCode === 'AbsentParent',
    MGt: value => error(value.errno ?? 'EIO', 'backend ensure scope'),
    XQt: () => async value => {
      record('backend_realpath', { path: value }); fail('realpath');
      return input.backend_realpath_error ? { ok: false, error: input.backend_realpath_error }
        : { ok: true, value: { found: input.backend_realpath_found !== false, path: input.realpath_result ?? value } };
    },
  }, { codeGeneration: { strings: false, wasm: false } });
  vm.runInContext([src.nfc, src.hash, src.slug, src.worktreePath, src.portablePaths, src.projectsRootCache, src.projectCachePaths,
    src.unique, src.diskPath, src.persistence, src.errorCode, src.missing, src.expectedErrors,
    src.aliases, src.sort, src.dedup].join('\n'), ctx, { timeout: 1000 });
  ctx.Ye = vm.runInContext(`({${src.aliasKey}})`, ctx, { timeout: 1000 });
  const aliasFile = key => path.posix.join(configDir, 'projects', key.projectKey, '.session-aliases');
  let updateAttempts = 0;
  const backend = {
    readText: async keys => {
      record('backend_read', { keys }); fail('backend_read');
      if (input.backend_read_error) return { ok: false, error: input.backend_read_error };
      return { ok: true, value: { items: keys.map(key => files.has(aliasFile(key))
        ? { found: true, value: files.get(aliasFile(key)) } : { found: false }) } };
    },
    updateText: async (key, updater, options) => {
      updateAttempts++;
      const file = aliasFile(key), decision = clean(updater(files.has(file) ? { value: files.get(file) } : undefined));
      record('backend_update', { key, options, decision }); fail('backend_update');
      if (input.backend_absent_parent && updateAttempts === 1) return { ok: false, error: { code: 'Failed', telemetryCode: 'AbsentParent' } };
      if (input.backend_update_error) return { ok: false, error: input.backend_update_error };
      if (decision.write !== undefined) files.set(file, decision.write);
      return { ok: true, value: {} };
    },
    ensureScope: async scope => {
      record('backend_ensure_scope', { scope }); fail('backend_ensure_scope');
      return input.backend_ensure_error ? { ok: false, error: input.backend_ensure_error } : { ok: true, value: {} };
    },
  };
  return { ctx, record, calls, files, backend, snapshot: () => ({
    files: [...files].sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0)
      .map(([file, utf8]) => ({ path: file, utf8, hex: Buffer.from(utf8).toString('hex') })), calls, logs, errors,
  }) };
}

const paths = context().ctx;
const path_cases = [...earlier.path_cases.map(({ input }) => input),
  '/workspace/' + '😀'.repeat(110), '/workspace/' + 'é'.repeat(210),
  '/workspace/' + 'a'.repeat(198) + '😀'].map(input => ({ input,
  expected: { slug: paths.JD(input), project_dir: paths.Ck(input), nfc: paths.pn(input) } }));
const alias_write_cases = [];
for (const { name, input } of earlier.alias_write_cases) {
  const replay = context(input);
  for (let n = 0; n < (input.repeat ?? 1); n++) await replay.ctx.rco(input.added_dir, input.storage_v5 ? replay.backend : undefined);
  alias_write_cases.push({ name, input, expected: replay.snapshot() });
}
const alias_read_cases = [];
for (const { name, input } of earlier.alias_read_cases) {
  const replay = context(input);
  const aliases = await replay.ctx.jKe(input.project_path, input.storage_v5 ? replay.backend : undefined);
  alias_read_cases.push({ name, input, expected: { aliases: [...aliases], ...replay.snapshot() } });
}

function nativeRecord(value) {
  return { ...(value.session_id !== undefined ? { sessionId: value.session_id } : {}),
    modified: new Date(value.modified_ms), created: new Date(value.created_ms),
    ...(value.tag !== undefined ? { tag: value.tag } : {}),
    ...(value.full_path !== undefined ? { fullPath: value.full_path } : {}) };
}
function projectRecord(value) {
  return { session_id: value.sessionId ?? null, modified_ms: value.modified.getTime(), created_ms: value.created.getTime(),
    full_path: value.fullPath ?? null, project_path: value.projectPath ?? null, is_alias: value.isAlias === true,
    value: value.value, own_worktrees: value.ownWorktrees ?? null, ...(value.tag !== undefined ? { tag: value.tag } : {}) };
}
const dedup_cases = earlier.dedup_cases.map(({ name, input }) => ({ name, input,
  expected: paths.kZ(input.map(nativeRecord)).map(projectRecord) }));

function discoveryContext(input) {
  const replay = context(input), { ctx, record } = replay;
  Object.assign(ctx, {
    vk: 65536, tP: backend => backend, US: backend => backend,
    wKs: async cwd => { record('resolve_current_project', { path: cwd }); return input.resolved_project_dir; },
    YJt: async project => {
      record('list_sessions', { project_dir: project });
      return new Map((input.project_rows?.[project] ?? []).map(value => [value.session_id,
        { mtime: value.modified_ms, ctime: value.created_ms, size: value.file_size ?? 10,
          path: value.full_path ?? path.posix.join(project, `${value.session_id}.jsonl`) }]));
    },
    Rzt: async row => { record('controlled_foreign_header', { session_id: row.sessionId });
      return (input.foreign_session_ids ?? []).includes(row.sessionId); },
    IKe: async (cwd, project, maximum, descendants) => {
      record('controlled_collision_recovery', { path: cwd, project_dir: project, max_count: maximum ?? null, descendants });
      return (input.recovery_rows?.[cwd] ?? []).map(nativeRecord);
    },
    Azt: async () => { record('controlled_collision_inventory'); return []; },
    QI: async (directory, options) => {
      record('list_project_directories', { path: directory, ...options });
      if (input.failures?.list_project_directories) throw error(input.failures.list_project_directories, 'list_project_directories');
      return (input.project_directories ?? []).map(value => ({ name: value.name, isDirectory: () => value.is_directory }));
    },
    U_e: async () => { record('controlled_backend_project_directories'); return input.storage_project_directories; },
    SWo: async rows => record('observe_fork_statistics', { session_ids: rows.map(row => row.sessionId) }),
    E4n: async () => { throw new Error('Long-slug header validation is outside this replay'); },
  });
  vm.runInContext([src.projectListing, src.currentDiscovery, src.repositoryDiscovery, src.repositoryCompare, src.collisionBatch].join('\n'), ctx, { timeout: 1000 });
  return replay;
}
const discovery_cases = [];
for (const { name, input } of earlier.discovery_cases) {
  // Header/tail validation of truncated legacy slug buckets is covered by the
  // earlier independent disk oracle; do not replace it with a fabricated result.
  if (name.includes('long_slug') || name.includes('truncated')) continue;
  const replay = discoveryContext(input);
  const rows = input.selector === 'current'
    ? await replay.ctx.xfs(input.max_count, input.storage_v5 ? replay.backend : undefined)
    : await replay.ctx.Czt(input.repository_paths ?? [], input.max_count, input.storage_v5 ? replay.backend : undefined);
  discovery_cases.push({ name, input, expected: { sessions: rows.map(projectRecord), ...replay.snapshot() } });
}

function uuidContext(input) {
  assert(!input.storage_v5);
  const replay = context(input), { ctx, record } = replay;
  const transcripts = new Map((input.transcripts ?? []).map(file => [file.path, file]));
  const checkFile = (file, operation) => {
    const value = transcripts.get(file);
    if (value?.[`${operation}_error`]) throw error(value[`${operation}_error`], operation);
    if (!value) throw error('ENOENT', operation);
    return value;
  };
  Object.assign(ctx, {
    F: path.posix.dirname, E: path.posix.basename,
    J: async value => { record('resume_realpath', { path: value });
      if (input.realpath_error) throw error(input.realpath_error, 'resume realpath');
      return input.realpath_result ?? value; },
    b: async (directory, options) => {
      record('resume_readdir', { path: directory, with_file_types: options?.withFileTypes === true });
      if (directory === ctx.qm()) {
        if (input.project_directory_error) throw error(input.project_directory_error, 'resume readdir');
        return options?.withFileTypes ? (input.project_directories ?? []).map(value => ({ name: value.name, isDirectory: () => value.is_directory }))
          : (input.project_directories ?? []).map(value => value.name);
      }
      if ([...transcripts.keys()].some(file => path.posix.dirname(file) === directory)
        || (input.existing_project_directories ?? []).includes(directory)) return [];
      throw error('ENOENT', 'resume readdir');
    },
    R: async file => { record('resume_lstat', { path: file }); const value = checkFile(file, 'lstat');
      return { isFile: () => value.kind !== 'directory' }; },
    B: async (file, flags) => {
      record('resume_open', { path: file, flags }); const value = checkFile(file, 'open');
      return { createReadStream: () => value.stream_error
        ? Readable.from((async function* () { throw error(value.stream_error, 'resume stream'); })())
        : Readable.from([Buffer.from(value.utf8 ?? '')]), close: async () => record('resume_close', { path: file }) };
    },
    de: createInterface, Tua: () => undefined,
    wpn: async cwd => { record('controlled_git_worktrees', { path: cwd }); return input.repository_paths ?? []; },
    E4n: async () => { throw new Error('Long-slug header validation is outside this replay'); },
  });
  vm.runInContext([src.uuidSyntax, src.messagePresence, src.uuidMissing, src.uuidResolution,
    src.uuidPresenceProbe, src.uuidGlobal, src.uuidRealpath, src.uuidProjectCandidates,
    src.uuidWorktreeCandidates].join('\n'), ctx, { timeout: 1000 });
  return replay;
}
const message_presence_cases = [];
for (const { name, input } of earlier.message_presence_cases) {
  const replay = uuidContext(input), classification = await replay.ctx.j(input.path);
  const has_messages = await uuidContext(input).ctx._(input.path);
  message_presence_cases.push({ name, input, expected: { classification, has_messages, calls: replay.calls } });
}
const uuid_resolution_cases = [];
for (const { name, input } of earlier.uuid_resolution_cases) {
  const replay = uuidContext(input);
  const resolution = await replay.ctx.sre(input.session_id, input.current_dir, input.link_scan_path,
    { crossWorktree: input.cross_worktree !== false });
  uuid_resolution_cases.push({ name, input, expected: { resolution: clean(resolution), calls: replay.calls } });
}

const result = { schema_version: 1, version: '2.1.321', native_sha256, evidence_type: 'controlled-replay', sourceByteRanges,
  scope: 'Project path bytes, alias read/write bytes, current/repository discovery, deduplication, disk message presence and raw UUID job resolution.',
  limitations: ['Filesystem inventory, Git worktree enumeration, header collision classification and backend transport are controlled dependencies.',
    'Raw UUID job resolution is distinct from full CLI --resume hydration.',
    'This replay excludes long-slug header/tail validation, full transcript loading, UI and live remote storage.'],
  path_cases, alias_write_cases, alias_read_cases, dedup_cases, discovery_cases, message_presence_cases, uuid_resolution_cases };
const output = JSON.stringify(result, null, 2) + '\n';
const fixture = new URL('../../crates/session/tests/fixtures/session_discovery_2_1_321.json', import.meta.url);
if (args.includes('--check')) assert.equal(readFileSync(fixture, 'utf8'), output, 'Native session-discovery fixture changed');
else writeFileSync(fixture, output);
const counts = Object.fromEntries(Object.entries(result).filter(([name, values]) => name.endsWith('_cases')).map(([name, values]) => [name, values.length]));
console.log(JSON.stringify({ version: result.version, native_sha256, fixture_sha256: sha256(output), source_ranges: sourceByteRanges.length, counts }));

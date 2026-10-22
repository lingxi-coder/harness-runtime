#!/usr/bin/env node
// Controlled replay of narrow helpers from an installed, hash-pinned artifact.
// Only the generated inputs, outputs, and source receipts are checked in.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { constants as fsConstants, readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import { createInterface } from 'node:readline';
import { Readable } from 'node:stream';
import vm from 'node:vm';

const args = process.argv.slice(2);
assert(args.filter(arg => !arg.startsWith('--')).length <= 1, 'Expected at most one binary path');
assert(args.every(arg => !arg.startsWith('--') || arg === '--check'), 'Unknown option');
const binaryPath = args.find(arg => !arg.startsWith('--'))
  ?? path.join(homedir(), '.local/share/claude/versions/2.1.307');
const binary = readFileSync(binaryPath);
const sha256 = value => createHash('sha256').update(value).digest('hex');
const nativeSha256 = sha256(binary);
assert.equal(nativeSha256, '5b113a7752119470f1da419cb0e608af3361cae25b614216b13022093958fb2a',
  'Unrecognized Claude Code 2.1.307 oracle binary');
const sourceByteRanges = [];
function take(name, start, end, digest, first, next) {
  const bytes = binary.subarray(start, end);
  assert(bytes.length > 0 && bytes.length < 5000, `${name}: extraction is not bounded`);
  assert.equal(sha256(bytes), digest, `${name}: source digest changed`);
  assert(bytes.toString('utf8').startsWith(first), `${name}: start token changed`);
  assert.equal(binary.subarray(end, end + Buffer.byteLength(next)).toString('utf8'), next,
    `${name}: next token changed`);
  sourceByteRanges.push({ name, start, end, sha256: digest });
  return bytes.toString('utf8');
}
const src = {
  nfc: take('NFC path normalization', 216872691, 216872732,
    '771fc6097541a666fb7293c41230442a4c8903f8f859bf941aae9cc6ed76ca6c', 'function Pn(', 'function jn('),
  hash: take('project slug string hash', 217151688, 217151777,
    'f8264e2f3643d145919a7e8601ce00c66a42f3d48869ea1f7552cf2cd563030e', 'function Bue(', 'function Z0('),
  slug: take('project slug and truncation', 218109812, 218110015,
    'f97eb9e424020d37ce2db01485aef1db19cc54d82ee4987c5bd0df951587c6d9', 'var K_e=200;', 'function WVe('),
  worktreePath: take('worktree path slash comparison', 218110015, 218110089,
    '1ecf83c926a42dd7668fb82ad8b14d1c941be27b52ab595d79307898c93b5058', 'function WVe(', 'function N('),
  slugOverride: take('project slug override', 218110734, 218110769,
    '254e90bb8469a255c38d13151e8946b279bbb074d630e0534f847aeb309ae67d', 'function Uk(', 'function Vbn('),
  pinnedProject: take('pinned project storage directory', 218110863, 218110923,
    '9f77fc3e5c73c0bf152379968fd11e1cfe705eeb492401b93668b45770692f1d', 'function Jnt(', 'function j8o('),
  projectsRoot: take('projects root', 222143775, 222143815,
    '42616533d2964d5a6717a49575d8c703ac8ba5d3fc400fa5af7a5a5fff7d52df', 'function yf(', 'function r2('),
  projectPath: take('project and authoritative source storage paths', 222144329, 222144575,
    'ce380c8ce6b14e1f2b9d87d9f3ef92396fbbd36e0c1a03396359c87fad77a592', 'function zS(', 'function ep('),
  unique: take('ordered unique aliases', 216899764, 216899800,
    '8c94c4f0b38c07072c5d889a76cea71e49dafe6edfea26d66fe927b3932e5997', 'function D(', 'function Fi('),
  sort: take('session modified and created order', 217162773, 217162925,
    '9caa01f645b2ce71c098533c498f7deea31c976cb8a73b22d813eb060ec5d4ff', 'function fZ(', 'import n from'),
  diskPath: take('diskless path suppression', 226119388, 226119444,
    'fcb3205316ace4bfb62ee650bb07e5c732a570b06bc8697d41ce9deb904ec064', 'function i0(', '\nexport'),
  disabled: take('session persistence suppression', 226462691, 226462952,
    'e2396558e1cfc251c7f73350e1d9b974ba677614b52f379d7a957d24b471f757', 'function Ya(', 'function PPr('),
  aliases: take('alias storage selection, append and direct read', 226463507, 226465802,
    'bf748b3a4b590f4f00c6ac556e96bb52496c211448766a09da6c39cfe4477860', 'var j0e=', 'class K0e'),
  current: take('current and directly linked session discovery', 226562235, 226562805,
    '951e56a6e6f0c03d90ccd6c0fdcac74f8c76d5b84d1b007c30d1f3f8203ad64b', 'async function CCo(', 'function bxt('),
  repository: take('same repository session discovery', 226627795, 226630600,
    '05167643abee68afc2ce1032becc9192dc70c5765d1773a3257f3b9942071ccb', 'async function LAt(', 'function oHe('),
  dedup: take('session identity newest copy selection', 226648876, 226649097,
    '0cc8c2ff318ba821dc33b7bb730a1843faba2be53723db9c950019068c2f61e8', 'function G9(', 'async function KBe('),
  projectList: take('project lightweight session listing and local limit', 226649097, 226649557,
    '4417f14821d23f6dd29253b75582a99e6ada1d766eec6bb77ff6fb27094b4ebf', 'async function KBe(', 'async function jAt('),
  errorCode: take('filesystem error code', 216846233, 216846332,
    'fac610e89149796538ed2079d613248acb1ba8228f14c4b19c9a70581fe0af8b', 'function v(', 'function _r('),
  missing: take('missing file classification', 216848990, 216849027,
    '1a215cb6e1b034ee2266a15f7ef2b3f21a141201b3f3d6437ee59dd7c6f2fffd', 'function G(', 'function wi('),
  expectedErrors: take('recoverable filesystem errors', 216849740, 216849898,
    'f6c93e8ea8c99912499c5eefa74aa39facb666a506fed96597b9f6ed40c5af23', 'var u=new Set(', 'function QC('),
  aliasKey: take('session alias backend key', 217547867, 217547930,
    'ffe97361d521ca4e5a81481b22034411a206bc08687231a8d48b619cd893c3b9', 'sessionAliases:', ',dirSyncRecord:'),
  repositoryCompare: take('same repository comparison and collision gate', 226630600, 226630661,
    '146cedb31e39e02a40c0f818046e775ec1d60164eaffd312595386c0629983fe', 'function oHe(', 'async function Ice('),
  collisionBatch: take('all session headers foreign predicate', 226630661, 226630770,
    'c43bb399c257b2e9d11d72e44de088d85e79befc1a9e1f90495f4782731546b6', 'async function Ice(', 'async function NAt('),
  uuidSyntax: take('resume UUID syntax', 218099235, 218099383,
    '4f8851ee0afeee29a972562f6e5393a1639a6d138652b68e90957cc652908c5f', 'var ye=', 'function Y('),
  messagePresence: take('transcript message presence and unknown policy', 218103735, 218104230,
    'b9dcc0a76b40af25d6bf0c77233892f11bff60cedacab5fa5347a67c31cc11c5', 'async function H(', 'var Se='),
  uuidMissing: take('resume ENOENT classification', 218107498, 218107580,
    '29a91b0a81c0f07876d3772fd605d542835efb078cf889d18a61794188cc7108', 'function x(', 'function j('),
  uuidResolution: take('resume candidate priority and fallback', 218105835, 218107020,
    '8060ed6d7c3d65516c1ebb6cc8efd0d94c1a7f1f599b97708cf65432fe84ec30', 'async function Iue(', 'function Q('),
  uuidPresenceProbe: take('resume candidate presence probe', 218107182, 218107498,
    '0eece159c03e28f554d798058e9f3d6b468377eabb4676e4a83b73bb0f685053', 'async function Pe(', 'function x('),
  uuidGlobal: take('unique global resume candidate and duplicate rejection', 218108223, 218108598,
    '5ff51d87ce10fb0664a88799865d9110b08c9cb3f315e016fc4aea2743639cc1', 'async function f3r(', 'async function ve('),
  uuidRealpath: take('resume realpath and NFC fallback', 218110986, 218111157,
    'ed74f94e2522a2567c75d55741a3d911a4e882842801ecc7f2cb44f3c6e87f9d', 'async function wS(', 'function wgi('),
  uuidCwdCandidates: take('canonical resume cwd candidates', 218111157, 218111185,
    'cf491e8317df4df7a2320be2d3377c935eaebbce94e05ec24d4b8de87b77e34a', 'function wgi(', 'async function Kbn('),
  slugFallback: take('project slug override fallback', 218110769, 218110823,
    '12098a2201f71f2e7acdc00ccf39d6eb2f763c1071ddf8e245830e1e5cddd79f', 'function Vbn(', 'function Ub('),
  uuidProjectPath: take('resume project bucket path', 218110823, 218110863,
    '72c4998c391632f5d5d1e920f30aa8a05a4444dd672b162e264f73322600842a', 'function Ub(', 'function Jnt('),
  uuidProjectCandidates: take('current resume project directory candidates', 218112327, 218112834,
    'a967411d9a98320a0b36d81c49203e17f8b8cfd1a5653292ca63b4a2719209ff', 'async function rRe(', 'async function Be('),
  uuidWorktreeCandidates: take('resume linked worktree project candidates', 218113807, 218114046,
    'd49cf32b1d314a15f0f6b142b762cfba4cd0501fe798008856a2eb8cfd2e4e5b', 'async function te(', 'async function h3r('),
  uuidRealpathAdapter: take('resume backend realpath adapter', 218100628, 218100735,
    'd1f252a9b76174726e21eed83f11cd844438ea9318503df1ac53969b03d86c7d', 'function psi(', 'async function BVe('),
  cdMatcher: take('Cd deny and allow checks, rooted match and glob', 258505557, 258506714,
    'ad9480ac866736efe02e13d99952eac763652dd8977553862d9d0a2efcc693ab', 'var b="Cd";', 'async function Rbe('),
  cdError: take('Cd rejection messages', 258507411, 258508488,
    '70ed0af5fb7939c35342e8c37cf831d3d1c0f0ea4d04815143da4358956dca33', 'function Jpr(', 'async function U('),
  cdMove: take('directory move transaction and model message', 258508837, 258512305,
    '887e7f6476d0e6d2f19c38137f5036b050b7eea25dcc67ffd883320cceffd7de', 'async function WAt(', 'function Qpr('),
  cdControl: take('set_cwd validation, trust echo and control responses', 258512650, 258516668,
    'f29090b1191ae3d9cff1fdd40e32e91cbc69e7b477fbb4b876cb654b6e15a322', 'async function Byi(', '\nexport'),
  invisible: take('invisible and nonstandard space trust-boundary characters', 224468150, 224468243,
    '0b28e526c8d562f46f2abdbd2d7b02e4621c2745627941e67c28117a5d34f98b', 'var Son=', 'function gOr('),
  ruleEscape: take('permission rule escaping', 217177769, 217177860,
    '23c214fc12bc847c7f89cc62fc1f78cac028a1bee50b0750beca68e707dad62a', 'function h(', 'function _ue('),
  ruleFormat: take('permission rule display', 217179054, 217179156,
    '25d93fc3e13518109804ca7685d2ae03095e09f8d465c6bc0669e09817726be4', 'function gn(', 'var dj='),
  sourceLabels: take('permission rule source display labels', 217608972, 217609523,
    'c776f9a0a0ab3d0b5ac314966103a03c2fa28f014a1274512dac609988a15820', 'function vx(', 'function s3o('),
  sourceLabelAdapter: take('Cd source display adapter', 229138404, 229138433,
    'f88d07854710ba5670aa13f39fc9e277b120abebbd926245052c45d76f1a13bc', 'function Z1e(', 'function GTr('),
  ruleRoots: take('permission pattern roots', 222606536, 222607316,
    'e145be07facdb6bb26602b7e24e9aa2ca8de27ccb7867652e90c074b34cb57da', 'function Jl(', 'function rn('),
  ruleSourceRoots: take('permission source roots', 222603948, 222604211,
    '6d7d9816cd0b49153c34325073e38d3ed72c96374da7d655011bceef4da39d68', 'function LUt(', 'function Tr('),
  ruleRootAdapter: take('Cd rooted pattern adapter', 222607485, 222607527,
    '3a86cdb4acec3c14a8b3c86d3a3bc9a723d23c87d8a3e85aa820a10dc46fe2f3', 'function m0e(', 'function gNo('),
  relativePath: take('permission root relative path', 222582646, 222582752,
    'aff12c3894bee9bcd325c7691213105668d9e5dd8e86188a330cc9530f56ccd3', 'function T2e(', 'function tx('),
  canonicalAliases: take('permission canonical path spelling equivalence', 222614813, 222614914,
    '7e9bcc717c6b712ec38066675e928740b6738498daed2238dfebe2bfeb179d91', 'function Rcn(', 'function xcn('),
  safeDisplay: take('trust-boundary rule display suppression', 258512609, 258512650,
    '7b52ab623630581393a07250105c2360cdea0dd541319fe85d9cf9f75a484344', 'function Dli(', 'async function Byi('),
  settingsRoot: take('project permission rule root', 217966208, 217966296,
    'a1785c2caa55098283ef9945f8e3b02934350c1b7ae95f56673111eddf373f38', 'function M4o(', 'function XEt('),
  settingsSourceRoot: take('settings rule source root', 217963835, 217964242,
    'e978bd94cec409fe3bf9f06411bf16185af173dc68ef957b50c95e3810bf811d', 'function cSn(', 'function kU('),
  reminder: take('model system reminder wrapper', 231132082, 231132123,
    '295679b31a1c6adb93f79d5ddac6ca8e1fcdc816818aebea6f70633b044640b5', 'function Ul(', 'function _Le('),
  reminderMarkers: take('model system reminder markers', 217165493, 217165544,
    'cc5c95bc59ac961746ae5160530731257648c5bb815c052f4a79d881480d432c', 'var DU=', ',ckt='),
  kbnTail: take('relocated cwd tail JSON scanning', 218099955, 218100334,
    'f6028c6c0e91ff8853fe0c8a5bba1af6c086f130a97ac5173dafd1ba256c18e9', 'function Ynt(', 'function Pue('),
  kbnHead: take('head cwd JSON scanning', 218100334, 218100596,
    '36ae57f4a4444d6bdfb70cfbe8d6b42506d3b45c10a2816f660413af4df4a4d5', 'function Pue(', 'function k6t('),
  kbn: take('truncated project slug transcript probe', 218111185, 218111606,
    '0e568c1e0afec173fa1650508ff3a5378fe1f259b61f9be60cd6322f11ea6bf9', 'async function Kbn(', 'function ne('),
  kbnIdentity: take('truncated project slug recorded cwd identity', 218111606, 218111776,
    '520440bf46543e83f1363670a7db80837acc59395480ff5903d6a240e52462f4', 'function ne(', 'async function Te('),
  kbnRead: take('transcript head and tail descriptor reads', 218109141, 218109672,
    'b1fd9cb6ed187ec9a166cdcab51e09055a121bdc6650d84c9ec116c065444306', 'async function V_e(', 'async function ee('),
  kbnFlags: take('transcript safe read flags', 218102591, 218102634,
    '08a6a204b57adef4801ec190f6412dbe4a86cf2f4640e0f3bcb6256a252245a2', 'var T=', 'async function Gbn('),
  kbnBuffer: take('transcript head and tail buffer size', 218098964, 218099020,
    '276dae64bc4e1debff85c563667db898f7c36f661a65ff0f7d1b0b6a40703b27', 'var $b=', 'function Tki('),
  addDirValidate: take('add-dir target validator', 234801279, 234802261,
    'aab1855549669ca539b839c55f00da5c623bf433b769ff6ab7eb07496430ad3c', 'async function xwe(', 'function Pwe('),
  addDirMessages: take('add-dir validation messages', 234802261, 234803677,
    '2f149c6e1a1ec9f24324d02b3b9c57f61a3fe85c2e6e3cc00c846409db9eef4a', 'function Pwe(', '\nexport'),
  addDirEffect: take('successful add-dir effect and alias call', 241955857, 241957317,
    'c5e20092a37ab8c48eb21a2cf390de9e3cf35c9410beb7be980d2f4f011a5c97', 'async function UDn(', 'function g7t('),
  addDirChild: take('current child directory skills without alias recording', 241957317, 241958898,
    'ff714c393b9de3fdacaa65ea671836061d6e9a29d747af238b29fe1ddf6af022', 'function g7t(', '\nexport'),
  addDirText: take('noninteractive add-dir caller short circuit', 241974737, 241975034,
    'c1c1281f2ecdddb26a844b8bbb4cd347795e12a0c518a921883d13b6001bfec2', 'async function d(', 'export{d as call}'),
  addDirSkills: take('additional prompt directory registration', 241955733, 241955857,
    '047d04c789eec7c388cf7d9c65928629636ca5ef78e1c8736dff3e39f7cc1d79', 'function h(', 'async function UDn('),
  permissionRoots: take('permission working directory roots', 222601395, 222601474,
    '0792330702b31005c35324c90057eb857486e4b367ecb81915dc73bc36c0d06e', 'function gk(', 'function HJe('),
  permissionAdditionalRoots: take('read fence additional directory roots', 222601695, 222601828,
    'fa7e6e54358fce6ac6e9cd8e65cc7f768e58cac5b266f0127942278833ebada6', 'function ql(', 'function Ccn('),
  permissionRestrictedRoots: take('read fence working directory roots', 222601876, 222601924,
    '7565f478b75e7eca1552d45a1d12aa7d21687a737a59a252732d8ffaca4c8755', 'function J5n(', 'function f0e('),
  permissionInsideRoot: take('directory containment and private alias comparison', 222603537, 222603948,
    'ba96cf4a5508570930455a7c7cee406622894c83cb3b2cebe565b3d8af79a385', 'function Zu(', 'function LUt('),
  permissionSpellings: take('permission root spelling cache', 222601474, 222601695,
    '604ea0e996ffffb7f576a82d0aef5d8c2dd00e8914af1bd1b5b0716c57b42120', 'function HJe(', 'function ql('),
  permissionCoverage: take('permission working directory coverage', 222602136, 222602289,
    '0e30abf0c166e7d3321b380c174e0635cb1b315a37f47d0c140c690df3b5d999', 'function my(', 'var Xl='),
  rawCd: take('interactive cd caller and validation branches', 262608081, 262610492,
    'c295dcc381fff5f0eb2386f475c0bf6acfcf546cf2caf45e7a753d3773517e4d', 'async function Bo(', 'async function Fe('),
  flagResume: take('CLI resume loader selection before transcript processing', 227912880, 227917613,
    'b90e6010c76c88c8d892277435aabf770b364a623b24ed3935aaef4c4ce84c15', 'async function FIe(', 'async function iQo('),
  flagRepositoryResume: take('CLI resume repository bucket iteration', 227917613, 227917817,
    '43e3b143910d6f24a45486fdc296698ede9b4e839630ec7deb12caec15f0dfde', 'async function iQo(', 'async function fQo('),
  flagGlobalResume: take('CLI resume global candidate full-log hydration', 227917817, 227917980,
    'd228ff102f99d3943b478031a248ce01e590d946915436361dae8f41c873b6c0', 'async function fQo(', 'function mQo('),
  slashResume: take('interactive resume command argument selection', 261391979, 261394333,
    '3eabdc5718cf8864bab6a80bfa5c7d66db9feaf1c0bff6b5d95de0de1b25db4d', 'var De=async', 'export{De as call'),
};

const configDir = '/oracle/.claude';
const A = '/workspace/A', B = '/workspace/B', C = '/workspace/C';
const clean = value => JSON.parse(JSON.stringify(value));
function controlledError(code, operation) {
  return Object.assign(new Error(`${code}: controlled ${operation}`), { code });
}

function replayContext(input = {}) {
  const calls = [], logs = [], errors = [];
  const files = new Map((input.initial_files ?? []).map(file => [file.path, file.utf8]));
  const cache = { projectDirCache: new Map(), cacheProjectDir(key, value) { this.projectDirCache.set(key, value); } };
  const record = (operation, fields = {}) => calls.push({ operation, ...fields });
  const fail = operation => {
    const code = input.failures?.[operation];
    if (code) throw controlledError(code, operation);
  };
  const context = vm.createContext({
    Buffer,
    c: { of: () => cache }, B: () => ({ host: 'controlled-host' }),
    i: path.posix.join, S: path.posix.join, Gc: path.posix.join,
    eS: path.posix.dirname, $$: path.posix.basename,
    He: () => configDir, Bf: () => path.posix.join(configDir, 'projects'),
    Uwn: () => input.project_key, pi: () => input.pinned_project_path ?? null,
    pSe: () => input.source_project_dir, ke: () => input.current_dir ?? A,
    z: () => input.storage_v5 === true, Z: () => input.diskless === true,
    hui: () => input.persistence?.runtime_environment ?? 'production',
    a: { TEST_ENABLE_SESSION_PERSISTENCE: input.persistence?.test_enable === true,
      CLAUDE_CODE_SKIP_PROMPT_HISTORY: input.persistence?.skip_prompt_history === true },
    lH: () => input.persistence?.explicit_disable === true,
    Xet: () => input.persistence?.nested_marker === true,
    t: (message, options) => logs.push({ message, ...(options ? { level: options.level } : {}) }),
    l: error => errors.push({ code: error.code ?? null, message: error.message ?? String(error) }),
    BT: (kind, file) => record('diskless_write_suppressed', { kind, path: file }),
    Hxt: async value => {
      record('realpath', { path: value }); fail('realpath');
      return input.realpath_result ?? value;
    },
    yI: async (file, encoding) => {
      record('read', { path: file, encoding }); fail('read');
      if (!files.has(file)) throw controlledError('ENOENT', 'read');
      return files.get(file);
    },
    hI: async (directory, options) => { record('mkdir', { path: directory, ...options }); fail('mkdir'); },
    r0e: async (file, text, options) => {
      record('append', { path: file, utf8: text, ...options }); fail('append');
      files.set(file, (files.get(file) ?? '') + text);
    },
    // Storage-key validation and backend transport are controlled dependencies.
    fm: () => input.invalid_storage_key ? 'controlled invalid key' : undefined,
    Pvt: () => undefined,
    bt: error => error.code + (error.telemetryCode ? ` ${error.telemetryCode}` : ''),
    bkt: error => error.code === 'Failed' && error.telemetryCode === 'NotFound',
    xrt: error => error.code === 'Failed' && error.telemetryCode === 'AbsentParent',
    iAt: error => controlledError(error.errno ?? 'EIO', 'backend ensure scope'),
    bLt: () => async value => {
      record('backend_realpath', { path: value }); fail('realpath');
      return input.backend_realpath_error
        ? { ok: false, error: input.backend_realpath_error }
        : { ok: true, value: { found: input.backend_realpath_found !== false, path: input.realpath_result ?? value } };
    },
  }, { codeGeneration: { strings: false, wasm: false } });
  const basic = [src.nfc, src.hash, src.slug, src.worktreePath, src.slugOverride,
    src.pinnedProject, src.projectsRoot, src.projectPath, src.unique, src.sort,
    src.diskPath, src.disabled, src.errorCode, src.missing, src.expectedErrors, src.aliases];
  vm.runInContext(basic.join('\n'), context, { timeout: 1000 });
  context.We = vm.runInContext(`({${src.aliasKey}})`, context, { timeout: 1000 });
  const aliasFile = projectKey => path.posix.join(configDir, 'projects', projectKey, '.session-aliases');
  let updateAttempts = 0;
  const backend = {
    readText: async keys => {
      record('backend_read', { keys });
      fail('backend_read');
      if (input.backend_read_error) return { ok: false, error: input.backend_read_error };
      return { ok: true, value: { items: keys.map(key => files.has(aliasFile(key.projectKey))
        ? { found: true, value: files.get(aliasFile(key.projectKey)) } : { found: false }) } };
    },
    updateText: async (key, updater, options) => {
      updateAttempts += 1;
      const file = aliasFile(key.projectKey);
      const decision = clean(updater(files.has(file) ? { value: files.get(file) } : undefined));
      record('backend_update', { key, options, decision }); fail('backend_update');
      if (input.backend_absent_parent && updateAttempts === 1)
        return { ok: false, error: { code: 'Failed', telemetryCode: 'AbsentParent' } };
      if (input.backend_update_error) return { ok: false, error: input.backend_update_error };
      if (decision.write !== undefined) files.set(file, decision.write);
      return { ok: true, value: {} };
    },
    ensureScope: async scope => {
      record('backend_ensure_scope', { scope }); fail('backend_ensure_scope');
      return input.backend_ensure_error
        ? { ok: false, error: input.backend_ensure_error } : { ok: true, value: {} };
    },
  };
  return { context, files, calls, logs, errors, backend, record, fail,
    snapshot: () => ({ files: [...files].sort(([left], [right]) => left < right ? -1 : left > right ? 1 : 0)
      .map(([file, utf8]) => ({ path: file, utf8, hex: Buffer.from(utf8).toString('hex') })),
    calls, logs, errors }) };
}

const pathContext = replayContext().context;
const projectDir = value => pathContext.zS(value);
const aliasesPath = value => path.posix.join(projectDir(value), '.session-aliases');
const seed = (value, utf8) => ({ path: aliasesPath(value), utf8 });
const sourceA = projectDir(A);
const path_cases = [A, '/workspace/a-b', '/workspace/a_b', '/workspace/a b', '/workspace/a.b',
  '/工作区/项目😀', '/workspace/cafe\u0301', '/workspace/' + 'a'.repeat(189),
  '/workspace/' + 'a'.repeat(190), '/workspace/' + 'a'.repeat(201),
  'C:\\Users\\User\\Repo'].map(input => ({ input,
    expected: { slug: pathContext.J0(input), project_dir: projectDir(input), nfc: pathContext.Pn(input) } }));

const aliasWriteInputs = [
  ['one_way_A_adds_B', {}],
  ['duplicate_call_is_noop', { repeat: 2 }],
  ['same_source_is_noop', { added_dir: A }],
  ['canonical_same_source_is_noop', { added_dir: '/workspace/link-A', realpath_result: A }],
  ['symlink_target_bucket', { added_dir: '/workspace/link-B', realpath_result: B }],
  ['realpath_NFC_target_bucket', { added_dir: '/workspace/cafe\u0301', realpath_result: '/workspace/cafe\u0301' }],
  ['realpath_ENOENT_preserves_raw_NFD_target_bucket', { added_dir: '/workspace/cafe\u0301', failures: { realpath: 'ENOENT' } }],
  ['realpath_EACCES_preserves_raw_NFD_target_bucket', { added_dir: '/workspace/cafe\u0301', failures: { realpath: 'EACCES' } }],
  ['authoritative_source_parent_is_written_verbatim', { source_project_dir: '/outside/actual-session-parent/' }],
  ['source_parent_pins_current_bucket', { pinned_project_path: C }],
  ['existing_exact_line_is_noop', { initial_files: [seed(B, sourceA + '\n')] }],
  ['existing_exact_unterminated_line_is_noop', { initial_files: [seed(B, sourceA)] }],
  ['unterminated_other_line_is_not_repaired', { initial_files: [seed(B, '/foreign-project')] }],
  ['existing_CR_line_does_not_match_source', { initial_files: [seed(B, sourceA + '\r\n')] }],
  ['existing_whitespace_line_does_not_match_source', { initial_files: [seed(B, ' ' + sourceA + ' \n')] }],
  ['existing_other_lines_are_preserved', { initial_files: [seed(B, '\n/foreign-project\n\n')] }],
  ...['ENOENT', 'EACCES', 'EIO'].map(code => [`realpath_${code}_retains_added_path`, { failures: { realpath: code } }]),
  ...['EACCES', 'ENOTDIR', 'EIO'].map(code => [`read_${code}_stops_write`, { failures: { read: code } }]),
  ...['EPERM', 'ENOSPC'].map(code => [`mkdir_${code}_stops_write`, { failures: { mkdir: code } }]),
  ...['EPERM', 'EIO'].map(code => [`append_${code}_stops_write`, { failures: { append: code } }]),
  ['persistence_test_environment_is_noop', { persistence: { runtime_environment: 'test' } }],
  ['persistence_test_environment_explicitly_enabled', { persistence: { runtime_environment: 'test', test_enable: true } }],
  ['persistence_explicit_disable_is_noop', { persistence: { explicit_disable: true } }],
  ['persistence_skip_prompt_history_is_noop', { persistence: { skip_prompt_history: true } }],
  ['persistence_nested_marker_is_noop', { persistence: { nested_marker: true } }],
  ['diskless_without_backend_is_noop', { diskless: true }],
  ['backend_one_way_append', { storage_v5: true, diskless: true }],
  ['backend_duplicate_call_is_noop', { storage_v5: true, diskless: true, repeat: 2 }],
  ['backend_missing_scope_is_created_then_retried', { storage_v5: true, diskless: true, backend_absent_parent: true }],
  ['backend_existing_line_without_LF_is_noop', { storage_v5: true, initial_files: [seed(B, sourceA)] }],
  ['backend_unterminated_other_line_is_not_repaired', { storage_v5: true, initial_files: [seed(B, '/foreign-project')] }],
  ['backend_realpath_not_found_retains_added_path', { storage_v5: true, backend_realpath_found: false }],
  ['backend_realpath_error_retains_added_path', { storage_v5: true, backend_realpath_error: { code: 'Unavailable' } }],
  ['backend_update_error_is_reported', { storage_v5: true, backend_update_error: { code: 'Unavailable' } }],
  ['backend_update_throw_is_reported', { storage_v5: true, failures: { backend_update: 'EIO' } }],
  ['backend_ensure_scope_error_stops_write', { storage_v5: true, backend_absent_parent: true,
    backend_ensure_error: { code: 'Failed', errno: 'EACCES' } }],
  ['backend_invalid_key_diskless_is_noop', { storage_v5: true, diskless: true, invalid_storage_key: true }],
];
const alias_write_cases = [];
for (const [name, overrides] of aliasWriteInputs) {
  const input = { current_dir: A, added_dir: B, ...overrides };
  const replay = replayContext(input);
  for (let attempt = 0; attempt < (input.repeat ?? 1); attempt++)
    await replay.context.OPr(input.added_dir, input.storage_v5 ? replay.backend : undefined);
  alias_write_cases.push({ name, input, expected: replay.snapshot() });
}

const aliasReadInputs = [
  ['missing', {}], ['empty', { content: '' }], ['only_empty_lines', { content: '\n\n\n' }],
  ['direct_paths_preserve_first_seen_order', { content: `${projectDir(B)}\n${projectDir(C)}\n${projectDir(B)}\n` }],
  ['CR_and_whitespace_are_not_trimmed', { content: '\nalpha\nalpha\r\n \n\t\nalpha\n\r\n' }],
  ['unterminated_line_is_kept', { content: 'alpha\nbeta' }],
  ['unicode_and_NUL_are_not_normalized', { content: 'cafe\u0301\ncafé\n\u0000\n' }],
  ['disabled_persistence_does_not_disable_read', { content: 'alpha\n', persistence: { explicit_disable: true } }],
  ['diskless_without_backend_does_not_read', { content: 'alpha\n', diskless: true }],
  ...['ENOENT', 'EACCES', 'EIO'].map(code => [`read_${code}_returns_empty`, { failures: { read: code } }]),
  ['backend_missing', { storage_v5: true, diskless: true }],
  ['backend_exact_line_semantics', { storage_v5: true, diskless: true, content: '\nalpha\nalpha\r\n \nalpha\n' }],
  ['backend_error_returns_empty', { storage_v5: true, backend_read_error: { code: 'Unavailable' } }],
  ['backend_throw_returns_empty', { storage_v5: true, failures: { backend_read: 'EIO' } }],
];
const alias_read_cases = [];
for (const [name, options] of aliasReadInputs) {
  const { content, ...rest } = options;
  const input = { project_path: A, ...rest,
    ...(content !== undefined ? { initial_files: [seed(A, content)] } : {}) };
  const replay = replayContext(input);
  const aliases = await replay.context.W0e(input.project_path, input.storage_v5 ? replay.backend : undefined);
  alias_read_cases.push({ name, input, expected: { aliases: [...aliases], ...replay.snapshot() } });
}

const ids = Array.from({ length: 8 }, (_, index) => `00000000-0000-4000-8000-${String(index + 1).padStart(12, '0')}`);
const row = (index, modified, created = modified, tag) => ({ session_id: ids[index], modified_ms: modified,
  created_ms: created, ...(tag !== undefined ? { tag } : {}) });
function nativeRecord(value) {
  return { ...(value.session_id !== undefined ? { sessionId: value.session_id } : {}),
    modified: new Date(value.modified_ms), created: new Date(value.created_ms),
    ...(value.tag !== undefined ? { tag: value.tag } : {}),
    ...(value.full_path !== undefined ? { fullPath: value.full_path } : {}) };
}
function projectRecord(value) {
  return { session_id: value.sessionId ?? null, modified_ms: value.modified.getTime(),
    created_ms: value.created.getTime(), full_path: value.fullPath ?? null,
    project_path: value.projectPath ?? null, is_alias: value.isAlias === true,
    value: value.value, own_worktrees: value.ownWorktrees ?? null,
    ...(value.tag !== undefined ? { tag: value.tag } : {}) };
}
const dedupInputs = [
  ['newest_UUID_copy_wins', [row(0, 10, 1, 'old'), row(1, 20, 2), row(0, 30, 3, 'new')]],
  ['physical_mtime_wins_independently_of_supplied_logical_timestamp', [
    { ...row(0, 100, 1, 'newer-file-older-logical'), logical_ms: 10 },
    { ...row(0, 90, 2, 'older-file-newer-logical'), logical_ms: 80 },
  ]],
  ['equal_modified_preserves_first_copy', [row(0, 20, 1, 'first'), row(0, 20, 99, 'second')]],
  ['created_orders_different_UUIDs_after_modified_tie', [row(0, 20, 1), row(1, 20, 3), row(2, 20, 2)]],
  ['equal_modified_and_created_preserves_insertion_order', [row(2, 20, 1), row(0, 20, 1), row(1, 20, 1)]],
  ['missing_or_empty_identity_is_dropped', [{ modified_ms: 100, created_ms: 100 },
    { session_id: '', modified_ms: 200, created_ms: 200 },
    { session_id: null, modified_ms: 300, created_ms: 300 }, row(0, 10)]],
  ['UUID_case_is_not_normalized', [{ session_id: 'abcdefab-0000-4000-8000-000000000001', modified_ms: 10, created_ms: 10 },
    { session_id: 'ABCDEFAB-0000-4000-8000-000000000001', modified_ms: 20, created_ms: 20 }]],
];
vm.runInContext(src.dedup, pathContext, { timeout: 1000 });
const dedup_cases = dedupInputs.map(([name, input]) => ({ name, input,
  expected: pathContext.G9(input.map(nativeRecord)).map(projectRecord) }));

function installNativeKbn(replay, input) {
  const { context, record } = replay;
  const probes = new Map([...(input.transcripts ?? []), ...(input.probe_files ?? [])].map(file => [file.path, file]));
  const find = (file, operation) => {
    const value = probes.get(file);
    if (!value) throw controlledError('ENOENT', operation);
    if (value[`${operation}_error`]) throw controlledError(value[`${operation}_error`], operation);
    return value;
  };
  context.KBN_FS = {
    readdir: async directory => {
      record('kbn_readdir', { path: directory });
      if (input.kbn_readdir_error) throw controlledError(input.kbn_readdir_error, 'kbn readdir');
      return [...probes.values()].filter(file => path.posix.dirname(file.path) === directory)
        .map(file => ({ name: path.posix.basename(file.path), isFile: () => file.kind === undefined || file.kind === 'file' }));
    },
    lstat: async file => ({ isFile: () => find(file, 'lstat').kind !== 'directory' }),
    open: async (file, flags) => {
      record('kbn_open', { path: file, flags });
      const value = find(file, 'open'), bytes = Buffer.from(value.utf8 ?? '');
      return {
        stat: async () => {
          record('kbn_stat', { path: file }); find(file, 'stat');
          return { isFile: () => value.kind === undefined || value.kind === 'file',
            mtime: new Date(value.modified_ms ?? 1), size: bytes.length };
        },
        read: async (buffer, offset, length, position) => {
          record('kbn_read', { path: file, offset, length, position }); find(file, 'read');
          const count = bytes.copy(buffer, offset, position, Math.min(bytes.length, position + length));
          return { bytesRead: count };
        },
        close: async () => record('kbn_close', { path: file }),
      };
    },
  };
  context.KBN_CONSTANTS = fsConstants;
  context.Kbn = vm.runInContext(`(()=>{
    const b=KBN_FS.readdir,L=KBN_FS.open,J=KBN_FS.lstat,P=KBN_CONSTANTS;
    ${src.kbnFlags}${src.kbnBuffer}${src.kbnTail}${src.kbnHead}${src.kbnRead}${src.kbnIdentity}${src.kbn}
    return Kbn;
  })()`, context, { timeout: 1000 });
}

function discoveryContext(input) {
  const replay = replayContext(input);
  const { context, record, fail } = replay;
  Object.assign(context, {
    $b: 256,
    AW: backend => backend,
    ak: backend => backend,
    W8o: async current => {
      record('resolve_current_project', { path: current });
      return input.resolved_project_dir;
    },
    RDt: async project => {
      record('list_sessions', { project_dir: project }); fail('list_sessions');
      return new Map((input.project_rows?.[project] ?? []).map(value => [value.session_id,
        { mtime: value.modified_ms, ctime: value.created_ms, size: value.file_size ?? 10,
          path: value.full_path ?? path.posix.join(project, `${value.session_id}.jsonl`) }]));
    },
    NAt: async value => {
      record('controlled_foreign_header', { session_id: value.sessionId });
      return (input.foreign_session_ids ?? []).includes(value.sessionId);
    },
    O0e: async (cwd, project, maximum, descendants) => {
      record('controlled_collision_recovery', { path: cwd, project_dir: project,
        max_count: maximum ?? null, descendants });
      return (input.recovery_rows?.[cwd] ?? []).map(nativeRecord);
    },
    FAt: async () => { record('controlled_collision_inventory'); return []; },
    XA: async (directory, options) => {
      record('list_project_directories', { path: directory, ...options }); fail('list_project_directories');
      return (input.project_directories ?? []).map(value => ({ name: value.name,
        isDirectory: () => value.is_directory }));
    },
    Gce: async () => {
      record('controlled_backend_project_directories');
      return input.storage_project_directories;
    },
    nco: async values => record('observe_fork_statistics', { session_ids: values.map(value => value.sessionId) }),
  });
  installNativeKbn(replay, input);
  vm.runInContext([src.repositoryCompare, src.collisionBatch, src.projectList, src.dedup,
    src.current, src.repository].join('\n'), context, { timeout: 1000 });
  const nativeList = context.KBe;
  context.KBe = (project, maximum, projectPath, backend, ownWorktrees) => {
    record('load_project', { project_dir: project, max_count: maximum ?? null,
      project_path: projectPath ?? null, own_worktrees: ownWorktrees ?? null });
    return nativeList(project, maximum, projectPath, backend, ownWorktrees);
  };
  return replay;
}

const W = '/workspace/A-worktree';
const currentSubdir = A + '/src';
const orphanDirName = pathContext.J0(A) + '--claude-worktrees-orphan';
const orphanDir = path.posix.join(configDir, 'projects', orphanDirName);
const longRoot = '/workspace/' + 'a'.repeat(210);
const longLegacyName = pathContext.J0(longRoot).slice(0, 200) + '-old';
const longLegacyDir = path.posix.join(configDir, 'projects', longLegacyName);
const dirs = (...names) => names.map(name => ({ name, is_directory: true }));
const projectRows = (project, ...values) => ({ [projectDir(project)]: values });
const discoveryInputs = [
  ['current_unlimited_order', { selector: 'current', project_rows: projectRows(A, row(0, 10), row(1, 30), row(2, 30, 31)) }],
  ['current_direct_alias_newest_copy_wins', { selector: 'current', initial_files: [seed(A, projectDir(B) + '\n')],
    project_rows: { ...projectRows(A, row(0, 10), row(1, 20)), ...projectRows(B, row(0, 30), row(2, 40)) } }],
  ['current_alias_older_copy_loses', { selector: 'current', initial_files: [seed(A, projectDir(B) + '\n')],
    project_rows: { ...projectRows(A, row(0, 30)), ...projectRows(B, row(0, 10), row(1, 20)) } }],
  ['current_alias_physical_mtime_precedes_ignored_logical_timestamp', { selector: 'current',
    initial_files: [seed(A, projectDir(B) + '\n')],
    project_rows: {
      ...projectRows(A, { ...row(0, 100, 1, 'newer-file-older-logical'), logical_ms: 10 }),
      ...projectRows(B, { ...row(0, 90, 2, 'older-file-newer-logical'), logical_ms: 80 }),
    } }],
  ['current_alias_equal_modified_copy_loses_even_with_newer_created', { selector: 'current',
    initial_files: [seed(A, projectDir(B) + '\n')],
    project_rows: { ...projectRows(A, row(0, 20, 1)), ...projectRows(B, row(0, 20, 99)) } }],
  ['current_limit_applies_per_bucket_and_after_alias_merge', { selector: 'current', max_count: 2,
    initial_files: [seed(A, projectDir(B) + '\n')], project_rows: { ...projectRows(A, row(0, 10), row(1, 50), row(2, 30)),
      ...projectRows(B, row(3, 20), row(4, 60), row(5, 40)) } }],
  ['current_zero_limit_without_alias_does_not_cap_listing', { selector: 'current', max_count: 0,
    project_rows: projectRows(A, row(0, 10), row(1, 20)) }],
  ['current_zero_limit_with_alias_caps_merged_listing_to_empty', { selector: 'current', max_count: 0,
    initial_files: [seed(A, projectDir(B) + '\n')], project_rows: { ...projectRows(A, row(0, 10)), ...projectRows(B, row(1, 20)) } }],
  ['current_alias_expansion_is_direct_only', { selector: 'current',
    initial_files: [seed(A, projectDir(B) + '\n'), seed(B, projectDir(C) + '\n')],
    project_rows: { ...projectRows(A, row(0, 10)), ...projectRows(B, row(1, 20)), ...projectRows(C, row(2, 30)) } }],
  ['current_alias_bytes_are_passed_as_storage_path_without_trimming', { selector: 'current',
    initial_files: [seed(A, projectDir(B) + '\r\n')],
    project_rows: { ...projectRows(A, row(0, 10)), [projectDir(B) + '\r']: [row(1, 20)] } }],
  ['current_resolved_storage_bucket_is_used', { selector: 'current', resolved_project_dir: projectDir(C),
    project_rows: { ...projectRows(A, row(0, 10)), ...projectRows(C, row(1, 20)) } }],
  ['current_foreign_full_batch_is_reloaded_and_filtered_with_controlled_headers', { selector: 'current', max_count: 1,
    foreign_session_ids: [ids[0]], project_rows: projectRows(A, row(0, 30), row(1, 20), row(2, 10)) }],
  ['current_ambiguous_slug_uses_controlled_collision_recovery', { selector: 'current', current_dir: '/workspace/A+B',
    foreign_session_ids: [ids[0]], project_rows: projectRows('/workspace/A+B', row(0, 30)),
    recovery_rows: { '/workspace/A+B': [{ ...row(1, 20), full_path: '/collision/recovered.jsonl' }] } }],
  ['current_diskless_without_backend_returns_empty', { selector: 'current', diskless: true,
    project_rows: projectRows(A, row(0, 10)) }],
  ['current_diskless_backend_reads_aliases', { selector: 'current', diskless: true, storage_v5: true,
    initial_files: [seed(A, projectDir(B) + '\n')],
    project_rows: { ...projectRows(A, row(0, 10)), ...projectRows(B, row(1, 20)) } }],
  ['repository_single_root_includes_matching_orphan_prefix', { selector: 'repository', repository_paths: [A],
    project_directories: [...dirs(orphanDirName, pathContext.J0(A) + '-claude-worktrees-wrong'), { name: orphanDirName + '-file', is_directory: false }],
    project_rows: { ...projectRows(A, row(0, 10)), [orphanDir]: [row(1, 20)] } }],
  ['repository_single_subdir_reads_current_and_root_aliases_but_not_root_bucket', { selector: 'repository',
    current_dir: currentSubdir, repository_paths: [A],
    initial_files: [seed(currentSubdir, projectDir(B) + '\n'), seed(A, projectDir(C) + '\n' + projectDir(B) + '\n')],
    project_rows: { ...projectRows(currentSubdir, row(0, 10)), ...projectRows(A, row(1, 50)),
      ...projectRows(B, row(2, 20)), ...projectRows(C, row(3, 30)) } }],
  ['repository_single_root_limit_does_not_globally_cap_current_plus_orphans', { selector: 'repository', max_count: 1,
    repository_paths: [A], initial_files: [seed(A, projectDir(B) + '\n')], project_directories: dirs(orphanDirName),
    project_rows: { ...projectRows(A, row(0, 10), row(1, 20)), ...projectRows(B, row(2, 30), row(3, 5)),
      [orphanDir]: [row(4, 40), row(5, 4)] } }],
  ['repository_single_root_equal_modified_current_copy_precedes_orphan_and_alias', { selector: 'repository', repository_paths: [A],
    initial_files: [seed(A, projectDir(B) + '\n')], project_directories: dirs(orphanDirName),
    project_rows: { ...projectRows(A, row(0, 20, 1)), ...projectRows(B, row(0, 20, 99)), [orphanDir]: [row(0, 20, 100)] } }],
  ['repository_multiple_worktrees_use_enumeration_order_for_equal_modified_copy', { selector: 'repository', repository_paths: [A, W],
    project_directories: dirs(pathContext.J0(W), pathContext.J0(A)),
    project_rows: { ...projectRows(A, row(0, 20, 99)), ...projectRows(W, row(0, 20, 1), row(1, 10)) } }],
  ['repository_multiple_worktrees_only_expand_current_and_containing_root_aliases', { selector: 'repository',
    current_dir: currentSubdir, repository_paths: [A, W],
    initial_files: [seed(currentSubdir, projectDir(B) + '\n'), seed(A, projectDir(C) + '\n'), seed(W, '/unread-other-worktree-alias\n')],
    project_directories: dirs(pathContext.J0(A), pathContext.J0(W)),
    project_rows: { ...projectRows(A, row(0, 10)), ...projectRows(W, row(1, 20)), ...projectRows(B, row(2, 30)),
      ...projectRows(C, row(3, 40)), '/unread-other-worktree-alias': [row(4, 100)] } }],
  ['repository_multiple_worktrees_without_containing_root_prepend_current_bucket', { selector: 'repository', current_dir: C,
    repository_paths: [A, W], project_directories: dirs(pathContext.J0(W), pathContext.J0(A)),
    initial_files: [seed(C, projectDir(B) + '\n'), seed(A, '/unread-root-alias\n')],
    project_rows: { ...projectRows(C, row(0, 20, 1)), ...projectRows(A, row(1, 10)), ...projectRows(W, row(0, 20, 99)),
      ...projectRows(B, row(2, 30)) } }],
  ['repository_multiple_worktrees_use_longest_slug_match', { selector: 'repository', repository_paths: [A, W],
    project_directories: dirs(pathContext.J0(W) + '-child', pathContext.J0(A) + '-child', 'unrelated'),
    project_rows: { [projectDir(W) + '-child']: [row(0, 20)], [projectDir(A) + '-child']: [row(1, 10)],
      [path.posix.join(configDir, 'projects', 'unrelated')]: [row(2, 100)] } }],
  ['repository_multiple_worktrees_limit_does_not_globally_cap_union', { selector: 'repository', max_count: 1,
    repository_paths: [A, W], project_directories: dirs(pathContext.J0(A), pathContext.J0(W)),
    project_rows: { ...projectRows(A, row(0, 10), row(1, 20)), ...projectRows(W, row(2, 30), row(3, 40)) } }],
  ['repository_directory_read_error_falls_back_to_limited_current_and_aliases', { selector: 'repository', max_count: 1,
    repository_paths: [A, W], failures: { list_project_directories: 'EACCES' },
    initial_files: [seed(A, projectDir(B) + '\n')],
    project_rows: { ...projectRows(A, row(0, 10), row(1, 20)), ...projectRows(B, row(2, 30), row(3, 40)) } }],
  ['repository_diskless_backend_enumerates_supplied_scope', { selector: 'repository', diskless: true, storage_v5: true,
    repository_paths: [A, W], storage_project_directories: [pathContext.J0(W), pathContext.J0(A)],
    project_rows: { ...projectRows(A, row(0, 10)), ...projectRows(W, row(1, 20)) } }],
  ['repository_diskless_without_backend_falls_back_to_current', { selector: 'repository', diskless: true,
    repository_paths: [A, W], project_rows: projectRows(A, row(0, 10)) }],
  ['repository_long_root_matches_transcript_cwd_in_truncated_bucket', { selector: 'repository', current_dir: longRoot,
    repository_paths: [longRoot, W], project_directories: dirs(longLegacyName, pathContext.J0(W)),
    probe_files: [{ path: longLegacyDir + '/probe.jsonl', utf8: JSON.stringify({ cwd: longRoot }) + '\n' }],
    project_rows: { [longLegacyDir]: [row(0, 30)], ...projectRows(W, row(1, 20)) } }],
  ['repository_long_root_rejects_different_transcript_cwd_in_truncated_bucket', { selector: 'repository', current_dir: longRoot,
    repository_paths: [longRoot, W], project_directories: dirs(longLegacyName, pathContext.J0(W)),
    probe_files: [{ path: longLegacyDir + '/probe.jsonl', utf8: JSON.stringify({ cwd: longRoot + 'x' }) + '\n' }],
    project_rows: { [longLegacyDir]: [row(0, 30)], ...projectRows(W, row(1, 20)) } }],
];
const discovery_cases = [];
for (const [name, overrides] of discoveryInputs) {
  const input = { current_dir: A, ...overrides };
  const replay = discoveryContext(input);
  const backend = input.storage_v5 ? replay.backend : undefined;
  const values = input.selector === 'current'
    ? await replay.context.CCo(input.max_count, backend)
    : await replay.context.LAt(input.repository_paths ?? [], input.max_count, backend);
  discovery_cases.push({ name, input, expected: { sessions: values.map(projectRecord), ...replay.snapshot() } });
}

const resumeEntrypointInputs = [
  ['flag_current_precedes_newer_repository_but_slash_uses_catalog', {
    repository_paths: [A, W], project_directories: dirs(pathContext.J0(A), pathContext.J0(W)),
    project_rows: { ...projectRows(A, row(0, 10)), ...projectRows(W, row(0, 30)) },
    local_read_path: projectDir(A) + '/' + ids[0] + '.jsonl',
    repository_read_path: projectDir(W) + '/' + ids[0] + '.jsonl',
  }],
  ['flag_controlled_global_failure_but_slash_selects_direct_alias', {
    repository_paths: [A], initial_files: [seed(A, projectDir(B) + '\n')],
    project_rows: projectRows(B, row(0, 30)),
    global_read_paths: [projectDir(B) + '/' + ids[0] + '.jsonl', projectDir(C) + '/' + ids[0] + '.jsonl'],
  }],
  ['flag_repository_read_precedes_global', {
    repository_paths: [A, W], project_directories: dirs(pathContext.J0(A), pathContext.J0(W)),
    project_rows: projectRows(W, row(0, 30)),
    repository_read_path: projectDir(W) + '/' + ids[0] + '.jsonl',
    global_read_paths: [projectDir(C) + '/' + ids[0] + '.jsonl'],
  }],
  ['flag_global_unique_but_slash_empty_catalog_returns_no_conversations', {
    repository_paths: [A], global_read_paths: [projectDir(C) + '/' + ids[0] + '.jsonl'],
  }],
  ['slash_local_fallback_after_nonempty_catalog_UUID_miss', {
    repository_paths: [A], project_rows: projectRows(A, row(1, 20)),
    local_read_path: projectDir(A) + '/' + ids[0] + '.jsonl',
  }],
  ['flag_explicit_path_skips_repository_and_global', {
    repository_paths: [A], initial_files: [seed(A, projectDir(B) + '\n')],
    project_rows: projectRows(B, row(0, 30)), flag_explicit_path: '/explicit/transcript.jsonl',
    local_read_path: '/explicit/transcript.jsonl',
  }],
  ['controlled_local_full_reader_success_with_spaced_input_precedes_global', {
    repository_paths: [A], project_rows: projectRows(A, row(0, 20)),
    local_read_path: projectDir(A) + '/' + ids[0] + '.jsonl',
    local_reader_input_utf8: '{"type" : "user","sessionId":"' + ids[0] + '","message":{"role":"user","content":"hello"}}\n',
    controlled_local_reader_messages: [{ type: 'user', sessionId: ids[0], message: { role: 'user', content: 'hello' } }],
    global_read_paths: [projectDir(C) + '/' + ids[0] + '.jsonl'],
  }],
  ['repository_loop_respects_supplied_containing_worktree_first', {
    current_dir: W + '/deep/sub', repository_paths: [W, A],
    project_directories: dirs(pathContext.J0(W), pathContext.J0(A)),
    project_rows: { ...projectRows(W, row(0, 30)), ...projectRows(A, row(0, 40)) },
    repository_read_paths: [projectDir(W) + '/' + ids[0] + '.jsonl', projectDir(A) + '/' + ids[0] + '.jsonl'],
  }],
  ['controlled_local_reader_null_then_repository_full_log', {
    repository_paths: [A, W], project_directories: dirs(pathContext.J0(A), pathContext.J0(W)),
    project_rows: projectRows(W, row(0, 30)), local_reader_input_utf8: 'not valid JSON\n',
    repository_read_path: projectDir(W) + '/' + ids[0] + '.jsonl',
  }],
  ['local_reader_exception_aborts_before_repository_fallback', {
    repository_paths: [A, W], project_directories: dirs(pathContext.J0(A), pathContext.J0(W)),
    project_rows: projectRows(W, row(0, 30)), local_read_error: 'EIO',
    repository_read_path: projectDir(W) + '/' + ids[0] + '.jsonl',
  }],
];
const resume_entrypoint_cases = [];
for (const [name, options] of resumeEntrypointInputs) {
  const input = { current_dir: A, session_id: ids[0], ...options };
  const replay = discoveryContext(input), dispatchCalls = [], replies = [];
  let phase = 'flag', flagSelection = null, slashSelection = null, flagError = null;
  const log = fullPath => fullPath ? { sessionId: input.session_id, fullPath, messages: [] } : null;
  const selected = new Error('controlled selection boundary');
  Object.assign(replay.context, {
    gOe: async (id, file) => {
      dispatchCalls.push({ entrypoint: phase, dependency: 'controlled_full_log_read', id, file: file ?? null });
      if (file === undefined || file === input.flag_explicit_path) {
        if (input.local_read_error) throw controlledError(input.local_read_error, 'local full-log read');
        const value = log(input.local_read_path);
        if (value && input.controlled_local_reader_messages) value.messages = input.controlled_local_reader_messages;
        return value;
      }
      if (file === input.repository_read_path || input.repository_read_paths?.includes(file)
        || input.global_read_paths?.includes(file)) return log(file);
      return null;
    },
    rRe: async cwd => {
      dispatchCalls.push({ entrypoint: phase, dependency: 'controlled_repository_bucket_candidates', cwd });
      return [projectDir(cwd)];
    },
    T7o: path.posix.join,
    f3r: async id => {
      dispatchCalls.push({ entrypoint: phase, dependency: 'controlled_global_candidate', id });
      return input.global_read_paths?.length === 1 ? input.global_read_paths[0] : null;
    },
    RESUME_TELEMETRY: () => {},
    Xae: () => false, wm: value => value.sessionId, Vt: value => value === input.session_id ? value : null,
    p: () => {}, m: () => {}, g: () => {}, d: error => error.message,
    gW: class ControlledRefusal extends Error {}, Kee: () => true,
    Rt: () => false, Gfe: async () => input.repository_paths,
    vrn: async repositoryPaths => {
      dispatchCalls.push({ entrypoint: phase, dependency: 'controlled_vrn_dispatch_to_native_LAt' });
      return replay.context.LAt(repositoryPaths);
    },
    UZ: async () => null, aYe: () => false,
    e: (_, props) => props,
    K: result => result.resultType + ': ' + result.arg,
  });
  for (const [key, nativeName] of [['flagRepositoryResume', 'iQo'], ['flagGlobalResume', 'fQo']])
    replay.context[nativeName] = vm.runInContext(`(()=>{const i=RESUME_TELEMETRY;${src[key]};return ${nativeName}})()`,
      replay.context, { timeout: 1000 });
  vm.runInContext(src.flagResume, replay.context, { timeout: 1000 });
  try {
    await replay.context.FIe(input.session_id, input.flag_explicit_path, {
      beforeUse: (value, id) => { flagSelection = { session_id: id, path: value.fullPath }; throw selected; },
    });
  } catch (error) {
    if (error !== selected) {
      assert.equal(error.code, input.local_read_error, 'Flag selector reached an unexpected failure before beforeUse');
      flagError = { code: error.code, message: error.message };
    }
  }
  phase = 'slash';
  const slash = vm.runInContext(`(()=>{const B='validation';${src.slashResume};return De})()`, replay.context, { timeout: 1000 });
  const rendered = await slash((message, metadata) => replies.push({ message: message ?? null, ...metadata }),
    { resume: async (id, value, source) => { slashSelection = { session_id: id, path: value.fullPath, source }; } }, input.session_id);
  if (rendered?.onDone) rendered.onDone();
  // De launches its local F callback without awaiting it; flush the two awaits
  // supplied by the controlled UZ and context.resume dependencies.
  await Promise.resolve();
  await Promise.resolve();
  resume_entrypoint_cases.push({ name, input, expected: { flag_selection: flagSelection,
    ...(flagError ? { flag_error: flagError } : {}),
    flag_stop_boundary: flagSelection ? 'beforeUse; full transcript processing not executed' : null,
    slash_selection: slashSelection, slash_message: rendered?.message ?? null,
    replies, dispatch_calls: dispatchCalls, ...replay.snapshot() } });
}

function uuidContext(input) {
  assert(!input.storage_v5, 'UUID backend transport is outside this replay');
  const replay = replayContext(input);
  const { context, record } = replay;
  const transcripts = new Map((input.transcripts ?? []).map(file => [file.path, file]));
  const checkFile = (file, operation) => {
    const value = transcripts.get(file);
    const code = value?.[`${operation}_error`];
    if (code) throw controlledError(code, operation);
    if (!value) throw controlledError('ENOENT', operation);
    return value;
  };
  Object.assign(context, {
    E: path.posix.basename, F: path.posix.dirname,
    z: async value => {
      record('resume_realpath', { path: value });
      if (input.realpath_error) throw controlledError(input.realpath_error, 'resume realpath');
      return input.realpath_result ?? value;
    },
    b: async (directory, options) => {
      record('resume_readdir', { path: directory, with_file_types: options?.withFileTypes === true });
      if (directory === context.Bf()) {
        if (input.project_directory_error) throw controlledError(input.project_directory_error, 'resume readdir');
        return options?.withFileTypes
          ? (input.project_directories ?? []).map(value => ({ name: value.name, isDirectory: () => value.is_directory }))
          : (input.project_directories ?? []).map(value => value.name);
      }
      if ([...transcripts.keys()].some(file => path.posix.dirname(file) === directory)
        || (input.existing_project_directories ?? []).includes(directory)) return [];
      throw controlledError('ENOENT', 'resume readdir');
    },
    R: async file => {
      record('resume_lstat', { path: file });
      const value = checkFile(file, 'lstat');
      return { isFile: () => value.kind !== 'directory' };
    },
    L: async (file, flags) => {
      record('resume_open', { path: file, flags });
      const value = checkFile(file, 'open');
      return {
        createReadStream: () => value.stream_error
          ? Readable.from((async function* () { throw controlledError(value.stream_error, 'resume stream'); })())
          : Readable.from([Buffer.from(value.utf8 ?? '')]),
        close: async () => record('resume_close', { path: file }),
      };
    },
    pe: createInterface,
    v6t: async current => {
      record('controlled_git_worktrees', { path: current });
      return input.repository_paths ?? [];
    },
  });
  installNativeKbn(replay, input);
  vm.runInContext([src.uuidSyntax, src.messagePresence, src.uuidMissing, src.uuidResolution,
    src.uuidPresenceProbe, src.uuidGlobal, src.uuidRealpath, src.uuidCwdCandidates,
    src.slugFallback, src.uuidProjectPath, src.uuidProjectCandidates, src.uuidWorktreeCandidates,
    src.uuidRealpathAdapter].join('\n'), context, { timeout: 1000 });
  return replay;
}

const transcriptPath = (project, id = ids[0]) => path.posix.join(projectDir(project), `${id}.jsonl`);
const userLine = '{"type":"user","message":{"content":"hello"}}\n';
const assistantLine = '{"type":"assistant","message":{"content":[]}}\n';
const metadataLine = '{"type":"summary","summary":"metadata only"}\n';
const transcript = (project, utf8 = userLine, options = {}) => ({ path: transcriptPath(project), utf8, ...options });
function uuidInput(overrides) {
  const input = { session_id: ids[0], current_dir: A, ...overrides };
  if (!input.project_directories) input.project_directories = dirs(...new Set((input.transcripts ?? [])
    .map(file => path.posix.dirname(file.path))
    .filter(directory => path.posix.dirname(directory) === pathContext.yf())
    .map(directory => path.posix.basename(directory))));
  return input;
}

const presenceInputs = [
  ['missing_transcript', {}],
  ['empty_transcript', { utf8: '' }],
  ['metadata_only_transcript', { utf8: metadataLine }],
  ['compact_user_type_substring', { utf8: userLine }],
  ['compact_assistant_type_substring', { utf8: assistantLine }],
  ['spaced_type_key_does_not_match', { utf8: '{"type": "user"}\n' }],
  ['malformed_JSON_with_type_substring_matches', { utf8: 'not-json "type":"user" ignored\n' }],
  ['matching_nested_type_substring_matches', { utf8: '{"nested":{"type":"assistant"}}\n' }],
  ['unterminated_matching_line_matches', { utf8: '{"type":"user"}' }],
  ['CRLF_matching_line_matches', { utf8: '{"type":"user"}\r\n' }],
  ['directory_does_not_match', { utf8: userLine, kind: 'directory' }],
  ['lstat_ENOENT_is_none', { utf8: userLine, lstat_error: 'ENOENT' }],
  ['lstat_EACCES_is_unknown', { utf8: userLine, lstat_error: 'EACCES' }],
  ['open_ENOENT_is_none', { utf8: userLine, open_error: 'ENOENT' }],
  ['open_EACCES_is_unknown', { utf8: userLine, open_error: 'EACCES' }],
  ['stream_EIO_is_unknown', { utf8: userLine, stream_error: 'EIO' }],
];
const message_presence_cases = [];
for (const [name, options] of presenceInputs) {
  const file = transcriptPath(A);
  const input = { path: file, transcripts: options.utf8 === undefined ? [] : [{ path: file, ...options }] };
  const replay = uuidContext(input);
  const classification = await replay.context.M(file);
  const second = uuidContext(input);
  const hasMessages = await second.context.H(file);
  message_presence_cases.push({ name, input, expected: { classification, has_messages: hasMessages, calls: replay.calls } });
}

const uuidInputs = [
  ['current_precedes_worktree_and_global_copies', { repository_paths: [A, W],
    transcripts: [transcript(A), transcript(W), transcript(B)] }],
  ['worktree_precedes_unique_global', { repository_paths: [A, W],
    transcripts: [transcript(A, metadataLine), transcript(W), transcript(B)] }],
  ['worktree_first_matching_order_is_preserved', { repository_paths: [A, W, C],
    transcripts: [transcript(W), transcript(C)] }],
  ['unique_global_is_used_after_scoped_candidates', { transcripts: [transcript(B)] }],
  ['duplicate_valid_global_copies_are_rejected', { transcripts: [transcript(B), transcript(C)] }],
  ['global_metadata_only_copy_does_not_make_duplicate', { transcripts: [transcript(B), transcript(C, metadataLine)] }],
  ['global_unknown_copy_is_ignored', { transcripts: [transcript(B), transcript(C, userLine, { lstat_error: 'EACCES' })] }],
  ['global_directory_is_ignored', { transcripts: [transcript(B), transcript(C, userLine, { kind: 'directory' })] }],
  ['global_no_usable_copy_returns_scoped_candidate_without_messages', { transcripts: [transcript(B, metadataLine)] }],
  ['global_root_read_failure_returns_scoped_candidate_without_messages', { transcripts: [transcript(B)], project_directory_error: 'EACCES' }],
  ['current_unknown_presence_is_accepted', { transcripts: [transcript(A, userLine, { lstat_error: 'EACCES' }), transcript(B)] }],
  ['worktree_unknown_presence_is_accepted', { repository_paths: [A, W],
    transcripts: [transcript(W, userLine, { open_error: 'EACCES' }), transcript(B)] }],
  ['cross_worktree_false_disables_worktree_and_global', { cross_worktree: false, repository_paths: [A, W],
    transcripts: [transcript(W), transcript(B)] }],
  ['cross_worktree_false_still_allows_current', { cross_worktree: false, transcripts: [transcript(A), transcript(B)] }],
  ['link_scan_exact_path_precedes_current', { link_scan_path: '/linked/' + ids[0] + '.jsonl',
    transcripts: [transcript(A), { path: '/linked/' + ids[0] + '.jsonl', utf8: userLine }] }],
  ['link_scan_other_filename_uses_its_parent', { link_scan_path: '/linked/other.jsonl',
    transcripts: [transcript(A), { path: '/linked/' + ids[0] + '.jsonl', utf8: userLine }] }],
  ['unusable_link_scan_path_falls_through_to_current', { link_scan_path: '/linked/' + ids[0] + '.jsonl',
    transcripts: [transcript(A), { path: '/linked/' + ids[0] + '.jsonl', utf8: metadataLine }] }],
  ['invalid_UUID_returns_invalid_resume_path', { session_id: 'not-a-uuid', transcripts: [transcript(A)] }],
  ['uppercase_UUID_is_valid_but_filename_lookup_preserves_case', { session_id: 'ABCDEFAB-0000-4000-8000-000000000001',
    transcripts: [{ path: transcriptPath(A, 'abcdefab-0000-4000-8000-000000000001'), utf8: userLine }] }],
  ['spaced_message_type_is_not_usable', { transcripts: [transcript(A, '{"type": "user"}\n')] }],
  ['malformed_JSON_type_substring_is_usable', { transcripts: [transcript(A, 'bad "type":"user"\n')] }],
  ['realpath_error_keeps_original_cwd', { realpath_error: 'EACCES', transcripts: [transcript(A)] }],
  ['canonical_cwd_bucket_is_used', { current_dir: '/workspace/link-A', realpath_result: A, transcripts: [transcript(A)] }],
  ['empty_transcript_is_not_usable', { transcripts: [transcript(A, '')] }],
  ['no_transcript_uses_computed_candidate', {}],
];
const uuid_resolution_cases = [];
for (const [name, options] of uuidInputs) {
  const input = uuidInput(options);
  const replay = uuidContext(input);
  const resolution = await replay.context.Iue(input.session_id, input.current_dir, input.link_scan_path,
    { crossWorktree: input.cross_worktree !== false });
  uuid_resolution_cases.push({ name, input, expected: { resolution: clean(resolution), calls: replay.calls } });
}

const cwdLine = value => JSON.stringify({ cwd: value }) + '\n';
const relocatedLine = value => JSON.stringify({ type: 'relocated', relocatedCwd: value, sessionId: 's' }) + '\n';
const kbnInputs = [
  ['head_cwd_matches', { utf8: cwdLine(longRoot) }],
  ['different_head_cwd_does_not_match', { utf8: cwdLine(longRoot + 'x') }],
  ['relocated_tail_overrides_head_cwd', { utf8: cwdLine(longRoot + 'x') + relocatedLine(longRoot) }],
  ['relocated_tail_can_reject_matching_head', { utf8: cwdLine(longRoot) + relocatedLine(longRoot + 'x') }],
  ['later_relocated_tail_wins', { utf8: cwdLine(longRoot) + relocatedLine(longRoot + 'x') + relocatedLine(longRoot) }],
  ['null_relocation_falls_back_to_head', { utf8: cwdLine(longRoot) + relocatedLine(null) }],
  ['bad_head_JSON_does_not_hide_later_valid_line', { utf8: 'bad "cwd":true\n' + cwdLine(longRoot) }],
  ['space_before_colon_is_not_a_head_candidate', { utf8: '{"cwd" : ' + JSON.stringify(longRoot) + '}\n' }],
  ['non_UUID_filename_is_scanned', { name: 'not-a-uuid.jsonl', utf8: cwdLine(longRoot) }],
  ['non_jsonl_file_is_ignored', { name: 'probe.txt', utf8: cwdLine(longRoot) }],
  ['directory_entry_is_ignored', { kind: 'directory', utf8: cwdLine(longRoot) }],
  ['symlink_entry_is_ignored', { kind: 'symlink', utf8: cwdLine(longRoot) }],
  ['empty_file_is_ignored', { utf8: '' }],
  ['open_failure_is_ignored', { utf8: cwdLine(longRoot), open_error: 'EACCES' }],
  ['read_failure_is_ignored', { utf8: cwdLine(longRoot), read_error: 'EIO' }],
  ['head_cwd_and_far_tail_relocation_are_read', { utf8: cwdLine(longRoot + 'x') + ' '.repeat(70000) + '\n' + relocatedLine(longRoot) }],
  ['cwd_only_in_middle_outside_head_and_tail_is_ignored', { utf8: ' '.repeat(65536) + '\n' + cwdLine(longRoot) + ' '.repeat(65536) }],
];
const kbn_cases = [];
for (const [name, options] of kbnInputs) {
  const { name: fileName = 'probe.jsonl', ...fileOptions } = options;
  const input = { project_dir: longLegacyDir, target_root: longRoot, case_insensitive: false,
    probe_files: [{ path: longLegacyDir + '/' + fileName, ...fileOptions }] };
  const replay = replayContext(input);
  installNativeKbn(replay, input);
  const matches = await replay.context.Kbn(input.project_dir, input.target_root, input.case_insensitive);
  kbn_cases.push({ name, input, expected: { matches, calls: replay.calls } });
}

function addDirContext(input = {}) {
  const replay = replayContext(input);
  const { context, record } = replay;
  const pendingAliases = [], validations = [];
  let promptDirectories = [...(input.prompt_directories ?? [])];
  let permission = {
    additionalWorkingDirectories: new Map((input.additional_directories ?? []).map(directory =>
      [directory.path, { path: directory.path, source: directory.source }])),
    blockReadsOutsideWorkingDirectories: input.block_reads_outside === true,
  };
  class ControlledNullPath extends Error {}
  const expand = (value, base = input.current_dir ?? A) => {
    if (value.includes('\0')) throw new ControlledNullPath('Controlled null path');
    if (input.expand_error) throw controlledError(input.expand_error, 'expand');
    return path.posix.resolve(base, value === '~' ? '/home/oracle'
      : value.startsWith('~/') ? '/home/oracle/' + value.slice(2) : value).normalize('NFC');
  };
  // The validator's path and stat imports share minified names with other
  // modules. Preserve the alias helpers' properties when supplying functions.
  context.a = Object.assign(value => path.posix.resolve(value), context.a);
  context.c = Object.assign(async value => {
    record('stat_directory', { path: value });
    if (input.stat_error) throw controlledError(input.stat_error, 'stat');
    return { isDirectory: () => input.is_directory !== false };
  }, context.c);
  Object.assign(context, {
    pi: () => input.project_unavailable ? null : input.current_dir ?? A,
    dt: expand, P: value => path.posix.resolve(input.current_dir ?? A, value),
    Doe: ControlledNullPath, qs: value => /^[\\/]{2}/.test(value),
    uRe: async value => { record('controlled_network_probe', { path: value }); return input.network_path === true; },
    NY: () => [], ce: () => permission, d: path.posix.dirname,
    he: { bold: value => value }, _: value => JSON.stringify(value), Yn: value => value,
    et: path.posix, O: () => 'darwin', X0: value => value, So: value => value.toLowerCase(),
    rL: value => /(?:^|[\\/])\.\.(?:[\\/]|$)/.test(value), jn: value => /^[\\/]{2}/.test(value),
    DA: () => spellingCache, Mo: value => input.path_spellings?.[value] ?? [value],
    Ac: () => promptDirectories,
    Pj: values => { promptDirectories = [...values]; record('register_prompt_directories', { paths: values }); },
    gT: () => record('refresh_prompt_directory_skills'),
    nP: (_, reason) => record('refresh_prompt_directory_agents', { reason }),
    b5e: () => record('clear_prompt_directory_cache'),
    ie: () => ({}), vo: (_, value) => input.canonical_results?.[value]
      ?? { resolvedPath: value, isCanonical: true },
    Ka: () => false, sM: () => false,
    Qt: () => input.project_settings_enabled !== false,
    br: feature => input.disabled_features?.includes(feature) === true,
    Cd: feature => input.disabled_features?.includes(feature) === true,
    ze: { refreshConfig: () => record('refresh_client_config') },
    Ru: (previous, update) => {
      record('controlled_permission_update', update);
      return { ...previous, additionalWorkingDirectories: new Map([
        ...previous.additionalWorkingDirectories,
        ...update.directories.map(value => [value, { path: value, source: update.destination }]),
      ]) };
    },
    $Po: (flag, value) => record('record_launch_flag', { flag, value }),
    fJ: async () => { if (input.save_error) throw controlledError(input.save_error, 'save'); },
    ADD_DIR_UUID: () => 'controlled-hook-notification-id', UHt: async () => ({ results: [], systemMessages: [] }),
    F: (values, predicate) => values.filter(predicate).length,
    I: (count, word) => count === 1 ? word : word + 's', Ze: () => undefined,
  });
  const spellingCache = { resolvedWorkingDirPaths: new Map() };
  vm.runInContext([src.relativePath, src.permissionRoots, src.permissionSpellings,
    src.permissionAdditionalRoots, src.permissionRestrictedRoots, src.permissionInsideRoot,
    src.permissionCoverage, src.addDirValidate, src.addDirMessages, src.addDirSkills,
    src.addDirChild].join('\n'), context, { timeout: 1000 });
  // UDn imports UUID generation as k; the project slug module uses that same
  // short name for its sanitizer. Keep those original module bindings apart.
  context.UDn = vm.runInContext(`(()=>{const k=ADD_DIR_UUID;${src.addDirEffect};return UDn})()`,
    context, { timeout: 1000 });
  const nativeAlias = context.OPr, nativeValidate = context.xwe;
  context.OPr = (...values) => {
    record('native_alias_call', { path: values[0] });
    const pending = nativeAlias(...values);
    pendingAliases.push(pending);
    return pending;
  };
  context.xwe = async (...values) => {
    const validation = await nativeValidate(...values);
    validations.push(clean(validation));
    return validation;
  };
  const call = vm.runInContext(`(()=>{${src.addDirText};return d})()`, context, { timeout: 1000 });
  const runtime = {
    session: 'controlled-session', ...(input.storage_v5 ? { storageV5: replay.backend } : {}),
    setToolPermissionContext: update => { permission = update(permission); },
    messageQueue: { enqueuePendingNotification: value => record('enqueue_notification', value) },
  };
  return { async run() {
    const result = await call(input.path, runtime);
    // UDn deliberately does not await OPr; drain that native promise for an
    // observable deterministic filesystem snapshot after the command result.
    await Promise.all(pendingAliases);
    return { result: clean(result), validations, prompt_directories: promptDirectories,
      additional_directories: [...permission.additionalWorkingDirectories.values()], ...replay.snapshot() };
  } };
}

const addDirInputs = [
  ['empty_path_prints_usage', { path: ' \t ' }],
  ['new_directory_records_one_way_alias', { path: B }],
  ['new_directory_backend_alias', { path: B, storage_v5: true }],
  ['current_directory_short_circuits_alias', { path: A }],
  ['current_child_registers_skills_without_alias', { path: A + '/child' }],
  ['current_child_already_registered_without_alias', { path: A + '/child', prompt_directories: [A + '/child'] }],
  ['current_child_features_disabled_without_alias', { path: A + '/child', project_settings_enabled: false }],
  ['current_child_link_outside_does_not_grant_or_record_alias', { path: A + '/link',
    canonical_results: { [A + '/link']: { resolvedPath: B, isCanonical: true } } }],
  ['current_child_unresolved_does_not_grant_or_record_alias', { path: A + '/child',
    canonical_results: { [A + '/child']: { resolvedPath: A + '/child', isCanonical: false } } }],
  ['project_settings_exact_grant_short_circuits_alias', { path: B,
    additional_directories: [{ path: B, source: 'projectSettings' }] }],
  ['additional_directory_child_short_circuits_alias', { path: B + '/child',
    additional_directories: [{ path: B, source: 'localSettings' }] }],
  ['read_fence_excludes_project_settings_grant', { path: B, block_reads_outside: true,
    additional_directories: [{ path: B, source: 'projectSettings' }] }],
  ['read_fence_retains_user_settings_grant', { path: B, block_reads_outside: true,
    additional_directories: [{ path: B, source: 'userSettings' }] }],
  ['missing_path_does_not_record_alias', { path: B, stat_error: 'ENOENT' }],
  ['stat_denied_does_not_record_alias', { path: B, stat_error: 'EACCES' }],
  ['file_reports_parent_without_alias', { path: B + '/file', is_directory: false }],
  ['network_path_does_not_record_alias', { path: B, network_path: true }],
  ['null_path_does_not_record_alias', { path: B + '\0hidden' }],
  ['unavailable_project_does_not_record_alias', { path: B, project_unavailable: true }],
];
const add_dir_cases = [];
for (const [name, options] of addDirInputs) {
  const input = { current_dir: A, ...options };
  add_dir_cases.push({ name, input, expected: await addDirContext(input).run() });
}

function cdContext(input = {}) {
  const calls = [], logs = [], reminders = [];
  const state = { cwd: input.current_dir ?? A, original_cwd: input.original_cwd ?? input.current_dir ?? A };
  const effect = (operation, ...values) => {
    calls.push({ operation, ...(values.length ? { values: clean(values) } : {}) });
    if (input.effects?.[operation]) throw controlledError(input.effects[operation], operation);
  };
  let chdirCount = 0;
  const expand = (value, base = state.cwd) => path.posix.resolve(base,
    value === '~' ? '/home/oracle' : value.startsWith('~/') ? '/home/oracle/' + value.slice(2) : value);
  const context = vm.createContext({
    Ee: '/', be: '/', et: path.posix, O: () => input.platform ?? 'darwin',
    X0: value => value, pt: () => '/home/oracle',
    pe: () => state.cwd, ke: () => state.original_cwd, dt: expand,
    Mo: value => input.requested_spellings ?? [value],
    zp: () => new Map(input.canonical_equivalences ?? []),
    Zd: permission => permission.deny ?? [], EN: permission => permission.allow ?? [],
    KDe: source => context.settingsRuleRoot(source),
    t: (message, options) => logs.push({ message, ...(options ? { level: options.level } : {}) }),
    qg: value => {
      chdirCount += 1;
      effect(chdirCount === 1 ? 'chdir' : 'rollback_chdir', value);
      state.cwd = value;
    },
    qp: value => effect('set_session_cwd', value),
    ZP: value => { effect('set_original_cwd', value); state.original_cwd = value; },
    fe: source => input.settings?.[source] ?? null,
    jpt: async () => effect('relocate_transcript'),
    ein: async value => effect('rehome_background_state', value),
    ra: { rehome: async () => effect('settings_watcher_rehome'),
      notifyChange: (...values) => effect('settings_notify_change', ...values) },
    Die: { rehome: async () => effect('skills_watcher_rehome') },
    Gee: async (...values) => { effect('load_skills', ...values); return []; },
    qrn: async () => effect('register_skills'),
    Nd: () => ({ refreshGitBranch: () => effect('refresh_git_branch') }),
    ze: { refreshConfig: () => effect('refresh_client_config') },
    yun: { of: () => ({ clear: () => effect('clear_session_state') }) },
    dc: { cache: { clear: () => effect('clear_project_cache') } },
    x0: () => ({ reset: () => effect('reset_trust_state') }),
    U: async () => { effect('load_memory_context'); return input.memory_context ?? ''; },
    Xu: value => value,
    a: { CLAUDE_CODE_SANDBOXED: input.sandboxed === true },
    QP: () => input.agent_owned === true, Jm: () => input.headless === true,
    mcn: () => input.project_grants_gated === true,
    c: value => value,
    i: (...values) => effect('telemetry', ...values),
    AO: () => input.trusted !== false,
    Pk: () => input.trust_root,
    Rbe: async (...values) => effect('accept_directory_trust', ...values),
    oN: (...values) => { effect('controlled_raw_network_check', ...values); return { ok: input.safe_precheck !== false }; },
    QM: () => input.resolved_network_path ? 'controlled network path' : undefined,
    Xpr: async value => {
      effect('controlled_path_validation', value);
      return input.validation ?? { result: 'ok', directory: value };
    },
  }, { codeGeneration: { strings: false, wasm: false } });
  for (const name of ['n6e', 'a2', 'T4', 'p6t', 'Jzn', 'SRe', 'NX', 'Ann', 'qOs', 'rMs',
    'ZWn', 'vEn', 'dbn', 'Tbn', 'tE', 'PBe', 'DR']) context[name] = (...values) => effect(name, ...values);
  context.settingsRuleRoot = vm.runInContext(`((source)=>{
    const pe=value=>value,eo=value=>ROOT_DIRNAME(value),He=()=>'/oracle/.claude';
    const state={cwd:CURRENT_CWD,flagPath:FLAG_PATH};
    ${src.settingsSourceRoot}${src.settingsRoot}
    return M4o(source,state);
  })`, vm.createContext({ ROOT_DIRNAME: path.posix.dirname, CURRENT_CWD: state.original_cwd,
    FLAG_PATH: input.flag_path }), { timeout: 1000 });
  vm.runInContext([src.unique, src.ruleRoots, src.ruleSourceRoots, src.ruleRootAdapter,
    src.relativePath, src.canonicalAliases, src.cdMatcher, src.ruleEscape, src.ruleFormat,
    src.sourceLabels, src.sourceLabelAdapter, src.cdError, src.invisible, src.safeDisplay,
    src.reminderMarkers + ';', src.reminder, src.cdMove, src.cdControl].join('\n'), context, { timeout: 1000 });
  const nativeReminder = context.Ul;
  context.Ul = value => { reminders.push(value); return nativeReminder(value); };
  let busyCalls = 0, freshCalls = 0;
  const sequence = (values, index, fallback) => values?.[Math.min(index, values.length - 1)] ?? fallback;
  const runtime = {
    session: 'controlled-session', toolPermissionContext: { trustedNetworkDirectories: [] },
    isBusy: () => sequence(input.busy_sequence, busyCalls++, false),
    isFreshSession: () => sequence(input.fresh_sequence, freshCalls++, input.fresh_session === true),
    ...(input.request_built !== undefined ? { requestBuilt: () => input.request_built } : {}),
    settlePlanAutoMode: () => effect('settle_plan_auto_mode'),
    retireDepartedAdditionalDirectories: values => effect('retire_departed_additional_directories', values),
    enqueueMoveNotice: value => effect('enqueue_move_notice', value),
  };
  return { context, runtime, state, calls, logs, reminders };
}

const cdGlobInputs = [
  ['', ''], ['', 'x'], ['*', ''], ['*', 'x'], ['*', 'x/y'], ['*', '\n'],
  ['**', ''], ['**', 'x/y'], ['**', '\n'], ['**/', ''], ['**/a', 'a'], ['**/a', 'x/a'],
  ['a/**', 'a'], ['a/**', 'a/'], ['a/**', 'a/b/c'], ['a/**/b', 'a/b'], ['a/**/b', 'a/x/y/b'],
  ['a/*/b', 'a/b'], ['a/*/b', 'a/x/b'], ['a/*/b', 'a//b'], ['a***b', 'ab'], ['a***b', 'axb'],
  ['a?b', 'a?b'], ['a?b', 'axb'], ['a.b', 'a.b'], ['a.b', 'axb'], ['[a]', '[a]'],
  ['a+b', 'a+b'], ['(a)', '(a)'], ['a\\b', 'a\\b'], ['a|b', 'a|b'], ['a^b$', 'a^b$'],
  ['Project/**', 'project/file'], ['k', 'K'], ['s', 'ſ'], ['é', 'É'], ['σ', 'ς'], ['ß', 'SS'],
  ['😀', '😀'], ['*', '😀'], ['*', '/'], ['a', 'a\n'], ['a', 'a\r'], ['a', 'a\r\n'],
  ['a', 'a\u2028'], ['a', 'a\u2029'], ['a', 'a\n\n'], ['**', 'a\nb'], ['*', 'a\nb'],
];
const cdBase = cdContext().context;
const cd_glob_cases = cdGlobInputs.map(([pattern, value]) => ({ input: { pattern, value },
  expected: { matches: cdBase.N(pattern).test(value), regex_source: cdBase.N(pattern).source } }));
const cdRootInputs = [
  ['plain', 'dir/**', 'projectSettings'], ['dot_relative', './dir/**', 'projectSettings'],
  ['filesystem_absolute', '//tmp/**', 'projectSettings'], ['home_relative', '~/dir/**', 'projectSettings'],
  ...['userSettings', 'projectSettings', 'localSettings', 'flagSettings', 'policySettings', 'cliArg', 'command',
    'session', 'toolsNarrowing', 'mcpServerPolicy', 'hostCredential'].map(source => [source, '/dir/**', source]),
];
const cd_root_cases = cdRootInputs.map(([name, pattern, source]) => {
  const input = { pattern, source, current_dir: A, flag_path: '/external/config/settings.json' };
  return { name, input, expected: clean(cdContext(input).context.m0e(pattern, source)) };
});
const cdMatcherInputs = [
  ['dir/**', 'session', A + '/dir'], ['dir/**', 'session', A + '/dir/child'],
  ['*', 'session', A], ['**', 'session', A], ['**', 'session', '/workspace'],
  ['../B', 'session', B], ['dir//**/', 'session', A + '/dir/child'],
  ['//workspace/B/**', 'session', B], ['/B/**', 'session', B],
  ['~/dir/**', 'session', '/home/oracle/dir/child'], ['/dir/**', 'userSettings', configDir + '/dir'],
  ['/dir/**', 'flagSettings', '/external/config/dir'], ['*', 'session', A + '/x/y'],
  ['**/A', 'session', A],
];
const cd_matcher_cases = cdMatcherInputs.map(([pattern, source, target]) => {
  const input = { pattern, source, target, current_dir: A, flag_path: '/external/config/settings.json' };
  const context = cdContext(input).context;
  const roots = clean(context.m0e(pattern, source));
  return { input, relative_pattern: roots.relativePattern,
    relative_path: path.posix.relative(roots.root ?? A, target), expected: context.B(pattern, source, target) };
});

const rule = (pattern, source = 'projectSettings', tool = 'Cd') => ({ source,
  ruleValue: { toolName: tool, ...(pattern !== undefined ? { ruleContent: pattern } : {}) } });
const cdPermissionInputs = [
  ['no_rules_allow', {}], ['other_tool_deny_ignored', { deny: [rule(undefined, 'session', 'Bash')] }],
  ['tool_wide_deny', { deny: [rule(undefined)] }],
  ['deny_before_allow', { deny: [rule('dir/**')], allow: [rule('**')] }],
  ['outside_allow_patterns', { allow: [rule('other/**'), rule('safe/**', 'userSettings')] }],
  ['matching_allow', { allow: [rule('dir/**')] }],
  ['tool_wide_allow', { allow: [rule(undefined)] }],
  ['requested_spelling_deny_survives_canonical_allow', { requested_spellings: [A + '/blocked/path'],
    deny: [rule('blocked/**')], allow: [rule('dir/**')] }],
  ['canonical_spelling_deny_applies', { requested_spellings: [A + '/elsewhere'], deny: [rule('dir/**')] }],
  ['canonical_equivalence_only_expands_allow', { canonical_path: '/private/tmp/project',
    canonical_equivalences: [['/private/tmp', '/tmp']], allow: [rule('//tmp/**')] }],
  ['canonical_equivalence_does_not_expand_deny', { canonical_path: '/private/tmp/project',
    canonical_equivalences: [['/private/tmp', '/tmp']], deny: [rule('//tmp/**')] }],
];
const cd_permission_cases = cdPermissionInputs.map(([name, options]) => {
  const { deny = [], allow = [], ...rest } = options;
  const input = { requested_path: A + '/dir', canonical_path: A + '/dir', current_dir: A, deny, allow, ...rest };
  const context = cdContext(input).context;
  return { name, input, expected: clean(context.x({ requestedPath: input.requested_path, canonicalPath: input.canonical_path }, { deny, allow })) };
});
const cd_rejection_cases = [];
for (const terminal_affordances of [true, false]) {
  for (const source of ['userSettings', 'projectSettings', 'localSettings', 'flagSettings', 'policySettings', 'session']) {
    for (const pattern of [undefined, 'blocked/(path)\\name']) {
      const check = { result: 'blockedByRule', rule: rule(pattern, source) };
      const input = { path: B, check, terminal_affordances };
      cd_rejection_cases.push({ input, expected: cdBase.Jpr(B, check, undefined, { terminalAffordances: terminal_affordances }) });
    }
  }
  const check = { result: 'outsideAllowedPatterns', allowedPatterns: ['one/**', 'two/*'] };
  cd_rejection_cases.push({ input: { path: B, check, terminal_affordances },
    expected: cdBase.Jpr(B, check, undefined, { terminalAffordances: terminal_affordances }) });
}

const cdMoveInputs = [
  ['cd_used_session', { source: 'cd_command' }],
  ['remote_used_session', { source: 'set_cwd' }],
  ['request_not_built_omits_stale_environment', { source: 'cd_command', request_built: false }],
  ['request_built_includes_stale_environment', { source: 'set_cwd', request_built: true }],
  ['fresh_session_suppresses_notice', { source: 'cd_command', fresh_session: true }],
  ['fresh_confirmation_failed_uses_notice', { source: 'cd_command', fresh_session: true, confirm_fresh: false }],
  ['gated_project_rules_notice', { source: 'set_cwd', project_grants_gated: true }],
  ['sandbox_suppresses_project_grants_gate', { source: 'set_cwd', project_grants_gated: true, sandboxed: true }],
  ['memory_context_is_appended', { source: 'cd_command', memory_context: '<memory>new project</memory>' }],
  ['additional_directories_resolve_against_both_departed_cwds', { source: 'cd_command', original_cwd: '/workspace/original',
    settings: { projectSettings: { permissions: { additionalDirectories: ['shared', '/absolute'] } },
      localSettings: { permissions: { additionalDirectories: ['shared', '../sibling'] } } } }],
  ['transcript_move_failure_rolls_back_and_throws', { source: 'set_cwd', effects: { relocate_transcript: 'EIO' } }],
  ['transcript_and_rollback_failure_complete_with_relocation_false', { source: 'set_cwd',
    effects: { relocate_transcript: 'EIO', rollback_chdir: 'EACCES' } }],
  ['nonfatal_watcher_and_memory_failures_continue', { source: 'cd_command',
    effects: { settings_watcher_rehome: 'EIO', skills_watcher_rehome: 'EIO', load_memory_context: 'EIO' } }],
];
const cd_move_cases = [];
for (const [name, options] of cdMoveInputs) {
  const input = { current_dir: A, target_dir: B, ...options };
  const replay = cdContext(input);
  const moveOptions = { freshSession: input.fresh_session === true,
    ...(input.confirm_fresh !== undefined ? { confirmFresh: () => input.confirm_fresh } : {}),
    ...(input.request_built !== undefined ? { requestBuilt: () => input.request_built } : {}) };
  let result, thrown;
  try { result = clean(await replay.context.WAt('controlled-session', input.target_dir, input.source, moveOptions)); }
  catch (error) { thrown = { code: error.code ?? null, message: error.message }; }
  cd_move_cases.push({ name, input, expected: { ...(result ? { result } : { thrown }),
    base_message: replay.reminders[0] ?? '', reminders: replay.reminders,
    state: replay.state, calls: replay.calls, logs: replay.logs } });
}

const cdControlInputs = [
  ['busy_rejects_before_validation', { busy_sequence: [true], request: { path: B } }],
  ['missing_path_is_invalid', { request: {} }], ['whitespace_path_is_invalid', { request: { path: ' \t ' } }],
  ['trust_echo_is_required', { request: { path: B, trust_accepted: true } }],
  ['raw_unsafe_path_is_not_echoed', { safe_precheck: false, request: { path: '//secret/network' } }],
  ['raw_path_check_exception_is_unsafe', { effects: { controlled_raw_network_check: 'EIO' }, request: { path: B } }],
  ['resolved_invisible_path_is_not_echoed', { request: { path: B }, validation: { result: 'ok', directory: B + '\u202fsecret' } }],
  ['resolved_network_path_is_not_echoed', { request: { path: B }, resolved_network_path: true }],
  ['not_found_response', { request: { path: B }, validation: { result: 'not_found', path: B } }],
  ['not_directory_response', { request: { path: B }, validation: { result: 'not_a_directory', path: B, parent: '/workspace' } }],
  ['blocked_rule_has_remote_wording', { request: { path: B },
    validation: { result: 'blocked_by_rule', directory: B, check: { result: 'blockedByRule', rule: rule(undefined) } } }],
  ['invisible_rule_uses_safe_fallback', { request: { path: B },
    validation: { result: 'blocked_by_rule', directory: B, check: { result: 'blockedByRule', rule: rule('hidden\u200b/**') } } }],
  ['same_directory_is_noop', { request: { path: A }, validation: { result: 'same', directory: A } }],
  ['untrusted_directory_requires_trust', { request: { path: B }, trusted: false }],
  ['untrusted_directory_reports_distinct_trust_root', { request: { path: B }, trusted: false, trust_root: '/workspace' }],
  ['invisible_trust_root_is_omitted', { request: { path: B }, trusted: false, trust_root: '/workspace\u200b' }],
  ['wrong_trust_echo_repeats_needs_trust', { request: { path: B, trust_accepted: true, trusted_directory: C }, trusted: false }],
  ['exact_trust_echo_allows_move', { request: { path: B, trust_accepted: true, trusted_directory: B }, trusted: false }],
  ['turn_started_during_validation_rejects', { request: { path: B }, busy_sequence: [false, true] }],
  ['used_session_move_enqueues_notice', { request: { path: B } }],
  ['fresh_session_move_has_no_notice_and_settles_plan', { request: { path: B }, fresh_session: true }],
  ['fresh_session_race_uses_notice', { request: { path: B }, fresh_sequence: [true, false] }],
  ['notice_delivery_error_is_nonfatal', { request: { path: B }, effects: { enqueue_move_notice: 'EIO' } }],
];
const set_cwd_cases = [];
for (const [name, options] of cdControlInputs) {
  const input = { current_dir: A, ...options };
  const replay = cdContext(input);
  let result, thrown;
  try { result = clean(await replay.context.Byi(input.request, replay.runtime)); }
  catch (error) { thrown = { code: error.code ?? null, message: error.message }; }
  set_cwd_cases.push({ name, input, expected: { ...(result ? { result } : { thrown }),
    state: replay.state, calls: replay.calls, logs: replay.logs } });
}

const rawCdInputs = [
  ['empty_path_prints_usage', { path: ' \t ' }],
  ['same_directory_returns_without_move_or_notice', { path: A, validation: { result: 'same', directory: A } }],
  ['missing_path_message', { path: B, validation: { result: 'not_found', path: B } }],
  ['file_message_suggests_parent', { path: B + '/file',
    validation: { result: 'not_a_directory', path: B + '/file', parent: B } }],
  ['permission_message', { path: B,
    validation: { result: 'blocked_by_rule', directory: B, check: { result: 'blockedByRule', rule: rule(undefined) } } }],
  ['used_session_success_has_display_and_model_notice', { path: B }],
  ['raw_cd_has_no_fresh_or_request_built_callback', { path: B, fresh_session: true, request_built: false }],
  ['move_error_reports_staying_directory', { path: B, effects: { relocate_transcript: 'EIO' } }],
  ['cancel_untrusted_move', { path: B, trusted: false, action: 'cancel' }],
];
const raw_cd_cases = [];
for (const [name, options] of rawCdInputs) {
  const input = { current_dir: A, ...options };
  const replay = cdContext(input), replies = [];
  replay.context.RAW_CD_DEPS = {
    e: (component, props) => ({ component, props }), G: 'validation', Se: 'trust',
    U: 'project_grants', _: 'mcp_approval', he: { bold: value => value }, Yn: value => value,
    ce: () => replay.runtime.toolPermissionContext,
    Fe: async () => ({ pendingServers: [] }),
    Ee: async () => replay.calls.push({ operation: 'controlled_refresh_project_tools' }),
    m: () => {}, g: () => {}, ue: value => value, _xn: () => undefined,
  };
  const call = vm.runInContext(`(()=>{
    const {e,G,Se,U,_,he,Yn,ce,Fe,Ee,m,g,ue,_xn}=RAW_CD_DEPS;
    ${src.rawCd};return Bo;
  })()`, replay.context, { timeout: 1000 });
  const element = await call((message, metadata) => replies.push({ message, ...(metadata ? { metadata: clean(metadata) } : {}) }),
    replay.runtime, input.path);
  let rendered = null;
  if (element) {
    rendered = { component: element.component,
      ...Object.fromEntries(Object.entries(element.props).filter(([, value]) => typeof value !== 'function')) };
    if (element.props.onDone) await element.props.onDone();
    if (input.action === 'cancel') await element.props.onCancel();
  }
  raw_cd_cases.push({ name, input, expected: { rendered, replies, state: replay.state,
    calls: replay.calls, logs: replay.logs } });
}

const fixture = {
  schema_version: 1,
  version: '2.1.307',
  native_sha256: nativeSha256,
  evidence_type: 'controlled-replay-of-shipped-functions',
  sourceByteRanges,
  case_group_contracts: {
    uuid_resolution_cases: 'Iue job/helper API, not the CLI flag or interactive slash resume entrypoint',
    resume_entrypoint_cases: 'Actual FIe/iQo/fQo selection stopped at beforeUse, with controlled full-log readers, repository bucket candidates and global candidate; actual De UUID dispatch through controlled vrn to native LAt. local_reader_input_utf8 samples are not parsed by native code in this group',
    add_dir_cases: 'Actual text caller d, xwe, g7t, UDn and OPr; controlled stat/network/canonical probes and permission mutation',
    raw_cd_cases: 'Actual Bo caller and WAt; controlled Xpr validation, terminal display, React elements and project-tool refresh',
  },
  scope: {
    unchanged_native_functions: ['Pn', 'Bue', 'J0', 'Uk', 'Jnt', 'yf', 'zS', 'C0e', 'a8', 'D', 'fZ', 'i0',
      'Ya', 'T2n', 'tAt', 'Cuo', 'OPr', 'W0e', 'G9', 'KBe', 'CCo', 'LAt', 'oHe', 'Ice', 'G', 'jt',
      'oL', 'H', 'M', 'Iue', 'Pe', 'f3r', 'wS', 'wgi', 'Vbn', 'Ub', 'rRe', 'te',
      'Kbn', 'V_e', 'ne', 'Ynt', 'Pue',
      'x/B/N Cd permission helpers', 'Jl', 'LUt', 'm0e', 'T2e', 'Rcn', 'M4o', 'cSn',
      'gn', 'vx', 'Z1e', 'Jpr', 'Dli', 'Ul', 'WAt', 'Byi', 'Bo raw cd caller',
      'gk', 'ql', 'J5n', 'Zu', 'HJe', 'A2e', 'my', 'xwe', 'Pwe', 'h prompt directory registration',
      'g7t', 'UDn', 'd text add-dir caller', 'FIe selector through beforeUse hook', 'iQo', 'fQo', 'De slash resume argument dispatch'],
    controlled_dependencies: ['host config/cwd/pinned project directory and source parent', 'in-memory filesystem and explicit errors',
      'storage key validators and backend operations', 'current-project directory resolution',
      'RDt transcript directory inventory and metadata', 'project directory enumeration',
      'NAt header foreign-cwd classification and O0e/FAt collision recovery',
      'AW/ak backend availability', 'fork statistics observer',
      'in-memory transcript lstat/open/stream handles; ordinary Node readline',
      'v6t git worktree membership supplied as repository_paths',
      'permission requested spelling inventory and canonical equivalence map',
      'directory move effects, watchers, memory, trust predicates and path expansion',
      'set_cwd raw network check and Xpr path validation result',
      'add-dir stat, path expansion, network probes and canonical path resolution',
      'add-dir permission mutation, feature gates and empty DirectoryAdded hook results',
      'raw cd React element construction, project-tool refresh and empty MCP approval inventory',
      'FIe/gOe full log parser, iQo repository bucket candidates and fQo global candidate',
      'De UUID parse for fixture UUID, background holder lookup, and vrn dispatch to native LAt',
      'identity Xu/Yn and terminal-bold display for ordinary printable fixture paths'],
    excluded: ['complete CLI and interactive add-dir confirmation UI', 'authentication, network and model requests',
      'real filesystem permissions, races and concurrent writers', 'storage validator/pagination correctness',
      'git worktree discovery and repository membership resolution', 'full transcript JSON parsing and picker metadata hydration',
      'UUID and long-slug collision resolution through the storage V5 backend',
      'all-project picker, flag resume processing after beforeUse, SDK host integration and end-to-end resume',
      'real directory movement, trust persistence, watchers and grant retirement',
      'complete network-path safety implementation and path-display escaping'],
  },
  path_cases, alias_write_cases, alias_read_cases, dedup_cases, discovery_cases,
  message_presence_cases, uuid_resolution_cases, kbn_cases, resume_entrypoint_cases,
  add_dir_cases, cd_glob_cases, cd_root_cases, cd_matcher_cases, cd_permission_cases, cd_rejection_cases,
  cd_move_cases, set_cwd_cases, raw_cd_cases,
};
const output = JSON.stringify(fixture, null, 2) + '\n';
const fixturePath = new URL('../../crates/session/tests/fixtures/workspace_session_2_1_307.json', import.meta.url);
if (args.includes('--check')) assert.equal(readFileSync(fixturePath, 'utf8'), output, 'Native workspace fixture differs');
else writeFileSync(fixturePath, output);
console.log(`${args.includes('--check') ? 'checked' : 'wrote'} native workspace session fixture: `
  + `${path_cases.length} path, ${alias_write_cases.length} alias write, ${alias_read_cases.length} alias read, `
  + `${dedup_cases.length} dedup, ${discovery_cases.length} discovery, ${message_presence_cases.length} message presence, `
  + `${uuid_resolution_cases.length} UUID helper, ${kbn_cases.length} long slug, ${resume_entrypoint_cases.length} resume entrypoint, ${add_dir_cases.length} add-dir, `
  + `${cd_glob_cases.length} Cd glob, ${cd_root_cases.length} Cd root, `
  + `${cd_matcher_cases.length} Cd rooted match, ${cd_permission_cases.length} Cd permission, `
  + `${cd_rejection_cases.length} Cd rejection, ${cd_move_cases.length} Cd move, ${set_cwd_cases.length} set_cwd, ${raw_cd_cases.length} raw cd cases; `
  + `${sourceByteRanges.length} source ranges`);

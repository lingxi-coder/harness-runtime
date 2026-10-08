#!/usr/bin/env node
// Source-pinned 2.1.289 Projects-session consent oracle.
// Usage: node scripts/tests/mods_projects_consent_289_oracle.mjs <2.1.289-binary> [--check]
//
// The executable source below is extracted from the pinned Native binary. The
// actual MCP registry, Projects session owner, GrowthBook network/storage and
// $.tool.call W1 tool runner are represented only by explicit test seams.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';

const args = process.argv.slice(2);
const check = args.includes('--check');
const binaries = args.filter(arg => arg !== '--check');
assert.equal(binaries.length, 1,
  'usage: node scripts/tests/mods_projects_consent_289_oracle.mjs <2.1.289-binary> [--check]');

const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
const binary = readFileSync(binaries[0]);
assert.equal(sha256(binary),
  '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 macOS arm64 binary SHA-256');

const ranges = [];
function nativeRange(label, start, end, expectedHash, startText) {
  assert(end > start, `${label}: invalid range`);
  const bytes = binary.subarray(start, end);
  assert.equal(bytes.length, end - start, `${label}: short binary read`);
  if (startText) assert.equal(bytes.subarray(0, Buffer.byteLength(startText)).toString(), startText,
    `${label}: start marker moved`);
  assert.equal(sha256(bytes), expectedHash, `${label}: Native source hash changed`);
  ranges.push({label, binary_start: start, binary_end_exclusive: end, sha256: expectedHash});
  return bytes.toString('utf8');
}

const src = {
  // Environment parser and proxy. The separate export/schema ranges below pin
  // the Projects variable to M.bool(); they are also included in the fixture.
  le: nativeRange('Le environment bool parser', 179188527, 179188669,
    'c7c851b23895c896ce38865f13ecbb54e11e83018c8ced9cae422c41298e6dfd', 'function Le(e){'),
  ss: nativeRange('Ss environment false parser', 179188669, 179188820,
    '599a5bb5a7104160e8cc604642196289f13e642368eaa6052404652f7ccdbf5e', 'function Ss(e){'),
  envSchemaFactory: nativeRange('M.bool environment schema factory', 179454477, 179455158,
    'ea0d69c684767230a5ab95f695d98f495cdbcad75584b1ae9d5d92e7b8009fd7', 'function i(n){'),
  envExportBinding: nativeRange('CLAUDE_CODE_PROJECTS_SESSION export binding', 179483151, 179483186,
    '1bb9e88c1ebae7cd2510b0a1aa6d559965064d8f44f6f9a0ae7705025d792753',
    'CLAUDE_CODE_PROJECTS_SESSION:()=>Un'),
  envParserBinding: nativeRange('Projects environment field uses M.bool()', 179490117, 179490128,
    '2a8dcb5c9721fdaa241462558f0509f6df118060b29c989566c381231063bf64', 'Un=M.bool()'),
  envProxy: nativeRange('Native typed environment proxy', 179520867, 179521201,
    '5d185334bd02cda9a78fdc620d4fc8956ee72f1ca610258597f7e4dbdab70cf6', 'function ant(o,t){'),

  // Session / host stores. z and Is are Native; current-session dispatch is
  // supplied by the test harness exactly at the imported accessor boundary.
  zStore: nativeRange('Native lazy WeakMap store', 179227685, 179227829,
    'cee89b130a2f5db0c77c76f307ffc83ab3fa6541e44f33023710e68965bc126b', 'class z{'),
  sessionAndHostAccessors: nativeRange('U/n/Is default-host and active-session accessors', 179291610, 179291705,
    '339f41d1136cbf3482b3ae3286ce29b937fe29410b48a1180fb6df732bbd2298', 'function U(){'),
  nativeSsAccessor: nativeRange('SS current-session MCP clients accessor', 179323539, 179323603,
    'd801a4d382bec9dbffd44bba081be565dd6ea7effdb77ebde4feadb913264522', 'function SS(){'),

  // hearthbot server matching. qS uses a startup URL origin and recognized
  // ingress path; Jkn then restricts the exact hearthbot route.
  hearthbotName: nativeRange('Native hearthbot server name', 181495916, 181495959,
    '0c64b2a4150af9428d92704b81415bd74803db77077435e9b758cfeb3500a85c', 'var kc="hearthbot"'),
  cliOwnedMark: nativeRange('Native CLI-owned config marker', 181499037, 181499398,
    'd17f7d3fcd1028b404f0efb36775837c8c4e881e07ba99cc8e56a0642a79fbec', 'function OEe(e,r){'),
  cliOwnedPredicate: nativeRange('Native config ownership exclusion', 181499398, 181499479,
    '9cadb46a3609fcbc06e3f765ac5ab0e1fe057f3f1da5e12d6243ac36ad583e06', 'function nf(e){'),
  currentHostCliStateAccessor: nativeRange('Kt current-host MCP state accessor', 181498935, 181498971,
    '7bae2b952e287e1c3a189689302fbc38281a9aea4fb2862e06c4777fde3c91fb',
    'function Kt(){return T.of(U().host)}'),
  startupPathsAndStore: nativeRange('Native recognized ingress paths and startup URL store', 181502232, 181502661,
    '1ed0acccbb5e68c796d521d25e3a4cc4830d1ead50e70cf0b3191c09ad136a28', 'var g=['),
  startupOriginPredicate: nativeRange('Native startup URL origin/path predicate', 181502797, 181503054,
    'ee962826757386373f730209c9ddc69982dd11cf15e854e55630383ac1ee3b35', 'function qS(e){'),
  hearthbotUrlPredicate: nativeRange('Native hearthbot route predicate', 181506639, 181506912,
    '34dbdfee8e9c62c21e2a68fb14202aaf7d71c6609dba78d8f684646e097df954', 'var q="/v1/code/mcp/hearthbot"'),
  startupUrlEnv: nativeRange('Native startup URL environment selection', 181508045, 181508129,
    'be0cac9bd887d1a62be3220c574ecf0dcf686cf427eeafc60f9a5339db23954b',
    'function C(){return process.env.SESSION_INGRESS_URL'),
  mcpClientPredicate: nativeRange('Native Sbt/gzt session MCP client predicate', 181808884, 181809071,
    'a1c25e2e69a503d209add1b9b535b208d07be2df74425913a18a90b79b48c6e2', 'function gzt(e){'),

  // Main consent gate and its direct producers/callers.
  webFetchAlias: nativeRange('Native Uoe WebFetch alias', 183190392, 183190431,
    'a54a825ea4a9f70c0e0a26180866f0076cc8d8b3dcb14f4cf1e22eaf7ae9fcde', 'var _r="WebFetch"'),
  webSearchAlias: nativeRange('Native Uoe WebSearch alias', 183304712, 183304751,
    'dd6beace88fabb5ea7984a88a74082229eeaf84b73195d7e7be1de6802f400f9', 'var Pm="WebSearch"'),
  gateSetup: nativeRange('Native Tf/el/NSn/Uoe setup', 183805484, 183805858,
    'c07601d85715ef0f4c9ab3e648ffc33812bb1e0e381e456c0727c551c28ea812', 'var Tf='),
  projectsGate: nativeRange('Native Projects consent predicate Uoe', 183805687, 183805858,
    '7507090d046e83385390644695e89b695b66ec7b919048cc1dda2e69e98fa3cb', 'function Uoe(e){'),
  remoteDynamicConfigCall: nativeRange('Remote dynamic config calls NSn', 196544676, 196544722,
    'c4f77132cd9624c389e069983eb4d960b5bd488274c9642586527ebc41c9a03b',
    'if(a.CLAUDE_CODE_REMOTE)mpn(zi,ar),NSn(zi,fo);'),
  remoteRefreshCall: nativeRange('Remote config refresh calls NSn', 205965340, 205965418,
    '39d067a8ac92c3a53d3891bb2807d55a44ac70e4bd3dc05f839001619ca29343',
    'if(_&&a.CLAUDE_CODE_REMOTE)for(let[k,T]of Object.entries(I))'),
  toolCallConsent: nativeRange('Native eU $.tool.call consent contract', 188853212, 188855653,
    'a8488d8e2775de457b6ff288249a54f1367ef3116b6f9b64b78863bedf22dd66', 'async function eU('),

  // GrowthBook source roles and lifecycle owner.
  gbEnvOverrideGetter: nativeRange('Native Fu environment override getter', 181838076, 181838114,
    '2127b64f96492c66a5d8a0a79c35120bb2b205dbd2bf26d7ad4ff15bd56de55b',
    'getEnvironmentOverrides(){return null}'),
  gbConfigOverrideGetter: nativeRange('Native Fu config override getter', 181838503, 181838532,
    'f2ebbe9e144f8ca849a426749bb2a136876d15f45c0671ddbf4e2fde15d7c1bd',
    'readConfigOverrides(){return}'),
  gbFeatureGetter: nativeRange('Native source-aware feature getter', 181846341, 181847084,
    '106782695c7016daf255548f5737e6472e2bb5bb1b860e58d9eae8b253220113',
    'getFeatureValueWithSource(e,n){'),
  gbDu: nativeRange('Native feature-value defaulting helper', 181851707, 181851744,
    '4ef000b7340a6cab5590571f5e21f7e35d53cda372f9e54110b403c19ada178e', 'function Du(e,n){'),
  gbDefaultHost: nativeRange('Native GrowthBook API host constant', 181836102, 181836162,
    'e364071f5633d829ba6a0f3bc20f472801f4273f5d0e890cb410af584f785924',
    'function Nu(){return VR}'),
  gbNonDefaultField: nativeRange('Native GrowthBook per-client host history initialization', 181836572, 181836597,
    '119dc1c25ca51b86a8768e5cdd588472e0af12fb443488cb962e73aa30128dfb',
    'usedNonDefaultHost=!1;'),
  gbHasNonDefault: nativeRange('Native GrowthBook host-history getter', 181838771, 181838826,
    '7bcfcb84779b6635c30cc46b133bcf8dea6d61e33208385bf75cff41a15f147d',
    'hasUsedNonDefaultHost(){'),
  gbSetNonDefault: nativeRange('Native GrowthBook host-history assignment', 181844245, 181844304,
    'ff3f1dfaab8de2deb0652d1ef73120a16578edf781006d622abcad595b8cb0b2',
    'this.usedNonDefaultHost||=h!=='),
  gbHostOwner: nativeRange('Native current-session GrowthBook client owner', 182188098, 182188206,
    'ca7a8653cbd341e6585599fc3e6a3ef0ce5492dbf39e096ff72ddbe422dc8cca', 'class eXo{'),
  gbClientInitializer: nativeRange('Native GrowthBook client lazy initializer', 182188260, 182188813,
    '1aae4728aa575c9f6400393b8f393b0ce37fde222381e02a761e755ee77f7bd9', 'function Ne(){'),
  gbUse: nativeRange('Native Use accessor', 182189510, 182189561,
    '283c216316d0e34b03db2d231144a94646c9b7f587573fcb043814102177e92b', 'function Use(){'),
  gbIo: nativeRange('Native io accessor', 182191232, 182191292,
    '7d6db44275cce4ac2d5ef5a14dc215f9ca277e2d76f2444d94d59a76fb49ec18', 'function io(e,n){'),
  growthBookDisableExport: nativeRange('DISABLE_GROWTHBOOK typed env export', 179485794, 179485820,
    '95ca25b27037d79aab08801b767447f980e45f8078e55338cda9db5c96a6c99a',
    'DISABLE_GROWTHBOOK:()=>Jn,'),
  growthBookDiskCacheExport: nativeRange('disk-cache-while-telemetry-off typed env export', 179481409, 179481461,
    '0587b094bda84ee631f9a63ad0f8d1ed9c33f39376305514e3d41ef7c69e9866',
    'CLAUDE_CODE_GB_DISK_CACHE_WHEN_TELEMETRY_OFF:()=>Zr,'),
  growthBookDisableSchema: nativeRange('DISABLE_GROWTHBOOK is boolean env schema', 179490508, 179490520,
    '39d379eb0dfaf8bb28101e5b6886e66a2df353e7f685fe8a786f35107dde23bd', 'Jn=M.bool(),'),
  growthBookDiskCacheSchema: nativeRange('disk-cache env flag is boolean schema', 179488313, 179488325,
    '76213ae0ad133fc303b268e81eeede353430fa895bda05dfd3efaf3d87e3bb57', 'Zr=M.bool(),'),
  growthBookInitKickExport: nativeRange('optional GrowthBook startup-kick env export', 179481592, 179481637,
    '9f6a9081d20158c7e6a6e87c0e06be390cac47983882cdf3c7e011c2fdd0dd3e',
    'CLAUDE_CODE_GROWTHBOOK_KICK_FROM_INIT:()=>De,'),
  growthBookInitKickSchema: nativeRange('startup-kick env flag is boolean schema', 179489345, 179489357,
    'cf4f120a966ce3c81bee060b2f9150025000f73de9271d7db1d955e4bf8f287f', 'De=M.bool(),'),
  growthBookTelemetryMode: nativeRange('telemetry traffic mode and non-default predicate', 179441125, 179441433,
    '1d399b4e227889efaf535677cf7a7acb7f10ef254c6a98eaf8b5988e01ef492c',
    'function Ivt(){if(process.env.CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC)'),
  growthBookProviderGate: nativeRange('provider, gateway and custom OAuth disable predicate', 181601954, 181602237,
    '7c491381d0b6eabd839834a9344348bf588aeaab3f091e73b7209e6adad985b7',
    'function u(){if(a.CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST)'),
  growthBookProviderIdentity: nativeRange('Native provider classification and first-party predicate', 180332633, 180333068,
    '12a78e3b9fe2afef9d10b53f90ca42582f02b958afb695776087c0938d3bb344',
    'function He(){if(go()||Q4t()||Z4t())'),
  growthBookGatewayAuth: nativeRange('Native gateway-auth presence predicate', 179315397, 179315457,
    'e9d6f3267ce2d54a5043627a0120b2b6c7bb5d486b23b54ae622c36312e7c39b',
    'function go(){return n().host.credentialSlots.gatewayAuth()}'),
  growthBookBaseEnabled: nativeRange('Native GrowthBook base enabled predicate', 182182552, 182182578,
    '1cd034bad94b05e88674ab7e1abde363f964a1f3baf9dfae7c2103556e052cc7',
    'function Kz(){return!Ih()}'),
  growthBookDependencies: nativeRange('Fu enabled and disk-cache-readable dependencies', 182189752, 182189903,
    '648fdcadd3707a05a510ddc4d3b1598055012efcfb6ad6c37c28645060174329',
    'function th(){return!a.DISABLE_GROWTHBOOK&&Kz()}'),
  growthBookDependencyWiring: nativeRange('Fu dependency construction and current-host owner', 182188260, 182188797,
    'a2f1195c3e8d064743039c55d93b4dab33ab5a94f157e3fbc0f43e3a227a7b1a',
    'function Ne(){let e=ui();'),
  growthBookPayloadUpdater: nativeRange('Native SDK payload to remoteEvalFeatureValues updater', 181839986, 181841899,
    'f848a85e6c29b4aeb26c7901962d9e9dfd02fd691e0ec697e44867aa6870b27b',
    'async processRemoteEvalPayload(e){'),
  growthBookDiskWriter: nativeRange('Native feature snapshot writer into global config', 181841899, 181842441,
    '5966aeb06af717d4089ad62de96a5134fd8d2dd6f6ee4fb33eecb390e4d733a8',
    'syncRemoteEvalToDisk(e){'),
  growthBookRemoteEvalRequest: nativeRange('Native remote evaluation request and SDK fallback', 181842552, 181843404,
    '7ad678fa323696e490c84a87cf81d447325e99dcff470bbe2908637919d96b33',
    'async requestRemoteEval(e){'),
  growthBookCreateClient: nativeRange('Native GrowthBook SDK client creation and initial payload load', 181843510, 181845518,
    'ed90daf5bc948ca2456c9b8a77fe0cc281e69d71678c41d81a917151c76d96ca',
    'async createClient(){'),
  growthBookInitialize: nativeRange('Native lazy initialization and periodic-refresh start', 181845518, 181845969,
    'ce2864e34b1e1cbbc1ee9ed4abf58962837f35d1703c77f47d5fa0b77b353c04',
    'initialize(){if(this.disposed)return Promise.resolve(null);'),
  growthBookBlockingGetter: nativeRange('Native blocking lookup starts lazy initialization', 181845969, 181846341,
    '2cd67afb0f3deab2af768f8088a45bdde3ce85dbdf0172b47475be6a2852fcf8',
    'async getFeatureValueBlocking(e,n){'),
  growthBookReset: nativeRange('Native GrowthBook reset clears accepted payload state', 181847787, 181848858,
    '2d03f9b15615977c89a64f3c4683494b97f5fa34668a65ecc9e12d84d5c2f788', 'reset(e){'),
  growthBookDispose: nativeRange('Native GrowthBook dispose unregisters SDK route', 181848858, 181849087,
    'd917fe55d45aa1b17adfe5a6fca5b0940dd7406e4de1923b7d10ea03c71b9af2', 'dispose(){'),
  growthBookRefresh: nativeRange('Native authenticated SDK refresh and payload replacement', 181849087, 181850371,
    'abc5e9cb5acb3d7d5d35182698fc94c6f5e32392bc8ad552bc202f8dc6541567',
    'async refreshFeatures(){'),
  growthBookRefreshAtisPin: nativeRange('Native attribute-triggered refresh', 181850371, 181850631,
    '0c148bb4fab9134599150c88691c0e44e2d31abd9a3559c792233f914a75f815', 'refreshAtisPin(){'),
  growthBookPeriodicRefresh: nativeRange('Native periodic feature refresh lifecycle', 181850631, 181851437,
    'f5aa178908a5922d85eef16f0d486b2fb6baabd1a956e8267ac739f5cd4e2c8f',
    'startPeriodicRefresh(){'),
  growthBookPeriodicStop: nativeRange('Native periodic refresh abort and listener removal', 181851437, 181851707,
    '43a588792f68910c96a8afaa85fc63fe87accb70bd39340e70f68bd3a2a7b66a',
    'stopPeriodicRefresh(){'),
  globalConfigFilePath: nativeRange('Native global Claude config file path selection', 179459495, 179459733,
    'ff65b45549fc64764bfa8f6e6205ea27a07c42a043583f6cb18b566dd1482e04',
    'function To(){if(se().existsSync(i(we(),".config.json")))'),
  globalConfigFileAccessor: nativeRange('Native memoized global Claude config file accessor', 179464376, 179464434,
    'd269f927afe6173ac3051158cabd7f7914ed74d9e5ed59c9b6c59ab758aaf838',
    'getGlobalClaudeFile(){return this.globalClaudeFile??=To()}'),
  globalConfigCacheReader: nativeRange('Native current global config cache reader', 182239514, 182239690,
    '3df4808eca365d5ff3e0f0fd53307d0d00ec4630955911c3e3d1905a7bd7ed50',
    'function le(){let e=ne();if(gn())'),
  globalConfigFileLoader: nativeRange('Native V5-or-global-file config loader', 182240110, 182240530,
    '9a4b0cbd1bb13201e3ea772dd998ab97eb8e90a23e2a154ce0fcb0875a5133f9',
    'async function Gzt(e){if(gn())'),
  globalConfigCacheMethod: nativeRange('GlobalConfig in-memory cache read method', 182213210, 182213309,
    '6608e6f53459b7c0a99f115a09c053985ed67d6c5089026e35ee94cc667efade',
    'readCache(){if(this.cache.config)'),
  globalConfigDefaultShape: nativeRange('Native global config default includes cachedGrowthBookFeatures', 182221210, 182221992,
    'f2010f3f6e9dd283e183e00ac4f08067cf4c0009e0707c7fdc3b2d3d7e808b15',
    'function qe(){return{numStartups:0'),
  globalConfigPersistedKeys: nativeRange('Native global-config persisted feature-cache keys', 182227939, 182228814,
    'b92e1fecc00153efc5ffa122279c6bf8802ea0d14617b736ca18368784e711e9',
    'var W0=new Set(['),
  growthBookInitKickCaller: nativeRange('Native app initializer conditionally supplies GrowthBook kick', 196842027, 196842209,
    'e6b3075214f846e94b21e3aee6362278a8b957330a918fd48c0a8587c452db96',
    'r=await Aur({showInvalidConfigDialog:n.showInvalidConfigDialog,storageV5EnvPin:n.storageV5EnvPin,startupGrowthBookKick:a.CLAUDE_CODE_GROWTHBOOK_KICK_FROM_INIT?()=>at("init"):void 0})'),
  growthBookInitKickInvocation: nativeRange('Initialization invokes optional GrowthBook kick after network setup', 195707515, 195708044,
    'f0e7d2cac6036f96034558ad04322428785db38cfa9d6b8958802fd0f44fe9a0',
    'Qr("init_network_configured"),C(),e.startupGrowthBookKick?.()'),
};

const consentInitStart = src.toolCallConsent.indexOf('async function eU(');
const consentInitEnd = src.toolCallConsent.indexOf('let he=new AbortController');
assert(consentInitStart === 0 && consentInitEnd > 0, 'eU consent guard source shape changed');

// Extracted `Fu.getFeatureValueWithSource` is installed in a minimal Native
// owner. The Native accessors and getter execute unchanged; only remote eval,
// disk config, process env, MCP session wiring and physical tool execution are
// fixture inputs.
const getterClassSource = `
class Fu {
  ${src.gbNonDefaultField}
  constructor(deps){
    this.deps=deps;
    this.experimentDataByFeature=new Map();
    this.nonDefaultFeatureKeys=new Set();
    this.remoteEvalFeatureValues=new Map();
  }
  ${src.gbEnvOverrideGetter}
  ${src.gbConfigOverrideGetter}
  ${src.gbHasNonDefault}
  logExposureForFeature(){}
  recordDeferredExposure(){}
  ${src.gbFeatureGetter}
  ${src.growthBookPayloadUpdater}
  ${src.growthBookDiskWriter}
}`;

const storeSources = `${src.zStore}
${src.sessionAndHostAccessors}
function b(){return {session:state.currentSession}}`;

const nativeRuntimeSource = `
${src.le}
${src.ss}
${src.envSchemaFactory}
${src.envProxy}
var Un=M.bool();
var _x={CLAUDE_CODE_PROJECTS_SESSION:Un};
var a=ant(_x,null);
${storeSources}
var el=new z(()=>({value:!1}));
class h { constructor(){this.cliOwnedConfigs=new WeakSet()} }
var T=new z(()=>new h);
${src.currentHostCliStateAccessor}
${src.hearthbotName}
${src.cliOwnedMark}
${src.cliOwnedPredicate}
${src.startupUrlEnv}
function R(){return void 0}
function y(){return []}
${src.startupPathsAndStore}
${src.startupOriginPredicate}
${src.hearthbotUrlPredicate}
${src.mcpClientPredicate}
${src.gbDu}
${src.gbDefaultHost}
${src.gbHostOwner}
function Els(){}
var IU=()=>{};
function th(){return state.gbEnabled}
function NU(){return state.diskReadable}
function vls(){return !1}
function Wi(){}
function mAn(){}
function ZVt(){}
function KU(){}
function drr(){}
function le(){return state.globalConfig}
function Ae(update,storage){
  const config=update(state.globalConfig);
  state.savedGlobalConfigWrites.push({config,storage});
  state.globalConfig=config;
}
class P extends Error{}
function c(value){state.growthBookWarnings.push(String(value?.message ?? value))}
function ZYo(){}
function Als(){return {intervalMs:1000,flagged:!1}}
function ee(){}
${getterClassSource}
${src.gbClientInitializer}
${src.gbIo}
${src.gbUse}
`;

const consentTail = src.toolCallConsent.slice(consentInitEnd);

function newRuntime({projectsEnv = undefined, startupUrl = 'https://bridge.example', clients = []} = {}) {
  const currentSession = {
    host: {},
    cliOwnedConfigs: new WeakSet(),
    mcpSessionWiring: {clientsFromAccessor: () => clients},
  };
  const state = {
    currentSession,
    defaultHost: {},
    gbEnabled: true,
    diskReadable: false,
    globalConfig: {cachedGrowthBookFeatures: {}},
    savedGlobalConfigWrites: [],
    growthBookWarnings: [],
    consentMessages: [],
    observedToolMessages: null,
  };
  const env = {};
  if (projectsEnv !== undefined || Object.prototype.hasOwnProperty.call(arguments[0] ?? {}, 'projectsEnv')) {
    env.CLAUDE_CODE_PROJECTS_SESSION = projectsEnv;
  }
  if (startupUrl !== undefined) env.SESSION_INGRESS_URL = startupUrl;
  const context = vm.createContext({
    AbortController,
    Buffer,
    Date,
    URL,
    process: {env},
    state,
    v: {host: state.defaultHost},
    // Imported session lookup is an app-owned boundary. The Native U/n/Is and
    // SS helpers remain source-extracted; b() supplies the current session.
    // Test-only W1 is defined below for the eU contract.
    NativeApiError: class NativeApiError extends Error {},
  });
  vm.runInContext(`${nativeRuntimeSource}
    ${src.nativeSsAccessor}
    ${src.gateSetup.slice(src.gateSetup.indexOf('var Tf='), src.gateSetup.indexOf('function NSn('))}
    ${src.gateSetup.slice(src.gateSetup.indexOf('function NSn('), src.gateSetup.indexOf('function Uoe('))}
    ${src.webFetchAlias.match(/^var _r="[^"]+"/)[0]};
    ${src.webSearchAlias.match(/^var Pm="[^"]+"/)[0]};
    ${src.projectsGate}
    this.native = {a, state, el, M_, Uoe, NSn, Sbt, gzt, OEe, nf, Is, SS, Ne, io, Use, ui, Fu, C, qS, Jkn, kc, Tf};
  `, context);
  return {context, state, native: context.native};
}

function serialize(value) {
  return JSON.parse(JSON.stringify(value));
}

function setGrowthBookFixture(runtime, {remote = {}, disk = {}, enabled = true, diskReadable = false} = {}) {
  const {context, state} = runtime;
  state.gbEnabled = enabled;
  state.diskReadable = diskReadable;
  state.globalConfig = {cachedGrowthBookFeatures: {...disk}};
  const client = context.native.Ne();
  client.remoteEvalFeatureValues.clear();
  for (const [key, value] of Object.entries(remote)) client.remoteEvalFeatureValues.set(key, value);
  return client;
}

function runNativeFeatureCase(id, options, expected) {
  const runtime = newRuntime({projectsEnv: 'true'});
  const client = setGrowthBookFixture(runtime, options);
  const actual = serialize(client.getFeatureValueWithSource(runtime.native.Tf, false));
  assert.deepEqual(actual, expected, id);
  return {id, result: actual};
}

const featureCases = [
  runNativeFeatureCase('remote-flag-value-is-payload',
    {remote: {bot_protected_tengu_atomic_magpie: true}}, {value: true, source: 'payload'}),
  runNativeFeatureCase('remote-map-miss-is-fallback',
    {remote: {other_feature: true}}, {value: false, source: 'fallback'}),
  runNativeFeatureCase('disk-cache-value-is-disk',
    {disk: {bot_protected_tengu_atomic_magpie: true}}, {value: true, source: 'disk'}),
  runNativeFeatureCase('disabled-without-readable-disk-is-disabled',
    {enabled: false, diskReadable: false}, {value: false, source: 'disabled'}),
  runNativeFeatureCase('no-remote-or-disk-value-is-fallback',
    {}, {value: false, source: 'fallback'}),
];

// Run the extracted Native SDK payload reducer and global-cache writer against
// an explicit SDK response fixture. This does not run SDK network or auth.
async function runNativePayloadUpdaterCase() {
  const runtime = newRuntime();
  const client = runtime.native.Ne();
  let sdkPayload = {features: {
    experiment_true: {
      value: true, source: 'experiment',
      experiment: {key: 'exp-projects'}, experimentResult: {variationId: 2},
    },
    explicit_false: {value: false, source: 'defaultValue'},
    null_value: {value: null, source: 'unknownFeature'},
    valueless: {source: 'payload'},
    primitive: true,
  }};
  const sdk = {
    getPayload: () => sdkPayload,
    setPayload: async payload => { sdkPayload = payload; return true; },
  };
  client.client = sdk;
  const accepted = await client.processRemoteEvalPayload(sdk);
  assert.equal(accepted, true);
  assert.deepEqual(Object.fromEntries(client.remoteEvalFeatureValues), {
    experiment_true: true,
    explicit_false: false,
    null_value: null,
  });
  assert.equal(sdkPayload.features.experiment_true.defaultValue, true,
    'Native normalizes value into SDK defaultValue before setPayload');
  assert.equal(client.nonDefaultFeatureKeys.has('experiment_true'), true);
  assert.equal(client.nonDefaultFeatureKeys.has('explicit_false'), false,
    'Native excludes source=defaultValue from non-default key tracking');
  assert.deepEqual(serialize(client.experimentDataByFeature.get('experiment_true')), {
    experimentId: 'exp-projects', variationId: 2,
  });
  assert.deepEqual(serialize(client.getFeatureValueWithSource('null_value', true)),
    {value: true, source: 'payload'},
    'a present remote map entry keeps payload source even when null falls back to default');
  assert.equal(runtime.state.growthBookWarnings.length, 2,
    'Native updater reports primitive and valueless entries but still adopts valid values');

  client.syncRemoteEvalToDisk(false);
  const persisted = runtime.state.savedGlobalConfigWrites.at(-1).config;
  assert.deepEqual(serialize(persisted.cachedGrowthBookFeatures), {
    experiment_true: true, explicit_false: false, null_value: null,
  });
  assert.deepEqual(serialize(persisted.cachedExperimentFeatures), ['experiment_true']);
  assert.deepEqual(serialize(persisted.cachedExperimentData.experiment_true), {
    experimentId: 'exp-projects', variationId: 2, value: true,
  });

  const emptyRuntime = newRuntime();
  const emptyClient = emptyRuntime.native.Ne();
  emptyClient.remoteEvalFeatureValues.set('old', true);
  emptyClient.client = {getPayload: () => ({features: {}}), setPayload: async () => true};
  const emptyAccepted = await emptyClient.processRemoteEvalPayload(emptyClient.client);
  assert.equal(emptyAccepted, false);
  assert.deepEqual(Object.fromEntries(emptyClient.remoteEvalFeatureValues), {old: true},
    'empty Native payload returns before replacing the previously accepted map');
  return {
    accepted,
    values: Object.fromEntries(client.remoteEvalFeatureValues),
    sourceForNull: serialize(client.getFeatureValueWithSource('null_value', true)),
    persistedFeatureKeys: persisted.cachedGrowthBookFeatures,
    emptyPayloadAccepted: emptyAccepted,
    oldMapAfterEmptyPayload: Object.fromEntries(emptyClient.remoteEvalFeatureValues),
  };
}

const remotePayloadUpdate = await runNativePayloadUpdaterCase();

const boolInputs = [
  {label: 'missing', projectsEnv: undefined, expected: false},
  {label: 'null', projectsEnv: null, expected: false},
  {label: 'empty', projectsEnv: '', expected: false},
  {label: 'false', projectsEnv: 'false', expected: false},
  {label: 'zero', projectsEnv: '0', expected: false},
  {label: 'no', projectsEnv: 'no', expected: false},
  {label: 'off', projectsEnv: 'off', expected: false},
  {label: 'unknown', projectsEnv: 'projects', expected: false},
  {label: 'one', projectsEnv: '1', expected: true},
  {label: 'true', projectsEnv: 'true', expected: true},
  {label: 'trimmed-yes', projectsEnv: ' YES ', expected: true},
  {label: 'on', projectsEnv: 'on', expected: true},
  {label: 'boolean-true', projectsEnv: true, expected: true},
  {label: 'boolean-false', projectsEnv: false, expected: false},
];
const environmentCases = boolInputs.map(({label, projectsEnv, expected}) => {
  const runtime = newRuntime({projectsEnv});
  const actual = runtime.native.a.CLAUDE_CODE_PROJECTS_SESSION;
  assert.equal(actual, expected, label);
  return {id: label, parsed: actual};
});

const projectsUrl = 'https://bridge.example/v1/code/mcp/hearthbot';
const validConfig = {name: 'hearthbot', config: {scope: 'dynamic', url: projectsUrl}};
const invalidUrlConfig = {name: 'hearthbot', config: {scope: 'dynamic', url: 'https://other.example/v1/code/mcp/hearthbot'}};
const mcpCases = [];
function runSbtCase(id, clients, expected, startupUrl = 'https://bridge.example') {
  const runtime = newRuntime({startupUrl, clients});
  const actual = runtime.native.Sbt(runtime.native.SS());
  assert.equal(actual, expected, id);
  mcpCases.push({id, accepted: actual});
  return runtime;
}
runSbtCase('matching-session-dynamic-hearthbot-client', [validConfig], true);
runSbtCase('no-current-session-clients', [], false);
runSbtCase('wrong-server-name', [{...validConfig, name: 'other'}], false);
runSbtCase('non-dynamic-scope', [{...validConfig, config: {...validConfig.config, scope: 'user'}}], false);
runSbtCase('startup-origin-mismatch', [invalidUrlConfig], false);
runSbtCase('missing-startup-url', [validConfig], false, null);
runSbtCase('query-on-url', [{...validConfig, config: {...validConfig.config, url: `${projectsUrl}?x=1`}}], false);
runSbtCase('empty-query-and-fragment-are-accepted', [
  {...validConfig, config: {...validConfig.config, url: `${projectsUrl}?#`}},
], true);
runSbtCase('opaque-file-origin-serializes-as-null', [
  {...validConfig, config: {...validConfig.config, url: 'file:///v1/code/mcp/hearthbot'}},
], true, 'file:///startup/path');
runSbtCase('different-opaque-schemes-share-null-origin', [
  {...validConfig, config: {...validConfig.config, url: 'data:/v1/code/mcp/hearthbot'}},
], true, 'file:///startup/path');
runSbtCase('websocket-target-normalizes-to-http-origin', [
  {...validConfig, config: {...validConfig.config, url: 'ws://bridge.example/v1/code/mcp/hearthbot'}},
], true, 'http://bridge.example/startup/path');
runSbtCase('secure-websocket-target-normalizes-to-https-origin', [
  {...validConfig, config: {...validConfig.config, url: 'wss://bridge.example/v1/code/mcp/hearthbot'}},
], true, 'https://bridge.example/startup/path');
runSbtCase('startup-websocket-origin-is-not-normalized', [
  {...validConfig, config: {...validConfig.config, url: 'wss://bridge.example/v1/code/mcp/hearthbot'}},
], false, 'wss://bridge.example/startup/path');
const cliRuntime = newRuntime({clients: [validConfig]});
cliRuntime.native.OEe(validConfig.config);
assert.equal(cliRuntime.native.Sbt(cliRuntime.native.SS()), false,
  'Native nf must exclude a config that OEe marked CLI-owned');
mcpCases.push({id: 'cli-owned-config-marked-by-OEe', accepted: false});

const remoteSignalRuntime = newRuntime({startupUrl: 'https://bridge.example'});
remoteSignalRuntime.native.NSn('hearthbot', validConfig.config);
assert.equal(remoteSignalRuntime.native.Is(remoteSignalRuntime.native.el).value, true,
  'NSn should set the host-scoped signal for a matching non-CLI hearthbot URL');
const rejectedRemoteSignalRuntime = newRuntime({startupUrl: 'https://bridge.example'});
rejectedRemoteSignalRuntime.native.NSn('hearthbot', invalidUrlConfig.config);
assert.equal(rejectedRemoteSignalRuntime.native.Is(rejectedRemoteSignalRuntime.native.el).value, false,
  'NSn must leave the host signal clear when the Projects URL predicate fails');
const cliRemoteSignalRuntime = newRuntime({startupUrl: 'https://bridge.example'});
cliRemoteSignalRuntime.native.OEe(validConfig.config);
cliRemoteSignalRuntime.native.NSn('hearthbot', validConfig.config);
assert.equal(cliRemoteSignalRuntime.native.Is(cliRemoteSignalRuntime.native.el).value, false,
  'NSn must leave the host signal clear for Native CLI-owned config');
const firstHostSignal = remoteSignalRuntime.native.Is(remoteSignalRuntime.native.el).value;
remoteSignalRuntime.state.currentSession = {
  host: {}, cliOwnedConfigs: new WeakSet(),
  mcpSessionWiring: {clientsFromAccessor: () => []},
};
assert.equal(remoteSignalRuntime.native.Is(remoteSignalRuntime.native.el).value, true,
  'Is(el) remains keyed to v.host while current session changes');
assert.equal(remoteSignalRuntime.native.Sbt(remoteSignalRuntime.native.SS()), false,
  'SS reads the new current session after session switch');

const growthBookOwnerRuntime = newRuntime({projectsEnv: 'true'});
const sessionA = growthBookOwnerRuntime.state.currentSession;
const clientA = growthBookOwnerRuntime.native.Ne();
assert.equal(growthBookOwnerRuntime.native.Use(), false,
  'a fresh per-session GrowthBook client starts with non-default-host false');
clientA.usedNonDefaultHost = true;
assert.equal(growthBookOwnerRuntime.native.Use(), true,
  'Use reads the current session host client history bit');
const sessionB = {
  host: {}, cliOwnedConfigs: new WeakSet(),
  mcpSessionWiring: {clientsFromAccessor: () => []},
};
growthBookOwnerRuntime.state.currentSession = sessionB;
const clientB = growthBookOwnerRuntime.native.Ne();
assert.notEqual(clientB, clientA, 'GrowthBook client store is keyed by current session host');
assert.equal(growthBookOwnerRuntime.native.Use(), false,
  'a different current session gets its own default host-history bit');
growthBookOwnerRuntime.state.currentSession = sessionA;
assert.equal(growthBookOwnerRuntime.native.Ne(), clientA);
assert.equal(growthBookOwnerRuntime.native.Use(), true,
  'returning to session A restores that session host client history bit');

const eligibleCases = [];
function runEligibilityCase(id, {toolName = 'WebFetch', projectsEnv, clients = [], setRemoteSignal = false,
  feature = {}, use = false, expected}) {
  const runtime = newRuntime({projectsEnv, clients});
  if (setRemoteSignal) runtime.native.NSn('hearthbot', validConfig.config);
  const client = setGrowthBookFixture(runtime, feature);
  client.usedNonDefaultHost = use;
  const actual = runtime.native.Uoe(toolName);
  assert.equal(actual, expected, id);
  eligibleCases.push({id, eligible: actual});
}
runEligibilityCase('other-tools-never-use-projects-consent-gate', {
  toolName: 'OtherTool', projectsEnv: 'true',
  feature: {remote: {bot_protected_tengu_atomic_magpie: true}}, expected: false,
});
runEligibilityCase('web-search-is-also-subject-to-projects-consent-gate', {
  toolName: 'WebSearch', projectsEnv: 'true', expected: true,
});
runEligibilityCase('projects-env-activates-gate-with-fallback-feature', {
  projectsEnv: 'true', expected: true,
});
runEligibilityCase('current-session-mcp-client-activates-gate', {
  clients: [validConfig], expected: true,
});
runEligibilityCase('host-scoped-remote-signal-activates-gate', {
  setRemoteSignal: true, expected: true,
});
runEligibilityCase('payload-true-default-growthbook-host-is-exception', {
  projectsEnv: 'true', feature: {remote: {bot_protected_tengu_atomic_magpie: true}}, expected: false,
});
runEligibilityCase('payload-true-nondefault-host-does-not-except', {
  projectsEnv: 'true', feature: {remote: {bot_protected_tengu_atomic_magpie: true}}, use: true, expected: true,
});
runEligibilityCase('disk-true-is-not-payload-exception', {
  projectsEnv: 'true', feature: {disk: {bot_protected_tengu_atomic_magpie: true}}, expected: true,
});

// Actual eU runs with a fake W1 generator. This verifies the Native consent
// argument boundary, not the physical permission system or any provider call.
const eUCases = [];
async function runConsentCase(id, consent, {projectsEnv, expectError, expectedErrorText, expectedMessages}) {
  const runtime = newRuntime({projectsEnv});
  const {context, state} = runtime;
  state.consentMessages = [];
  state.observedToolMessages = null;
  Object.assign(context, {
    _se: e => e.input ?? {},
    Ie: context.NativeApiError,
    Mx: () => false,
    t: () => undefined,
    Hu: () => ({tools: () => [{name: 'WebFetch'}], canUseTool: {testOnly: true}}),
    yn: (tools, name) => tools.find(tool => tool.name === name),
    av: () => () => undefined,
    $At: () => ({messages: [], taskRegistry: {}}),
    Re: message => {
      state.consentMessages.push(message.content);
      return {type: 'user', message: {content: message.content}, origin: message.origin};
    },
    hfo: (name, input) => ({type: 'tool_use', id: 'fixed-tool-use', name, input}),
    wc: message => ({type: 'assistant', uuid: 'fixed-virtual', message, isVirtual: true}),
    bfo: frame => frame,
    r5r: content => typeof content === 'string' ? content : 'ok',
    Aq: value => value,
    _fo: () => undefined,
    BAt: (_plugin, error) => { throw error; },
    W1: async function* (_toolUse, _assistant, _permission, toolContext) {
      state.observedToolMessages = [...toolContext.messages];
      yield {message: {content: [{type: 'tool_result', tool_use_id: 'fixed-tool-use', content: 'ok'}]}};
    },
  });
  vm.runInContext(`${src.toolCallConsent}\nthis.callTool=eU;`, context);
  let actual;
  try {
    actual = await context.callTool({tool: 'WebFetch', input: {}, ...(consent !== 'missing' ? {consent} : {})},
      {plugin: 'fixture-plugin'});
    if (expectError) assert.fail(`${id}: expected native error`);
  } catch (error) {
    if (!expectError) throw error;
    assert.match(String(error.message), expectedErrorText, id);
  }
  if (expectedMessages !== undefined) {
    const observed = state.observedToolMessages ?? [];
    assert.equal(observed.length, expectedMessages, `${id}: tool context message count`);
    if (expectedMessages > 0) {
      assert.equal(observed.at(-1).message.content, consent, `${id}: injected human consent value`);
    }
  }
  eUCases.push({
    id,
    rejected: expectError,
    observedConsentMessages: state.consentMessages,
    observedToolMessageCount: state.observedToolMessages?.length ?? 0,
    returnedText: actual?.text,
  });
}

await runConsentCase('missing-consent-projects-tool-runs-normal-permission', 'missing', {
  projectsEnv: 'true', expectedMessages: 0,
});
await runConsentCase('null-consent-fails-string-validation-before-projects-gate', null, {
  projectsEnv: 'true', expectError: true, expectedErrorText: /consent, when given, is a string/,
});
await runConsentCase('empty-consent-is-present-and-rejected-in-projects-session', '', {
  projectsEnv: 'true', expectError: true, expectedErrorText: /does not accept `consent` in a Projects session/,
});
await runConsentCase('empty-consent-is-injected-outside-projects-session', '', {
  projectsEnv: 'false', expectedMessages: 1,
});

const output = {
  version: '2.1.289',
  binary: {
    path: binaries[0],
    sha256: '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
    origin: 'Official Claude Code v2.1.289 macOS arm64 release asset; see docs/parity/mods-projects-consent-2.1.289.md',
  },
  sourceByteRanges: ranges,
  projectsEnvBoolCases: environmentCases,
  growthBookSourceCases: featureCases,
  growthBookPayloadUpdate: remotePayloadUpdate,
  sessionMcpPredicateCases: mcpCases,
  projectsEligibilityCases: eligibleCases,
  eUConsentCases: eUCases,
  hostSignalScope: {
    afterNativeRemoteConfigCall: firstHostSignal,
    afterSwitchingCurrentSession: remoteSignalRuntime.native.Is(remoteSignalRuntime.native.el).value,
    newSessionSbt: remoteSignalRuntime.native.Sbt(remoteSignalRuntime.native.SS()),
    rejectedNonProjectsConfig: rejectedRemoteSignalRuntime.native.Is(rejectedRemoteSignalRuntime.native.el).value,
    rejectedCliOwnedConfig: cliRemoteSignalRuntime.native.Is(cliRemoteSignalRuntime.native.el).value,
  },
  growthBookHostScope: {
    separateClientOnSessionSwitch: clientA !== clientB,
    originalSessionUseAfterSwitchBack: growthBookOwnerRuntime.native.Use(),
  },
  boundaries: {
    executedNative: [
      'Le/Ss, M.bool and ant parse CLAUDE_CODE_PROJECTS_SESSION using the Native typed environment path.',
      'Native z, U/n/Is, SS, OEe/nf/Kt, qS/Jkn, gzt/Sbt, NSn, Uoe, Fu source getter, io/Use, and the complete eU function execute from extracted 2.1.289 source.',
      'Native processRemoteEvalPayload and syncRemoteEvalToDisk execute against a fixture GrowthBook SDK client and a fake host global-config writer.',
      'The GrowthBook host-specific Ne/ui owner and Native eU consent checks execute with a test Native Fu instance and a fake W1 result generator.',
    ],
    injectedBoundaries: [
      'The active session object/client list and startup URL are test inputs; this does not run the Native MCP lifecycle or remote Projects-session producer.',
      'Fu is a minimal class carrying the exact extracted accessors/getFeatureValueWithSource method; its OAuth, network, disk writes and periodic refresh are not run.',
      'The SDK payload object and global-config save callback are fixture inputs; they do not execute Native SDK transport, real global-config file I/O, or V5 storage.',
      'The private GrowthBook usedNonDefaultHost bit is set directly in one truth-table case; the oracle does not simulate a non-default-host request or claim a production setter path.',
      'W1 is a fake generator that yields one successful tool_result; Native permission/tool hooks, network tools, user account and provider are not invoked.',
    ],
    sourceOnly: [
      'The two remote dynamic-config call sites show when NSn is called; they are pinned but the full remote MCP config loader/reconnect lifecycle is not executed.',
      'GrowthBook requestRemoteEval, createClient, initialize, refreshFeatures, and periodic-refresh lifecycle are source-pinned but not run; no live fetcher or credentials are supplied.',
      'Native Fu has source branches for override, but current getters return null/undefined and setters are no-ops; the override branch is not reachable from the ordinary client path in this pin.',
      'Desktop composition captures the startup ingress URL from SESSION_INGRESS_URL with ANTHROPIC_BASE_URL fallback and injects a pure ProjectsSessionHostContext; McpRegistry::new reads no environment, the active model api_base is not substituted, and a missing host context remains unknown.',
      'ProjectsSessionHostContext initializes the Native default-host latch to false. MobileConfig.projects_session_startup_url defaults to None and current Android/iOS FFI inputs do not populate it; mobile construction reads no environment and shares one context with the main and Local-App registries.',
      'No in-repository first-party remote dynamic-config ingress updates the sticky latch. The former bare record_remote_normalized_config function was removed; a generic dynamic-scope MCP connection does not supply Native remote-ingress provenance.',
      'Harness has no GrowthBook source-aware resolver. feature_result remains unknown rather than being fabricated as payload or fallback when the Projects-session gate is active.',
      'Desktop assembly marks the actual cli_mcp_servers configs cli_owned before registry ingestion. This fixes the earlier missing CLI ownership marker; it does not establish a first-party remote config ingress.',
    ],
  },
};

const json = JSON.stringify(output, null, 2) + '\n';
const fixture = new URL('../../docs/parity/mods-projects-consent-2.1.289.json', import.meta.url);
if (check) assert.equal(readFileSync(fixture, 'utf8'), json, '289 Projects consent oracle fixture differs');
else writeFileSync(fixture, json);
console.log(`${check ? 'checked' : 'wrote'} 2.1.289 Projects consent oracle: ${environmentCases.length} env values, ${featureCases.length} GrowthBook source cases, ${mcpCases.length} MCP predicates, ${eligibleCases.length} Uoe cases, ${eUCases.length} eU consent cases`);

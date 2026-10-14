#!/usr/bin/env node
// Source-pinned Native Claude Code 2.1.289 Agent launch/lifecycle probe.
// Usage:
//   node scripts/tests/mods_agent_lifecycle_289_probe.mjs <native-binary> [--write|--check]
//
// Extracted pure Native helpers run in a Node VM with controlled host seams.
// No app startup, provider, or network call is made. The Agent tool executor,
// task registry, team-file lock writer, and filesystem reads are injected.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';
import path from 'node:path';
import vm from 'node:vm';

const args = process.argv.slice(2);
const modes = args.filter(arg => arg.startsWith('--'));
assert(modes.length <= 1 && modes.every(arg => ['--write', '--check'].includes(arg)),
  'usage: node scripts/tests/mods_agent_lifecycle_289_probe.mjs <native-binary> [--write|--check]');
const binaryPaths = args.filter(arg => !arg.startsWith('--'));
assert.equal(binaryPaths.length, 1,
  'usage: node scripts/tests/mods_agent_lifecycle_289_probe.mjs <native-binary> [--write|--check]');
const mode = modes[0];
const binary = readFileSync(binaryPaths[0]);
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
assert.equal(sha256(binary),
  '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 macOS arm64 binary SHA-256');

const sourceWindows = [];
function take(label, start, end, expectedHash, startText) {
  assert(start >= 0 && end > start, `${label}: invalid source byte range`);
  const bytes = binary.subarray(start, end);
  assert.equal(bytes.length, end - start, `${label}: source range is short`);
  if (startText) {
    assert.equal(bytes.subarray(0, Buffer.byteLength(startText)).toString(), startText,
      `${label}: source start marker changed`);
  }
  assert.equal(sha256(bytes), expectedHash, `${label}: source hash changed`);
  sourceWindows.push({label, start, end_exclusive: end, sha256: expectedHash});
  return bytes.toString('utf8');
}

// Native $.agent API and AgentTool launch/settlement route.
const agentConstants = take('Agent tool constant module', 183186057, 183186439,
  'bf9ef56e8aa155f24c3d07df7efa5652fd7bd8644d05e1b77c7d5a5e6f98e78a', 'var ht="Agent"');
const workerAgentNameSet = take('N5 async-idle Agent tool-name set', 194759826, 194759847,
  'b115186b6407cf0232ff71738a113e2873a15743bc3ff8b00d65be213409c464', 'var _l=new Set([ht]);');
const agentWrapper = take('rw $.agent.list/spawn wrapper', 182844300, 182844999,
  '1f639d650c75229f9e9ebac6cf50ae7694bb4e6fea77fec2763158c8966b47a7', 'var rw=');
const modelResultHelpers = take('agent-spawn quota default and LAt projection', 188508125, 188508286,
  'cadbadbd8584f178c650fb910a96972bdff50e856f1822f45151794e1869a299', 'var ffo=20;');
const agentIdHelper = take('async_launched Agent id extractor', 188508499, 188508612,
  'ad60de1ed98d460840f58b1427f6bc00a15e9b526078b73548a8fbd73e1ec948', 'function _fo(');
const toolContextHelper = take('Agent tool API context projection', 188508612, 188508693,
  '728dd2ce55ef1c33361b9377a85efb109048e09ca338f7258e2c689e0db4e4ee', 'var $At=');
const resultMessageHelpers = take('virtual tool result row helpers', 188508693, 188508932,
  '89cc9adc39c72964386f8db2929af58a446c5dd19f324158373d0381c778c70e', 'function kfo(');
const promptModelHelper = take('Agent wrapper resolved model helper', 182844131, 182844217,
  'd5e529c1e38c148dd82169f5b3386ed096ffa039e2d3ad397d32047b36e79566', 'function ptr(');
const directSpawnEndpoint = take('direct agent.spawn Host API route', 188896338, 188896451,
  'f6a3fad95285deacb10c27fe5a386824309967a48edeef3e3ae89832b90e5310', '"agent.spawn":uLe(');
const toolCallRoute = take('$.tool.call shared eU route', 188892087, 188892605,
  '1a9ef07bed39f6a442c825693c0479491c19a33adb14a8b7893b4d1772d6fcb0', 'var l0t=');
const eUSource = take('eU virtual Agent execution and launch-only branch', 188853212, 188855653,
  'a8488d8e2775de457b6ff288249a54f1367ef3116b6f9b64b78863bedf22dd66', 'async function eU(');
const boTSource = take('BOt detached Agent settlement waiter', 188853103, 188853212,
  'caa26e4acdc771ea59d5ecfdc7ab29d7d447a55ce62bfaa6537b302aa5a7b0ad', 'function BOt(');
const yyoSource = take('Yyo per-plugin spawn quota lease', 188855653, 188856054,
  '871a5bd9cefdb815fd357ef4ada0687c23924b4e2c766ec4b8f166806e1ccf87', 'function Yyo(');
const settleSource = take('UOt task-registry settlement helper', 188856054, 188857230,
  '5cc59e9e5cb646dae8f99b544e87f369842ae2b5db725146ceb7b076374573bb', 'var pae=');
const teammateBridge = take('c_o teammate stable-id bridge', 188857631, 188858131,
  'f6999ad4718a1d17944ae61c5f02fc8d1f70eb16fe3b55f376fe7c4edb0c2e97', 'var VOt=');
const terminalPredicate = take('Native task terminal status predicate', 185139913, 185139979,
  '97b7f941c1c09455cb734a557cb33b37fcdd82297be2af30b4d77296dd6ccbcd', 'function Rs(');
const agentListModelBridge = teammateBridge;
const nativeModelAliasSet = take('Native model aliases and 1m suffix helper', 179588224, 179588545,
  '20d559fcbc34c301f5f2ddd6a6a09e5ee9affa43e71625890d9e73fe42546340', 'RW=[');
const nativeAtModelResolver = take('Native At(model) canonicalizer used by VOt', 182058341, 182058885,
  '2c89875db104852dc30d6bdcc8746bd27ba8fe89098abd42d4dea745d44a50ae', 'function At(e){');
const nativeLegacyRemap = take('Native legacy Opus IDs and remap flag', 182059234, 182059432,
  '8dacfaf870f77868a191a159fbf64080d1101b8e4eff032b6d645a341229b904', 'var oN=[');
const nativeDefaultModelHelpers = take('Native configurable default model selectors', 182034000, 182035355,
  '6504246fb40bcc2967d1ec59b47e6c3dc7fdbbbe918b31fbc073018ec276bb9b', 'var UI={firstParty:"first_party"');
const nativeFable51Record = take('embedded Fable 5.1 model catalog record', 180321533, 180322597,
  '1aa57accbbe4542ee0cdbeb268a309968f7804ac82fa7a1fac7f98fb853e8789', '{id:"claude-fable-5-1"');
const nativeFable5Record = take('embedded Fable 5 model catalog record', 180320565, 180321532,
  '128508c0ed612a156f58caf41ed94ad8ebbf4f9863300e0c60d659ce40080428', '{id:"claude-fable-5"');
const nativeFableAliasEntry = take('embedded Fable alias catalog entry', 180324304, 180324378,
  '9b81701cb225f228ae81148601d52b486008de8d820e3b9b450662f73b98b007',
  'fable:{default:"claude-fable-5-1"');
const nativeCatalogAliasResolver = take('Native catalog alias provider selector QWe', 180328974, 180329131,
  '776928835e74d1e4a4324635b00e6f0d32cbd50f41b2899fcf41c887c58df8df', 'function QWe(e,t){');
const nativeCatalogProviderIdMapper = take('Native catalog provider ID mapper D', 180329878, 180330236,
  'c83cd89310ff7e65b796e506d3ac699e29981ac568623fde8364585d843c6f83', 'function D(e){');
const nativeBestModelHelper = take('Native best-alias dynamic selector', 182029166, 182029364,
  'e0a5ee06a2e39bebd4631b2ae437a633f51fab55d640db6ca408e0a37a522cfe', 'function kI(){');
const nativeOneMinuteDisabledFlag = take('Native disable-1m-context flag reader', 182063933, 182063987,
  'e3564c21fea9fc724c50317934f506fabcd8a61df2aec800660b2f2371f1bab8', 'function _I(){');
const nativeOneMinuteGate = take('Native [1m] model-context gate', 182063987, 182064540,
  '4d474daa66af006303903b33d831d4bb84d0f34baa7da85a619102cfbbc4c057', 'function Xd(e){');
const nativeFablePredicate = take('Native fable-family predicate', 182033873, 182033929,
  '5efb4eec5e22422e7fc49868feb8f06e5693509c305b6f75d2fdec5a9809d4ef', 'function FI(e){');
const nativeFirstPartyPredicate = take('Native provider-first-party predicate', 180333359, 180333425,
  'de840b41441378c63a741c9a8c41b484c8ea6149481502b001d3b7ddf8f074bf', 'function ad(');
const nativeIdentityModelMapper = take('Native identity model mapper', 181702158, 181702182,
  '6de1c795288a69a41d3687906d55ec6eabd18bf6ad34eedd0a5177a15ff2ded3', 'function lA(e){');
const nativeOneMinuteSuffixBuilder = take('Native 1m suffix builder', 182037196, 182037253,
  '6a671212aca2a2b68db48030c2be9c02fc125ccdc98c56e3dce359d3eff17dd0', 'function e$(');

assert.match(agentConstants, /var ht="Agent"/);
assert.match(workerAgentNameSet, /new Set\(\[ht\]\)/);
assert.match(modelResultHelpers, /var ffo=20;function pVn\(\)\{return a\.CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS\?\?ffo\}/);
assert.match(directSpawnEndpoint, /e\.budgets\.spawns\.spawn\(n,r\)/);
assert.match(directSpawnEndpoint, /c_o\(/);
assert.match(toolCallRoute, /run:async\(e,n\)=>q4r\(await eU\(e,n\)\)/);
assert.match(eUSource, /W1\(Se,ve,B\.canUseTool,ke/);
assert.match(eUSource, /BOt\(\{registry:ke\.taskRegistry,agentId:qe,pluginName:w\}\)/);
assert.match(eUSource, /n\.leftRunning\?\.\(Ze\.then\(\(\)=>\{return\}/);
assert.match(eUSource, /\{result:LAt\(qe,ptr\(Pe\)\),text:""\}/);
assert.match(boTSource, /signal:new AbortController\(\)\.signal,settleWaitMs:Number\.POSITIVE_INFINITY/);
assert.match(yyoSource, /b>=w/);
assert.match(yyoSource, /K\.finally\(M\)/);
assert.match(teammateBridge, /status==="teammate_spawned"/);
assert.match(terminalPredicate, /completed.*failed.*killed/);
assert.match(agentListModelBridge, /resolvedModel:At\(e\.model\)/);
assert.match(nativeAtModelResolver, /case"opusplan":return s\?lA\(e\$\(Lm\(\)\)\):Lm\(\)/);
assert.match(nativeAtModelResolver, /if\(ad\(\)&&T2e\(g\)&&ibt\(\)\)/);
assert.match(nativeLegacyRemap, /CLAUDE_CODE_DISABLE_LEGACY_MODEL_REMAP/);
assert.match(nativeDefaultModelHelpers, /function hm\(\).*ANTHROPIC_DEFAULT_OPUS_MODEL/);
assert.match(nativeDefaultModelHelpers, /function Lm\(\).*ANTHROPIC_DEFAULT_SONNET_MODEL/);
assert(nativeDefaultModelHelpers.includes('function k0e(){let e=a.ANTHROPIC_DEFAULT_FABLE_MODEL;return lA(e!==void 0?qb(e):Wd())}'));
assert(nativeDefaultModelHelpers.includes('function Wd(e=jS()){let n=pa("fable",e)??e.fable51;return qb(n)}'));
assert(nativeDefaultModelHelpers.includes('function pa(e,n,r=He()){let s=Yb(e,r);return s!==void 0?n[s]:void 0}'));
assert(nativeDefaultModelHelpers.includes('function Yb(e,n=He()){let r=QWe(e,UI[n]);if(r===void 0)return;let s=La(r)?.provider_ids.first_party;return s!==void 0?aB[s]:void 0}'));
assert(nativeCatalogProviderIdMapper.includes('firstParty:t.first_party,bedrock:t.bedrock??null,vertex:t.vertex??null,foundry:t.foundry??null,anthropicAws:t.anthropic_aws??null,anthropicGoogleCloud:t.anthropic_google_cloud??null,mantle:t.mantle??null,gateway:t.gateway??t.first_party'));
assert.match(nativeOneMinuteDisabledFlag, /CLAUDE_CODE_DISABLE_1M_CONTEXT/);
assert.match(nativeOneMinuteGate, /function Xd\(e\).*lbt\(e\)/);
assert.match(nativeOneMinuteSuffixBuilder, /replace\(\/\(\\\[1m\\\]\)\+\$\/i,""\)\+"\[1m\]"/);

// Execute Native QWe against the alias literal and model records extracted from
// the embedded source catalog. Only the lookup wrapper is supplied; the
// aliases, resolver, and provider IDs below are Native bytes, not catalog
// fixtures or user-setting defaults.
const embeddedFableAlias = vm.runInNewContext(`({${nativeFableAliasEntry}}).fable`);
const embeddedFable51 = vm.runInNewContext(`(${nativeFable51Record})`);
const embeddedFable5 = vm.runInNewContext(`(${nativeFable5Record})`);
const catalogAliasContext = vm.createContext({
  Object,
  TW: () => ({aliases: {fable: embeddedFableAlias}}),
});
vm.runInContext(nativeCatalogAliasResolver, catalogAliasContext);
const fableProviderAliasVectors = [
  ['first_party', 'claude-fable-5-1'],
  ['bedrock', 'claude-fable-5-1'],
  ['vertex', 'claude-fable-5-1'],
  ['foundry', 'claude-fable-5-1'],
  ['anthropic_aws', 'claude-fable-5-1'],
  ['anthropic_google_cloud', 'claude-fable-5-1'],
  ['mantle', 'claude-fable-5-1'],
  ['gateway', 'claude-fable-5'],
].map(([provider, expected]) => {
  const actual = catalogAliasContext.QWe('fable', provider);
  assert.equal(actual, expected, `embedded Native Fable alias for ${provider}`);
  return {provider, actual, expected};
});
assert.deepEqual(JSON.parse(JSON.stringify(embeddedFable51.provider_ids)), {
  first_party: 'claude-fable-5-1',
  bedrock: 'us.anthropic.claude-fable-5-1',
  vertex: 'claude-fable-5-1',
  foundry: 'claude-fable-5-1',
  anthropic_aws: 'claude-fable-5-1',
  anthropic_google_cloud: 'claude-fable-5-1',
  mantle: 'anthropic.claude-fable-5-1',
  gateway: 'claude-fable-5-1',
});
assert.equal(embeddedFable5.provider_ids.gateway, 'claude-fable-5');
const nativeFableCatalogProjection = {
  alias_vectors: fableProviderAliasVectors,
  selected_catalog_rows: {
    default_fable_alias: embeddedFable51.id,
    gateway_fable_alias: embeddedFable5.id,
  },
  fable_5_1_provider_ids: embeddedFable51.provider_ids,
  fable_5_gateway_id: embeddedFable5.provider_ids.gateway,
  selector_path: 'k0e missing env -> Wd -> pa("fable", current model map) ?? current map fable51',
  runtime_provider_cache_and_model_overrides_executed: false,
};

// Execute the exact Native At(model) function together with its source-pinned
// alias/default/suffix/remap helpers. Provider defaults and the best-model
// catalog entry are injected so the probe covers Native selection branches,
// not a particular machine's user settings or model catalog.
let modelProvider = 'firstParty';
const bestModelState = {resolvingBestModel: false};
const modelContext = vm.createContext({
  a: {
    ANTHROPIC_DEFAULT_OPUS_MODEL: 'native-opus-default',
    ANTHROPIC_DEFAULT_SONNET_MODEL: 'native-sonnet-default',
    ANTHROPIC_DEFAULT_HAIKU_MODEL: 'native-haiku-default',
    ANTHROPIC_DEFAULT_FABLE_MODEL: 'native-fable-default',
    CLAUDE_CODE_DISABLE_LEGACY_MODEL_REMAP: false,
    CLAUDE_CODE_DISABLE_1M_CONTEXT: false,
  },
  He: () => modelProvider,
  h$: () => false,
  nO: value => value,
  Ya: () => false,
  ia: {firstParty: {defaultModel: () => 'native-best-from-catalog'}},
  Wb: () => 'firstParty',
  Hr: () => bestModelState,
  Ur: () => true,
});
vm.runInContext(
  nativeModelAliasSet + nativeAtModelResolver + nativeLegacyRemap +
    nativeDefaultModelHelpers + nativeBestModelHelper + nativeOneMinuteDisabledFlag + nativeOneMinuteGate +
    nativeFablePredicate + nativeFirstPartyPredicate + nativeIdentityModelMapper +
    nativeOneMinuteSuffixBuilder + agentListModelBridge,
  modelContext,
);
const modelVectors = [];
function checkNativeModel(input, expected, label) {
  const actual = modelContext.At(input);
  assert.equal(actual, expected, label);
  modelVectors.push({input, actual, expected, label});
}
checkNativeModel('  CuStOm-ClAuDe-MoDeL  ', 'CuStOm-ClAuDe-MoDeL', 'trim but preserve non-alias casing');
checkNativeModel('FABLE', 'native-fable-default', 'Fable alias resolves through configured default');
checkNativeModel('OPUS', 'native-opus-default', 'case-insensitive Opus alias');
checkNativeModel('SoNnEt', 'native-sonnet-default', 'case-insensitive Sonnet alias');
checkNativeModel('HAIKU', 'native-haiku-default', 'case-insensitive Haiku alias');
checkNativeModel('opusplan', 'native-sonnet-default', 'opusplan chooses Sonnet outside plan-mode runtime');
checkNativeModel('BEST[1m]', 'native-best-from-catalog', 'best alias ignores explicit 1m suffix');
checkNativeModel('OPUS[1M]', 'native-opus-default[1m]', '1m Opus alias resolves then restores suffix');
checkNativeModel('CLAUDE-OPUS-4-1-20250805', 'native-opus-default', 'first-party legacy Opus remap enabled');
modelContext.a.CLAUDE_CODE_DISABLE_LEGACY_MODEL_REMAP = true;
checkNativeModel('CLAUDE-OPUS-4-1-20250805', 'CLAUDE-OPUS-4-1-20250805', 'legacy remap disable flag preserves explicit id');
modelContext.a.CLAUDE_CODE_DISABLE_LEGACY_MODEL_REMAP = false;
modelProvider = 'bedrock';
checkNativeModel('CLAUDE-OPUS-4-1-20250805', 'CLAUDE-OPUS-4-1-20250805', 'legacy remap is first-party-only');
modelProvider = 'firstParty';
modelContext.a.CLAUDE_CODE_DISABLE_1M_CONTEXT = true;
checkNativeModel('OPUS[1m]', 'OPUS[1m]', 'disabled 1m context leaves suffixed alias spelling untouched');
modelContext.a.CLAUDE_CODE_DISABLE_1M_CONTEXT = false;
checkNativeModel('Custom-Model[2m]', 'Custom-Model[2m]', 'At does not strip or reinterpret 2m suffix');
modelContext.Ya = () => true;
modelContext.cw = () => true;
checkNativeModel('Claude-Fable-4-6[1m]', 'Claude-Fable-4-6', 'enabled native 1m Fable context removes explicit suffix');
modelContext.cw = () => false;
checkNativeModel('Claude-Fable-4-6[1m]', 'Claude-Fable-4-6[1m]', 'unsupported native 1m Fable context keeps suffix');
modelContext.Ya = () => false;
modelContext.cw = () => false;
const modelBridgeInput = {model: 'OPUS[1M]', status: 'teammate_spawned', teammate_id: 'worker@team'};
const modelBridgeResult = modelContext.VOt(modelBridgeInput, 'loop-id');
assert.deepEqual(JSON.parse(JSON.stringify(modelBridgeResult)), {
  model: 'OPUS[1M]', status: 'teammate_spawned', teammate_id: 'worker@team',
  agentId: 'loop-id', resolvedModel: 'native-opus-default[1m]',
});
const nativeAgentModelProjection = {
  vectors: modelVectors,
  voT_model_rewrite: modelBridgeResult,
  configured_defaults: {
    opus: modelContext.a.ANTHROPIC_DEFAULT_OPUS_MODEL,
    sonnet: modelContext.a.ANTHROPIC_DEFAULT_SONNET_MODEL,
    haiku: modelContext.a.ANTHROPIC_DEFAULT_HAIKU_MODEL,
    fable: modelContext.a.ANTHROPIC_DEFAULT_FABLE_MODEL,
    best_model_catalog_stub: 'native-best-from-catalog',
  },
  live_provider_catalog_executed: false,
};

// The exact API wrapper, Yyo, eU, BOt, and UOt are run with a fake W1 Agent
// result and a mutable task registry. The wrapper/executor are Native source;
// only tool catalog, W1/tool permissions, transcript, clock, and registry are
// supplied. In particular this does not claim to execute an actual Agent tool.
const row = {id: 'child-id', type: 'local_agent', agentId: 'child-id', status: 'running'};
const taskRegistry = {
  all: () => ({child: row}),
  get: id => id === 'child-id' ? row : undefined,
  getTranscript: () => ({messages: []}),
};
const controllerInstances = [];
class TrackedAbortController extends AbortController {
  constructor() {
    super();
    controllerInstances.push(this);
  }
}
const polledIntervals = [];
const linkedSignals = [];
const w1Calls = [];
const agentTool = {name: 'Agent'};
const toolContext = {
  tools: () => [agentTool],
  canUseTool: async () => ({behavior: 'allow'}),
  toolContext: controller => ({taskRegistry, signal: controller.signal, messages: []}),
};
const launchContext = vm.createContext({
  AbortController: TrackedAbortController,
  Date,
  Promise,
  Map,
  Set,
  Object,
  String,
  Number,
  N: value => value !== null && typeof value === 'object' && !Array.isArray(value),
  Error,
  Ie: Error,
  a: {CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS: 1},
  kK: 1000,
  t: () => {},
  l: error => String(error),
  _se: input => {
    const {tool: _tool, consent: _consent, ...rest} = input;
    return rest;
  },
  Hu: () => toolContext,
  Mx: () => false,
  yn: (tools, name) => tools.find(tool => tool.name === name),
  Uoe: () => false,
  BAt: (_plugin, error) => { throw error; },
  av: (outerSignal, controller) => {
    linkedSignals.push({outer: outerSignal, inner: controller.signal});
    return () => {};
  },
  hfo: (name, input) => ({id: 'virtual-agent-tool-use', name, input}),
  wc: value => value,
  W1: async function* (toolUse, virtualAssistant, canUseTool, contextValue) {
    w1Calls.push({
      toolUseName: toolUse.name,
      isVirtual: virtualAssistant.isVirtual === true,
      contextSignalAborted: contextValue.signal.aborted,
      catalogToolName: agentTool.name,
      permissionExecutorInjected: typeof canUseTool === 'function',
    });
    yield {
      message: {
        type: 'user',
        message: {content: [{
          type: 'tool_result',
          tool_use_id: toolUse.id,
          content: 'launched',
          is_error: false,
        }]},
        toolUseResult: {
          status: 'async_launched',
          agentId: 'child-id',
          resolvedModel: 'model-x',
        },
      },
    };
  },
  r5r: value => typeof value === 'string' ? value : '',
  Aq: value => value,
  K3: () => {},
  GOt: () => ({text: '', isError: true}),
  ee: async milliseconds => {
    polledIntervals.push(milliseconds);
    await new Promise(resolve => setTimeout(resolve, 1));
  },
});
vm.runInContext(
  agentConstants +
    modelResultHelpers + promptModelHelper + agentIdHelper + toolContextHelper +
    resultMessageHelpers + terminalPredicate + settleSource + boTSource + eUSource + yyoSource,
  launchContext,
);

const callerController = new TrackedAbortController();
const spawnApi = launchContext.Yyo(launchContext.eU);
const firstSpawn = await spawnApi.spawn(
  {tool: 'Agent', prompt: 'start a child'},
  {plugin: 'plugin-one', signal: callerController.signal, origin: ['plugin-one']},
);
assert.deepEqual(JSON.parse(JSON.stringify(firstSpawn)), {
  result: {agentId: 'child-id', resolvedModel: 'model-x'},
  text: '',
});
assert.equal(row.status, 'running', 'launch API returns before child task settles');
assert.equal(w1Calls.length, 1);
assert.deepEqual(w1Calls[0], {
  toolUseName: 'Agent', isVirtual: true, contextSignalAborted: false,
  catalogToolName: 'Agent', permissionExecutorInjected: true,
});
assert.equal(linkedSignals[0].outer, callerController.signal,
  'the caller context signal is linked while W1 admits/starts the Agent tool');
assert.notEqual(controllerInstances.at(-1).signal, callerController.signal,
  'BOt creates an independent signal for post-launch settlement');
assert.equal(controllerInstances.at(-1).signal.aborted, false);
const configuredConcurrencyLimit = launchContext.pfo();
launchContext.a.CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS = undefined;
const defaultConcurrencyLimit = launchContext.pfo();
launchContext.a.CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS = 1;
assert.equal(defaultConcurrencyLimit, 20, 'Native default concurrent Agent spawn limit');
assert.equal(configuredConcurrencyLimit, 1, 'test overrides the Native environment cap');
let quotaRefusal;
try {
  await spawnApi.spawn({tool: 'Agent', prompt: 'second'}, {
    plugin: 'plugin-one', signal: callerController.signal, origin: ['plugin-one'],
  });
} catch (error) {
  quotaRefusal = error.message;
}
assert.equal(quotaRefusal, 'plugin-one: $.agent.spawn refused: 1 spawns are running at once');
assert.equal(w1Calls.length, 1, 'the per-plugin cap refuses before invoking the Native tool executor');
row.status = 'completed';
await new Promise(resolve => setTimeout(resolve, 15));
assert(polledIntervals.every(interval => interval === 150));
const afterSettle = await spawnApi.spawn(
  {tool: 'Agent', prompt: 'start after settlement'},
  {plugin: 'plugin-one', signal: callerController.signal, origin: ['plugin-one']},
);
assert.deepEqual(JSON.parse(JSON.stringify(afterSettle)), {
  result: {agentId: 'child-id', resolvedModel: 'model-x'}, text: '',
});
assert.equal(w1Calls.length, 2, 'executor runs again after the detached lease is released');
const launchProbe = {
  default_limit: 20,
  test_environment_limit: configuredConcurrencyLimit,
  immediate_result: firstSpawn,
  row_status_at_return: 'running',
  w1_virtual_tool_call: w1Calls[0],
  caller_signal_linked_during_tool_execution: linkedSignals[0].outer === callerController.signal,
  detached_settlement_signal_is_fresh: controllerInstances.at(-1).signal !== callerController.signal,
  quota_refusal_while_left_running: quotaRefusal,
  detached_settlement_poll_ms: [...new Set(polledIntervals)],
  same_plugin_accepted_after_terminal_row: afterSettle.result.agentId === 'child-id',
  runner_or_tool_catalog_executed: false,
};

// Exact local-Agent lifecycle sources and N5's actual assistant/user row
// handler. A complete assistant row is processed in one pass before Xt runs.
const localTaskConstructor = take('local Agent row initialization bJ', 191176717, 191177565,
  'c5ad60d5cc38f5a90ab6d79f4be405b40c54575d2b5b21e842825b50831cff9e', 'function bJ(');
const n5 = take('N5 local Agent query-event runner', 194760296, 194770594,
  'c3fbc7491ba06e1c1b4297004f310679fcf8df1eecb622159bcff42160783021', 'function N5(');
const n5Tracker = take('N5 outstanding tool-use / Agent-only idle expression', 194761173, 194761293,
  'de2ccf8c903f06142348454c66da950bcb66b5eadb4e80f394dd4c47d25bc6c3', 'Me=new Set,ft=new Set');
const n5RowBranch = take('N5 completed assistant/user row tool-use updates', 194764875, 194765302,
  'b313f4db1c32c2968e3c7f77813a8a9cffda4e943fd3ec5c157771638f6305ef', 'let Ne=!1;');
const n5McCaller = take('local Agent N5 makeStream feeds MC query events', 194853818, 194854211,
  '86f17195b67dbb036fbf15a499e6823cb9dc03d02ba18c507a081c23be72eb4f', 'let ln=N5(');
const w1ToolStart = take('W1 marks tool call in-progress after call-start', 188837912, 188838111,
  '07a14997feab7a9a2d76f4b23a4e6d3d9af60979f7c9623c4752b8527db2ca69', 's.markToolCallStarted?.(n)');
const w1ResultRemoval = take('serial W1 wrappers remove IDs after result yield', 194678722, 194679364,
  'c0031ad9213a5845a06f348cb70703140f054847f59a308d51990562ca33cb43', 'async function*ua(');
const localIdleContext = vm.createContext({
  Map,
  Set,
  Object,
  Date,
  b: {
    update: (_id, reducer) => { localAgentRow = reducer(localAgentRow); },
    updateTranscript: () => {},
  },
  e: 'local-agent',
  rt: undefined,
  st: 0,
  Te: undefined,
  He: undefined,
  St: undefined,
});
let localAgentRow = {isIdle: false};
vm.runInContext(agentConstants, localIdleContext);
vm.runInContext(workerAgentNameSet, localIdleContext);
vm.runInContext(`let ${n5Tracker};`, localIdleContext);
vm.runInContext(`function applyN5Row(le) { ${n5RowBranch} Xt(); return {row: {...localRow()}, outstanding: [...Me].sort(), agentUses: [...ft].sort()}; }
  function localRow() { return globalThis.readLocalRow(); }`, localIdleContext);
localIdleContext.readLocalRow = () => localAgentRow;
const rowSnapshots = [];
for (const message of [
  {type: 'assistant', message: {id: 'm1', content: [{type: 'tool_use', id: 'agent-1', name: 'Agent'}]}},
  {type: 'assistant', message: {id: 'm2', content: [
    {type: 'tool_use', id: 'agent-2', name: 'Agent'},
    {type: 'tool_use', id: 'read-1', name: 'Read'},
  ]}},
  {type: 'user', message: {content: [{type: 'tool_result', tool_use_id: 'read-1'}]}},
  {type: 'user', message: {content: [{type: 'tool_result', tool_use_id: 'agent-1'}]}},
  {type: 'user', message: {content: [{type: 'tool_result', tool_use_id: 'agent-2'}]}},
]) {
  const value = localIdleContext.applyN5Row(message);
  rowSnapshots.push(JSON.parse(JSON.stringify(value)));
}
assert.deepEqual(rowSnapshots.map(snapshot => snapshot.row.isIdle), [true, false, true, true, false]);
assert.deepEqual(rowSnapshots[1].outstanding, ['agent-1', 'agent-2', 'read-1']);
assert.deepEqual(rowSnapshots[1].agentUses, ['agent-1', 'agent-2']);
assert.match(n5, /Me\.add\(ke\.id\),_l\.has\(ke\.name\)\)ft\.add\(ke\.id\)/);
assert.match(n5, /if\(le\.type==="set_in_progress_tool_use_ids"\)/);
assert.match(n5, /le\.reason==="fallback_sweep"/);
assert.match(n5, /se\.push\(le\),b\.updateTranscript/);
assert.match(n5McCaller, /makeStream:\(A,z,ae\)=>MC\(/);
assert.match(w1ToolStart, /set_in_progress_tool_use_ids.*action:"add"/);
assert.match(w1ResultRemoval, /action:"remove"/);

// Exact local completion facts: bJ initializes only isIdle=false; N5 changes
// it from query rows. N5 passes finalizing=true after the row is completed and
// always clears it in finally. Its finalizingPass local is initialized to
// undefined and never assigned in this source window.
const emr = take('EMr local terminal/finalizing update', 191175230, 191176232,
  '9c70d273542cf503d4e25e12bec5ea203a086ad33161c575f29045a51666ecec', 'function EMr(');
const kho = take('kHo clears finalizing after completion sequence', 191176232, 191176367,
  '5e3643b9197b3fa4a06280502c660b0650f1fa461729eb630222b73c219fa4a1', 'function kHo(');
const n5FinalizingCall = take('N5 normal-success finalizing call', 194766535, 194766900,
  'ff76763788a2d77f87db917628f24ebecac196ad6e8e856c09fec8490e36fdda', 'ackInterim:yt});if(Q=');
assert.match(localTaskConstructor, /isIdle:!1/);
assert.match(localTaskConstructor, /keepaliveReasons:new Set/);
assert.match(n5, /let D=void 0/);
assert.equal([...n5.matchAll(/\bD\s*=/g)].length, 1, 'N5 finalizingPass local has no assignment');
assert.match(n5FinalizingCall, /EMr\(Ie,b,/);
assert.match(n5FinalizingCall, /finalizing:!0,finalizingPass:D/);
assert.match(emr, /status:"completed"/);
assert.match(emr, /finalizing:g\?\.finalizing\?\?!1/);
assert.match(kho, /return s=!0,\{\.\.\.g,finalizing:!1\}/);

// Per-reason keepalive sources. The VM only executes the helper and controlled
// identity registry; caller spans identify when actual Native subsystems add
// and remove their one reason.
const keepaliveHelpers = take('YP/hx single-reason Set helpers', 185181785, 185182220,
  '5afaa202299fcb45a856631a642f21b9c48dc42c44eac2d579969e15afe76413', 'function YP(');
const idleWindowCleanup = take('Iwr idle-window timer and owner cleanup', 191165306, 191165588,
  '74f503538d17fc5d45c509c05957f0731d6a1911ff483eb591b6741f11f7d410', 'function Iwr(');
const ownedChildCleanup = take('Yq task owner keepalive cleanup predicate', 191166682, 191167081,
  'f642cb79e37b7f633ee77002f33fe1696bb646e212d72d969e15d20642f2540c', 'function Yq(');
const killUnsettledProducer = take('kill-unsettled reason producer', 191172580, 191172780,
  '499c8779afd8282fbb4e8b7e511576c795d710b8ef38f8df925f2cc03fa7d377', 'rtController:void 0');
const bashReasonProducer = take('background Bash keepalive producer', 191201280, 191201365,
  '39e596560429c552047532ff66cb0bf0e3bf188c9ce2c0d1bd2afe7af436ce12', '),c(s)}}functio');
const monitorWebsocketReasonProducer = take('monitor websocket keepalive producer', 194299360, 194299500,
  'e5c72ba48c02e4ef616512457c4744677fbb2c2219d34064d5796605b8493286', 'eLive:{...e.frameLive,armedAt:Date.now()}}};');
const monitorCommandReasonProducer = take('monitor command keepalive producer', 194303910, 194304045,
  'efdcab152939058b90612c235771cd9b96aa00f9546d59e058540f09e2e23ad6', 'ind:"monitor"');
const workflowReasonProducer = take('workflow keepalive producer', 200084060, 200084180,
  'dfdd6030ebe0647ed4f98c4864d8d455f1b3bf655c4c427e8c815ef37543ab1f', 'er,{settleRequiresEmptyAgentAssociations:!0});');
const agentReasonProducer = take('local Agent child keepalive producer', 194849120, 194849370,
  '3fc739482e58eb739749eabcb2ad2ccf38874b7e4965ece0cc0a23382cd282bf', 'erName(D,an);throw');
const agentReasonResumeProducer = take('backgrounded Agent result keepalive producer', 194855185, 194855370,
  '9b25972472ebc773bd07d0b4515012b7ff4ac086b34b7211b6c8550dd60d6afa', 'YP(et,`agent:${S}`,q);');
const customAgentReasonProducer = take('custom Agent child keepalive producer', 194887493, 194887690,
  '11523244339e68b95804ba236060180724bf215d1f5ea2d93987fa7dda6310b4', 'YP(Kq(o.agentId,d),`agent:${e}`,d);');
assert.match(keepaliveHelpers, /new Set\(XC\(s\)\)\.add\(n\)/);
assert.match(keepaliveHelpers, /u\.delete\(n\)/);
assert.match(keepaliveHelpers, /u\.size===0&&Rs\(i\.status\)&&!i\.retain/);
assert.match(emr, /new Set\(XC\(ye\)\)\.add\(yG\)/);
assert.match(idleWindowCleanup, /hx\(e,yG,n\)/);
assert.match(ownedChildCleanup, /hx\(e,s,n\)/);

const keepaliveState = new Map([['local', {
  type: 'local_agent', status: 'completed', retain: false,
  keepaliveReasons: new Set(), evictAfter: undefined,
}]]);
const keepaliveContext = vm.createContext({
  Map, Set, Date,
  gx: 1,
  Tn: row => row !== null && typeof row === 'object',
  XC: row => row.keepaliveReasons ?? new Set(),
  Rs: status => ['completed', 'failed', 'killed'].includes(status),
  r: {get: id => keepaliveState.get(id), update: (id, reduce) => keepaliveState.set(id, reduce(keepaliveState.get(id)))},
  Rft: () => {},
});
vm.runInContext(keepaliveHelpers, keepaliveContext);
keepaliveContext.YP('local', 'agent:child', keepaliveContext.r);
keepaliveContext.YP('local', 'agent:child', keepaliveContext.r);
keepaliveContext.YP('local', 'bash:child', keepaliveContext.r);
keepaliveContext.hx('local', 'agent:child', keepaliveContext.r);
const afterOneRemoval = [...keepaliveState.get('local').keepaliveReasons].sort();
keepaliveContext.hx('local', 'bash:child', keepaliveContext.r);
const keepaliveFinal = keepaliveState.get('local');
assert.deepEqual(afterOneRemoval, ['bash:child']);
assert.deepEqual([...keepaliveFinal.keepaliveReasons], []);
assert.equal(typeof keepaliveFinal.evictAfter, 'number');

// Ordered Native Map producer used by $.agent.list's name projection.
const nameRegistrySource = take('pwo ordered agentNameRegistry writer', 200985406, 200986838,
  '99d75adb94adcb3d2fc8e7052c0b1c547a8c30e70d714be56cedb85f3eeda7e8', 'function pwo(');
const nameScope = vm.createContext({
  appState: {agentNameRegistry: new Map()},
  uO: () => false,
  t: () => {},
  Ir: () => false,
});
vm.runInContext('function readAppState(){return appState} function updateAppState(f){appState=f(appState)}', nameScope);
vm.runInContext(nameRegistrySource, nameScope);
const nativeNames = nameScope.pwo(nameScope.readAppState, nameScope.updateAppState);
nativeNames.registerName('alpha', 'agent-a');
nativeNames.registerName('beta', 'agent-b');
nativeNames.registerName('gamma', 'agent-a');
nativeNames.registerName('alpha', 'agent-a');
const firstNameSnapshot = [...nameScope.appState.agentNameRegistry.entries()];
assert.deepEqual(JSON.parse(JSON.stringify(firstNameSnapshot)), [
  ['alpha', 'agent-a'], ['beta', 'agent-b'], ['gamma', 'agent-a'],
]);
assert.equal(nativeNames.nameOf('agent-a'), 'alpha', 'nameOf returns the first inserted matching alias');
nativeNames.registerName('alpha', 'agent-c');
assert.deepEqual(JSON.parse(JSON.stringify([...nameScope.appState.agentNameRegistry.entries()])), [
  ['alpha', 'agent-c'], ['beta', 'agent-b'], ['gamma', 'agent-a'],
]);

// In-process teammate producer fills the resumable identity; the external
// tmux/backend producer constructs a distinct identity object without it.
const inProcessTeammate = take('j_r in-process teammate resumable-id producer', 212153014, 212153570,
  'db3566808294c13b5f06d57915f714f06da6cff3fd77e5c01f1d10b4f99a4403', 'async function j_r(');
const externalTeammate = take('te external teammate row identity producer', 212182907, 212183800,
  'b63c3796861af0028c080aac9fe26f315ec60092685e321ab16c7d6a95fbef46', 'function te(');
assert.match(inProcessTeammate, /f=e\.resumableAgentId\?\?Lg\(s\)/);
assert.match(inProcessTeammate, /resumableAgentId:f/);
assert.match(externalTeammate, /identity:\{agentId:n,agentName:o,teamName:s/);
assert(!externalTeammate.includes('resumableAgentId'),
  'external teammate producer leaves the independent resumable-id field absent');

// Team membership is a separate file fact. oBt is the real boolean writer;
// the caller functions below show actual false/true lifecycle triggers.
const teamMemberWriter = take('oBt exact team-member isActive writer', 186842404, 186842863,
  '08e29f637c4d9b81cebb846b5ed16197147ff0b8fbeb66b3c941e9c1cc5f5bd0', 'async function oBt(');
const stopHookInactive = take('nonleader teammate Stop-hook inactive write', 208548468, 208549035,
  '5a9878faa156de33a946314e055fe315f251a1b13549fe96d6af32e0efc753e1', 'v.addFunctionHook(M,"Stop"');
const failureInactive = take('teammate failure-notification inactive write', 208549035, 208549742,
  '15c7aa064342e639621b466e1fd8e5868aadf706bbc747bb2b95b7f586823557', 'async function NRe(');
const mainTurnActive = take('current teammate active write at query entry', 209678585, 209678720,
  'f150db77ed56f94424accaa96a2e6ab155d1d1a7b0c24f684ccefa36c6509af9', '...Qo,$t(Zo,"warni');
assert.match(teamMemberWriter, /s\.isActive===n/);
assert.match(teamMemberWriter, /s\.isActive=n/);
assert.match(stopHookInactive, /oBt\(he,Pe,!1,X\)/);
assert.match(failureInactive, /oBt\(h\.teamName,X,!1,N\)/);
assert.match(mainTurnActive, /oBt\(Zo,Qo,!0,bt\)/);
const teamFiles = new Map([['team-name', {members: [{agentId: 'worker@team', name: 'Worker', isActive: true}]}]]);
const teamActiveLog = [];
let teamWrites = 0;
const teamWriterContext = vm.createContext({
  t: message => teamActiveLog.push(message),
  l: error => String(error),
  lpe: async (teamName, update) => {
    const team = teamFiles.get(teamName);
    if (!team) throw new Error('missing team');
    const before = JSON.stringify(team);
    update(team);
    const changed = JSON.stringify(team) !== before;
    if (changed) teamWrites += 1, teamActiveLog.push('persisted');
    return changed;
  },
});
vm.runInContext(teamMemberWriter, teamWriterContext);
await teamWriterContext.oBt('team-name', 'Worker', false, {});
await teamWriterContext.oBt('team-name', 'Worker', false, {});
assert.equal(teamFiles.get('team-name').members[0].isActive, false);
await teamWriterContext.oBt('team-name', 'Worker', true, {});
assert.equal(teamFiles.get('team-name').members[0].isActive, true);
assert.equal(teamWrites, 2, 'the repeated same-value write does not persist twice');

// Native Py uses either V5 team namespace or the sanitized filesystem config.
const teamPathImports = take('team path Node imports', 186835420, 186835523,
  'ae3f3476adebb17fe1260713c95c7355dd616a8c5ab230c1862ba9dfda9aadbe', 'import{join as B}from"path"');
const teamPathHelpers = take('team name sanitizer, config path and member filter', 186835822, 186836941,
  'c1e196c4426d3e9a362133ab862011fc5bcd267f2b9c63874eec17d39f2282c1', 'function Vhn(');
const teamFileReader = take('Py V5/filesystem team config reader and sanitizer', 186836941, 186837441,
  'de6f3328d7a7cbbcb620d50e432ef40c2af070bd6baf2f5fbe78814248f61ce6', 'async function Py(');
const configRootImports = take('homedir and path join imports for team config root', 179345763, 179345860,
  'b7965c0d30dd10a7b759b5d790542dcb2d0278666bdfcc4d6f24219cae1c3823', 'import{homedir as R}');
const configRootHelpers = take('CLAUDE_CONFIG_DIR and teams root helpers', 179346876, 179347229,
  '4166408c3a7e021e7cda59f3b3f39fda3a32432626a71ad6d704288d4b7bc0d5', 'function o(){');
assert.match(teamPathHelpers, /function Vhn\(e\)\{return e\.replace\(\/\[\^a-zA-Z0-9\]\/g,"-"\)\.toLowerCase\(\)\}/);
assert.match(teamPathHelpers, /function jxe\(e\)\{return B\(C\(e\),"config\.json"\)\}/);
assert.match(teamFileReader, /r\.read\(\[I\(e\)\]\)/);
assert.match(teamFileReader, /O\(jxe\(e\),"utf-8"\)/);
const configPathContext = vm.createContext({
  process: {env: {CLAUDE_CONFIG_DIR: '/custom/.claude'}},
  R: () => '/controlled-home',
  u: path.join,
  B: path.join,
  us: fn => fn,
  O: async filePath => {
    diskReadPath = filePath;
    return JSON.stringify({members: [{agentId: 'worker@team', name: 'Worker', isActive: false}], extra: 'preserved'});
  },
  De: {team: slug => `v5:teams/${slug}`},
  J: JSON.parse,
  Buffer,
  t: () => {},
  l: error => String(error),
  E: error => error?.code,
});
let diskReadPath;
vm.runInContext(configRootHelpers + teamPathHelpers + teamFileReader, configPathContext);
const teamSlugVectors = [
  ['Worker@Team', 'worker-team'],
  ['A B__C', 'a-b--c'],
  ['é漢', '--'],
  ['💩', '--'],
].map(([input, expected]) => ({input, actual: configPathContext.Vhn(input), expected}));
assert.deepEqual(teamSlugVectors.map(({actual}) => actual), ['worker-team', 'a-b--c', '--', '--']);
const fsReadResult = await configPathContext.Py('Worker@Team');
assert.equal(diskReadPath, path.join('/custom/.claude', 'teams', 'worker-team', 'config.json'));
assert.deepEqual(JSON.parse(JSON.stringify(fsReadResult)), {
  members: [{agentId: 'worker@team', name: 'Worker', isActive: false}],
  extra: 'preserved',
});
configPathContext.process.env.CLAUDE_CONFIG_DIR = undefined;
const defaultTeamsRoot = configPathContext.oze();
assert.equal(defaultTeamsRoot, path.join('/controlled-home', '.claude', 'teams'));
configPathContext.process.env.CLAUDE_CONFIG_DIR = '/custom/.claude';
let v5ReadPaths;
const v5ReadResult = await configPathContext.Py('Worker@Team', {
  read: async paths => {
    v5ReadPaths = paths;
    return {ok: true, value: {items: [{found: true, value: Buffer.from(JSON.stringify({
      members: [
        {agentId: 'worker@team', name: 'Worker', isActive: false},
        {agentId: 'bad', name: 3},
      ],
      extra: 'preserved',
    }))}]}};
  },
});
assert.deepEqual(JSON.parse(JSON.stringify(v5ReadPaths)), ['v5:teams/worker-team']);
assert.deepEqual(JSON.parse(JSON.stringify(v5ReadResult)), {
  members: [{agentId: 'worker@team', name: 'Worker', isActive: false}],
  extra: 'preserved',
});

const result = {
  version: '2.1.289',
  binary_sha256: sha256(binary),
  source_windows: sourceWindows,
  probes: {
    agent_spawn: launchProbe,
    agent_model_canonicalizer: nativeAgentModelProjection,
    native_fable_catalog_default: nativeFableCatalogProjection,
    local_idle_tracker: rowSnapshots,
    name_registry: {
      after_insertions: firstNameSnapshot,
      first_name_for_duplicate_id: 'alpha',
      after_updating_existing_key: [...nameScope.appState.agentNameRegistry.entries()],
      first_name_for_agent_a_after_update: nativeNames.nameOf('agent-a'),
      name_for_agent_c_after_update: nativeNames.nameOf('agent-c'),
    },
    keepalive_single_reason_updates: {
      after_removing_agent_reason: afterOneRemoval,
      after_removing_last_reason: [...keepaliveFinal.keepaliveReasons],
      terminal_delayed_eviction_written: typeof keepaliveFinal.evictAfter === 'number',
    },
    team_member_active: {
      controlled_false_setter_value: false,
      controlled_true_setter_value: teamFiles.get('team-name').members[0].isActive,
      no_op_duplicate_false_write: teamWrites === 2,
      writer_logs: teamActiveLog,
    },
    teammate_resumable_identity: {
      in_process_has_defaulted_resumable_id: true,
      external_backend_identity_field: 'missing',
    },
    team_file_read: {
      filesystem_path: diskReadPath,
      default_teams_root: defaultTeamsRoot,
      v5_namespace_key: v5ReadPaths[0],
      sanitizer_vectors: teamSlugVectors,
      filesystem_config: fsReadResult,
      v5_config_after_native_member_filter: v5ReadResult,
      v5_path_builder_internal_serialization: 'injected De.team stub; namespace argument is source-pinned',
    },
  },
  limitations: [
    'eU/Yyo/BOt/UOt use exact Native source, but W1 catalog/tool dispatch and Agent task creation are injected; no provider or permission engine is run.',
    'The Yyo VM overrides CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS to 1 for the cap/refusal/settlement case; exact Native fallback default 20 is source-pinned.',
    'The UOt 150 ms poll value is source-pinned; the VM wait primitive is stubbed to 1 ms so the controlled test stays fast.',
    'oBt/Py use exact Native source with injected lock-backed task file and filesystem/V5 readers; no real user team file is read or written.',
    'At(model) canonicalizer vectors use controlled model-default env values, provider identity, best-model catalog entry, and [1m] capability facts. A separate Fable alias probe executes exact QWe against the embedded Native alias literal and model rows; it does not read this host’s provider cache, modelOverrides, or user settings.',
    'The `De.team(slug)` V5 namespace is passed through and recorded by a stub; this probe does not claim its backend-key serializer or product reader wiring.',
    'j_r and te are source-asserted producer windows, not invoked as complete runners.',
  ],
};

const fixturePath = fileURLToPath(new URL('../../docs/parity/mods-agent-lifecycle-2.1.289.json', import.meta.url));
if (mode === '--write') {
  writeFileSync(fixturePath, `${JSON.stringify(result, null, 2)}\n`);
  console.log(`WROTE ${fixturePath}`);
} else if (mode === '--check') {
  assert.deepEqual(JSON.parse(readFileSync(fixturePath, 'utf8')), JSON.parse(JSON.stringify(result)),
    'Native agent lifecycle fixture changed; review evidence before updating with --write');
  console.log(`PASS: exact 2.1.289 source windows and controlled lifecycle VM match ${fixturePath}`);
} else {
  console.log(JSON.stringify(result, null, 2));
}

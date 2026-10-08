#!/usr/bin/env node
// Source-pinned inputs to the 2.1.289 server-refusal formatter.
// This runs exact Native model-capability, refusal-candidate, label, provider,
// and feedback-policy helpers against controlled host/catalog snapshots. It
// does not start Claude Code, read user configuration, or make provider calls.
//
// Usage:
//   node scripts/tests/mods_refusal_formatter_inputs_289_probe.mjs <2.1.289-binary>

import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const [binaryPath] = process.argv.slice(2);
assert(binaryPath,
  'usage: node scripts/tests/mods_refusal_formatter_inputs_289_probe.mjs <2.1.289-binary>');
const binary = readFileSync(binaryPath);
const sha256 = value => createHash('sha256').update(value).digest('hex');
assert.equal(sha256(binary),
  '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 binary SHA-256');

const sourceWindows = [];
function take(label, start, end, expectedHash, marker) {
  const bytes = binary.subarray(start, end);
  assert.equal(bytes.length, end - start, `${label}: source range is short`);
  if (marker) assert.ok(bytes.toString('utf8').startsWith(marker), `${label}: start marker changed`);
  assert.equal(sha256(bytes), expectedHash, `${label}: source hash changed`);
  sourceWindows.push({label, start, end_exclusive: end, sha256: expectedHash});
  return bytes.toString('utf8');
}

const bakedCatalogSource = take(
  'hand-maintained baked model catalog', 180307020, 180324545,
  'f87f1b4aef70a13ec78e3a9b6f38bd1db7c67ef7ed2b3ec5c21e037816771b1b',
  'var Hsr={');
const bakedCatalogReaderSource = take(
  'baked catalog index and La reader', 180324545, 180327360,
  '707aaf22673b7246b659e5ea25c5db0b71fde648acd54b2bc15643f8060a1661',
  'function k(){return{bakedCatalog:void 0');
const providerAliasResolverSource = take(
  'provider-aware alias lookup from baked catalog aliases',
  180328974, 180329131,
  '776928835e74d1e4a4324635b00e6f0d32cbd50f41b2899fcf41c887c58df8df',
  'function QWe(e,t){let n=TW().aliases');
const capabilitySource = take(
  'environment, served-catalog, and baked refusal capability resolution',
  180328062, 180328974,
  'cb84a7ad675edaecf2f5c1684286f555249ed11b7ca7bd74668395b9e2581485',
  'function SQo(e){IL().servedCapabilityLookup=e}');
const bakedProviderIdsSource = take(
  'baked provider-id table used to classify catalog model IDs',
  180329330, 180331654,
  '5d518394ad391b3bc22554b00b0ba835b27eaa08c633908544373045ca8e6871',
  'var U={"claude-3-5-haiku"');
const providerAliasOrderSource = take(
  'baked Opus alias priority order and first-party provider reverse map',
  180331314, 180331491,
  '4c9b7308beb97fc8c6dda17d1fb518e2d86bfe4ebd1e50d8e1f710f7e420cbf3',
  'Dqt=["opus55","opus5","opus48"');
const providerIdentityPredicateSource = take(
  'provider identity test used by model display labels',
  180331491, 180331654,
  '00ef3ff704985dcc6c51e349e4816c58a5856c805025488f32824a703d5cf7c1',
  'function nP(e){let t=e.toLowerCase();');
const familyPredicateSource = take(
  'baked family lookup and Opus-family predicate',
  182027136, 182027879,
  '0e184cf52740e4cff3dcc90479f661047aa0f64ceb6deed5544f3d378d07832b',
  'function fa(e,n){return La(e)?.family===n}');
const modelIdentitySuffixSource = take(
  'model identity suffix normalizer used by formatter helpers',
  179588498, 179588545,
  'f399497412cd2286f289bd067290ff9bc545a77af6047731ff4d274dcfd22ed8',
  'function Rt(e){return e.replace(/\\[1m\\]$/i,"")}');
const modelNmSource = take(
  'catalog-family model identity entry point Nm',
  182054282, 182054328,
  'f421b492d2dade2ec068dcf8c296e8c601f1a7df21758edf959b6a223ec3dfdf',
  'function Nm(e){return Rt(Be(e,{identity:!0}))}');
const modelAllSuffixSource = take(
  'label resolver normalization of both 1m and 2m suffixes',
  179588611, 179588662,
  'a4d795f092297c1da48837e8342f8a6b308a4fc1b4cd8b34f2ed7bbcdd775ac3',
  'function Mn(e){return e.replace(/\\[(1|2)m\\]/gi,"")}');
const oneMegSuffixSource = take(
  'model capability suffix normalizer', 180328928, 180328974,
  '1563e658e7174696e2678e2a84eddcda19fd9f1b77893f2f3b531696fa5e9c3a',
  'function m(e){return e.replace(/\\[1m\\]/gi,"")');
const dynamicCatalogSource = take(
  'served catalog and dynamic capability lookup resolver',
  182002562, 182004282,
  'c0a61a2eb09088f76735aa2626b6b33ad1731d07c29f54b70efef9df1d4f6709',
  'function XF(){let e=Hr();');
const servedCapabilityRegistrationSource = take(
  'model layer wires capability lookup to catalog runtime metadata',
  182057652, 182057675,
  '0ab34e96b4a03e16f58a1b2b1bf9f61fd1a234b02c37b86c2efedc9dc9ec47cd',
  'SQo((e,n)=>Oy(e,...n));');
const bakedProviderLookupSource = take(
  'served catalog lookup by exact model ID', 181997296, 181997362,
  '84211b45330ee7d1b950e16339652757c7f1808603b77ff46888292d7d792f95',
  'function UZ(e,n){let r=Dp(n);');

const refusalCandidateSource = take(
  'exact refusal candidate resolver eYe/KI and family-default fallback',
  186201217, 186202286,
  '78852a33137cb7cb45ce56b35a1f084d8d6fcf604832c41688add53209ec3dc9',
  'function jI(e){let n=La(e)?.family');
const refusalSpecialModelPredicatesSource = take(
  'Fable and Mythos model predicates plus mitigation capability',
  183122844, 183123162,
  '317839d1dcc2693503ce58ce4f4d69f2d731f84463fbd8ec290caaa65b063aca',
  'function KD(o,n){let e=fy(o,"fable_5_mitigations"');
const loggiaRosterSource = take(
  'GrowthBook refusal model roster and EAP suffix checks',
  183197213, 183197621,
  '5387a5907945c768a3217a0aa84205f20dd0bba7dcb85da56b48b0b31fe9e2f9',
  'function ewe(e){return/-eap($|\\[)/i.test(e)}');
const fableDefaultSource = take(
  'explicit configured default Fable model comparison',
  182031092, 182031181,
  'ec753829a5e805deab8d94010c0ea4cf7dab9272414179484c4d11ece96b6ddc',
  'function nY(e){let n=a.ANTHROPIC_DEFAULT_FABLE_MODEL;');
const alwaysFalseModelPredicateSource = take(
  'eg model exception predicate', 186198584, 186198608,
  '0946254fd1d19b756f9e1edeb4506c4eb6657d95838f5fc90978886f8aa21762',
  'function eg(e){return!1}');
const modelHelpSource = take(
  'Fable and Opus help-link selector', 186204278, 186204775,
  'cc581a5b365323ffb9430ee8803d18d4d162a26aebb8fe2f0181fd8c2cacc6af',
  'var _1r="https://support.claude.com/en/articles/16049681"');
const familyCopySource = take(
  'family copy table, La(Nm(model)), and explicit Opus 5.5 exception',
  186206171, 186207723,
  'b29ec1877e6d928aecd32e2c93f06f75f880b1135aff954fb29249a4c66a069d',
  'var R7="this session"');
const fableHelpPolicySource = take(
  'Dw Fable suppression and b1r help URL decision',
  186204443, 186204656,
  'b3295f6a33160c3dacc471298a90c5758e4369a75925a101c5eb1ad5ea155e17',
  'function Dw(e){let n=Be(e);');
const fableAliasSource = take(
  'Fable help alias set and catalog matcher', 186193447, 186193506,
  'dde7eae9e9618f82480a586f5c4c6800c5286de21e5186ecab8abe065086073a',
  'function Di(e){return e==="fable"||e==="fable[1m]"||Rfe(e)}');
const fablePrefixSource = take(
  'Fable model prefix test', 183123030, 183123083,
  'f622a8db0ae4acef0a12a8a476624524447dc82dc24adbb3eb4f0c4f0dce4fd5',
  'function EIe(o){return o.startsWith("claude-fable-")}');
const eapHelpSource = take(
  'Fable model help family matcher', 182029462, 182029513,
  'db2f41b3aa8fb2c265b6a468128e874250e668681bc64ab81291050e35dbc1b9',
  'function Rfe(e){return e.includes("claude-fable-")}');
const modelHelpSuffixSource = take(
  'one-meg context suffix stripping for help-link selection',
  182061267, 182061314,
  '1d5c5e72e07948c66b0b99c9fbf309f0565af2c66749065b5bc71e865167c4b6',
  'function nO(e){return e.replace(/\\[1m\\]/gi,"")');
const bakedModelLabelSource = take(
  'baked display name fallback and 1M suffix label',
  182056382, 182056576,
  '66cd7a1adca7c1b524b3c86cd2fe07281d3656d2dd9a0b9b067f3409c95e8b6f',
  'function cE(e){return nv(Be(e,{identity:!0}),e.endsWith("[1m]"))}');
const pickerLabelSource = take(
  'model picker display-label override', 182056819, 182056941,
  '76e55885f00958248863e9b010f1e3181a1fce5aa72464a9e8affc82430f6fda',
  'function tN(e){let n=e.trim()');
const displayLabelSource = take(
  'full model display label resolver $o', 182057675, 182058133,
  'e183021156d7a04d442e223123365ae9b57f9d47ff478d6e5e68b3b78acd2b2f',
  'function $o(e){let n=tN(e);');

const providerModeSource = take(
  'provider kind precedence and cyber-copy allowlist',
  180332633, 180333499,
  '6b09cc3f86c4cfd10bca85995b4c7c2343880839ba2dbd816349f6bf0c7a91cb',
  'function He(){if(go()||Q4t()||Z4t())return"gateway"');
const gatewayCredentialSlotsSource = take(
  'gateway credential and host-policy identity sources',
  179315397, 179316014,
  '859fe50c1a561601f6db2fd6e37e7fdd5487dd838faad1f3bf2db8e5ad0b13f1',
  'function go(){return n().host.credentialSlots.gatewayAuth()}');
const candidateRemoteRemapGateSource = take(
  'Opus 5 candidate remote remap gate', 182008503, 182008589,
  '4b09e86dffe740d869dfbc29dcf77c7d7286ea570f9518ae3eff551e4addfa02',
  'function GQe(){return He()==="firstParty"');
const candidateModelDefaultSource = take(
  'Opus alias default selector and E9r configured candidate',
  182032490, 182032782,
  'b1bd523f292e0ad8a3d92170954ac0fd7fa16788da3ff6075971914aa3113aa8',
  'function Gd(){let e=ad()?Yb("opus")');
const candidateModelDefaultStateSource = take(
  'configured Opus alias baseline and ordered alias source',
  182032468, 182032490,
  'b04e4ef429619034f59c921d40da4569b31d10a2df8195ae1685fc96a8ce4a75',
  'var Lb="opus5",Kd=Dqt;');
const candidateModelIdentitySource = take(
  'Opus 5 canonical model comparison used by candidate backport',
  182031336, 182031387,
  'd4cbcc51d4039dce639765c36acb4a24cdf5ef63dd1e139cfcce93dc93aef730',
  'function NGt(e){return Rt(Be(e))');
const providerAliasNamesSource = take(
  'provider-key names used to resolve model aliases',
  182034000, 182034263,
  '359486f29ff0c387ac8f6a30fe2915bfbffca047f78b71f9ab7b3309ec8dc6a4',
  'var UI={firstParty:"first_party"');
const providerAliasResolverYbSource = take(
  'provider-specific alias model selection',
  182034263, 182034394,
  '1e9bcdfa9872ada581014f0273d74c2ff83bf1b34f77760970b947e8d784728d',
  'function Yb(e,n=He())');
const modelLaunchOrderSource = take(
  'provider-resolved default-model accessor', 181703824, 181703968,
  'c1c9fee1af345f853053ce4c2a5fb4fa340ce398fde1cb0fb89c3f3730001b01',
  'function jS(){let e=Qvt();');
const availabilityFallbackAccessorSource = take(
  'current model availability fallback chain accessor',
  179303440, 179303509,
  'be7703ef2998eaa2dd520590be3a3c4b5c803d0a871af0d6f37837e6e2b26321',
  'function voo(){return n().modelSelection.availabilityFallbackChain()}');
const identityModelNormalizerSource = take(
  'lA model spelling canonicalizer', 181702158, 181702182,
  '6de1c795288a69a41d3687906d55ec6eabd18bf6ad34eedd0a5177a15ff2ded3',
  'function lA(e){return e}');
const interactiveSource = take(
  'interactive mode reads host launchOptions', 179307203, 179307263,
  'c5962d665b6699f61261e7a0bccf7d54659c6deee200a6725eeb6402707056f1',
  'function Ce(){return!n().host.launchOptions.isInteractive()}');
const feedbackCallerSource = take(
  'QA feedback command, disable flags, and allow_product_feedback check',
  186171443, 186171956,
  '5f5a000f728ef8cea9c617397a1334ea1729022b0e90aaf5d21bfbb91974f872',
  'function Nre(e="/feedback")');
const feedbackPolicyMetadataSource = take(
  'allow_product_feedback native policy metadata and policy key sets',
  183104513, 183110955,
  'd63179cd87ac8debe86eb11f1d2072dc577a4ce2723db985c628a596eb91132c',
  'var YKr={allow_product_feedback:{deniedUnder:["hipaa","zdr"]');
const feedbackPolicyDecisionSource = take(
  'Native effective permission and policy-denial reason helpers',
  183109600, 183114613,
  'c56fedb4ac86ae2e1ac159a456be6156dcd571f09814ac9aede87761e9b34947',
  'function m4o(e){return KJe(e)?.transcriptScan===!0');

const liveServingModelSource = take(
  'main query resolves current model ge before server-fallback formatting',
  195432246, 195433666,
  '0da0ff364f7d22f5c952892626f075324d11b3cfe087aa81b83aee1434744d58',
  'let br=()=>Es({refusalOverride:Fe,fableConsentOverride:dt');
const currentModelPrecedenceSource = take(
  'query-local model override precedence', 195413357, 195413473,
  'ca70f36e5a01726ba4d9c57d9145847e0eac6d8366228f1ef786e8bcb7fd4b97',
  'function Es(e){return e.refusalOverride??e.fableConsentOverride');
const planModelClampSource = take(
  'permission-mode model clamp Jf', 182036250, 182037196,
  'c625974b3e65d261ee632ef1ea3439170bd64d7deb6828a1849e6678a0224bc9',
  'function Jf(e){let{model:n,clampWarning:r}=wnr(e);');

const feedbackPolicyProbeContext = vm.createContext({
  // These are explicit policy/host state inputs consumed by the exact native
  // decision source. They are not sourced from availableModels or GrowthBook.
  a: {DISABLE_FEEDBACK_COMMAND: undefined, DISABLE_BUG_COMMAND: undefined},
  process: {env: {}},
  p: () => feedbackPolicyProbeContext.nativePolicyState,
  dS: () => feedbackPolicyProbeContext.nativePolicySnapshot,
  C_: () => feedbackPolicyProbeContext.policyServiceEnabled,
  vwe: () => false,
  $Ie: () => false,
  Tt: () => feedbackPolicyProbeContext.nonessentialTrafficOnly,
  _Y: () => false,
  m4o: () => false,
  uYo: () => false,
  dsr: () => [],
  oie: () => [],
  E6t: () => false,
  UIe: () => false,
  J: () => false,
  // Denial text constructors are not on QA's boolean path. The exact dp/sn
  // functions still decide whether the value is null versus non-null.
  NZe: () => 'route missing',
  Yse: () => 'policy unavailable',
  EEe: () => 'policy latched',
  pwt: () => 'policy denied',
  PJo: () => {},
  t: () => {},
  l: String,
});
vm.runInContext(feedbackPolicyMetadataSource, feedbackPolicyProbeContext);
vm.runInContext(feedbackPolicyDecisionSource, feedbackPolicyProbeContext);
vm.runInContext(feedbackCallerSource, feedbackPolicyProbeContext);

const feedbackPolicyCases = [
  {
    name: 'loaded policy explicitly allows feedback',
    service: true,
    snapshot: {restrictions: {allow_product_feedback: {allowed: true}},
      compliance_taints: [], sessionFromServerFetch: true},
    expected: true,
  },
  {
    name: 'loaded policy explicitly denies feedback',
    service: true,
    snapshot: {restrictions: {allow_product_feedback: {allowed: false}},
      compliance_taints: [], sessionFromServerFetch: true},
    expected: false,
  },
  {
    name: 'Native onCacheMiss deny blocks an enabled service without a loaded policy snapshot',
    service: true,
    snapshot: null,
    expected: false,
  },
  {
    name: 'disabled service does not infer a deny when no policy hints are present',
    service: false,
    snapshot: null,
    expected: true,
  },
];
const feedbackPolicyOutputs = feedbackPolicyCases.map(testCase => {
  feedbackPolicyProbeContext.nativePolicySnapshot = testCase.snapshot;
  feedbackPolicyProbeContext.nativePolicyState = {
    hintedDeniedKeys: [], hintedTaints: [], sessionFromServerFetch: true,
    sessionCache: testCase.snapshot,
    lastFetchOutcome: {success: true, httpStatus: 200},
  };
  feedbackPolicyProbeContext.policyServiceEnabled = testCase.service;
  feedbackPolicyProbeContext.nonessentialTrafficOnly = false;
  feedbackPolicyProbeContext.a.DISABLE_FEEDBACK_COMMAND = undefined;
  feedbackPolicyProbeContext.a.DISABLE_BUG_COMMAND = undefined;
  const allowed = feedbackPolicyProbeContext.QA();
  const reason = feedbackPolicyProbeContext.Nre('/feedback');
  assert.equal(allowed, testCase.expected, testCase.name);
  assert.equal(allowed, reason === null, `${testCase.name}: QA mirrors Nre nullability`);
  return {name: testCase.name, qa: allowed, nre_is_null: reason === null,
    nre_non_null_reason: reason !== null};
});

// The exact Native disable check uses JavaScript truthiness on the already
// materialized environment fields. The strings "false" and "0" remain truthy.
const feedbackDisableCases = [];
for (const disabledValue of ['false', '0']) {
  feedbackPolicyProbeContext.nativePolicySnapshot = {
    restrictions: {allow_product_feedback: {allowed: true}},
    compliance_taints: [], sessionFromServerFetch: true,
  };
  feedbackPolicyProbeContext.nativePolicyState = {
    hintedDeniedKeys: [], hintedTaints: [], lastFetchOutcome: {success: true, httpStatus: 200},
  };
  feedbackPolicyProbeContext.policyServiceEnabled = true;
  feedbackPolicyProbeContext.a.DISABLE_FEEDBACK_COMMAND = disabledValue;
  assert.equal(feedbackPolicyProbeContext.QA(), false,
    `DISABLE_FEEDBACK_COMMAND=${disabledValue} must suppress feedback`);
  feedbackDisableCases.push({key: 'DISABLE_FEEDBACK_COMMAND', value: disabledValue,
    nre: feedbackPolicyProbeContext.Nre('/feedback')});
}

function makeModelProbeContext({provider = 'firstParty', servedCatalog = null,
  modelCapabilities, defaultOpusModel = 'claude-opus-5', defaultFableModel,
  growthbookRoster = [], fallbackChain = [], backportGate = false} = {}) {
  const env = {
    CLAUDE_CODE_MODEL_CAPABILITIES: modelCapabilities,
    ANTHROPIC_DEFAULT_OPUS_MODEL: defaultOpusModel,
    ANTHROPIC_DEFAULT_FABLE_MODEL: defaultFableModel,
    CLAUDE_CODE_USE_BEDROCK: provider === 'bedrock',
    CLAUDE_CODE_USE_FOUNDRY: provider === 'foundry',
    CLAUDE_CODE_USE_ANTHROPIC_AWS: provider === 'anthropicAws',
    CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD: provider === 'anthropicGoogleCloud',
    CLAUDE_CODE_USE_MANTLE: provider === 'mantle',
    CLAUDE_CODE_USE_VERTEX: provider === 'vertex',
  };
  const ctx = vm.createContext({
    a: env,
    f: () => undefined,
    // Baked catalog parsing only constructs the native static object/index;
    // its lazy Zod-schema factories are deliberately not invoked here.
    u: () => undefined,
    n: () => ({host: {credentialSlots: {
      gatewayAuth: () => ctx.nativeInputs.gatewayAuth,
      gatewayServerProcess: () => ctx.nativeInputs.gatewayServerProcess,
      gatewayRequiredByHostPolicy: () => ctx.nativeInputs.gatewayRequiredByHostPolicy,
    }}, modelSelection: {
      availabilityFallbackChain: () => ctx.nativeInputs.fallbackChain,
    }}),
    nativeInputs: {gatewayAuth: false, gatewayServerProcess: false,
      gatewayRequiredByHostPolicy: false, fallbackChain},
    Hr: () => ctx.nativeCatalogState,
    Ya: () => ctx.nativeInputs.servedCatalogAllowed,
    lv: catalog => catalog?.models ?? [],
    Dp: value => String(value).trim().toLowerCase(),
    ef: () => ctx.nativeInputs.backportGate,
    Zp: () => false,
    dw: () => null,
    $fe: () => [],
    // Model identity rewriting is host/model-catalog state outside these
    // pure functions; canonical IDs used here are passed through unchanged.
    Be: value => value,
    lA: value => value,
    b3: () => [],
    PA: value => value,
    knr: value => value,
    nP: () => null,
    E2e: () => [],
    sW: () => [],
    onr: () => false,
    abt: value => value,
    lQn: value => ctx.nativeInputs.growthbookRoster.some(entry =>
      String(entry).toLowerCase() === String(value).toLowerCase()),
  });
  ctx.nativeInputs.servedCatalogAllowed = servedCatalog !== null;
  ctx.nativeInputs.fallbackChain = fallbackChain;
  ctx.nativeInputs.backportGate = backportGate;
  ctx.nativeInputs.growthbookRoster = growthbookRoster;
  ctx.nativeCatalogState = {
    servedCatalogSuppressDepth: 0,
    servedCatalogActive: servedCatalog === null ? null : {
      source: 'served', catalog: servedCatalog,
    },
    servedCatalogDeactivatedRows: undefined,
  };
  for (const source of [bakedCatalogSource, bakedCatalogReaderSource,
    capabilitySource, providerAliasResolverSource, bakedProviderLookupSource,
    dynamicCatalogSource,
    bakedProviderIdsSource,
    providerAliasOrderSource, providerAliasNamesSource,
    providerAliasResolverYbSource,
    providerIdentityPredicateSource, familyPredicateSource,
    modelIdentitySuffixSource, modelNmSource, modelAllSuffixSource,
    oneMegSuffixSource, bakedModelLabelSource, pickerLabelSource,
    refusalSpecialModelPredicatesSource, loggiaRosterSource,
    fableDefaultSource, alwaysFalseModelPredicateSource,
    modelHelpSource, familyCopySource, fableHelpPolicySource,
    fableAliasSource, fablePrefixSource, eapHelpSource,
    modelHelpSuffixSource, modelLabelSourceForVm,
    providerModeSource, gatewayCredentialSlotsSource,
    candidateRemoteRemapGateSource, candidateModelDefaultSource,
    candidateModelDefaultStateSource,
    candidateModelIdentitySource, identityModelNormalizerSource,
    refusalCandidateSource,
    availabilityFallbackAccessorSource, modelNmSource]) {
    vm.runInContext(source, ctx);
  }
  // GrowthBook's live roster is explicit host state in this probe; the exact
  // lQn/Gr implementation is source-pinned above but its cache lookup is not
  // connected to a real Native account/session here.
  ctx.lQn = model => ctx.nativeInputs.growthbookRoster.some(entry =>
    String(entry).toLowerCase() === String(model).toLowerCase());
  vm.runInContext(servedCapabilityRegistrationSource, ctx);
  // model catalog identity matching in this probe is scoped to canonical IDs.
  ctx.Rt = value => String(value).replace(/\[1m\]$/i, '');
  return ctx;
}

// `$o` is executed in the same model VM. The canonical-ID branch reaches the
// baked La display_name fallback. The served custom-model branch is reached
// only because its injected resolver identity is not one of Native's baked
// provider IDs (`nP`); this is a controlled custom-catalog record.
const modelLabelSourceForVm = displayLabelSource;
function resolveDisplayLabel(inputs) {
  const ctx = makeModelProbeContext(inputs);
  ctx.tN = () => undefined;
  ctx.PA = value => value;
  ctx.E2e = () => [];
  ctx.sW = () => [];
  return {context: ctx, label: model => ctx.$o(model)};
}

const modelCapabilityCases = [];
function assertCapability(name, inputs, model, capability, expected) {
  const ctx = makeModelProbeContext(inputs);
  const actual = ctx.fy(model, capability, model);
  assert.equal(actual, expected, name);
  modelCapabilityCases.push({name, model, capability,
    result: actual === undefined ? null : actual});
}
assertCapability('baked Sonnet 5 has no refusal_fallback capability', {},
  'claude-sonnet-5', 'refusal_fallback', undefined);
assertCapability('baked Sonnet 5.5 has refusal_fallback capability', {},
  'claude-sonnet-5-5', 'refusal_fallback', true);
assertCapability('1m selector normalizes before baked capability lookup', {},
  'claude-sonnet-5-5[1m]', 'refusal_fallback', true);
assertCapability('exact environment grant can add a capability absent from baked catalog', {
  modelCapabilities: 'claude-sonnet-5=refusal_fallback',
}, 'claude-sonnet-5', 'refusal_fallback', true);
assertCapability('explicit environment deny overrides baked capability', {
  modelCapabilities: 'claude-sonnet-5-5=-refusal_fallback',
}, 'claude-sonnet-5-5', 'refusal_fallback', false);
assertCapability('later matching environment segment wins', {
  modelCapabilities: 'claude-sonnet-5=-refusal_fallback;claude-sonnet-5=refusal_fallback',
}, 'claude-sonnet-5', 'refusal_fallback', true);
assertCapability('prefix wildcard applies to canonical model ID', {
  modelCapabilities: 'claude-sonnet-5*=refusal_fallback',
}, 'claude-sonnet-5-5', 'refusal_fallback', true);
assertCapability('capability token matching is case-sensitive', {
  modelCapabilities: 'claude-sonnet-5=REFUSAL_FALLBACK',
}, 'claude-sonnet-5', 'refusal_fallback', undefined);

const servedCatalogForCapability = {models: [{
  id: 'claude-sonnet-5', name: 'Served Sonnet 5',
  runtime: {capabilities: ['refusal_fallback']},
}]};
assertCapability('served runtime metadata can enable an absent baked capability', {
  servedCatalog: servedCatalogForCapability,
}, 'claude-sonnet-5', 'refusal_fallback', true);
assertCapability('environment deny has precedence over served capability true', {
  servedCatalog: servedCatalogForCapability,
  modelCapabilities: 'claude-sonnet-5=-refusal_fallback',
}, 'claude-sonnet-5', 'refusal_fallback', false);
assertCapability('served lookup absence falls through to baked capability', {
  servedCatalog: {models: [{id: 'claude-sonnet-5-5', runtime: {capabilities: []}}]},
}, 'claude-sonnet-5-5', 'refusal_fallback', true);
{
  const ctx = makeModelProbeContext({});
  ctx.SQo(() => false);
  assert.equal(ctx.fy('claude-sonnet-5-5', 'refusal_fallback', 'claude-sonnet-5-5'), true,
    'served false is not a veto; Native falls through to baked capabilities');
  ctx.bQo(() => false);
  assert.equal(ctx.S('refusal_fallback'), true,
    'refusal_fallback is not in the feature-gated capability map');
}

const refusalEligibilityCases = [];
function assertEligibility(name, inputs, model, expected, extra = {}) {
  const ctx = makeModelProbeContext(inputs);
  const candidate = ctx.KI(model);
  const actual = ctx.eYe(model);
  assert.equal(actual, expected !== undefined, `${name}: eYe boolean`);
  assert.equal(candidate, expected, `${name}: KI candidate`);
  refusalEligibilityCases.push({name, model, eligible: actual,
    candidate: candidate ?? null, dw_fable_suppression: ctx.Dw(model),
    provider: ctx.He(), ...extra});
  return ctx;
}
assertEligibility('baked eligible Sonnet 5.5 resolves the configured Opus candidate', {},
  'claude-sonnet-5-5', 'claude-opus-5');
assertEligibility('explicit capability deny prevents baked eligibility', {
  modelCapabilities: 'claude-sonnet-5-5=-refusal_fallback',
}, 'claude-sonnet-5-5', undefined);
assertEligibility('unflagged Sonnet 5 is not made eligible by a model catalog membership', {},
  'claude-sonnet-5', undefined);
assertEligibility('Fable capability qualifies while Dw suppresses first-time copy', {},
  'claude-fable-5-1', 'claude-opus-5');
assertEligibility('EAP suffix is an independent model eligibility exception', {},
  'claude-sonnet-5-eap', 'claude-opus-5');
assertEligibility('GrowthBook model roster is required before Mythos policy exception', {},
  'claude-mythos-5', undefined);
assertEligibility('rostered Mythos 5 can use the explicit mitigation exception', {
  growthbookRoster: ['claude-mythos-5'],
}, 'claude-mythos-5', 'claude-opus-5');
assertEligibility('configured default Fable identity is an eligibility condition', {
  defaultFableModel: 'claude-sonnet-5',
}, 'claude-sonnet-5', 'claude-opus-5');
assertEligibility('candidate must resolve to Opus family', {
  defaultOpusModel: 'claude-sonnet-5-5',
}, 'claude-sonnet-5-5', undefined);
assertEligibility('non-first-party providers use the same-family configured default', {
  provider: 'bedrock',
}, 'claude-sonnet-5-5', 'claude-opus-5');
assertEligibility('first-party Opus 5 candidate is remapped when exact remote gate is true', {
  backportGate: true,
}, 'claude-opus-5', 'claude-opus-4-8', {fallback_chain: []});
assertEligibility('a fallback chain containing Opus 5 suppresses the remap', {
  backportGate: true, fallbackChain: ['claude-opus-5'],
}, 'claude-opus-5', 'claude-opus-5', {fallback_chain: ['claude-opus-5']});

const modelHelpCases = [];
const bakedLabels = resolveDisplayLabel({});
for (const model of ['claude-sonnet-5-5', 'claude-opus-5-5']) {
  modelHelpCases.push({model, display_label: bakedLabels.label(model),
    baked_family: bakedLabels.context.La(model)?.family,
    baked_capabilities: bakedLabels.context.La(model)?.capabilities ?? []});
}
const customLabelContext = resolveDisplayLabel({
  servedCatalog: {models: [{id: 'vendor.custom-sonnet', name: 'Served Custom Label'}]},
});
assert.equal(customLabelContext.label('vendor.custom-sonnet'), 'Served Custom Label');
modelHelpCases.push({model: 'vendor.custom-sonnet',
  display_label: customLabelContext.label('vendor.custom-sonnet'),
  baked_family: customLabelContext.context.La('vendor.custom-sonnet')?.family ?? null,
  source: 'served custom model record'});

const helpAndFamilyContext = makeModelProbeContext({
  defaultFableModel: 'claude-sonnet-5',
});
const firstTimeModels = ['claude-sonnet-5-5', 'claude-opus-5-5', 'claude-opus-5'];
const firstTimeEligibility = firstTimeModels.map(model => ({model,
  is_first_time_model: helpAndFamilyContext.v7(model),
  baked_family: helpAndFamilyContext.La(helpAndFamilyContext.Nm(model))?.family ?? null,
}));
assert.deepEqual(firstTimeEligibility.map(item => item.is_first_time_model), [true, true, false]);
const fableAndHelpLinks = [
  'claude-fable-5-1', 'fable', 'claude-opus-5[1m]', 'claude-opus-5-5', null,
].map(model => ({model, fable_suppressed: model === null ? false : helpAndFamilyContext.Dw(model),
  help_url: helpAndFamilyContext.b1r(model)}));
assert.equal(fableAndHelpLinks[0].help_url,
  'https://support.claude.com/en/articles/15363606');
assert.equal(fableAndHelpLinks[2].help_url,
  'https://support.claude.com/en/articles/16049681');
assert.equal(fableAndHelpLinks[3].help_url,
  'https://support.claude.com/en/articles/8106465');

const providerContext = makeModelProbeContext({});
const providerCases = [
  {name: 'first party default', provider: 'firstParty', env: {}},
  {name: 'Bedrock environment flag', provider: 'bedrock', env: {CLAUDE_CODE_USE_BEDROCK: true}},
  {name: 'Foundry environment flag', provider: 'foundry', env: {CLAUDE_CODE_USE_FOUNDRY: true}},
  {name: 'Anthropic AWS environment flag', provider: 'anthropicAws', env: {CLAUDE_CODE_USE_ANTHROPIC_AWS: true}},
  {name: 'Anthropic Google Cloud environment flag', provider: 'anthropicGoogleCloud', env: {CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD: true}},
  {name: 'Mantle environment flag', provider: 'mantle', env: {CLAUDE_CODE_USE_MANTLE: true}},
  {name: 'Vertex environment flag', provider: 'vertex', env: {CLAUDE_CODE_USE_VERTEX: true}},
  {name: 'gateway credential wins over Bedrock flag', provider: 'gateway',
    env: {CLAUDE_CODE_USE_BEDROCK: true}, gatewayAuth: true},
];
const providerOutputs = providerCases.map(testCase => {
  Object.assign(providerContext.a, {
    CLAUDE_CODE_USE_BEDROCK: false, CLAUDE_CODE_USE_FOUNDRY: false,
    CLAUDE_CODE_USE_ANTHROPIC_AWS: false,
    CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD: false,
    CLAUDE_CODE_USE_MANTLE: false, CLAUDE_CODE_USE_VERTEX: false,
  }, testCase.env);
  providerContext.nativeInputs.gatewayAuth = testCase.gatewayAuth === true;
  providerContext.nativeInputs.gatewayServerProcess = false;
  providerContext.nativeInputs.gatewayRequiredByHostPolicy = false;
  const actual = providerContext.He();
  assert.equal(actual, testCase.provider, testCase.name);
  const specialCyber = providerContext.ad();
  const opusDefaultKey = specialCyber ? providerContext.Gd() : null;
  if (testCase.provider === 'firstParty') assert.equal(opusDefaultKey, 'opus5');
  if (testCase.provider === 'gateway') assert.equal(opusDefaultKey, 'opus47');
  return {name: testCase.name, provider: actual,
    special_cyber_copy: specialCyber, opus_default_alias_key: opusDefaultKey};
});

const modelSelectorContext = vm.createContext({});
vm.runInContext(currentModelPrecedenceSource, modelSelectorContext);
const currentModelPrecedence = modelSelectorContext.Es({
  refusalOverride: 'claude-opus-5', fableConsentOverride: 'claude-fable-5-1',
  liveSwitchOverride: 'claude-opus-4-8', chainModel: 'claude-sonnet-5-5',
  primaryModel: 'claude-sonnet-5',
});
assert.equal(currentModelPrecedence, 'claude-opus-5', 'refusal override is the highest precedence');

const output = {
  oracle: 'Claude Code 2.1.289 refusal formatter input resolver',
  native_binary_sha256: sha256(binary),
  execution_scope: 'Exact Native model capability, eYe/KI, $o, b1r, provider, QA/Nre, and policy decision helpers executed against explicit controlled host/policy/catalog snapshots. No Native CLI, account, provider, network, or real user configuration was read.',
  source_windows: sourceWindows,
  observations: {
    formatter_input_model: 'At the actual server-fallback refusal callsite $dt receives ge. The query reducer computes ge as Jf({ permissionMode, mainLoopModel: br(), exceeds200kTokens }); br selects refusalOverride ?? fableConsentOverride ?? liveSwitchOverride ?? chainModel ?? primaryModel. ge may change after a model-switch transition. The actual callsite, override precedence, and plan clamp are separately pinned.',
    refusal_eligibility: 'eYe(model) is KI(model) !== undefined. KI gates on environment capability rules, baked/dynamic catalog capability, Fable/EAP/default-Fable exceptions, a GrowthBook model roster for Mythos IDs, provider-kind routing, and an Opus-family-valid current fallback candidate. It does not consult organization availableModels membership.',
    capability_order: 'fy uses an explicit CLAUDE_CODE_MODEL_CAPABILITIES match first (including false); a served runtime capability true second; then the hand-maintained baked Hsr catalog. The served callback returns only true or undefined and cannot veto a baked true; refusal_fallback is absent from the feature-gated S map, so its dynamic true is not blocked by that map.',
    dynamic_vs_baked: 'La reads only Hsr, a hand-maintained baked catalog, for family/display_name/capabilities. The separate WQe path reads active or deactivated served-catalog rows and can provide dynamic custom-model names/runtime capabilities. Dynamic display labels are consulted only when nP(modelId) does not identify a baked provider ID; clean canonical built-in IDs use the baked label fallback.',
    model_label: '$o resolves the model picker label first, then the eligible dynamic catalog/custom model paths, then cE through baked La.display_name (and optional 1M context label), then a sanitized model spelling or unknown model. The full dynamic label cache and host modelKnowledge state remain live inputs.',
    family_copy: 'Lw uses La(Nm(model))?.family and the baked $I family table. In this build first-time wording supports Sonnet/cyber and the explicit claude-opus-5-5 model exception; Dw suppresses it for Fable identity/aliases/configured default.',
    provider_copy: 'He chooses gateway first from three host credential-slot facts, then the provider environment flags in Bedrock, Foundry, Anthropic AWS, Anthropic Google Cloud, Mantle, Vertex order, else firstParty. ad permits only firstParty, Anthropic AWS, Anthropic Google Cloud, or gateway.',
    feedback_gate: 'QA is Nre()==null. Nre first applies raw truthiness disable flags for DISABLE_FEEDBACK_COMMAND / DISABLE_BUG_COMMAND and the nonessential-traffic mode, then checks dp(allow_product_feedback, /feedback, is). dp is a projection of the Native effective policy decision sn: allow gives null, deny/missing policy produces a non-null reason. The built-in key is server-populated, denied under hipaa/zdr, and deny-on-cache-miss.',
    missing_runtime_inputs: 'The probe uses explicit settings-policy state, custom/served catalog records, model picker absence, current provider credentials, GrowthBook roster, model-switch state, and candidate model config. It does not reconstruct the Native account/model-catalog fetch, modelKnowledge resolver, the current account policy service, or dynamic model picker caches.',
  },
  current_query_model: {override_precedence: currentModelPrecedence},
  model_capability_cases: modelCapabilityCases,
  refusal_eligibility_cases: refusalEligibilityCases,
  model_labels: modelHelpCases,
  family_lookup: firstTimeEligibility,
  fable_and_help_links: fableAndHelpLinks,
  provider_cases: providerOutputs,
  feedback_policy_cases: feedbackPolicyOutputs,
  feedback_disable_cases: feedbackDisableCases,
};
process.stdout.write(`${JSON.stringify(output, null, 2)}\n`);

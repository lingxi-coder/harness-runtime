#!/usr/bin/env node
// Pin Native 2.1.289 cold/default behavior for the refusal policy's model and
// feedback inputs. This executes source slices with explicit cache/host seams;
// it never starts the CLI or reads account state.

import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const [binaryPath] = process.argv.slice(2);
assert(binaryPath,
  'usage: node scripts/tests/mods_refusal_cold_state_289_probe.mjs <2.1.289-binary>');
const binary = readFileSync(binaryPath);
const sha256 = value => createHash('sha256').update(value).digest('hex');
assert.equal(sha256(binary),
  '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 binary SHA-256');

const sourceWindows = [];
const assertJsonEqual = (actual, expected, message) =>
  assert.deepEqual(JSON.parse(JSON.stringify(actual)), expected, message);
function take(label, start, end, expectedHash, marker) {
  const bytes = binary.subarray(start, end);
  assert.equal(bytes.length, end - start, `${label}: short source range`);
  if (marker) assert.ok(bytes.toString('utf8').startsWith(marker), `${label}: source moved`);
  assert.equal(sha256(bytes), expectedHash, `${label}: source hash changed`);
  sourceWindows.push({label, start, end_exclusive: end, sha256: expectedHash});
  return bytes.toString('utf8');
}

const bakedCatalogSource = take(
  'baked hand-maintained catalog Hsr', 180307020, 180324545,
  'f87f1b4aef70a13ec78e3a9b6f38bd1db7c67ef7ed2b3ec5c21e037816771b1b', 'var Hsr={');
const bakedReaderSource = take(
  'baked catalog index, La, and cold capability resolver state', 180324545, 180327360,
  '707aaf22673b7246b659e5ea25c5db0b71fde648acd54b2bc15643f8060a1661',
  'function k(){return{bakedCatalog:void 0');
const coldCapabilityStateSource = take(
  'initial IL resolver slots', 180324545, 180324763,
  '4109ccec64d48bef1a02be7cf78b2311bf0eae008f1436561a0a1bafee81806d',
  'function k(){return{bakedCatalog:void 0');
const bakedProviderIdsSource = take(
  'baked provider IDs, provider model rows, and fallback table', 180329330, 180331654,
  '5d518394ad391b3bc22554b00b0ba835b27eaa08c633908544373045ca8e6871',
  'var U={"claude-3-5-haiku"');
const capabilitySource = take(
  'capability precedence and optional served resolver', 180328062, 180328974,
  'cb84a7ad675edaecf2f5c1684286f555249ed11b7ca7bd74668395b9e2581485',
  'function SQo(e){IL().servedCapabilityLookup=e}');
const servedCatalogRegistrationSource = take(
  'model module installs the served capability resolver', 182057652, 182057675,
  '0ab34e96b4a03e16f58a1b2b1bf9f61fd1a234b02c37b86c2efedc9dc9ec47cd',
  'SQo((e,n)=>Oy(e,...n));');
const coldCatalogStateSource = take(
  'per-host served catalog cold state initializer', 181696189, 181697613,
  'cf260d4711c9341dcc7ff6a48521bb21445ef4cccad93b64dee0e7028088bece',
  'function AA(){return{providerCache:$5r()');
const servedCatalogLookupSource = take(
  'served-catalog miss behavior', 182002562, 182004282,
  'c0a61a2eb09088f76735aa2626b6b33ad1731d07c29f54b70efef9df1d4f6709',
  'function XF(){let e=Hr();');
const providerAliasAndClassifierSource = take(
  'Native provider kind and approved refusal-copy providers', 180332633, 180333499,
  '6b09cc3f86c4cfd10bca85995b4c7c2343880839ba2dbd816349f6bf0c7a91cb',
  'function He(){if(go()||Q4t()||Z4t())return"gateway"');
const gatewayCredentialSlotsSource = take(
  'provider classifier gateway host credential slots', 179315397, 179316014,
  '859fe50c1a561601f6db2fd6e37e7fdd5487dd838faad1f3bf2db8e5ad0b13f1',
  'function go(){return n().host.credentialSlots.gatewayAuth()}');
const candidateAliasSource = take(
  'provider alias order and cold Opus candidate selector', 180331314, 180331491,
  '4c9b7308beb97fc8c6dda17d1fb518e2d86bfe4ebd1e50d8e1f710f7e420cbf3',
  'Dqt=["opus55","opus5","opus48"');
const aliasLookupSource = take(
  'provider model alias lookup from the baked catalog', 180328974, 180329131,
  '776928835e74d1e4a4324635b00e6f0d32cbd50f41b2899fcf41c887c58df8df',
  'function QWe(e,t){let n=TW().aliases');
const providerAliasNamesSource = take(
  'provider names used by baked alias selection', 182034000, 182034263,
  '359486f29ff0c387ac8f6a30fe2915bfbffca047f78b71f9ab7b3309ec8dc6a4',
  'var UI={firstParty:"first_party"');
const aliasProviderSelectorSource = take(
  'provider-specific alias selector Yb', 182034263, 182034394,
  '1e9bcdfa9872ada581014f0273d74c2ff83bf1b34f77760970b947e8d784728d',
  'function Yb(e,n=He())');
const candidateModelDefaultStateSource = take(
  'cold Opus alias baseline', 182032468, 182032490,
  'b04e4ef429619034f59c921d40da4569b31d10a2df8195ae1685fc96a8ce4a75',
  'var Lb="opus5",Kd=Dqt;');
const candidateModelDefaultSource = take(
  'Gd/E9r Native candidate resolver', 182032490, 182032782,
  'b1bd523f292e0ad8a3d92170954ac0fd7fa16788da3ff6075971914aa3113aa8',
  'function Gd(){let e=ad()?Yb("opus")');
const candidateRemoteRemapSource = take(
  'Opus candidate remote remap gate', 182008503, 182008589,
  '4b09e86dffe740d869dfbc29dcf77c7d7286ea570f9518ae3eff551e4addfa02',
  'function GQe(){return He()==="firstParty"');
const candidateModelIdentitySource = take(
  'canonical Opus identity check', 182031336, 182031387,
  'd4cbcc51d4039dce639765c36acb4a24cdf5ef63dd1e139cfcce93dc93aef730',
  'function NGt(e){return Rt(Be(e))');
const fallbackChainAccessorSource = take(
  'availability fallback chain host getter', 179303440, 179303509,
  'be7703ef2998eaa2dd520590be3a3c4b5c803d0a871af0d6f37837e6e2b26321',
  'function voo(){return n().modelSelection.availabilityFallbackChain()}');
const providerFallbackRowsSource = take(
  'cold provider fallback table from baked model rows', 181702158, 181702394,
  '2dbb0e644a48967586ddf86686241181cf03de9726435a61510f688434784db8',
  'function lA(e){return e}var Dl=Object.keys(vo)');
const modelOverrideSource = take(
  'user modelOverrides applied over provider defaults', 181703344, 181703625,
  '3a1f140ff85330424aa1655ba832fc9eed80666ff22c2fcb8dfdbfc83ca6c9cd',
  'function qf(e){let n=lt().modelOverrides');
const coldModelStringsInitializerSource = take(
  'cold modelStringsCache initialization path', 181703740, 181703968,
  '1fc3be9719feebfdb38ed64f5b79babe39adbafc2584722704641fa9d2054596',
  'function Il(){if(Qvt()!==null)return;');
const nativeModelStringsHostAccessSource = take(
  'modelStringsCache host getter',
  179305694, 179305758,
  '8c3c14691033638eb8122eed8ec4afc04eac631ff0fbe673f440507d0804e83d',
  'function Qvt(){return n().host.modelStringsCache.modelStrings()}');
const nativeModelStringsHostWriteSource = take(
  'modelStringsCache host writer', 179305758, 179305824,
  '685493f52e4d04aa1769d8fb06c31a3208d20f61b7e70d42160668c7c26abffb',
  'function TPn(e){n().host.modelStringsCache.replaceModelStrings(e)}');
const refusalCandidateSource = take(
  'exact KI/eYe and candidate selection', 186201217, 186202286,
  '78852a33137cb7cb45ce56b35a1f084d8d6fcf604832c41688add53209ec3dc9',
  'function jI(e){let n=La(e)?.family');
const refusalSpecialPredicatesSource = take(
  'Fable/Mythos refusal predicates', 183122844, 183123162,
  '317839d1dcc2693503ce58ce4f4d69f2d731f84463fbd8ec290caaa65b063aca',
  'function KD(o,n){let e=fy(o,"fable_5_mitigations"');
const familyAndOpusPredicateSource = take(
  'static family predicate and first-time Opus exception', 182027136, 182027879,
  '0e184cf52740e4cff3dcc90479f661047aa0f64ceb6deed5544f3d378d07832b',
  'function fa(e,n){return La(e)?.family===n}');
const rosterSource = take(
  'GrowthBook refusal roster default and membership', 183197213, 183197621,
  '5387a5907945c768a3217a0aa84205f20dd0bba7dcb85da56b48b0b31fe9e2f9',
  'function ewe(e){return/-eap($|\\[)/i.test(e)}');
const fableDefaultSource = take(
  'configured Fable model is an environment-only equality condition', 182031092, 182031181,
  'ec753829a5e805deab8d94010c0ea4cf7dab9272414179484c4d11ece96b6ddc',
  'function nY(e){let n=a.ANTHROPIC_DEFAULT_FABLE_MODEL;');
const fableCopyExceptionSource = take(
  'Native eg exception predicate', 186198584, 186198608,
  '0946254fd1d19b756f9e1edeb4506c4eb6657d95838f5fc90978886f8aa21762',
  'function eg(e){return!1}');
const oneMegCandidateSuffixSource = take(
  'candidate suffix removal', 182061267, 182061314,
  '1d5c5e72e07948c66b0b99c9fbf309f0565af2c66749065b5bc71e865167c4b6',
  'function nO(e){return e.replace(/\\[1m\\]/gi,""');

const growthBookLookupSource = take(
  'feature lookup cold, disabled, disk, and fallback sources', 181846341, 181847084,
  '106782695c7016daf255548f5737e6472e2bb5bb1b860e58d9eae8b253220113',
  'getFeatureValueWithSource(e,n){let r=this');
const growthBookNullFallbackSource = take(
  'GrowthBook null-to-default value conversion', 181851707, 181851744,
  '4ef000b7340a6cab5590571f5e21f7e35d53cda372f9e54110b403c19ada178e',
  'function Du(e,n){return e===null?n:e}');
const growthBookClientSource = take(
  'GrowthBook lookup lazily creates its client resolver', 182188260, 182188813,
  '1aae4728aa575c9f6400393b8f393b0ce37fde222381e02a761e755ee77f7bd9',
  'function Ne(){let e=ui();if(e.client===null)e.client=new Fu({isEnabled:');
const growthBookServiceSwitchSource = take(
  'GrowthBook service enabled and disk-readable gates', 182189752, 182189903,
  '648fdcadd3707a05a510ddc4d3b1598055012efcfb6ad6c37c28645060174329',
  'function th(){return!a.DISABLE_GROWTHBOOK&&Kz()}');
const growthBookWrapperSource = take(
  'feature lookup wrapper k/io', 182191232, 182191465,
  '61732312a4b5da02bdac6db71125555355bb0752eb11f299d632fef3ba5712a7',
  'function io(e,n){return Ne().getFeatureValueWithSource(e,n)}');

const policyServiceSource = take(
  'policy service eligibility: first-party route and accepted credential conditions',
  183094674, 183095246,
  'a8d1c9b012dfa732e5ddb810cd6839e26b54f5184089a0953c25950f5927ae56',
  'function ter(e={}){if(He()!=="firstParty")');
const policyStateSource = take(
  'policy cold session state constructor', 183088968, 183092134,
  '8e48921f6ac419646f6c20e9baad6a5c7fa4b5c08e493df4266447e8b0d2e590',
  'class p4o{sessionCache=null;');
const policySnapshotResolverSource = take(
  'policy snapshot lookup with cache-miss result', 183115886, 183116056,
  'c95de8b8d86e7b3e67af30e461be4938b2cc8b9ed26216f45d5e2407ae7d90c4',
  'function dS(){if(!C_())return null;');
const policyMetadataSource = take(
  'allow_product_feedback server-populated deny-on-cache-miss metadata',
  183104513, 183110955,
  'd63179cd87ac8debe86eb11f1d2072dc577a4ce2723db985c628a596eb91132c',
  'var YKr={allow_product_feedback:{deniedUnder:["hipaa","zdr"]');
const policyDecisionSource = take(
  'effective Native policy decision sn', 183109600, 183114613,
  'c56fedb4ac86ae2e1ac159a456be6156dcd571f09814ac9aede87761e9b34947',
  'function m4o(e){return KJe(e)?.transcriptScan===!0');
const feedbackCallerSource = take(
  'feedback command environment gates before policy evaluation', 186171443, 186171956,
  '5f5a000f728ef8cea9c617397a1334ea1729022b0e90aaf5d21bfbb91974f872',
  'function Nre(e="/feedback"){');

const featureContext = vm.createContext({});
featureContext.a = {
  CLAUDE_CODE_MODEL_CAPABILITIES: undefined,
  ANTHROPIC_DEFAULT_OPUS_MODEL: undefined,
  ANTHROPIC_DEFAULT_FABLE_MODEL: undefined,
  CLAUDE_CODE_USE_BEDROCK: false,
  CLAUDE_CODE_USE_FOUNDRY: false,
  CLAUDE_CODE_USE_ANTHROPIC_AWS: false,
  CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD: false,
  CLAUDE_CODE_USE_MANTLE: false,
  CLAUDE_CODE_USE_VERTEX: false,
};
featureContext.f = () => undefined;
featureContext.u = () => undefined;
featureContext.T = () => undefined;
featureContext.n = () => ({
  host: {
    credentialSlots: {
      gatewayAuth: () => false,
      gatewayServerProcess: () => false,
      gatewayRequiredByHostPolicy: () => false,
    },
    modelStringsCache: {
      modelStrings: () => featureContext.nativeModelStrings,
      replaceModelStrings: value => { featureContext.nativeModelStrings = value; },
    },
  },
  modelSelection: {availabilityFallbackChain: () => []},
});
featureContext.nativeModelStrings = null;
featureContext.nativeSettings = {modelOverrides: undefined};
featureContext.lt = () => featureContext.nativeSettings;
featureContext.Xf = () => { throw new Error('unexpected Bedrock cold path in first-party probe'); };
featureContext.Dp = value => String(value).trim().toLowerCase();
featureContext.Be = value => value;
featureContext.Rt = value => String(value).replace(/\[1m\]$/i, '');
featureContext.lA = value => value;
featureContext.lv = catalog => catalog?.models ?? [];
featureContext.Ya = () => true;
featureContext.ef = () => false;
featureContext.Zp = () => false;
featureContext.dw = () => null;
featureContext.$fe = () => [];
featureContext.nativeCatalogState = undefined;

// Execute Native's own cold host-state initializer; only OS/runtime services
// called while constructing it are represented by fixed inert stubs.
featureContext.$5r = () => ({validatedModels: new Set(), modelConfigs: new Map()});
featureContext.no = () => ({});
featureContext.Oe = () => ({emit() {}});
featureContext.Ct = class { constructor(make) { this.make = make; } of() { return this.make(); } };
featureContext.U = () => ({});
vm.runInContext(coldCatalogStateSource, featureContext);
featureContext.nativeCatalogState = featureContext.AA();

for (const source of [bakedCatalogSource, bakedReaderSource, bakedProviderIdsSource,
  capabilitySource, servedCatalogLookupSource, providerAliasAndClassifierSource,
  gatewayCredentialSlotsSource, candidateAliasSource, aliasLookupSource,
  providerAliasNamesSource, aliasProviderSelectorSource, candidateModelDefaultStateSource,
  candidateModelDefaultSource, candidateRemoteRemapSource, candidateModelIdentitySource,
  fallbackChainAccessorSource, providerFallbackRowsSource, modelOverrideSource,
  coldModelStringsInitializerSource, nativeModelStringsHostAccessSource,
  nativeModelStringsHostWriteSource, refusalCandidateSource,
  refusalSpecialPredicatesSource, familyAndOpusPredicateSource,
  fableDefaultSource, fableCopyExceptionSource, oneMegCandidateSuffixSource]) {
  vm.runInContext(source, featureContext);
}
// The exact source files are separate Bun modules, whose minified local names
// do not share one global lexical scope. Bind the Native per-host state object
// obtained above as the controlled module-host seam for this combined VM.
featureContext.Hr = () => featureContext.nativeCatalogState;

// Native's model source registers the served resolver during module setup.
// Run the registration after first observing IL's literal unconnected state.
const initialResolverAbsent = featureContext.IL().servedCapabilityLookup === undefined;
vm.runInContext(servedCatalogRegistrationSource, featureContext);
assert.equal(typeof featureContext.IL().servedCapabilityLookup, 'function');
assert.equal(featureContext.XF(), null);
assert.equal(featureContext.ea(), null);
const servedCapabilityColdMiss = featureContext.IL().servedCapabilityLookup(
  'refusal_fallback', 'claude-sonnet-5-5');
assert.equal(servedCapabilityColdMiss, undefined);

// Reproduce Native GrowthBook's lazy client attachment and execute its exact
// feature-value method. Constructor/state and disk/auth are controlled seams;
// feature/default selection itself is Native code.
featureContext.nativeGrowthBook = {client: null};
featureContext.ui = () => featureContext.nativeGrowthBook;
featureContext.nativeInputs = {
  growthBookEnabled: false,
  diskReadable: false,
  globalConfig: {cachedGrowthBookFeatures: {}},
};
for (const name of ['vls', 'Els', 'Wi', 'mAn', 'ZVt', 'KU', 'drr', 'Ae',
  'ZYo', 'IU', 'Als', 'ee']) {
  featureContext[name] = () => undefined;
}
featureContext.le = () => featureContext.nativeInputs.globalConfig;
featureContext.Kz = () => featureContext.nativeInputs.growthBookEnabled;
featureContext.YL = () => featureContext.nativeInputs.diskReadable;
featureContext.Jn = () => featureContext.nativeInputs.diskReadable;
featureContext.le = () => featureContext.nativeInputs.globalConfig;
featureContext.gs = () => false;
featureContext.cge = () => false;
featureContext.Ce = () => false;
featureContext.a.CLAUDE_CODE_GB_DISK_CACHE_WHEN_TELEMETRY_OFF = false;
featureContext.a.DISABLE_GROWTHBOOK = false;
vm.runInContext(growthBookNullFallbackSource, featureContext);
featureContext.Fu = class {
  constructor(deps) {
    this.deps = deps;
    this.experimentDataByFeature = new Map();
    this.remoteEvalFeatureValues = new Map();
    this.environmentOverrides = null;
    this.configOverrides = null;
  }
  getEnvironmentOverrides() { return this.environmentOverrides; }
  readConfigOverrides() { return this.configOverrides; }
  logExposureForFeature() {}
  recordDeferredExposure() {}
};
// Add the exact Native prototype method to an inert host-side test class.
vm.runInContext(`Object.defineProperty(Fu.prototype, 'getFeatureValueWithSource', {
  value: function ${growthBookLookupSource}, configurable: true
});`, featureContext);
vm.runInContext(growthBookClientSource, featureContext);
vm.runInContext(growthBookServiceSwitchSource, featureContext);
vm.runInContext(growthBookWrapperSource, featureContext);

const growthBookColdCases = [];
const growthBookResolverInitiallyAbsent = featureContext.nativeGrowthBook.client === null;
function readRosterValue(name, {enabled, disabledEnv = false, diskReadable = false,
  diskRoster = undefined} = {}) {
  featureContext.nativeGrowthBook.client = null;
  featureContext.nativeInputs.growthBookEnabled = enabled;
  featureContext.nativeInputs.diskReadable = diskReadable;
  featureContext.nativeInputs.globalConfig = {
    cachedGrowthBookFeatures: diskRoster === undefined ? {} : {tengu_loggia_roster: diskRoster},
  };
  featureContext.a.DISABLE_GROWTHBOOK = disabledEnv;
  featureContext.a.CLAUDE_CODE_GB_DISK_CACHE_WHEN_TELEMETRY_OFF = diskReadable;
  return featureContext.io('tengu_loggia_roster', []);
}
const growthBookDisabled = readRosterValue('service disabled', {enabled: true, disabledEnv: true});
assertJsonEqual(growthBookDisabled, {value: [], source: 'disabled'});
assert.equal(featureContext.nativeGrowthBook.client !== null, true,
  'Native Ne lazily attaches the feature resolver before reading its value');
growthBookColdCases.push({name: 'DISABLE_GROWTHBOOK with no readable disk cache',
  value: growthBookDisabled.value, source: growthBookDisabled.source,
  resolver_was_absent_before_lookup: growthBookResolverInitiallyAbsent,
  resolver_attached_on_lookup: true});
const growthBookColdMiss = readRosterValue('no fetched or persisted feature', {
  enabled: true, diskReadable: false,
});
assertJsonEqual(growthBookColdMiss, {value: [], source: 'fallback'});
growthBookColdCases.push({name: 'enabled GrowthBook with no remote or disk value',
  value: growthBookColdMiss.value, source: growthBookColdMiss.source});
const growthBookDisk = readRosterValue('disk cache readable while service disabled', {
  enabled: false, diskReadable: true, diskRoster: ['claude-mythos-5'],
});
assertJsonEqual(growthBookDisk, {value: ['claude-mythos-5'], source: 'disk'});
growthBookColdCases.push({name: 'disabled service with explicitly readable cached feature',
  value: growthBookDisk.value, source: growthBookDisk.source});

// Execute exact Gr/lQn against the feature lookup above. The Zod/lazy-schema
// constructors only validate the already typed array input in this probe.
featureContext.f = factory => {
  let value;
  return () => value ??= factory();
};
featureContext.o = () => ({trim: () => ({})});
featureContext.T = () => ({safeParse: value => Array.isArray(value) &&
  value.every(entry => typeof entry === 'string')
  ? {success: true, data: value} : {success: false}});
featureContext.II = (left, right) => left === right;
vm.runInContext(rosterSource, featureContext);
featureContext.nativeGrowthBook.client = null;
featureContext.nativeInputs.growthBookEnabled = true;
featureContext.nativeInputs.diskReadable = false;
featureContext.nativeInputs.globalConfig = {cachedGrowthBookFeatures: {}};
featureContext.a.DISABLE_GROWTHBOOK = false;
featureContext.a.CLAUDE_CODE_GB_DISK_CACHE_WHEN_TELEMETRY_OFF = false;
featureContext.To = undefined;
assertJsonEqual(featureContext.Gr(), []);
assert.equal(featureContext.lQn('claude-mythos-5'), false);

// Cold provider model selection: no process default-opus override and a null
// modelStringsCache cause Native jS() to initialize its baked provider model
// table, then apply only the explicitly absent modelOverrides seam.
const coldModelStringTable = featureContext.jS();
const coldOpusAlias = featureContext.Gd();
const coldOpusCandidate = featureContext.E9r();
assert.equal(featureContext.He(), 'firstParty');
assert.equal(coldOpusAlias, 'opus5');
assert.equal(coldOpusCandidate, coldModelStringTable[coldOpusAlias]);
assert.equal(coldOpusCandidate, featureContext.vo.opus5.firstParty);
const coldSonnetFallbackCandidate = featureContext.KI('claude-sonnet-5-5');
assert.equal(coldSonnetFallbackCandidate, coldOpusCandidate);
const coldSonnetEligible = featureContext.eYe('claude-sonnet-5-5');
assert.equal(coldSonnetEligible, true);
const coldMythosEligible = featureContext.eYe('claude-mythos-5');
assert.equal(coldMythosEligible, false);
featureContext.a.CLAUDE_CODE_MODEL_CAPABILITIES = 'claude-sonnet-5-5=-refusal_fallback';
assert.equal(featureContext.eYe('claude-sonnet-5-5'), false,
  'explicit environment deny wins over cold baked capability');
const coldModelFacts = {
  model_strings_cache_initially_null: true,
  native_model_strings_cache_after_resolution: featureContext.nativeModelStrings,
  provider_kind: featureContext.He(),
  opus_alias: coldOpusAlias,
  opus_candidate: coldOpusCandidate,
  sonnet_5_5_baked_family: featureContext.La('claude-sonnet-5-5')?.family,
  sonnet_5_5_baked_display_name: featureContext.La('claude-sonnet-5-5')?.display_name,
  sonnet_5_5_eligible_without_served_catalog_or_env: coldSonnetEligible,
  mythos_eligible_with_empty_roster: coldMythosEligible,
  eligible_after_explicit_env_deny: featureContext.eYe('claude-sonnet-5-5'),
};

// Resolve Native policy service eligibility from exact `ter/C_/bK`; credential
// data below is synthetic and never read from the machine.
const policyContext = vm.createContext({});
policyContext.a = {DISABLE_FEEDBACK_COMMAND: undefined, DISABLE_BUG_COMMAND: undefined};
policyContext.He = () => policyContext.nativePolicyInputs.provider;
policyContext.JD = () => false;
policyContext.ni = () => !policyContext.nativePolicyInputs.customEndpoint;
policyContext.Pb = () => ({key: policyContext.nativePolicyInputs.apiKey});
policyContext.gd = () => false;
policyContext.mn = () => policyContext.nativePolicyInputs.oauth;
policyContext.xA = 'controlled-native-required-scope';
vm.runInContext(policyServiceSource, policyContext);

const policyServiceCases = [
  {name: 'first-party API key', provider: 'firstParty', apiKey: 'dummy', oauth: undefined,
    customEndpoint: false, expected: true},
  {name: 'first-party enterprise/team OAuth with required inference scope',
    provider: 'firstParty', apiKey: undefined,
    oauth: {accessToken: 'dummy', scopes: ['controlled-native-required-scope'],
      subscriptionType: 'enterprise'}, customEndpoint: false, expected: true},
  {name: 'first-party API route with no usable auth', provider: 'firstParty', apiKey: undefined,
    oauth: undefined, customEndpoint: false, expected: false},
  {name: 'OAuth missing the required inference scope', provider: 'firstParty', apiKey: undefined,
    oauth: {accessToken: 'dummy', scopes: [], subscriptionType: 'enterprise'},
    customEndpoint: false, expected: false},
  {name: 'prosumer OAuth without enterprise/team policy service eligibility',
    provider: 'firstParty', apiKey: undefined,
    oauth: {accessToken: 'dummy', scopes: ['controlled-native-required-scope'],
      subscriptionType: 'pro'}, customEndpoint: false, expected: false},
  {name: 'non-first-party provider', provider: 'bedrock', apiKey: 'dummy', oauth: undefined,
    customEndpoint: false, expected: false},
  {name: 'custom endpoint route', provider: 'firstParty', apiKey: 'dummy', oauth: undefined,
    customEndpoint: true, expected: false},
];
const policyServiceOutputs = policyServiceCases.map(testCase => {
  policyContext.nativePolicyInputs = testCase;
  const enabled = policyContext.C_();
  assert.equal(enabled, testCase.expected, testCase.name);
  return {name: testCase.name, policy_service_enabled: enabled};
});

// Use Native's own default p4o fields and dS/sn/QA code, with only local disk
// and credential readers held inert. This separates service-disabled from an
// enabled service whose server policy cache is still missing.
policyContext.Oe = () => ({emit() {}});
policyContext.me = {incomplete: false, seen: [], ruledOut: []};
vm.runInContext(policyStateSource, policyContext);
vm.runInContext('globalThis.NativePolicyState = p4o;', policyContext);
policyContext.nativePolicyState = new policyContext.NativePolicyState();
policyContext.p = () => policyContext.nativePolicyState;
policyContext.b6t = () => null;
policyContext.vwe = () => false;
policyContext.$Ie = () => false;
policyContext.Tt = () => false;
policyContext._Y = () => false;
policyContext.m4o = () => false;
policyContext.uYo = () => false;
policyContext.dsr = () => [];
policyContext.oie = () => [];
policyContext.E6t = () => false;
policyContext.M = () => false;
policyContext.UIe = () => false;
policyContext.J = () => false;
policyContext.NZe = () => 'route missing';
policyContext.Yse = () => 'policy unavailable';
policyContext.EEe = () => 'policy latched';
policyContext.pwt = () => 'policy denied';
policyContext.PJo = () => {};
policyContext.t = () => {};
policyContext.l = String;
vm.runInContext(policyMetadataSource, policyContext);
vm.runInContext(policyDecisionSource, policyContext);
vm.runInContext(policySnapshotResolverSource, policyContext);
vm.runInContext(feedbackCallerSource, policyContext);

const policyColdCases = [];
for (const testCase of [policyServiceCases[0], policyServiceCases[2]]) {
  policyContext.nativePolicyInputs = testCase;
  policyContext.nativePolicyState = new policyContext.NativePolicyState();
  const cache = policyContext.dS();
  assert.equal(cache, null, `${testCase.name}: no disk/session policy cache`);
  const allowed = policyContext.sn('allow_product_feedback');
  const qa = policyContext.QA();
  const expected = !testCase.expected;
  assert.equal(allowed, expected, `${testCase.name}: cold policy result`);
  assert.equal(qa, expected, `${testCase.name}: QA follows policy result`);
  policyColdCases.push({name: testCase.expected
    ? 'service eligible but server-policy cache is absent'
    : 'service unavailable and cold state has no policy hints/cache',
  policy_service_enabled: policyContext.C_(), snapshot: cache,
  session_cache: policyContext.p().sessionCache,
  last_fetch_outcome: policyContext.p().lastFetchOutcome,
  fail_open_verdict: policyContext.p().failOpenVerdict,
  initial_hinted_denies: policyContext.p().hintedDeniedKeys,
  initial_hinted_taints: policyContext.p().hintedTaints,
  policy_allowed: allowed, qa: qa});
}
for (const envKey of ['DISABLE_FEEDBACK_COMMAND', 'DISABLE_BUG_COMMAND']) {
  policyContext.nativePolicyInputs = policyServiceCases[0];
  policyContext.nativePolicyState = new policyContext.NativePolicyState();
  policyContext.a[envKey] = 'false';
  assert.equal(policyContext.QA(), false, `${envKey} raw truthiness disable`);
  policyColdCases.push({name: `${envKey} string false is truthy and disables QA`,
    policy_service_enabled: true, qa: false});
  policyContext.a[envKey] = undefined;
}
policyContext.nativePolicyInputs = policyServiceCases[0];
policyContext.nativePolicyState = new policyContext.NativePolicyState();
policyContext.Tt = () => true;
assert.equal(policyContext.QA(), false, 'nonessential-traffic gate disables feedback');
policyColdCases.push({name: 'nonessential traffic mode disables QA before policy', qa: false});

const output = {
  oracle: 'Claude Code 2.1.289 refusal cold-state/default audit',
  native_binary_sha256: sha256(binary),
  execution_scope: 'Exact pinned Native default/source functions with controlled host, credential, policy-cache, and GrowthBook cache seams. No CLI, user settings, account, network, or provider request was used. Stubs are listed per case and do not stand in for live Native fetches.',
  source_windows: sourceWindows,
  observations: {
    capability_resolver: initialResolverAbsent
      ? 'IL starts with servedCapabilityLookup undefined; the model module source then registers its callback. With no active/deactivated served catalog the attached callback returns undefined, so fy falls through to baked Hsr capability. A matching explicit CLAUDE_CODE_MODEL_CAPABILITIES=false is checked first and remains a veto.'
      : 'Unexpected capability resolver state; see failed assertion.',
    growthbook_roster: 'Gr asks k("tengu_loggia_roster", []). With the Native feature lookup source, disabled/no-readable-disk returns that fallback with source=disabled; enabled but no remote/disk value returns it with source=fallback. An explicitly readable cached value still wins with source=disk. An empty cold roster is not proof that no later refresh can load it.',
    policy_service: 'C_ is a Native policy-service eligibility gate, not a cache-presence test. In the controlled helper execution, API-key auth and enterprise/team OAuth with the required scope enable it on the first-party route; missing auth, missing inference scope, prosumer OAuth, non-first-party provider and custom endpoint disable it.',
    policy_cache: 'Native p4o begins with sessionCache=null, failOpenVerdict=false, and empty hintedDeniedKeys/hintedTaints. dS returns null when the service is disabled; when enabled and disk/session cache is absent it also returns null. sn denies allow_product_feedback for the enabled-service cache miss (metadata onCacheMiss=deny), but with the service unavailable and no local hints it returns allow. That is Native behavior, not a Host resolver-absence rule.',
    feedback_env: 'Nre checks DISABLE_FEEDBACK_COMMAND, DISABLE_BUG_COMMAND and nonessential-traffic mode before dp(allow_product_feedback). The disable fields use JavaScript truthiness; strings "false" and "0" are truthy.',
    model_route: 'When Native modelStringsCache is null, jS initializes its provider table from the baked provider-ID rows through ro(He()), then applies user modelOverrides. With the controlled empty-overrides first-party case, Gd chooses the baked opus5 alias and E9r resolves the candidate from that table; this is not a universal setting when Native model strings or user overrides are populated.',
    host_boundary: 'Native Hsr/La still supplies static labels, family and capabilities in the absence of served catalog rows. The Rust ProviderApiAdapter snapshot resolver is optional and returns None if no embedding-host source is attached; that Host composition state is not equivalent to Native C_=false or Native missing policy cache.',
  },
  cold_model_facts: coldModelFacts,
  growthbook_cold_cases: growthBookColdCases,
  policy_service_cases: policyServiceOutputs,
  policy_cold_cases: policyColdCases,
  native_initial_policy_state: {
    session_cache: new policyContext.NativePolicyState().sessionCache,
    last_fetch_outcome: new policyContext.NativePolicyState().lastFetchOutcome,
    fail_open_verdict: new policyContext.NativePolicyState().failOpenVerdict,
    hinted_denied_keys: new policyContext.NativePolicyState().hintedDeniedKeys,
    hinted_taints: new policyContext.NativePolicyState().hintedTaints,
    principal_rung: new policyContext.NativePolicyState().principalRung,
  },
};
console.log(JSON.stringify(output, null, 2));

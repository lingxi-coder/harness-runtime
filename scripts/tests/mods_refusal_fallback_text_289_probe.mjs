#!/usr/bin/env node
// Pins and executes the 2.1.289 server-refusal API-error formatter with explicit
// injected policy/catalog facts. This is not a full Native runtime or provider
// execution: catalog-backed model eligibility/display labels and low-level
// sanitizer imports are controlled inputs/stubs.
//
// Usage:
//   node scripts/tests/mods_refusal_fallback_text_289_probe.mjs <2.1.289-binary>

import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const [binaryPath] = process.argv.slice(2);
assert(binaryPath,
  'usage: node scripts/tests/mods_refusal_fallback_text_289_probe.mjs <2.1.289-binary>');
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

const apiErrorPrefixSource = take(
  'API error text prefix', 181462894, 181462922,
  '47f7ba124f8da99bda599c37e90063e2d49e6f645c8a721cd20a1895fdaa8b15',
  'var Ba="API Error",Htt=1000;');
const interactiveGateSource = take(
  'interactive-mode gate', 179307203, 179307263,
  'c5962d665b6699f61261e7a0bccf7d54659c6deee200a6725eeb6402707056f1',
  'function Ce(){return!n().host.launchOptions.isInteractive()}');
const providerModeSource = take(
  'provider mode and first-party cyber gate', 180332633, 180333499,
  '6b09cc3f86c4cfd10bca85995b4c7c2343880839ba2dbd816349f6bf0c7a91cb',
  'function He(){if(go()||Q4t()||Z4t())return"gateway"');
const feedbackGateSource = take(
  'feedback command and product setting gate', 186171443, 186171956,
  '5f5a000f728ef8cea9c617397a1334ea1729022b0e90aaf5d21bfbb91974f872',
  'function Nre(e="/feedback")');
const nonessentialTrafficGateSource = take(
  'nonessential-traffic environment interpretation', 179441125, 179441394,
  '158d4b04144dbc09ec8a0154aefcbce587dd255b92f55f8e4b0bb75d2a66b90d',
  'function Ivt(){if(process.env.CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC)');
const refusalRowSource = take(
  'refusal API-error row formatter', 188298052, 188299837,
  '7a19dc31283968e16b1583ddc200d3b9ad0097f094e9de501f61733083242455',
  'function $dt(e,n,r,s,g){');
const refusalCallsiteSource = take(
  'server fallback refusal callsite and metadata', 195455728, 195456130,
  'af0bc32abd5dfa44b69a773d6f4ccd44a63fb9e9de57ffa025433f46f3a5c149',
  'M.reason==="refusal"?$dt("refusal",');
const refusalVisibilitySource = take(
  'model refusal-eligibility gate', 186201771, 186202286,
  'afc3bc7a841ef1885df70297e3b1d17f5d360b552cd2517c11ccd1b11baa9abf',
  'function eYe(e){return KI(e)!==void 0}');
const fablePrefixSource = take(
  'Fable model prefix predicate used by refusal policy', 183123030, 183123083,
  'f622a8db0ae4acef0a12a8a476624524447dc82dc24adbb3eb4f0c4f0dce4fd5',
  'function EIe(o){return o.startsWith("claude-fable-")}');
const fableDefaultSource = take(
  'configured default Fable identity predicate', 182031092, 182031181,
  'ec753829a5e805deab8d94010c0ea4cf7dab9272414179484c4d11ece96b6ddc',
  'function nY(e){let n=a.ANTHROPIC_DEFAULT_FABLE_MODEL;');
const fableCatalogMatcherSource = take(
  'Fable family catalogue matcher', 182029462, 182029513,
  'db2f41b3aa8fb2c265b6a468128e874250e668681bc64ab81291050e35dbc1b9',
  'function Rfe(e){return e.includes("claude-fable-")}');
const fableAliasSource = take(
  'Fable aliases excluded from generic model learning link', 186193447, 186193506,
  'dde7eae9e9618f82480a586f5c4c6800c5286de21e5186ecab8abe065086073a',
  'function Di(e){return e==="fable"||e==="fable[1m]"||Rfe(e)}');
const refusalCategoryPredicateSource = take(
  'special refusal categories', 186198608, 186198654,
  '5ecb2ded983173867c841488e7286264be099d8512f8512488250c2d05058989',
  'function wfn(e){return e==="cyber"||e==="bio"}');
const modelHelpSource = take(
  'refusal model help URL selector', 186204278, 186204775,
  'cc581a5b365323ffb9430ee8803d18d4d162a26aebb8fe2f0181fd8c2cacc6af',
  'var _1r="https://support.claude.com/en/articles/16049681"');
const specialCyberCopySource = take(
  'Cyber Verification copy and link', 186204775, 186205263,
  'f36f2cea4045312d4e443997d3183ff80afbaa471926b0f80a97746c318b2fe3',
  'var w1r="https://support.claude.com/en/articles/14604842-real-time-cyber-safeguards-on-claude"');
const refusalCategoryCopyAndDetailsSource = take(
  'category-specific safeguard copy and Details suffix', 186205433, 186205940,
  '4f4c2ecfa8c4188122f8b66b6b41333c7839fb6bf9cab65997105793182813a7',
  'var E1r=');
const familyFirstTimeCopySource = take(
  'family-specific first-time refusal copy', 186206171, 186207723,
  'b29ec1877e6d928aecd32e2c93f06f75f880b1135aff954fb29249a4c66a069d',
  'var R7="this session"');
const apiTextSanitizerSource = take(
  'text sanitizer used by Details', 179180145, 179180355,
  '2b5d27192155093a7667d43136545373288704fce520d8b9c54f711f40ba3250',
  'function zee(e){');
const detailsCodeUnitCapSource = take(
  'Details suffix code-unit cap', 179180355, 179180367,
  'e501a6ad9408cc3b7ca35eff60f3eddf6ee576011f94aca9a9b7c0b401761e1d',
  'var DGe=255;');
const displayModelLabelSource = take(
  'model display-label resolver', 182057675, 182058133,
  'e183021156d7a04d442e223123365ae9b57f9d47ff478d6e5e68b3b78acd2b2f',
  'function $o(e){');
const requestIdFilterSource = take(
  'request ID suffix filter', 188301883, 188301957,
  '7111be7252031995a70a25118547e3a1cfca5087f5d6f6fe748fd9ae252db434',
  'function hTt(e){');

// Keep the route/copy boundary visible: $dt receives category and request ID,
// while its explanation and other refusal metadata are forced to null.
assert.match(refusalCallsiteSource,
  /\$dt\("refusal",\{type:"refusal",category:M\.apiRefusalCategory\?\?null,explanation:null,fallback_credit_token:null,fallback_has_prefill_claim:null,recommended_model:null\},M\.requestId,ge\)/);
assert.match(refusalRowSource,
  /if\(e!=="refusal"\)return;[\s\S]*cs\(\{content:w\+M\+V,error:"invalid_request"\}\);return he\.requestId=r\?\?void 0,he\.message\.stop_reason="refusal",he\.message\.stop_details=n\?\?null,he/);

const toolResultPollSource = take(
  'completed tool results enter attempt-local je through Tn', 195439205, 195439671,
  'acbac16529445942a6c430201d13f36f9f1401f1ed384d0ddebfa31164c77e5d',
  'let ue=ul({createStreamingToolExecutor:()=>new ds(f.options.tools,T,f,B.now),');
const toolResultProjectorSource = take(
  'attachments and projected user rows accumulate in je', 195429764, 195430089,
  '0f2b2a6e52dbbc3f5932f2f60e43d6912d64070d27545cbaa103f2dbe6411e23',
  'xr=function(h,A){');
const toolReturnedMessagesSource = take(
  'tool-returned new_messages enter executor rows', 188849094, 188849265,
  '7fb51a35eb372c51dcfc83e34a2bd7155962d9adb1cf3414bd1ce0a154d727dc',
  'let vi=[..._o.newMessages??[],...[]];');
const agentStreamOrderSource = take(
  'Native streaming tool block dispatcher W1', 188814536, 188819240,
  '2dac343f7459b2386e673fc7726ae690d0b2bb0304ae2e8727fda593fcbaa3d4');
const acceptedJeResetSource = take(
  'accepted fallback resets the full je array when tool uses were tombstoned',
  195457049, 195457553,
  '620a9fbe09bfbd2bc31b4de0331018e69829feff4bb46c7ea51cd2c881f422af',
  'if(X.tombstonedToolUse){');
assert.match(toolResultPollSource,
  /toolResults:je.*for\(let A of ue\.streamingToolExecutor\.getCompletedResults\(\)\).*xr\(A\.message,f\)/);
assert.match(toolResultProjectorSource,
  /if\(h\.type==="attachment"\)\{je\.push\(h\);return\}.*je\.push\(\.\.\.te\.filter\(\(W\)=>W\.type==="user"\)\)/);
assert.match(toolReturnedMessagesSource,
  /for\(let Lr of vi\)if\(Lr\.type==="user"\)\{let oi=\{\.\.\.Lr,turnCompanion:!0\};qe\.push\(\{message:oi\}\)\}else qe\.push\(\{message:Lr\}\)/);
assert.match(agentStreamOrderSource, /for await\(let Xe of jyo\(/);
assert.match(acceptedJeResetSource,
  /to=\[\.\.\.je\],ue\.reset\(\{clearAssistantMessages:!1\}\),ue\.rebuildStreamingToolExecutor\(\)/);

function makeSourceVm(testCase) {
  const env = {
    CLAUDE_CODE_USE_BEDROCK: testCase.provider === 'bedrock',
    CLAUDE_CODE_USE_FOUNDRY: testCase.provider === 'foundry',
    CLAUDE_CODE_USE_ANTHROPIC_AWS: testCase.provider === 'anthropicAws',
    CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD: testCase.provider === 'anthropicGoogleCloud',
    CLAUDE_CODE_USE_MANTLE: testCase.provider === 'mantle',
    CLAUDE_CODE_USE_VERTEX: testCase.provider === 'vertex',
    DISABLE_FEEDBACK_COMMAND: false,
    DISABLE_BUG_COMMAND: false,
    ANTHROPIC_DEFAULT_FABLE_MODEL: undefined,
  };
  const context = {
    a: env,
    process: {env: {
      CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC:
        testCase.disableNonessentialTraffic ? '1' : undefined,
      DISABLE_TELEMETRY: undefined,
      DO_NOT_TRACK: undefined,
    }},
    n: () => ({host: {launchOptions: {isInteractive: () => testCase.interactive}}}),
    go: () => testCase.provider === 'gateway',
    Q4t: () => false,
    Z4t: () => false,
    Le: value => value === '1' || value === 'true',
    dp: () => testCase.feedbackAllowed ? null : false,
    Be: value => value,
    Rt: value => value,
    nO: value => value.replace(/\[1m\]/gi, ''),
    eg: value => value === 'claude-opus-5-5',
    Nm: value => value,
    La: value => testCase.modelFamilies?.[value] ? {family: testCase.modelFamilies[value]} : undefined,
    // These are injected policy/catalog facts, not Native runtime calls.
    eYe: () => testCase.modelEligible,
    $o: () => testCase.displayLabel,
    i: () => {},
    d: value => value,
    Fre: value => value,
    _e: value => value,
    F$t: () => false,
    ESe: () => 'test-arm-source',
    Ba: 'API Error',
    cs: ({content, error}) => ({content, error, message: {}}),
    DGe: Number(detailsCodeUnitCapSource.match(/DGe=(\d+)/)?.[1]),
    // The categories exercised here are ASCII; imported sanitizer operations
    // are inert for them. The exact Native zee/fi and $1e bodies still execute.
    OV: value => value,
    tn: value => value,
    p: /[\u0000]/g,
    re: (value, limit) => value.slice(0, limit),
  };
  const contextified = vm.createContext(context);
  for (const source of [interactiveGateSource, providerModeSource, nonessentialTrafficGateSource,
    feedbackGateSource, fablePrefixSource, fableDefaultSource, fableCatalogMatcherSource,
    fableAliasSource,
    refusalCategoryPredicateSource, modelHelpSource, specialCyberCopySource,
    refusalCategoryCopyAndDetailsSource, familyFirstTimeCopySource,
    apiTextSanitizerSource, requestIdFilterSource]) {
    vm.runInContext(source, contextified);
  }
  vm.runInContext(`${refusalRowSource}\nglobalThis.formatNativeRow = $dt;`, contextified);
  return contextified;
}

const cases = [
  {
    name: 'first-party interactive sonnet cyber refusal',
    modelEligible: true,
    model: 'claude-sonnet-5',
    displayLabel: 'Claude Sonnet 5',
    provider: 'firstParty',
    interactive: true,
    feedbackAllowed: true,
    modelFamilies: {'claude-sonnet-5': 'sonnet'},
    category: 'cyber',
    requestId: 'req_289',
    expected: "API Error: Claude Sonnet 5's safeguards flagged this session (https://www.anthropic.com/legal/aup). You may be seeing this for the first time on a Sonnet model: Claude Sonnet 5 is more capable and has stronger safeguards as a result, which can sometimes flag non-cybersecurity work. We're improving these safeguards to reduce the amount of incorrectly flagged messages. Claude Code can't respond to your last message with Claude Sonnet 5.\n\nDouble press esc to edit your last message, or try a different model with /model.\n\nSend feedback with /feedback or learn more: https://support.claude.com/en/articles/8106465\n\nDetails: `[cyber]`\n\nRequest ID: req_289",
  },
  {
    name: 'non-interactive first-party cyber with no displayable model',
    modelEligible: false,
    model: 'claude-sonnet-5',
    displayLabel: 'Claude Sonnet 5',
    provider: 'firstParty',
    interactive: false,
    feedbackAllowed: true,
    category: 'cyber',
    requestId: 'req_289',
    expected: "API Error: Claude Sonnet 5's safeguards flagged this message. Our intentionally broad safeguards allow us to deliver more capabilities faster, but can sometimes flag legitimate cybersecurity work. Apply to the Cyber Verification Program to reduce these interruptions. Learn more: https://support.claude.com/en/articles/14604842-real-time-cyber-safeguards-on-claude\n\nDetails: `[cyber]`\n\nRequest ID: req_289",
  },
  {
    name: 'non-interactive Bedrock bio with feedback disabled and no model',
    modelEligible: false,
    model: null,
    displayLabel: 'unused',
    provider: 'bedrock',
    interactive: false,
    feedbackAllowed: false,
    category: 'bio',
    requestId: 'bad id',
    expected: "API Error: Claude can't help with this. Start a new session to continue.\n\nLearn more: https://www.anthropic.com/legal/aup\n\nDetails: `[bio]`",
  },
  {
    name: 'interactive mode with nonessential traffic disabled suppresses feedback CTA',
    modelEligible: false,
    model: null,
    displayLabel: 'unused',
    provider: 'bedrock',
    interactive: true,
    feedbackAllowed: true,
    disableNonessentialTraffic: true,
    category: null,
    requestId: null,
    expected: "API Error: Claude can't help with this. Start a new session to continue.\n\nLearn more: https://www.anthropic.com/legal/aup",
  },
  {
    name: 'eligible Fable model uses the Fable help page',
    modelEligible: true,
    model: 'claude-fable-5-1',
    displayLabel: 'Claude Fable 5.1',
    provider: 'foundry',
    interactive: true,
    feedbackAllowed: false,
    category: 'bio',
    requestId: null,
    expected: "API Error: Claude Fable 5.1's safeguards flagged this message (https://www.anthropic.com/legal/aup). Our intentionally broad safeguards allow us to deliver more capabilities faster, but can sometimes flag legitimate biology tasks. Claude Code can't respond to this message with Claude Fable 5.1.\n\nDouble press esc to edit your last message, or try a different model with /model.\n\nYou can learn more: https://support.claude.com/en/articles/15363606\n\nDetails: `[bio]`",
  },
  {
    name: 'eligible Opus 5 effort alias uses the Opus help page',
    modelEligible: true,
    model: 'claude-opus-5[1m]',
    displayLabel: 'Claude Opus 5',
    provider: 'firstParty',
    interactive: false,
    feedbackAllowed: true,
    category: 'bio',
    requestId: 'req_opus',
    expected: "API Error: Claude Opus 5's safeguards flagged this message (https://www.anthropic.com/legal/aup). Our intentionally broad safeguards allow us to deliver more capabilities faster, but can sometimes flag legitimate biology tasks. Claude Code can't respond to this message with Claude Opus 5.\n\nTry rephrasing the request in a new session or change your model.\n\nLearn more: https://support.claude.com/en/articles/16049681\n\nDetails: `[bio]`\n\nRequest ID: req_opus",
  },
];

const outputs = cases.map(testCase => {
  const context = makeSourceVm(testCase);
  const row = context.formatNativeRow('refusal', {
    type: 'refusal',
    category: testCase.category,
    explanation: null,
    fallback_credit_token: null,
    fallback_has_prefill_claim: null,
    recommended_model: null,
  }, testCase.requestId, testCase.model);
  assert.equal(row.content, testCase.expected, testCase.name);
  assert.equal(row.error, 'invalid_request', testCase.name);
  assert.equal(row.requestId, testCase.requestId ?? undefined, testCase.name);
  assert.equal(row.message.stop_reason, 'refusal', testCase.name);
  assert.equal(row.message.stop_details.category, testCase.category, testCase.name);
  return {name: testCase.name, content: row.content, error: row.error,
    requestId: row.requestId ?? null, stop_reason: row.message.stop_reason};
});

const output = {
  oracle: 'Claude Code 2.1.289 refusal fallback text projection',
  native_binary_sha256: sha256(binary),
  execution_scope: 'Exact $dt and text-helper source executed with explicit model eligibility, display-label, family, provider, interactive, and feedback facts. No Native CLI/provider/network path executed.',
  source_windows: sourceWindows,
  observations: {
    refusal_row: 'API error row, invalid_request, stop_reason refusal, stop_details is the supplied refusal object; explanation is null at the actual server-fallback callsite.',
    model_gate: 'eYe(servingModel) controls the model-labelled refusal branch. Its catalog/settings-backed KI decision is pinned but supplied as modelEligible in this formatter probe.',
    help_links: 'The generic help link is selected by b1r from the serving-model identity: Opus 5 routes to /16049681, Fable identities/aliases and the configured default Fable route to /15363606, and other or absent identities to /8106465.',
    first_party_copy: 'The no-displayable-model cyber branch is special only for provider kind firstParty, anthropicAws, anthropicGoogleCloud, or gateway; the provider and interactive/feedback facts are injected in this probe.',
    family_copy: 'Native family lookup enables first-time wording for the configured sonnet family on cyber; claude-opus-5-5 is an additional explicit first-time model. Actual family lookup is injected from the dynamic model catalog.',
    output_accumulator: 'je receives projected user rows from completed tool results and attachment rows; tool-returned newMessages are queued as completed executor rows, so whole-je cleanup is broader than raw tool_result.',
  },
  outputs,
};
process.stdout.write(`${JSON.stringify(output, null, 2)}\n`);

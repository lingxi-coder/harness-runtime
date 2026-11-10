#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import fs from 'node:fs';
import {stripVTControlCharacters} from 'node:util';
import vm from 'node:vm';

const binaryPath = process.argv[2];
assert(binaryPath, 'Usage: refusal_api_text_319_oracle.mjs <official 2.1.319 binary> [--check]');
const fd = fs.openSync(binaryPath, 'r');
const hash = createHash('sha256'), block = Buffer.alloc(1024 * 1024);
for (let at = 0, size; (size = fs.readSync(fd, block, 0, block.length, at)); at += size) hash.update(block.subarray(0, size));
const nativeSha256 = hash.digest('hex');
assert.equal(nativeSha256, '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd');
const base = 259144680, bytes = Buffer.alloc(274727937 - base);
assert.equal(fs.readSync(fd, bytes, 0, bytes.length, base), bytes.length);
fs.closeSync(fd);
const sourceByteRanges = [];
function take(name, start, end, after = base) {
  const begin = bytes.indexOf(start, after - base);
  assert(begin >= 0, `Missing ${name} start`);
  const finish = bytes.indexOf(end, begin + Buffer.byteLength(start));
  assert(finish > begin, `Missing ${name} end`);
  const source = bytes.subarray(begin, finish);
  sourceByteRanges.push({name, start: base + begin, endExclusive: base + finish, sha256: createHash('sha256').update(source).digest('hex')});
  return source.toString('utf8');
}
function span(name, start, end) {
  const source = bytes.subarray(start - base, end - base);
  sourceByteRanges.push({name, start, endExclusive: end, sha256: createHash('sha256').update(source).digest('hex')});
  return source.toString('utf8');
}
const catalog = vm.runInNewContext(`${span('baked native catalog', 260742015, 260762295)};kAo`);
const nativeGate = take('fmt and complete Xp gate', 'function fmt(e){', 'function Jp(e,n)', 264940000);
const nativeCategory = take('category copy, sanitization and first-time model copy', 'var bbs={bio:', 'function em(e,n,r)', 264944000);
const nativeE1i = take('no-fallback cyber copy', 'E1i="Our intentionally broad safeguards', ',k1i=', 264943000);
const nativeRefusal = take('complete terminal refusal builder e9t', 'function e9t(e,n,r,s,h){', 'function Hor(e,n,r,s)', 271104000);
const nativeIds = take('Nln request/message identifier filter', 'function Nln(e){', 'var t9t=', 271108000);
const nativeMessages = take('native assistant message builders', 'function Lpo({content:', 'function Yct(e)', 269336381);
const nativeTruncate = take('Q code-unit cap and surrogate-safe boundary', 'function Q(t,n){', 'function Pa(t,n)', 259200000);
const nativeWellFormed = take('native removal of lone surrogates', 'var E=/[\\uD800-', 'function Wnt(t)', 259201000);
const nativeControlClass = take('native Unicode control class', 'var Bjt=', 'function Bnt(e)', 259214000);
const nativeControlRegex = take('native control sanitizer regex', 'var ie=new RegExp(', ',ce=', 259215000);
const nativeSanitize = take('vme/Si native details sanitizer', 'function vme(e){', 'function ZFr(e,', 259219000);
const nativeVerification = take('cyber verification copy and schema', 'var _nr={a:', 'var knr=', 271043000);

const state = {};
const canonical = model => model?.replace(/\[1m\]/gi, '');
const modelRow = model => catalog.models.find(row => row.id === canonical(model));
const schema = parse => ({safeParse(value) {try {return {success: true, data: parse(value)};} catch {return {success: false};}}});
const literal = expected => schema(value => {assert.equal(value, expected); return value;});
const object = fields => schema(value => {assert(value && typeof value === 'object'); for (const [key, field] of Object.entries(fields)) assert(field.safeParse(value[key]).success); return value;});
const context = vm.createContext({
  state, Buffer, String, Bun: {stripANSI: stripVTControlCharacters},
  sd: modelRow, rt: canonical, Dp: canonical, vU: canonical,
  Rw: (model, capability) => capability === 'refusal_fallback' && state.capability !== null ? state.capability : modelRow(model)?.capabilities.includes(capability) ? true : undefined,
  jx: (capability, model) => capability === 'refusal_fallback' && state.capability !== null ? state.capability : capability === 'no_refusal_fallback' ? state.noRefusalFallback : modelRow(model)?.capabilities.includes(capability) ?? false,
  _zi: model => model.startsWith('claude-mythos-'), fAr: () => false,
  iQe: model => model.startsWith('claude-fable-'), d2: () => false, Zi: model => canonical(model) === 'claude-opus-5-5',
  BZe: () => state.fableCopySuppressed, nks: () => false, tte: () => false,
  P2: model => !!modelRow(model), Bu: () => ['FirstParty', 'AnthropicAws', 'AnthropicGoogleCloud', 'Gateway'].includes(state.provider),
  Yp: () => state.nonFirstPartyFallback ? 'claude-opus-4-8' : undefined,
  hOs: () => 'claude-opus-5', WSe: model => model.startsWith('claude-opus-'),
  Kp: () => false, eNe: () => false,
  Os: () => state.label, ye: () => !state.interactive, tO: () => state.feedbackEligible,
  ud: () => state.fableCopySuppressed,
  zpo: () => state.helpUrl,
  G7e: () => `${state.feedbackEligible ? 'Send feedback with /feedback or learn more' : 'You can learn more'}: ${state.helpUrl}`,
  BQe: undefined, Up: () => state.lifeSciencesSuffix ?? '',
  fkr: category => category === 'cyber' || category === 'bio',
  Qp: () => state.diskless,
  hd: 'API Error', ien: 'https://support.claude.com/en/articles/14604842-real-time-cyber-safeguards-on-claude', CJ: 255,
  Ocn: value => value,
  Zn: stripVTControlCharacters,
  p: thunk => () => thunk(), u: object, C: literal,
  B: values => schema(value => {assert(values.includes(value)); return value;}),
  Lr: (_key, schemas) => schema(value => {assert(schemas.some(candidate => candidate.safeParse(value).success)); return value;}),
  l() {}, g() {}, f: value => value, T_e: value => value, _e: value => value,
  DLn: () => false, fGe: () => 'fixture', hmo: () => false,
  Kk: () => `fixture-${++state.id}`, Mi: '<synthetic>', If: '(no content)',
});
// The entry point, gates, copy, sanitizer, verification helper and assistant
// message constructors execute verbatim. Only host facts and schema primitives
// are controlled. No provider/account request or CLI execution takes place.
const api = vm.runInContext(`(()=>{
  ${nativeGate}; ${nativeCategory}; var ${nativeE1i};
  ${nativeTruncate}; ${nativeWellFormed}; ${nativeControlClass}; ${nativeControlRegex}; ${nativeSanitize};
  ${nativeVerification}; ${nativeIds}; ${nativeMessages}; ${nativeRefusal};
  return {eligible: fmt, render: e9t, sanitize: e => Si(e,{drop:/[\x60\\[\\]]/g,maxCodeUnits:CJ})};
})()`, context);
const cases = [];
function add(name, options = {}) {
  const input = {model: 'claude-sonnet-4-5', label: 'Sonnet 4.5', provider: 'FirstParty', interactive: false,
    feedbackEligible: true, fableCopySuppressed: false, capability: null, noRefusalFallback: false,
    nonFirstPartyFallback: false, diskless: false, helpUrl: 'https://support.claude.com/en/articles/8106465',
    requestId: null, messageId: null, details: null, ...options};
  Object.assign(state, input, {id: 0});
  const details = input.categoryUtf16 === undefined ? input.details
    : {...input.details, category: String.fromCharCode(...input.categoryUtf16)};
  const row = api.render('refusal', details, input.requestId, input.model, input.messageId);
  const expected = {content: row.message.content[0].text, error: row.error, stopReason: row.message.stop_reason,
    requestId: row.requestId ?? null, refusedMessageId: row.refusedMessageId ?? null, stopDetails: row.message.stop_details};
  if (input.categoryUtf16 !== undefined) {
    input.sourceStopDetails = JSON.stringify(details);
    expected.stopDetailsSource = JSON.stringify(expected.stopDetails);
    delete expected.stopDetails;
    expected.contentUtf16 = Array.from({length: expected.content.length}, (_, index) => expected.content.charCodeAt(index));
  }
  cases.push({name, input, modelEligible: input.model !== null && api.eligible(input.model), expected});
}
add('known display label without fallback capability uses the generic branch', {requestId: 'req_319', messageId: 'msg_319'});
add('no fallback cyber branch drops obsolete Apply clause', {details: {category: 'cyber'}, requestId: 'req_cyber', messageId: 'msg_cyber'});
add('no fallback cyber feedback action', {interactive: true, details: {category: 'cyber'}});
add('no fallback cyber disabled feedback', {interactive: true, feedbackEligible: false, details: {category: 'cyber'}});
add('labelled eligible bio copy', {model: 'claude-opus-5', label: 'Opus 5', helpUrl: 'https://support.claude.com/en/articles/16049681', details: {category: 'bio'}, requestId: 'req_bio', messageId: 'msg_bio'});
add('labelled eligible generic copy', {model: 'claude-opus-5', label: 'Opus 5', interactive: true, helpUrl: 'https://support.claude.com/en/articles/16049681'});
add('first-time sonnet cyber copy', {model: 'claude-sonnet-5-5', label: 'Sonnet 5.5', interactive: true, details: {category: 'cyber'}});
add('sonnet bio uses ordinary category copy', {model: 'claude-sonnet-5-5', label: 'Sonnet 5.5', details: {category: 'bio'}});
for (const category of ['cyber', 'bio', 'frontier_llm']) add(`first-time opus55 ${category} copy`, {model: 'claude-opus-5-5', label: 'Opus 5.5', helpUrl: 'https://support.claude.com/en/articles/16049681', details: {category}});
add('Fable first-time suppression', {model: 'claude-fable-5-1', label: 'Fable 5.1', fableCopySuppressed: true, interactive: true, feedbackEligible: false, helpUrl: 'https://support.claude.com/en/articles/15363606', details: {category: 'bio'}});
add('explicit refusal fallback denial overrides baked model flag', {model: 'claude-opus-5', label: 'Opus 5', capability: false, details: {category: 'bio'}});
add('non first-party provider needs resolved fallback', {model: 'claude-opus-5', label: 'Opus 5', provider: 'Bedrock', details: {category: 'cyber'}});
add('no serving model uses native generic label', {model: null, label: null, provider: 'Bedrock', details: {category: 'bio'}});
add('terminal explanations stay out of display text', {model: 'private-vendor-model', label: 'Private Vendor Model', capability: false, details: {category: 'other', explanation: 'provider detail'}});
add('details removes ANSI controls delimiters and whitespace', {details: {category: '\ufeff\u001b[31m[bio]`\u001b[0m\u0085\t x\u2800'}, requestId: 'bad id', messageId: 'bad/id'});
add('details category cap preserves a complete surrogate pair', {details: {category: 'x'.repeat(253) + '😀'}});
add('details category cap drops a split high surrogate', {details: {category: 'x'.repeat(254) + '😀'}});
add('identifier upper bound is accepted', {requestId: 'r'.repeat(255), messageId: 'm'.repeat(255)});
add('oversized identifiers are omitted independently', {requestId: 'r'.repeat(256), messageId: 'msg_ok'});
add('empty request and unicode message identifier are omitted', {requestId: '', messageId: 'msg_é'});
for (const tier of ['a', 'b', 'c']) add(`active cyber verification ${tier}`, {interactive: true, details: {category: 'cyber', cyber_verification: {type: 'active', tier}}});
add('active cyber verification noninteractive retains action', {details: {category: 'cyber', cyber_verification: {type: 'active', tier: 'c'}}});
for (const reason of ['not_assigned_but_organization_approved', 'organization_not_approved']) add(`inactive cyber verification ${reason}`, {details: {category: 'cyber', cyber_verification: {type: 'inactive', reason}}});
add('verification data on other categories is ignored', {details: {category: 'bio', cyber_verification: {type: 'active', tier: 'c'}}});
add('invalid verification tier is ignored', {details: {category: 'cyber', cyber_verification: {type: 'active', tier: 'unknown'}}});
add('source lone high surrogate is removed from Details', {categoryUtf16: [0xd800]});
add('source lone low surrogate is removed from Details', {categoryUtf16: [0xdc00]});
add('malformed category never becomes the cyber discriminator', {categoryUtf16: [...'cyber'].map(ch => ch.charCodeAt(0)).concat(0xd800)});
add('source replacement character stays distinct from lone units', {categoryUtf16: [65, 0xd800, 0xfffd, 0xdc00, 66], messageId: 'msg_source'});
add('source ANSI category keeps exact source until after stripping', {categoryUtf16: [27, 91, 51, 49, 109, 65, 0xd800, 66, 27, 91, 48, 109]});
const gateCases = ['claude-sonnet-4-5', 'claude-sonnet-5', 'claude-sonnet-5-5', 'claude-opus-4-8', 'claude-opus-5', 'claude-opus-5-5', 'claude-fable-5-1', 'claude-mythos-5-1', 'unknown-model'].map(model => {
  Object.assign(state, {model, provider: 'FirstParty', capability: null, noRefusalFallback: false, fableCopySuppressed: false});
  return {model, eligible: api.eligible(model)};
});
const output = {version: '2.1.319', native_sha256: nativeSha256, sourceByteRanges,
  scope: 'Actual-source e9t, fmt/Xp, first-time/category text, Si/Q, BQe, Nln and native assistant message constructors. Canonical identity, served capability observations, provider/default fallback, help URL, feedback, diskless and Life Sciences suffix are controlled host inputs. Node ANSI stripping is used only for the retained standard SGR vector. Schema primitives mirror literal/object/union validation. No live account, provider response, Life Sciences lookup, or Claude CLI execution is asserted.',
  cases, gateCases};
const fixture = new URL('../../crates/core/tests/fixtures/refusal_api_text_2_1_319.json', import.meta.url);
const encoded = JSON.stringify(output, null, 2) + '\n';
if (process.argv.includes('--check')) assert.equal(fs.readFileSync(fixture, 'utf8'), encoded);
else fs.writeFileSync(fixture, encoded);
console.log(`${process.argv.includes('--check') ? 'checked' : 'wrote'} ${cases.length} terminal refusal cases and ${gateCases.length} native eligibility cases`);

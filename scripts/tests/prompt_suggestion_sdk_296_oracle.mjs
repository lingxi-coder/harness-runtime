// Execute the unmodified SDK controller embedded in the 2.1.296 native binary.
// Its provider function is a controlled promise: no model/network call occurs.
import fs from 'node:fs';
import crypto from 'node:crypto';
import vm from 'node:vm';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';

const binaryPath = process.env.CLAUDE_REFERENCE_BINARY
  ?? `${process.env.HOME}/.local/share/claude/versions/2.1.296`;
const binary = fs.readFileSync(binaryPath);
const binarySha = crypto.createHash('sha256').update(binary).digest('hex');
assert.equal(binarySha, 'c9b5341637becbd423ddffc5b254afb645682a3868cb708bbc6cc0e7bb419937');
const start = binary.indexOf('var WE=new Set(["scheduled","task_notification","auto_continuation","host_synthetic","system"])', 220000000);
const end = binary.indexOf('class nf{', start);
const controller = binary.subarray(start, end).toString();
const feedbackStart = binary.indexOf('function X$o(e,n,t,o){', 200000000);
const feedbackEnd = binary.indexOf('function ZH(', feedbackStart);
const feedback = binary.subarray(feedbackStart, feedbackEnd).toString();
assert(start > 0 && end > start && feedbackStart > 0 && feedbackEnd > feedbackStart);
const serializerStart = binary.indexOf('var o=/\\u2028|\\u2029/g;function sme(n){', 210000000);
const serializerEnd = binary.indexOf('\nexport{sme};', serializerStart);
assert(serializerStart > 0 && serializerEnd > serializerStart);
const serializer = binary.subarray(serializerStart, serializerEnd).toString();
const serialize = vm.runInNewContext(`${serializer}\nsme`, { _: JSON.stringify });

const after = (shouldQuery = true) => ({ op: 'after', shouldQuery });
const generated = (suggestion = 'run the tests', requestId = 'req-1', index = 0) =>
  ({ op: 'generated', suggestion, requestId, index });
const command = (content, shouldQuery = true, mode = 'prompt') =>
  ({ op: 'command', content, shouldQuery, mode });
const cases = [
  { name: 'accepted-case-insensitive', actions: [after(), generated(), { op: 'time', now: 1123 }, command('RUN THE TESTS')] },
  { name: 'ignored-utf16-length', actions: [after(), generated('😀 run'), { op: 'time', now: 1100 }, command('😀')] },
  { name: 'first-text-block-only', actions: [after(), generated(), command([{ type: 'image' }, { type: 'text', text: 'run the tests' }, { type: 'text', text: 'extra' }])] },
  { name: 'no-text-clears-attribution', actions: [after(), generated(), command([{ type: 'image' }]), command('run the tests')] },
  { name: 'local-command-does-not-cancel', actions: [after(), command('/compact', false), generated(), command('run the tests')] },
  { name: 'new-prompt-aborts-generation', actions: [after(), command('another query'), generated()] },
  { name: 'nonprompt-retains-attribution', actions: [after(), generated(), command('notification', true, 'notification'), command('run the tests')] },
  { name: 'later-generation-wins', actions: [after(), after(), generated('obsolete', 'req-1', 0), generated('run the tests', 'req-2', 1)] },
  { name: 'disabled-after-result-keeps-inflight', actions: [after(), { op: 'enabled', value: false }, after(), generated()] },
  { name: 'environment-false', environmentDisabled: true, actions: [after()] },
  { name: 'disabled-option', enabled: false, actions: [after()] },
  { name: 'shutdown', shuttingDown: true, actions: [after()] },
  { name: 'nonquery-result', actions: [after(false)] },
  { name: 'pause-rollout-off', actions: [{ op: 'pause', value: true }, after(), generated()] },
  { name: 'pause-rollout-on', sable: true, actions: [{ op: 'pause', value: true }, after(), { op: 'pause', value: false }, after(), generated()] },
  { name: 'pause-does-not-abort-inflight', sable: true, actions: [after(), { op: 'pause', value: true }, generated()] },
  { name: 'paused-next-result-aborts-inflight', sable: true, actions: [after(), { op: 'pause', value: true }, after(), generated()] },
  { name: 'negative-clock-clamps', actions: [after(), generated(), { op: 'time', now: 10 }, command('run the tests')] },
  { name: 'empty-request-id-omitted', actions: [after(), generated('run the tests', ''), command('run the tests')] },
  { name: 'held-release-clock', holdsResults: true, actions: [after(), generated(), { op: 'time', now: 1200 }, { op: 'release' }, { op: 'time', now: 1245 }, command('run the tests')] },
  { name: 'held-new-prompt-discards', holdsResults: true, actions: [after(), generated(), command('other'), { op: 'release' }] },
  { name: 'discard-clears-attribution', actions: [after(), generated(), { op: 'discard' }, command('run the tests')] },
  { name: 'discard-aborts', actions: [after(), { op: 'discard' }, generated()] },
  { name: 'empty-generation', actions: [after(), { op: 'generated', empty: true, index: 0 }] },
  { name: 'literal-line-separators', actions: [after(), generated('go\u2028now\u2029please'), command('go\u2028now\u2029please')] },
  { name: 'json-string-escapes', actions: [after(), generated('say "next" \\ path'), command('say "next" \\ path')] },
  ...['scheduled', 'task_notification', 'auto_continuation', 'host_synthetic', 'system', 'user', 'sdk', ''].flatMap(origin => [false, true].map(sable => ({
    name: `origin-${origin || 'empty'}-${sable}`, sable, origin, actions: [after(), ...(sable && ['scheduled', 'task_notification', 'auto_continuation', 'host_synthetic', 'system'].includes(origin) ? [] : [generated()])],
  }))),
];

async function nativeCase(test) {
  let now = 1000, counter = 0, enabled = test.enabled ?? true;
  const requests = [], frames = [], events = [];
  const context = vm.createContext({
    AbortController, setTimeout, clearTimeout,
    Date: class extends Date { static now() { return now; } },
    a: { CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION: test.environmentDisabled ? false : undefined },
    k: () => test.sable ?? false, c: error => { throw error; }, G: value => value,
    K: () => 'session-fixture', $E: () => `frame-${++counter}`,
    S: value => value, ye: value => value,
    i: (name, payload) => events.push({ name, payload }),
    ZH: (reason, source) => events.push({ name: 'tengu_prompt_suggestion', payload: { source, outcome: 'suppressed', reason, prompt_id: 'user_intent' } }),
  });
  const Controller = vm.runInContext(`${feedback}\n${controller}\nHS`, context);
  const owner = new Controller({
    emit: frame => frames.push(serialize(frame) + '\n'),
    promptSuggestions: () => enabled, shuttingDown: () => test.shuttingDown ?? false,
    holdsResults: () => test.holdsResults ?? false, cacheSafeParams: () => ({}),
    turnOrigin: () => test.origin,
    generate: (abort, _params) => new Promise(resolve => requests.push({ abort, resolve })),
  });
  for (const action of test.actions) {
    if (action.op === 'after') owner.afterResult({ shouldQuery: action.shouldQuery });
    else if (action.op === 'generated') requests[action.index].resolve(action.empty ? null : { suggestion: action.suggestion, generationRequestId: action.requestId });
    else if (action.op === 'command') owner.onCommand({ shouldQuery: action.shouldQuery, mode: action.mode }, action.content);
    else if (action.op === 'pause') owner.setPaused(action.value);
    else if (action.op === 'enabled') enabled = action.value;
    else if (action.op === 'time') now = action.now;
    else if (action.op === 'release') owner.emitPending();
    else if (action.op === 'discard') owner.discard();
    await Promise.resolve();
    await Promise.resolve();
  }
  return { ...test, expected: { frames, events: JSON.parse(JSON.stringify(events)), generations: requests.length, aborted: requests.map(request => request.abort.signal.aborted) } };
}

const fixture = {
  version: '2.1.296', binarySha256: binarySha, controllerOffset: start,
  controllerSha256: crypto.createHash('sha256').update(controller).digest('hex'),
  settleTimeoutMs: vm.runInNewContext(`${controller}\nKE`),
  serializerOffset: serializerStart,
  serializerSha256: crypto.createHash('sha256').update(serializer).digest('hex'),
  feedbackOffset: feedbackStart, cases: await Promise.all(cases.map(nativeCase)),
};
const target = fileURLToPath(new URL('../../crates/runtime/tests/fixtures/prompt_suggestion_sdk_2_1_296.json', import.meta.url));
const output = JSON.stringify(fixture, null, 2) + '\n';
if (process.argv.includes('--check')) assert.equal(fs.readFileSync(target, 'utf8'), output);
else { fs.mkdirSync(new URL('../../crates/runtime/tests/fixtures/', import.meta.url), { recursive: true }); fs.writeFileSync(target, output); }
console.log(`Claude Code 2.1.296 SDK suggestion controller: ${cases.length} native cases ${process.argv.includes('--check') ? 'verified' : 'recorded'}`);

// Execute the pure suggestion functions embedded in the pinned native binary.
// Only the telemetry callback is stubbed; no model/network/credentials are used.
import fs from 'node:fs';
import vm from 'node:vm';
import crypto from 'node:crypto';
const args = process.argv.slice(2);
const bytes = fs.readFileSync(args.find(a => !a.startsWith('--')) ?? `${process.env.HOME}/.local/share/claude/versions/2.1.296`);
const sha = crypto.createHash('sha256').update(bytes).digest('hex');
if (sha !== 'c9b5341637becbd423ddffc5b254afb645682a3868cb708bbc6cc0e7bb419937') throw Error(`Unexpected binary: ${sha}`);
const offset = bytes.indexOf('[SUGGESTION MODE:');
const start = bytes.lastIndexOf('// @bun @bytecode\n', offset);
const source = bytes.subarray(start, bytes.indexOf(0, offset)).toString('utf8');
function cut(a, z) {
  const begin = source.indexOf(a), end = source.indexOf(z, begin);
  if (begin < 0 || end < 0) throw Error(`Missing native function ${a}`);
  return source.slice(begin, end);
}
const context = { reason: null, candidates: [], ZH(reason) { context.reason = reason; }, k: () => false,
  g() {}, async I() { return { candidates: context.candidates, generationRequestId: 'req_native' }; } };
vm.runInNewContext(cut('var R=', 'function qIe(') + cut('var C=', ';async function I(') + ';' +
  cut('function W(', 'async function N(') + cut('async function N(', 'function q(') + cut('function q(', 'function X$o(') +
  ';globalThis.oracle = { prompt: M, normalize: W, generate: N, suppress: q, cacheCold: O };', context);
const { oracle } = context;
const promptPath = 'crates/orchestrator/src/conversation/prompt_suggestion_prompt.txt';
if (fs.readFileSync(promptPath, 'utf8') !== oracle.prompt) throw Error('Prompt bytes differ from native');
const raw = [
  '', ' ', 'yes', 'Yes', 'commit', 'continue', 'go ahead', 'run the tests', '/commit', '/x', 'x',
  '建议：继续测试', '回复:运行测试', '提案：テストを実行', '제안：테스트 실행',
  '回答：完了しました', '完成', '完成了', '완료', '완료됨', 'done', 'Done.',
  '沉默', '沈黙', '静默', '침묵', '提案なし', '特にありません', '没有建议', '無建議', '沒建議', '제안 없음',
  '（继续测试）', '［go ahead］', '【测试】', '〔test it〕', '[run tests]', '(run tests)',
  '继续', '好', 'テスト', '한글', '运行测试。然后提交', 'テスト！次に', 'run tests. Then commit',
  '谢谢', '谢谢你的帮助', '感谢您', 'ありがとう', 'ありがとうございます', '감사합니다', '고마워요',
  '完美', '看起来不错', 'いいですね', '좋네요', 'thanks for that', 'push nice changes',
  '让我看看', '让我们测试', '我来测试', '我会处理', '我将处理', 'you can run tests',
  'API Error: 500', 'prompt is too long', 'nothing found.', 'no suggestion', 'silence is best', 'silence',
  'User: run tests', '用户: 继续测试', 'run\ntests', 'run *tests*',
  '<suggestion>run tests</suggestion>', '<SUGGESTION>建议：继续测试</suggestion>',
  '<response> <response>run tests</response> </response>', 'Suggestion: go ahead',
  'a '.repeat(13), '中'.repeat(25), 'x'.repeat(99), 'x'.repeat(100), '🚀'.repeat(49) + ' go',
  '\ufeffrun tests\ufeff', '\u0085run tests\u0085', 'Suggestion\ufeff:\ufeffrun tests',
  'Suggestion\u0085:\u0085run tests', 'go\ufeffahead', 'go\u0085ahead', 'é: run tests', 'ſuggestion: run tests', '<ſuggestion>run tests</ſuggestion>', 'ſure',
  '(go\rahead)', '(go\u2028ahead)', 'ſilence is best', 'ſounds good',
];
for (const s of ['run tests', '继续测试', 'silence', 'Suggestion: run tests', 'a b']) {
  for (const whitespace of ['\u00a0', '\u1680', '\u2028', '\u2029', '\u202f', '\u3000', '\ufeff', '\u0085']) raw.push(whitespace+s+whitespace);
}
const cases = raw.map(raw => {
  context.reason = null;
  const normalized = oracle.normalize(raw);
  const suppressed = oracle.suppress(normalized);
  return { raw, normalized, reason: context.reason, expected: suppressed ? null : normalized };
});
const candidateCases = [];
for (const candidates of [[], [''], ['', 'run tests'], ['  ', 'Suggestion:', 'go ahead'],
  ['thanks for that', 'run tests'], ['done', 'go ahead'], ['<response></response>', 'run tests'],
  ['go ahead', 'run tests'], ['\ufeff', '建议：继续测试']]) {
  context.candidates = candidates;
  context.reason = null;
  const { suggestion } = await oracle.generate({}, {});
  const suppressed = suggestion === null || oracle.suppress(suggestion);
  candidateCases.push({ candidates, selected: suggestion, reason: context.reason,
    expected: suppressed ? null : suggestion });
}
const coldCases = [[0,0,0],[10000,0,0],[10001,0,0],[5000,5000,0],[5000,5001,0],[0,0,10001]]
 .map(([input,output,write])=>({ input,output,write,expected:oracle.cacheCold({message:{usage:{input_tokens:input,output_tokens:output,cache_creation_input_tokens:write}}}) }));
const fixture = { version:'2.1.296', binary_sha256:sha, prompt_sha256:crypto.createHash('sha256').update(oracle.prompt).digest('hex'), cases, coldCases, candidateCases };
const path = 'crates/orchestrator/tests/fixtures/prompt_suggestion_2_1_296.json';
const json = JSON.stringify(fixture, null, 2)+'\n';
if (args.includes('--check')) { if (fs.readFileSync(path,'utf8') !== json) throw Error('Oracle fixture differs'); }
else { fs.mkdirSync('crates/orchestrator/tests/fixtures',{recursive:true}); fs.writeFileSync(path,json); }
console.log(`Native ${fixture.version}: ${cases.length} normalization/filter cases; prompt bytes identical`);

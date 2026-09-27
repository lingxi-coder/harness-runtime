// Regenerate deterministic compact goldens from the pinned official release.
// Usage: node scripts/compact_prompt_oracle.mjs /path/to/2.1.261/claude [--check]
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const sha256 = createHash('sha256').update(binary).digest('hex');
const expectedSha = '5efecaff231b798be3c66def9be54183623b328b80eaef17f93c43987024e82a';
if (sha256 !== expectedSha) throw new Error(`Unexpected oracle SHA-256: ${sha256}`);
const start = binary.lastIndexOf('var CCo=`Your task is to create a detailed summary');
const end = binary.indexOf('async function PCo(', start);
if (start < 0 || end < start || end - start > 30_000) throw new Error('Prompt region not found');
const oracle = vm.runInNewContext(
  `${binary.subarray(start, end).toString('utf8')}; ({ base: CCo, prompt: F0e, format: RCo, continuation: ice })`,
  Object.create(null),
  { timeout: 1_000 },
);

const rawSummaries = [
  '', '  plain summary  ', '<analysis>draft</analysis>\n<summary>保留用户要求 🧭</summary>',
  'before\n<analysis>one</analysis>\n<analysis>two</analysis>\n<summary>\n a\n\n\nb \n</summary>\nafter',
  '<summary>first</summary>\n<summary>second</summary>', '<summary>unclosed',
  '\uFEFF<summary>\uFEFF中文\uFEFF</summary>\uFEFF',
  '\u0085<summary>\u0085kept\u0085</summary>\u0085',
  'before<summary>$$ $& $` $\' $1 $<name></summary>after',
  '<analysis>scratch</analysis>left<summary>$` + $\'</summary>right',
];
const fixture = {
  version: '2.1.261', sha256, sourceByteRange: [start, end],
  basePrompt: oracle.base,
  prompts: [null, '', ' \n\t', '\uFEFF', '\u0085', ' Focus on Rust 🧭\nPreserve tests. '].map(custom => ({
    custom, expected: oracle.prompt(custom ?? undefined),
  })),
  summaries: rawSummaries.map(raw => ({ raw, expected: oracle.format(raw) })),
  continuations: [undefined, '', '/tmp/会话.jsonl'].flatMap(transcriptPath =>
    [false, true].flatMap(suppressFollowUpQuestions => [false, true].map(recentMessagesPreserved => {
      const options = { transcriptPath, suppressFollowUpQuestions, recentMessagesPreserved };
      return { ...options, raw: '<summary> Continue 🧭 </summary>', expected: oracle.continuation('<summary> Continue 🧭 </summary>', options) };
    })),
  ),
};
const output = `${JSON.stringify(fixture, null, 2)}\n`;
const path = fileURLToPath(new URL('../crates/compaction/tests/fixtures/claude_2_1_261_prompt.json', import.meta.url));
if (process.argv.includes('--check')) {
  if (readFileSync(path, 'utf8') !== output) throw new Error('Compact oracle fixture differs');
  console.log(`Compact oracle fixture matches Claude Code ${fixture.version} (${sha256})`);
} else {
  writeFileSync(path, output);
  console.log(path);
}

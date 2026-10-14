// Regenerate total-token ledger cases from the hash-pinned native oracle.
// Usage: node scripts/tests/total_tokens_286_oracle.mjs /path/to/2.1.286 [--check]
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const sha256 = createHash('sha256').update(binary).digest('hex');
if (sha256 !== '75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433') {
  throw new Error(`Unexpected native oracle SHA-256: ${sha256}`);
}
const start = binary.indexOf('class wvt{', 183727495);
const end = binary.indexOf('var rnn=new At(', start);
if (start < 0 || end < start) throw new Error('Missing 2.1.286 total-token ledger');
const source = binary.subarray(start, end).toString('utf8');
const Ledger = vm.runInNewContext(`(()=>{${source};return wvt})()`, {
  Date, _no: [], i: () => {},
});
const cases = [
  {
    name: 'auto_compact_rolls_full_usage_then_new_context_starts_at_zero',
    actions: [
      { type: 'anchor', used: 100 },
      { type: 'usage', used: 1000 },
      { type: 'rollover', used: 1000 },
      { type: 'usage', used: 0 },
      { type: 'usage', used: 200 },
    ],
  },
  {
    name: 'clear_retains_root_ledger_until_next_user_reanchor',
    actions: [
      { type: 'anchor', used: 100 },
      { type: 'usage', used: 1000 },
      { type: 'rollover', used: 1000 },
      { type: 'usage', used: 0 },
      { type: 'usage', used: 50 },
      { type: 'anchor', used: 50 },
      { type: 'usage', used: 50 },
      { type: 'usage', used: 90 },
    ],
  },
  {
    name: 'manual_compact_has_no_rollover_high_water_mark_stays',
    actions: [
      { type: 'anchor', used: 100 },
      { type: 'usage', used: 1000 },
      { type: 'usage', used: 0 },
      { type: 'usage', used: 200 },
    ],
  },
];
for (const test of cases) {
  const ledger = new Ledger();
  for (const action of test.actions) {
    if (action.type === 'anchor') ledger.reanchorTaskBudget('main', action.used);
    if (action.type === 'rollover') ledger.rollOverContext('main', action.used);
    action.expectedUsed = ledger.cumulativeUsed('main', action.type === 'rollover' ? 0 : action.used);
  }
}
const fixture = JSON.stringify({
  version: '2.1.286', sha256,
  sourceByteRanges: [{ signature: 'class wvt{', start, end }], cases,
}, null, 2) + '\n';
const path = new URL('../../crates/orchestrator/tests/fixtures/total_tokens_286_oracle.json', import.meta.url);
if (process.argv.includes('--check')) {
  if (readFileSync(path, 'utf8') !== fixture) throw new Error('Total-token ledger fixture drift');
} else writeFileSync(path, fixture);
process.stdout.write(`${cases.length} total-token ledger oracle cases matched\n`);

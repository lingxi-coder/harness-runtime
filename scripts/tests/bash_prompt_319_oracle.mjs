#!/usr/bin/env node
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';

const binaryPath = process.argv[2];
assert(binaryPath, 'Usage: bash_prompt_319_oracle.mjs <official binary> [--check] [--capture-root <directory>]');
const fd = fs.openSync(binaryPath, 'r');
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const digest = createHash('sha256');
const block = Buffer.alloc(1024 * 1024);
for (let at = 0, size; (size = fs.readSync(fd, block, 0, block.length, at)); at += size) digest.update(block.subarray(0, size));
const binarySha256 = digest.digest('hex');
assert.equal(binarySha256, '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd');
const ranges = [];
function take(start, endExclusive, label) {
  const bytes = Buffer.alloc(endExclusive - start);
  assert.equal(fs.readSync(fd, bytes, 0, bytes.length, start), bytes.length);
  ranges.push({ label, start, endExclusive, sha256: hash(bytes) });
  return bytes.toString('utf8');
}

const sources = [
  take(270914167, 270918635, 'Krn complete standard Bash builder'),
  take(270910577, 270912980, 'LJo complete lean Bash builder'),
  take(270902870, 270909925, 'DJo complete standard git instructions'),
  take(270909925, 270910577, 'Des complete lean git instructions'),
  take(270901427, 270902870, 'Grn/Urn/jrn/Wrn background instructions'),
  take(270900439, 270901427, 'tW/Hrn/Brn shell adaptations'),
  take(270899948, 270900105, 'p9e/YEe communication and deferred attribution literals'),
  take(270899893, 270899948, 'Nrn/lne ordinary skill and default timeout projection'),
  take(270900105, 270900207, 'Frn/$rn attribution and git enablement'),
  take(270894844, 270895133, 'q5t GitHub mention guidance'),
  take(270895133, 270896001, 'OTn/Mrn/xJo precommit skill guidance'),
  take(270899514, 270899893, 'iPt/aPt/Vdr pull-request examples'),
  take(270918635, 270918940, 'Vrn working-directory guidance'),
  take(269443027, 269443117, 'Mk bullet formatting'),
  take(268063798, 268064040, '_On no-notification background guidance'),
  take(267464345, 267464713, 'ASr background timeout guidance'),
];
fs.closeSync(fd);

const state = { lean: false, monitor: false, attribution: 'deferred', includeGit: true };
const world = vm.createContext({
  state, i: { platform: 'darwin' },
  b0: ({ leanPrompt }) => leanPrompt, _5: () => false,
  wx: () => false, XEe: () => false, Nrn: () => null,
  xze: () => false, mce: () => state.monitor, WQe: () => false,
  GJt: () => true, M_: () => false, GQe: () => false, pf: () => false,
  Iao: () => false, m2i: () => false, rn: (_key, fallback) => fallback,
  qrn: () => null, chs: () => false, Ncn: () => false,
  Iz: () => 120000, KY: () => 600000, zJt: () => 600000, RDe: () => 7200000, Zdo: 7200000,
  Fdt: () => state.includeGit, FZt: () => state.attribution === 'deferred',
  vrn: async () => state.attribution === 'explicit'
    ? { commit: 'Co-Authored-By: Claude <noreply@anthropic.com>', pr: '🤖 Generated with [Claude Code](https://claude.com/claude-code)' }
    : { commit: '', pr: '' },
  kL: () => true, eW: () => false, Drn: () => null,
  l: () => {}, f: value => value, ITn: () => {}, yt: () => ({}),
  iX: 'code-review', y_e: 'verify', tDn: 'simplify', nDn: 'commit', rDn: 'pr',
  Zr: 'Glob', Sr: 'Grep', Ge: 'Read', ct: 'Edit', Xt: 'Write', Me: 'Bash', ft: 'Agent', p_: 'TaskCreate', nE: 'TodoWrite',
});
vm.runInContext(sources.join('\n'), world, { timeout: 1000 });

const cases = [];
for (const lean of [false, true]) for (const monitor of [false, true]) {
  for (const attribution of ['deferred', 'explicit', 'none']) {
    Object.assign(state, { lean, monitor, attribution });
    const output = await vm.runInContext('Krn("claude-opus-5",[],[],state.lean,false,false)', world, { timeout: 1000 });
    cases.push({ lean, monitor, attribution, output, bytes: Buffer.byteLength(output), sha256: hash(output) });
  }
}

let captureChecks = 0;
for (let index = 3; index < process.argv.length; index++) {
  if (process.argv[index] !== '--capture-root') continue;
  const root = process.argv[++index];
  assert(root, '--capture-root needs a directory');
  for (const model of ['claude-sonnet-4-6', 'claude-opus-5', 'claude-sonnet-5-5']) {
    const request = JSON.parse(fs.readFileSync(path.join(root, model, 'request-01.json'), 'utf8'));
    const actual = request.tools.find(tool => tool.name === 'Bash')?.description;
    assert.equal(typeof actual, 'string', `${model}: captured Bash definition missing`);
    const expected = cases.find(row => row.lean === (model !== 'claude-sonnet-4-6') && row.monitor && row.attribution === 'deferred');
    assert.equal(actual, expected.output, `${model}: native request disagrees with native source renderer`);
    captureChecks++;
  }
}

const output = JSON.stringify({
  version: '2.1.319', binarySha256, ranges, cases,
  controls: {
    shell: 'POSIX zsh on darwin; no macOS /bin/bash 3.2 compatibility note',
    prompts: 'Explicit per-request lean switch; leanRewrite=false; no embedded search/REPL/Bash-first mode; empty eligible skill list; no attached machines',
    timeouts: { foregroundDefault: 120000, foregroundMax: 600000, backgroundDefault: 600000, backgroundMax: 7200000 },
    background: 'Enabled; Monitor false/true are explicit runtime capability inputs; foreground sleep policy follows that capability',
    git: 'Enabled, TaskCreate available, precommit skill suggestions empty, alternate PR writing experiments absent',
    attribution: 'Deferred reminder references are the captured native default; explicit and empty values are controlled settings inputs',
    differences: [
      'LingXi supports its own sandbox configuration guidance, which is not present in this ordinary upstream Bash builder.',
      'Monitor guidance must follow actual runtime availability; LingXi defaults the Monitor flag off and mobile omits it.',
      'The native default defers attribution to a conversation system-reminder; preserve configured explicit attribution independently.',
    ],
  },
  scope: 'Complete Bash and git description renderers executed from digest-pinned native source ranges. Runtime shell, availability, settings and timeout facts are controlled inputs. Optional capture-root checks require byte equality with unmodified native request descriptions; no real provider request is dispatched.',
}, null, 2) + '\n';
const target = new URL('./fixtures/bash_prompt_319.json', import.meta.url);
if (process.argv.includes('--check')) assert.equal(fs.readFileSync(target, 'utf8'), output);
else fs.writeFileSync(target, output);
console.log(`Verified ${cases.length} complete Bash variants and ${captureChecks} captured native descriptions (${binarySha256})`);

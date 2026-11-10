#!/usr/bin/env node
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import vm from 'node:vm';

assert(process.argv[2], 'Usage: fork_subagent_319_oracle.mjs <official binary> [--check]');
const fd = fs.openSync(process.argv[2], 'r');
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
const source = take(271736036, 271736618, 'C_n/Xge/yPt fork availability and background gates');
const foregroundSource = take(259702832, 259702900, 'PC foreground-only host classification');
fs.closeSync(fd);

const state = {};
const world = vm.createContext({
  state, i: {},
  Er: () => state.cache,
  jft: () => state.coordinator,
  vg: () => ({ forkSubagentDisabled: state.runtimeDisabled }),
  ye: () => state.nonInteractive,
  rn: () => state.headlessCohortDisabled,
  Fp: () => state.sdkParent,
  n: () => ({ claudecode: state.claudecode }),
  _f: () => state.vscodeParent,
  l: () => {}, f: value => value,
});
vm.runInContext(source + foregroundSource, world, { timeout: 1000 });
const hosts = [
  { host: 'standard-cli', sdkParent: false, claudecode: false, vscodeParent: false },
  { host: 'sdk-parent', sdkParent: true, claudecode: false, vscodeParent: false },
  { host: 'vscode-parent', sdkParent: false, claudecode: false, vscodeParent: true },
  { host: 'embedded-claude-sdk', sdkParent: true, claudecode: true, vscodeParent: false },
];
const cases = [];
for (const environment of [null, false, true]) for (const coordinator of [false, true]) {
  for (const runtimeDisabled of [false, true]) for (const nonInteractive of [false, true]) {
    for (const headlessCohortDisabled of [false, true]) for (const cachedSource of [null, 'env', 'default']) {
      for (const host of hosts) {
        Object.assign(state, { coordinator, runtimeDisabled, nonInteractive, headlessCohortDisabled, ...host, cache: {} });
        if (cachedSource !== null) state.cache.forkSubagentEnabledSource = cachedSource;
        world.i.CLAUDE_CODE_FORK_SUBAGENT = environment ?? undefined;
        const foregroundOnly = vm.runInContext('PC()', world);
        const source = vm.runInContext('C_n()', world);
        const enabled = vm.runInContext('Xge()', world);
        const background = vm.runInContext('yPt()', world);
        cases.push({ environment, coordinator, runtimeDisabled, nonInteractive, headlessCohortDisabled, cachedSource, host: host.host, foregroundOnly, source, enabled, background });
      }
    }
  }
}

const output = JSON.stringify({
  version: '2.1.319', binarySha256, ranges, cases,
  scope: 'Actual C_n, Xge, yPt and PC source executed from the pinned official artifact. Environment is supplied as the native parsed tri-state; coordinator/runtime-disable/cohort/cached-source and host classification facts are explicit inputs. Native session caching remains owned by the caller, not by a process-global cache.',
  productAdaptation: 'The public product wrapper reads only LINGXI_FORK_SUBAGENT. Standard CLI foreground-only defaults to non-interactive; hosts with SDK/VSCode exceptions supply their actual PC observation to the pure background gate.',
}, null, 2) + '\n';
const target = new URL('../../crates/core/tests/fixtures/fork_subagent_2_1_319.json', import.meta.url);
if (process.argv.includes('--check')) assert.equal(fs.readFileSync(target, 'utf8'), output);
else fs.writeFileSync(target, output);
console.log(`Verified ${cases.length} native fork availability/background cases (${binarySha256})`);

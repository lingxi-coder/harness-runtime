// Extract the builtin computer-use MCP instruction block from the pinned native
// reference. No full reference code or binary is executed.
// Usage: node scripts/tests/computer_use_mcp_instructions_286_oracle.mjs /path/to/2.1.286 [--check]
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const sha256 = createHash('sha256').update(binary).digest('hex');
if (sha256 !== '75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433') {
  throw new Error(`Unexpected native oracle SHA-256: ${sha256}`);
}
const sourceByteRanges = [];
function range(offset, startMarker, endMarker) {
  const end = binary.indexOf('// @bun @bytecode\n', offset + 1);
  const chunk = binary.subarray(offset, end < 0 ? binary.length : end);
  const start = chunk.indexOf(startMarker);
  const stop = chunk.indexOf(endMarker, start + startMarker.length);
  if (start < 0 || stop < start) throw new Error(`Missing native source: ${startMarker}`);
  sourceByteRanges.push({ signature: startMarker, start: offset + start, end: offset + stop });
  return chunk.subarray(start, stop).toString('utf8');
}
const blockSource = range(183727495, 'var Gvt=', 'var _ne=');
const nameSource = range(177781947, 'var NR="computer-use"', ',IGr=') + ';';
const { serverName, block } = vm.runInNewContext(
  `(()=>{${nameSource}\n${blockSource}\nreturn {serverName:NR,block:Gvt}})()`,
  {}, { timeout: 1_000 },
);
if (serverName !== 'computer-use' || typeof block !== 'string') {
  throw new Error('Unexpected builtin computer-use instruction definition');
}
const fixture = JSON.stringify({
  version: '2.1.286', binary_sha256: sha256, sourceByteRanges, serverName, block,
}, null, 2) + '\n';
const outputs = [
  ['../../crates/orchestrator/tests/fixtures/instructions-2.1.286/computer_use_mcp.json', fixture],
  ['../../crates/orchestrator/src/prompt/computer_use_mcp_instructions_286.txt', block],
];
for (const [relative, content] of outputs) {
  const path = fileURLToPath(new URL(relative, import.meta.url));
  if (process.argv.includes('--check')) {
    if (readFileSync(path, 'utf8') !== content) throw new Error(`Native instruction fixture differs: ${relative}`);
  } else writeFileSync(path, content);
}
console.log(`Verified ${block.length} builtin computer-use instruction characters from Claude Code 2.1.286 (${sha256})`);

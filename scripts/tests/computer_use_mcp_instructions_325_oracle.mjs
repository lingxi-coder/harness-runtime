// Extract only the builtin computer-use MCP instruction string and server name
// from the official, hash-pinned darwin-arm64 Claude Code 2.1.325 artifact.
// The reference executable and full bundled source are never executed.
// Usage: node scripts/tests/computer_use_mcp_instructions_325_oracle.mjs /path/to/2.1.325 [--check]
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const sha256 = createHash('sha256').update(binary).digest('hex');
if (sha256 !== '84590fefe6338be9087f5814b61f9a54c5f21524163a4c127cb5a808e31009f2') {
  throw new Error(`Unexpected native oracle SHA-256: ${sha256}`);
}
const sourceByteRanges = [];
function range(startMarker, endMarker) {
  const start = binary.indexOf(startMarker);
  const end = binary.indexOf(endMarker, start + startMarker.length);
  if (start < 0 || end < start || binary.indexOf(startMarker, start + 1) >= 0) {
    throw new Error(`Missing or ambiguous native source: ${startMarker}`);
  }
  sourceByteRanges.push({ signature: startMarker, start, end });
  return binary.subarray(start, end).toString('utf8');
}
const blockSource = range('var QLn=`You have a computer-use MCP available', ';var L7r=') + ';';
const nameSource = range('var Ov="computer-use"', ',koi=') + ';';
const { serverName, block } = vm.runInNewContext(
  `(()=>{${nameSource}\n${blockSource}\nreturn {serverName:Ov,block:QLn}})()`,
  {}, { timeout: 1_000 },
);
if (serverName !== 'computer-use' || typeof block !== 'string') {
  throw new Error('Unexpected builtin computer-use instruction definition');
}
// This fixture is the unmodified reference string. There are deliberately no
// provider/model substitutions. Product-specific tool routing is a separate layer.
const old = JSON.parse(readFileSync(fileURLToPath(new URL(
  '../../crates/orchestrator/tests/fixtures/instructions-2.1.286/computer_use_mcp.json',
  import.meta.url,
)), 'utf8'));
const fixture = JSON.stringify({
  version: '2.1.325',
  binary_sha256: sha256,
  sourceByteRanges,
  serverName,
  block,
  block_utf8_sha256: createHash('sha256').update(block).digest('hex'),
  block_utf8_bytes: Buffer.byteLength(block),
  block_utf16_code_units: block.length,
  substitutions: [],
  comparison: {
    previous_version: old.version,
    previous_binary_sha256: old.binary_sha256,
    identical_utf8_bytes: Buffer.from(block).equals(Buffer.from(old.block)),
  },
}, null, 2) + '\n';
const outputs = [
  ['../../crates/orchestrator/tests/fixtures/instructions-2.1.325/computer_use_mcp.json', fixture],
  ['../../crates/orchestrator/src/prompt/computer_use_mcp_instructions_325.txt', block],
];
for (const [relative, content] of outputs) {
  const path = fileURLToPath(new URL(relative, import.meta.url));
  if (process.argv.includes('--check')) {
    if (!readFileSync(path).equals(Buffer.from(content))) {
      throw new Error(`Native instruction fixture differs: ${relative}`);
    }
  } else {
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, content);
  }
}
console.log(`Verified ${Buffer.byteLength(block)} UTF-8 bytes / ${block.length} UTF-16 code units of builtin computer-use instructions from Claude Code 2.1.325 (${sha256}); identical to 2.1.286: ${Buffer.from(block).equals(Buffer.from(old.block))}`);

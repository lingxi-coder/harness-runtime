# Claude Code 2.1.270 /loop prompt oracle

`generate.mjs` evaluates the official extracted JavaScript prompt builders in Node's VM with deterministic tool names, preambles and feature gates. It does not transcribe the Rust implementation. Regenerate with:

```sh
node lingxi-code/commands/core/tests/fixtures/loop_2_1_270/generate.mjs /tmp/lingxi-loop-oracle-2.1.270
```

The argument contains `package/claude` from the official Darwin arm64 npm package and `chunks/src_192949468.js` / `chunks/src_186131770.js` extracted verbatim from that binary. The generator checks the binary SHA-256, both chunk hashes, and that both chunks and both preambles occur verbatim in the binary. `provenance.json` records these anchors and hashes. No branding, expiry or whitespace normalization is applied.

The 60 command outputs cover explicit prompt, autonomous dynamic/cron and loop.md dynamic/cron, crossed with persistent preamble on/off, push notifications on/off, and persistent/10-minute/30-minute monitors. The 20 tick outputs cover autonomous dynamic/cron, loop.md dynamic/cron and missing loop.md dynamic, crossed with persistent preamble and push gates. Rust tests compare complete UTF-8 byte arrays, including final newlines.

The cloud-offer branch is disabled with the upstream `CLAUDE_CODE_REMOTE=true` condition: LingXi does not supply Anthropic cloud scheduling. Monitor timing branches are evaluated independently; the timeout selection itself is covered by the shared Monitor runtime tests. These fixtures establish exact prompt bytes for the covered variants, not equivalence of Anthropic-hosted services or end-to-end model behavior.

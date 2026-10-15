# Native 2.1.289 Mod Text, citations and UTF-16 evidence

The runner `scripts/tests/mods_native_utf16_citations_289_loopback.mjs` uses
the pinned darwin-arm64 binary SHA
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`. It starts
one local Native process for two queries, then starts a second process with
`--resume <sessionId>` for a third. All model requests go to a loopback-only
dummy server. The environment uses a fixed dummy key, disables inherited
environment and HOME, and turns off telemetry/update traffic. The runner does
not claim a system-wide network firewall.

The first assistant response contains a cited Text block, opaque
`extra_native` metadata, and lone UTF-16 units `55296, 937, 56320`. The Mod
clones the immutable event, changes the text, and forwards it through real
`next`. In the live process, before/forwarded/next-result hook observations,
`$.session.messages()` summary and `{as:"api"}` all retain the accepted string's
exact units. The accepted block is ordinary `{type:"text",text,citations:null}`;
the citation and opaque metadata are removed by the rewrite.

Native writes standard JSONL Text with escaped lone units. The raw line contains
the escaped text and no Mod sidecar or `utf16_code_units` field. After a second
Native process resumes the same session, its `session.messages()` summary and
API view expose the old row as replacement units `65533, 937, 65533`. The third
HTTP request also carries replacement units. The runner separately asserts
both the live process's second request and the resumed process's third request.
This cold-resume result is separate from the live-process exact projection and
the exact durable JSONL bytes.

The first subsequent HTTP body also contains replacement units for this exact
loopback configuration. That provider request observation is separate from
the public SDK's ability to encode exact UTF-16 overrides. It does not establish
the behavior of every provider or replace the Rust public-SDK assertion in the
Harness transcript regression.

The latest run passed with three model requests, two startup health probes,
exit 0 for both Native processes, no unexpected local routes, and empty stderr.
Receipt: `/var/folders/2c/gch8wj4s0d52rygkyd6b9f_h0000gn/T/mods-native-row-wire-289-sequential-6G5y0w/receipt.json`.
The receipt records the live API, resumed API, raw JSONL, both HTTP bodies, and
process exits. This is Native-only evidence; Harness transport parity and other
models/provider paths require their own tests.

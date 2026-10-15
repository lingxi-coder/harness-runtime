# Claude Code 2.1.289 server-fallback category gate

This source oracle pins the category gate used by both physical server-fallback
paths in the 2.1.289 binary. The executable probe is
[`mods_fallback_category_289_probe.mjs`](../../scripts/tests/mods_fallback_category_289_probe.mjs);
captured source windows and vectors are in
[`mods-fallback-category-2.1.289.json`](mods-fallback-category-2.1.289.json).

Run it with:

~~~sh
node scripts/tests/mods_fallback_category_289_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64
~~~

The pinned binary SHA-256 is
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`.
The probe executes the exact `kjt`, `TNe`, and `Sjt` functions from the
`189077574..189078881` window. It separately checks their production callers:
the stream event loop calls `TNe(ii)` for each decoded event, and the completed
response content projector calls `Sjt(block)` before returning its `lastHop`.
No provider request or CLI workflow is executed.

The Native predicate is exact and small: the trigger must be an object with
`type === "refusal"`, and `category` must be a string whose JavaScript
`.length` is from 1 through 64. JavaScript string length counts UTF-16 code
units. The helper returns an accepted category unchanged. Empty, missing,
non-string, or oversized categories become `null`, while a structurally valid
fallback block remains recognized. Both stream and completed-response
extractors use this same predicate.

The executed vectors cover 64 versus 65 ASCII units, 32 versus 33 astral
characters, an astral pair crossing the 64-unit boundary, non-string inputs,
and lone high/low surrogates. The category helper accepts either lone
surrogate as a one-unit JavaScript string. This is a helper-level value test;
it does not establish the behavior of Rust JSON ingress for that value.

The repository's `deps/llm-client/src/providers/anthropic/fallback_response.rs::category`
uses `encode_utf16().count()` with the same 1–64 bound, so its length gate
matches Native for valid Rust strings, including supplementary characters.
The in-repo Anthropic stream parser first decodes frames into
`serde_json::Value`; that representation does not retain lone UTF-16
surrogates. The source oracle therefore establishes the Native gate and the
SDK's matching bound, not full lone-surrogate transport parity.

This gate is distinct from the refusal Details formatter's later 255-unit
sanitization/truncation. Because both 2.1.289 server-fallback extractors pass
at most 64 units to the formatter, a supplementary character cannot land at
the formatter's 255-unit cut on this server-fallback path. The Core formatter's
255-unit split error remains a valid direct-helper boundary test, but it is not
reachable through this specific Native category producer. The textual
`Request ID` suffix is a separate ASCII allowlist and is not UTF-16-sliced.

The Native `$dt` rendering behavior is recorded separately in
[`mods-refusal-fallback-text-2.1.289.md`](mods-refusal-fallback-text-2.1.289.md);
that formatter oracle does not replace this input-gate proof.

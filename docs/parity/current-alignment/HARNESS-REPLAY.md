# Harness replay interface

The fixture inputs are shared data, not simulated results. A Harness replay
must execute the current production path and write its own raw artifacts under
an externally selected output directory. It must not edit Native baselines.

From a crate test, locate the shared inputs relative to
CARGO_MANIFEST_DIR using ../../docs/parity/current-alignment/fixtures. Write
the candidate artifacts to CLAUDE_ALIGNMENT_ARTIFACT_DIR, which the test
runner supplies. Preserve original request, event/result, stdout/stderr and
session bytes. Receipts should identify the candidate revision and exact test
entrypoint, list input/output hashes, and report every assertion. Do not emit
fabricated “Native-compatible” artifacts from a helper that bypasses runtime
code.

| Gate | Shared input and Native evidence | Required Harness production path | Raw comparison |
|---|---|---|---|
| SDK-ERR-1 | fixtures/provider-error/cases.json fixes three HTTP 400 bodies: nested error.message with one D800 UTF-16 unit, nested message null, and an empty body. Native raw stdout/stderr/JSON and receipt are under native-baseline/. | Send the fixed responses through the real llm-client Anthropic source-wrapper route, then through the integrated Host error formatter and actual Agent/user-visible failure output. Do not instantiate ProviderErrorText directly or call only the JSON helper. Verify one request per case and that the error does not retry. | Keep Native stdout JSON bytes untouched. First check the SDK carrier's source/status text and UTF-16 units at its own boundary; then verify the Host formatter extracts the nested error.message and compare that final user-visible text's code units/display with the parsed Native result. Do not treat the intermediate carrier as final UI text or require the full JSON output bytes to match. For null/empty cases, compare the final formatted result with the captured Native result. The Native result JSON includes a generated UUID; assert its separate format/reference invariant and do not claim whole-output byte equality across runs. |
| HOOK-1 | fixtures/hook-regexp/matcher-cases.json and inputs/*.json; Native receipt and output are under native-baseline/. | Feed each exact input through the current Hook registry/dispatcher and production matcher. Preserve event source, order, payload, and observed match IDs. | Compare fixed event JSON bytes where the event is fixed; assert match/nonmatch set and ordering separately. |
| MOD-1 | fixtures/mod-middleware/middleware.test.ts, hooks/register.js and the Native receipt. | Start the real worker with ModHost::start_with_store_root, load the same module, and reach session.append through the production caller. The continuation must be the real append/persistence operation, not a test-only identity closure. | Capture the raw event before/after middleware, continuation result, errors, and persisted row. Distinguish rewrite, direct return, deny, throw, and unload. |
| MOD-2 | fixtures/mod-tool-register/cases.json is the shared schema, prompt, model, session ID and three isDeferred variants. Exact Native tools slices are under native-baseline/; full requests are generated only in ignored Native run-output because metadata.user_id may be user-correlatable. | Start the real ConversationOrchestrator/turn path so session.start invokes the loaded Mod and ModSessionContext::tool_register registers into the live ToolRegistry. Let the production client serialize the first provider request to the loopback stub. Do not construct a tools array or request body in the test. | Save the original request body and exact top-level tools byte slice before parsing. Compare the Claude/Anthropic tools bytes and order when fixed. If metadata.user_id cannot be fixed, assert its own encoding/reference invariants and mark full-request byte equality unavailable; never rewrite or normalize it. Other providers use their own protocol gates. |
| TURN-1 | fixtures/commands/cases.json and native-baseline stdout/stderr/receipt. The Native runner writes full request/session bytes only to ignored run-output because captured messages include user-global instruction/account context. | Send every prompt through the production command parser and mode dispatcher; ParseFailure and unknown commands must reach the ordinary RunAsTurn path and the real SDK call/persistence path. | During the same local candidate run, retain original Native and Harness request/session artifacts. Compare exact fixed prompt units and persisted fields; claim whole-artifact byte equality only if context, IDs and paths match. Unfixable values get explicit field-level format/reference/lifecycle assertions, not normalization. Do not commit or redact the contaminated Native artifacts to claim byte equality. |
| SESSION-1 | fixtures/session-append/harness-byte-gate/oracle-cases.json and the Native provenance/receipt. | Re-run through the production session.append dispatch and persistence caller. Cargo execution is owned by the unified integration queue. | Compare the original Native result strings byte-for-byte; retain the existing two-case scoped result only as historical evidence until the current candidate reruns. |

The current public runtime entrypoints are in crates/hooks/src/mods.rs:
ModHost::start_with_store_root, load and dispatch. Tool registration reaches
the current turn adapter in crates/orchestrator/src/turn_loop/tool_dispatch.rs
through ModSessionContext::tool_register. Using these APIs in isolation is
insufficient for a whole-turn or persistence claim; the caller must exercise
the production continuation that consumes their result.

Byte-level comparison applies to captured fixed inputs and observable raw
seams. Fix IDs, clock, cwd, settings, locale, environment, provider response,
ordering and fixture paths when the public seam allows it. Do not delete,
sort, normalize or reserialize fields to manufacture equality. When Native
creates an unfixable time/ID/path, retain the raw artifact and assert the
field's encoding, format, identity references, ordering and lifecycle as a
separate contract; that does not pass whole-artifact byte equality.

Do not invoke Cargo from a fixture runner. E owns the shared build queue; the
acceptance owner records results only after E reports the tested candidate and
exact command. If Native's public API cannot reach a behavior, record it as
Blocked/Unverified and keep the production boundary intact.

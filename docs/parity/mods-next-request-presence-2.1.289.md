# Mod accepted-row presence at the Native request boundary

Reference: Claude Code 2.1.289 Darwin arm64, SHA-256
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`.

Run `node scripts/tests/mods_next_request_presence_289_probe.mjs <native-binary> --check`.
The probe verifies all eight extracted windows before evaluating the exact
Native `ozo` user/assistant mapper, Anthropic beta `messages.create` method,
and bound JSON body encoder. Flags/imports are controlled and the transport
only captures the request; no CLI launch, network or provider call occurs.

For the accepted-row vector from the append projection investigation, the
captured request retains Text `citations: null` and leaves `is_error` absent
when the ToolResult omits it. Thus absent, explicit null, false and true need
presence-aware representations through Mod projection, durable serialization,
resume and SDK request construction. Unchanged source blocks must not acquire
new fields solely because the representation supports them.

The main query builder and create-call linkage are statically source-pinned.
Earlier whole-conversation normalization, cache-enabled branches and a complete
Native CLI/live-provider execution are not executed by this probe. It proves
the isolated mapping/SDK body path, not overall byte alignment.

Root independently executed the temporary probe at v229, then this repository
copy. The complete window offsets/hashes and captured messages are in
`mods-next-request-presence-2.1.289.json`. Production Core/SDK field propagation
is still under implementation; this artifact does not claim it is repaired.

V237 also pins the exact compact request body string and its SHA-256, preserving
property order and omission rather than checking only parsed JSON values. The
fixture check re-executes the helpers and compares the complete report.

V246 executes the same Native mapping and SDK encoder with an absent,
explicit-false and explicit-true ToolResult error flag. All three forms retain
their own key presence and exact byte body. SDK unit/integration tests passed
2,112 cases across 187 targets at v247. Core passed 795 cases; LLM-runtime and
Hooks passed 1,106 and 697 cases at v250. These results do not close the separate
nonempty-citation replay-companion gap in accepted/durable rows, or prove full
Native CLI/live-provider alignment.

# LingXi LLM runtime

This crate owns LingXi's application policy around model calls: account/OAuth
lifecycle, credentials, routing, retries, prompt-cache policy, Fusion admission,
and durable physical-attempt accounting. Provider wire encoding/decoding,
native replay content, usage normalization and built-in catalog facts come from
`lingxi-llm-client`, pinned in Cargo.toml and Cargo.lock.

`upstream.rs` projects host request/events into the independent client's typed
contracts. It uses the client's low-level codec interface so host authentication,
exact UTF-16 request serialization, platform transports and synchronous dispatch
hooks remain on the existing physical send boundary. One catalog row is frozen
before encoding. A retry must settle the previous attempt before another send.
The independent client also exposes `PreparedCall` for hosts that delegate
request preparation and transport to its executor.

The `providers` module only projects Gemini file envelopes and exports codec
adapters. It contains no local provider codec implementation or copied catalog.
Host-owned ChatGPT login connection settings reuse upstream OpenAI model facts.
Unknown inference controls and prices stay unknown; a subscription or non-USD
price must not become a standard USD token price in the existing ledger.

The client carries published model prices locally; estimating token charges
does not make another provider request. A response supplies usage and inference
facts, and may carry a provider-reported monetary amount, which is distinct
from a catalog estimate. `PreparedCall::pricing_snapshot` freezes the selected
price row before dispatch. Its `FrozenPricing::estimate` requires complete
usage, a known executed tier when Fast was requested or configured by default, an execution time for
time-dependent rules, and a published price for every consumed bucket. The
host's USD ledger accepts that frozen estimate for the completed attempt, and
otherwise preserves independently published fixed-price buckets. A consumed
bucket whose price is unpublished uses the unknown-price fallback and is
flagged as unpriced. Conditional rates without a complete frozen estimate use
the same flagged fallback. Subscription and non-USD rates do not enter that
ledger.

Fusion reserves conservative SDK-selected rate ceilings across the captured
model's published context bands and schedules. Its version-2 attempt intents
mark those rates as budget bounds, and receipts persist the completed SDK
token quote separately from host-owned tool charges. Missing or incomplete
quotes retain known token counts and unverified budget occupancy; they never
turn the reserved ceiling into realized spend. Restart replay uses the saved
quote, without consulting the current catalog or clock. Version-1 fixed-price
attempt journals remain readable. Explicit zero reasoning overrides remain
free; omitted reasoning prices inherit the output rate.

Settings use `providerRegion`: `international` by default, or `china_mainland`.
Provider entries may declare `regions`; custom entries without this field are
available in both regions. Assembly filters the execution catalog before routing
aliases and fallback chains are resolved. Stored configuration and credentials
are preserved. Saving uses the existing layered settings path; applying requires
an idle-time reconnect/restart, and an unavailable saved model is not replaced
with a different account's model.

Validation belongs at both boundaries: the upstream suite covers wire behavior;
this crate tests host projections, authentication, fixtures, retry/settlement,
accounting observations and native-content replay. Gemini text/tool signatures and provider call IDs
are retained as canonical SDK metadata companions, then merged back into the
original part on replay; companions are never sent as duplicate tool calls. Upstream output events may
include protocol-tagged native blocks and explicit block-end events. Consumers
must not execute native server content as an application tool.

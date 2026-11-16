# Exact text and request projection, stage 1

This stage centralizes text construction and message coordinate changes. It
retains existing message DTOs, JSONL formats, provider encoders, cache policies,
and Native recovery behavior.

## Ownership

- `lingxi-exact-text` is a small dependency-free crate in llm-client. `ExactText`
  owns display text and optional exact UTF-16 units, validates paired inputs,
  and appends scalar or exact-unit fragments. It has no protocol, JSON,
  persistence, authorization, or cancellation policy.
- SDK `StreamAccumulator` and host `BlockAccumulator` use that same builder.
  Each accumulator still owns its block lifecycle, terminal admission,
  incomplete-tool policy, citations, and cancellation boundaries.
- SDK `MessageProjectionMap` records source blocks and their output positions,
  exact-field destinations, and whole-message destinations. Empty outputs mean
  an intentional deletion. Unknown coordinates and ambiguous string targets
  fail instead of silently discarding exact data.
- llm-runtime `request_projection` validates and applies one map to exact
  strings, cache endpoints, classifier layout and host provenance. Metadata is
  committed only after validation. Source identities are reused, never minted.
  Split blocks duplicate their existing provenance; merged blocks union it.

## Contracts

1. Ordinary scalar text stays in a String without an additional UTF-16 vector.
   An explicit exact carrier stays explicit even when its units are well
   formed. Equal display strings are insufficient to identify source content.
2. A high surrogate at a fragment boundary may be completed by the next
   fragment in the same builder. It cannot join text from a new response or
   attempt. Appending processes only the new suffix and one pending unit.
3. Host start-event display text is retained until a delta arrives, matching
   the existing no-delta behavior. The source units become authoritative when
   construction begins. An authoritative provider snapshot replaces the old
   builder state.
4. A cache endpoint follows the final output block. An exact string follows
   its explicitly selected field, which can differ from that endpoint. A
   source message split into multiple messages has no unique whole-message
   index; per-block provenance and accepted source membership remain intact.
5. Protocol adaptation may remove source-family cache policy. Applying a map
   must not reinstall that policy. Canonical history is never mutated by these
   route-local transformations.
6. Provider signing, final Claude-family sanitation, field presence, tool
   schemas and continuation comparison retain their existing policies.
7. JSONL and SDK payloads remain separate representations. The host flat arena
   retains JavaScript values; SDK JsonPayload retains JSON source. This stage
   does not replace either with serde_json::Value or normalize existing JSON.
8. File identity, read-state eviction, write fences, notification order and
   deduplication do not use display strings as a new source of authority.

## Validation

The component tests exhaust fragment partitions, mixed scalar/exact fragments,
display collisions, explicit carrier shape, expanded/split/removed coordinates,
and invalid/contradictory mappings. Codec fixtures compare the mapped request
bytes in Complete and Stream modes for Responses and Anthropic layouts.

The host lifecycle tests run both turn drivers, cancel before dispatch and
after response data, continue on the same instance or from actual JSONL, and
append exact human input during an active turn. They check exact input,
deduplication, ordering, final text and encoded history prefixes.

Existing classifier, native discovery, Skill retirement, notification,
changed-file, file-authority, filesystem and stream/nonstream recovery suites
remain acceptance gates. Test logs and the final immutable cache baseline are
recorded in the delivery evidence, not inferred from component tests alone.

## Later stages

The generic JSON containers, their parsers, public DTO formats, and legacy
sidecar compatibility APIs remain independent work. Removing them requires
separate format and source-preservation evidence; this stage does not authorize
using display text for model input, execution paths, permissions or cache keys.

---
name: mcp-tool-design
description: Turn one user workflow into a single meaningful MCP tool — name, title, description, and a closed input/output schema — never a mechanical rename of an app's CRUD or bridge API.
---

# Design one app's MCP tool

Own turning a real, user-completable workflow into one Tool: a name,
title, description, and a closed, bounded input/output schema — and
recognizing when a proposed tool is actually a duplicate, a filler, or a
mechanical rename of an internal API and should be dropped or merged
instead of proposed. Binding that tool to a Flow is `$mcp-flow-binding`'s
job; evaluating whether it actually works is `$mcp-qa`'s. This skill's job
ends at a well-formed, meaningful proposal.

## Status: design-only

This is §12.3/§12.4/§13 of the plugin's frozen design. There is no
`AppMcpProposal` type, no `mcp-designer` agent, and no enforcement of any
of the limits below anywhere in the repo today — confirmed by an empty
search for `AppMcpProposal`, `mcp_authoring_required`, and
`McpToolDefinitionDto` across the whole tree. Design one tool this way once
the authoring workflow (`$expose-as-mcp`) can actually run it; don't claim
a proposal was validated by a gate that doesn't exist yet.

## What makes a tool meaningful (§12.3)

Every proposed tool must:

- correspond to an explicit user or app business goal — not "a thing the
  API happens to expose";
- bind to one typed Flow in the same app (`$mcp-flow-binding` does the
  binding; this skill just needs the workflow to be nameable in plain
  language before handing it off);
- carry a concrete, specific title and description — not a restated field
  name;
- have a closed, bounded inputSchema;
- for anything that mutates state, say what it changes and how it fails,
  in the description itself;
- be something MCP QA can actually call and verify — not aspirational;
- not duplicate another tool already proposed for the same app;
- not be a mechanical rename of an internal CRUD or bridge operation
  (`create_record`/`get_record`/`update_record`/`delete_record` copied
  straight from a collection's schema is the canonical failure shape);
- prefer covering a complete workflow a user can finish end to end over
  exposing every underlying Flow step as its own tool;
- bound every list/search result with a cursor/`has_more`, and give every
  error an actionable recovery, not a bare failure string;
- return high-signal structured content — never raw logs, a full canvas
  frame, or internal state dumped into the response.

Design **from the user's goal down**, not from the app's data model up: ask
what a user would actually want the LLM to do with this app (view a
summary, check current state, start a challenge, save or export a result,
perform one domain action), then find or ask for the Flow that does it —
per §1.7, canvas/game/visualization apps are never rejected for their
technical family, only for genuinely having no safe, verifiable, Flow-
expressible business action.

## Schema limits (§13.2 — Local App product budget, not a protocol rule)

- `name`: canonical lower snake_case, `^[a-z][a-z0-9]*(?:_[a-z0-9]+)*$`,
  1–64 chars, no leading/trailing/doubled `_`.
- `title`: ≤100 chars. `description`: ≤1000 chars.
- `input`/`output` schema: ≤32 KiB each, max depth 8, closed object root,
  no remote `$ref`, no unbounded `anyOf`/`oneOf` combination.
- Tool result `structuredContent`: ≤64 KiB.
- 1–16 tools per published app (§1.7) — the floor is a capability
  requirement, not license to pad with filler; see below.

## The token budget

§12.3 pins a per-app cap of **2,048 estimated tokens / 5,120 UTF-16
units**, and a conversation-exposed aggregate cap of **16,384 tokens /
40,960 units** — both pinned as measured targets in
`docs/local-apps/performance-baselines/local-app-plugin-v1.json`
(`per_app_tool_definitions_max_tokens: 2048`,
`expanded_tool_definitions_max_tokens: 16384`). The counting method it
reuses is real and already shipping for generic tool-search deferral: sum
UTF-16 code units of `name + description + JSON.stringify(input_schema)`
per tool, then estimate tokens at 2.5 chars/token — see
`apply_defer_loading_with_context_and_tokens` in
`lingxi-code/tool-api/src/wire.rs:203-260` (the exact comment there:
*"Claude's fallback sums exactly `name.length + description.length +
JSON.stringify(inputSchema).length`"*). What does **not** exist yet is a
gate that applies those two numbers specifically to a Local App's tool
catalog and rejects an over-budget proposal with `proposal_invalid` and a
per-tool contribution ranking (§12.3, §19.9) — that enforcement is
design-only. Never truncate a description or schema to fit under budget;
an over-budget proposal should shrink the *content*, not get silently cut
into invalid JSON Schema.

## What you propose vs. what the Host derives (§12.4)

You (the agent) may only set:

- tool `name` / `title` / `description`;
- `inputSchema` / `outputSchema`;
- the business Flow semantic reference (which Flow, in plain terms — the
  actual typed binding is `$mcp-flow-binding`'s job);
- `required_flow_changes` when the app doesn't have a Flow for this yet;
- `excluded_capabilities` with a reason, when nothing safe/verifiable
  exists for a goal.

The Host derives, and you must never set: server identity, connection
scope, final Flow/build ID, `annotations` (§13.3's readOnlyHint /
destructiveHint / idempotentHint / openWorldHint), `icons`, `execution`,
`_meta` (§13.4 — searchHint, alwaysLoad, requiresUserInteraction), the
permission ceiling, rate limit/timeout, or the catalog digest. Proposing a
value for any of these is not "being helpful" — it's an input the Host
validation rejects outright.

## Never manufacture a tool to hit the floor

If nothing in the app supports a meaningful, verifiable business action,
the correct proposal is **zero tools**, which the workflow surfaces as
`mcp_authoring_required` (§1.7, §12.3) — not a filler tool that returns
static text, not a tool that duplicates another one under a different
name, not a raw pass-through of an internal API just to clear 1–16.

## Boundaries

- Never mirror an app's collections/fields 1:1 into
  `create_x`/`get_x`/`update_x`/`delete_x` tools — that's exactly the
  "mechanical CRUD rename" §12.3 forbids; find the workflow the CRUD
  operations serve and name the tool after that instead.
- Never set annotations, icons, `_meta`, ceiling, or any other Host-derived
  field — describe intent in the description, and let the Host derive the
  rest.
- Never bind inputs/outputs to Flow steps yourself — hand a named,
  schema'd tool to `$mcp-flow-binding`.
- Never declare a proposal QA-passed or ready to promote — that's
  `$mcp-qa`'s call, backed by Host evidence, not this skill's.

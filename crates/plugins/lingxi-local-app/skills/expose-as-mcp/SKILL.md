---
name: expose-as-mcp
description: Tell apart a first per-App MCP authoring pass from an update or a standalone revise, and start the local-app-mcp-authoring workflow — never designs, approves, or promotes a tool.
---

# Expose a local app as MCP tools

Own exactly two things: classify which of three authoring situations a
request is, and start the `local-app-mcp-authoring` workflow with the two
inputs the design allows for it. Everything the workflow does after
that — turning a workflow into a tool definition, binding it to a Flow,
evaluating whether it actually works, registering or promoting anything —
belongs to `$mcp-tool-design`, `$mcp-flow-binding`, `$mcp-qa`, or to
Host-owned validation this skill never performs itself.

## Host-authority boundary

Per-App MCP authoring is §12 of the plugin's frozen design. The Plugin ships
`agents/mcp-designer.md` and the discoverable
`lingxi-local-app:local-app-mcp-authoring` workflow. The workflow performs
the evidence → proposal → Host validation → approval → MCP QA → atomic
promotion chain; report `needs_input` or `mcp_authoring_required` when Host
validation cannot produce a safe candidate, and never bypass Host authority.

## The three situations

The Host-owned manifest field that settles this is schema v4's
`active_mcp_catalog: Option<AppMcpCatalogRef>` (§16.1), alongside
`runtime_profile` / `dependency_snapshot` / `surface`. Classify by this
Host-reloaded field; do not infer catalog state from caller arguments:

- **Initial** — the already-scaffolded app has no active catalog yet
  (`active_mcp_catalog` is `None`). App creation never runs MCP authoring;
  this explicit setup request uses its own MCP proposal approval sheet, then
  QA/promotes the first catalog while leaving service exposure disabled.
- **Update** — the app already has an active catalog, and the user explicitly
  asked to reconcile MCP after changing the app itself (source, data schema,
  Flow, dependencies). Ordinary app updates do not invoke this workflow.
  If the resulting `approval_contract_sha256` (§12.2 — full tool surface,
  semantic Flow references, permission ceiling) is unchanged from the
  active catalog, no new MCP approval is created at all; if it changed, the
  Native proposal diff (§17.4a) must be shown before anything promotes.
- **Standalone revise** — the app already has an active catalog, and the
  user asked to add, change, or remove tools without otherwise touching the
  app. Same proposal-diff / approval-receipt path as update (§12.2), run on
  its own rather than folded into an app-update transaction.

## Starting the workflow

```text
{"name": "lingxi-local-app:local-app-mcp-authoring", "args": {"app_id": "<id>", "user_goal": "<text>"}}
```

through the `Workflow` tool — the same generic invocation `$local-app-test`
uses for `lingxi-local-app:local-app-use-test`. Per §12.2 the workflow accepts
**only** `app_id` and `user_goal`; the Host derives everything else
(evidence, active build/catalog identity, capability graph) on its own.
Never pass raw tool definitions, a server name, annotations, permission
rules, a Flow ID, a workspace path, or a catalog/proposal digest — §12.2
names all six as explicitly rejected inputs, not merely unnecessary ones.

## Reading what comes back

The workflow returns a structured authoring handoff. The result can be
`needs_input`, `mcp_authoring_required`, `approval_required`, or `promoted`.

The workflow can return `needs_input` (relay the focused questions to the
user and resume — don't guess an answer for them) or
`mcp_authoring_required` (§1.7 / §12.3 — no proposed tool cleared the
quality gate; the candidate/staging work is preserved, nothing is
published, and the user can keep clarifying what they want the LLM able to
do). Neither is a failure of this skill; both are the workflow doing its
job and handing control back.

## Boundaries

- Never construct an `AppMcpProposal`, decide a tool's schema, or judge
  whether a name or description is meaningful — that's `$mcp-tool-design`'s
  job, run inside the workflow, not this skill's.
- Never bind a tool's input or output to a Flow step — that's
  `$mcp-flow-binding`'s job, also run inside the workflow.
- Never declare a proposal QA-passed, register a logical server, or promote
  a catalog yourself. Even once the Host-owned pieces exist, only the
  workflow's own atomic promote (§12.2, §16.4) does that, gated on Native
  approval and MCP QA — never on this skill's judgment.
- Never invent an `app_id` or a `user_goal` on the user's behalf; if either
  is missing or unclear, ask rather than guessing one to unblock the call.

---
name: mcp-flow-binding
description: Bind a proposed tool's input and step outputs to one typed Flow already in the same app, and flag Flow steps the app doesn't have yet — no JS handlers, no cross-app binding.
---

# Bind a tool to a typed Flow

Own mapping a `$mcp-tool-design`-produced tool's declared inputSchema
fields and a Flow's own step outputs onto that Flow's step inputs and the
tool's result — entirely inside one app — and flagging when the app
doesn't yet have a Flow that can do what the tool needs, as a
`required_flow_change`, rather than inventing a workaround. Deciding what
the tool should be named or do is `$mcp-tool-design`'s job; this skill only
wires an already-designed tool to Flow steps that already exist (or names
the ones that don't).

## What a Flow actually is today (built)

`local_apps::runtime_v2::FlowDefinition` (`local-apps/src/runtime_v2.rs:943`)
is `{ flow_id, version, steps: Vec<FlowStep> }`. Each `FlowStep`
(`runtime_v2.rs:952`) is `{ step_id, capability: CapabilityId, depends_on:
Vec<String>, input_json: String }`, where `capability` is one of the 32
fixed variants of `CapabilityId` (`runtime_v2.rs:51-86`) — `DataQuery`,
`DataMutate`, `NetworkRequest`, `FlowExecute`, `BackgroundSchedule`, the
Agent-session family, and so on. `FlowDefinition::validate`
(`runtime_v2.rs:963`) enforces 1–128 steps, unique step ids, that
`depends_on` only names an earlier step (acyclic), and that `input_json` is
well-formed JSON under a bounded payload size — but `input_json` is a
**fixed literal string** today. Nothing in this type lets a step's input
reference a tool's caller-supplied argument or a prior step's output; every
Flow that exists right now was authored with its literal inputs baked in.

## What this skill actually produces (design-only, §12.5)

The typed binding layer that lets a tool's input flow into a Flow step, and
a Flow step's output flow into the tool's result, is proposed but not
built:

```rust
enum FlowValueBinding {
    Literal(Value),
    ToolInput { json_pointer: String },
    StepOutput { step_id: String, json_pointer: String },
}

struct AppMcpFlowBinding {
    flow_id: String,
    inputs: BTreeMap<String, FlowValueBinding>,
    result: FlowValueBinding,
}
```

No `FlowValueBinding` or `AppMcpFlowBinding` type exists in the codebase —
confirmed by an empty search across the tree. This skill's job is to
produce a proposal shaped exactly like this, for the (not-yet-existing)
authoring workflow to validate; it is not something you can execute today.

## Host validation your binding must satisfy (§12.5)

- `flow_id` must belong to the **same app**: for create, the app's own
  staging source; for update/revise, its candidate or active build. The
  final build binding is re-derived by the Host at MCP QA time — never
  assume a candidate build already exists when proposing at create time.
- A `ToolInput` pointer must fall inside the tool's own declared
  `inputSchema` — you can't reference a field the tool doesn't accept.
- A `StepOutput` may only reference a step that is **already complete
  earlier in the same Flow's DAG** (per `depends_on`), and its pointer must
  fall inside that step's own declared output schema.
- The final `result` binding must satisfy the tool's `outputSchema`.
- Numeric limits, specific to this Conversation MCP binding layer only:
  ≤32 steps per `AppMcpFlowBinding`; a JSON Pointer ≤512 UTF-8 bytes and
  ≤32 segments; binding-tree depth ≤8; one step's intermediate structured
  value ≤64 KiB; ≤256 KiB cumulative per call. These do **not** shrink the
  generic Flow's existing ceilings — `FlowDefinition` still allows up to
  128 steps at runtime (`runtime_v2.rs:965`), and the generic
  `flow_execute`/background path keeps its own 15-minute
  `FLOW_EXECUTION_TIMEOUT`. A Local App MCP call is bounded instead by
  §14.4's separate 5-minute timeout.

## What's rejected outright (§12.5)

No template expressions, no JavaScript, no string interpolation, no
dynamic Flow ID, no reference to another app's Flow. V1 additionally
refuses to bind a tool to a step whose `capability` is recursive
`FlowExecute`, an LLM call, an Agent call, `BackgroundSchedule`, or
anything that depends on an interactive WebView/device UI — those steps
exist for other purposes (background jobs, agent orchestration) and are
not safe to expose as a synchronous, conversation-callable MCP result.

## When the app doesn't have the Flow yet

If the workflow `$mcp-tool-design` handed you needs a capability sequence
none of the app's existing Flows cover, propose it as a
`required_flow_change` (§12.4) and stop there — describe what the Flow
would need to do, in plain terms. Writing the actual Flow (or the app
source a Flow step calls into) is a build-path operation, not this skill's;
authoring resumes once that Flow exists.

## Boundaries

- Never propose JavaScript, a bridge callback, or any handler outside the
  `Literal`/`ToolInput`/`StepOutput` vocabulary — a binding that needs more
  expressiveness than that is a sign the Flow itself is missing a step, not
  a reason to reach for script.
- Never bind across apps. `flow_id` is always this app's own staging,
  candidate, or active source — never another app's, even one with an
  identical-looking Flow.
- Never invent a step's `capability`, `annotations`, permission ceiling, or
  server identity — those are Host-derived exactly as in
  `$mcp-tool-design`, not something this skill sets.
- Never assume a candidate build already exists when binding at create
  time; the final build binding only exists after the Host re-derives it at
  MCP QA.

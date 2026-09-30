//! LingXi workflow runtime.
//!
//! Executes claude-code-style workflow scripts — JavaScript that orchestrates
//! subagents via injected globals (`agent()` / `parallel()` / `pipeline()` /
//! `phase()` / `log()` / `budget` / `workflow()`). claude-code runs these with
//! a JS `AsyncFunction`; LingXi embeds QuickJS (via `rquickjs`) so the same
//! model-authored scripts run with matching semantics.
//!
//! The full global surface is implemented (`agent`, `parallel`,
//! `pipeline`, `phase`, `log`, `budget`, `args`, `workflow`) with CONCURRENT
//! batch dispatch of `agent()` calls (agents pending together run as one batch)
//! via the pluggable `agent_runner`. The host
//! (`tasks::handlers::local_workflow`) bridges `agent_runner` to `LingXi`'s
//! subagent spawner and adds live progress, a real token budget
//! ([`WorkflowBudgetSource`]), structured-output `agent({schema})`,
//! journaling/resume, and `workflow()` nesting; the
//! `Workflow` tool (`tool-workflow`) is registered + wired at the desktop
//! composition root.

// Documentation debt, not a decision that docs do not matter: this crate had
// 6 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]

pub mod description;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

mod plugin_registry;
pub use plugin_registry::{PluginWorkflowEntry, PluginWorkflowRegistry, MAX_WORKFLOW_SCRIPT_BYTES};

/// JS prelude defining `agent()` (deferred promises) + the
/// `parallel()` / `pipeline()` orchestration primitives, injected before the
/// workflow body.
/// The argument-validation `throw` messages are byte-locked to claude-code's.
/// `parallel`/`pipeline` start every chain before awaiting, so agents pending at
/// the same time are dispatched as ONE concurrent batch by the Rust driver
/// (`run`'s pump loop). A throwing thunk/stage resolves to `null` (claude-code's
/// `.filter(Boolean)` contract).
const WORKFLOW_PRELUDE: &str = r#"
// agent() returns a DEFERRED promise and queues the request; it never blocks.
// The Rust driver pumps the queue between microtask drains, dispatching every
// promise that is pending at the same time as ONE concurrent batch (via the
// native __wf_dispatch_batch), then resolving them. So agents that are started
// before the first await (parallel's fan-out) run concurrently, while a
// sequential `await agent()` chain dispatches one at a time.
globalThis.__wf_queue = [];
// Generalized "throw this agent()" channel: when the host refuses a spawn
// (agent-cap `k6a`/`c0p`, or budget ceiling `I6a`), it returns the result slot
// as `THROW_PREFIX + message`; the pump then REJECTS that agent()'s promise with
// `new Error(message)` so a runaway/over-budget loop throws (in parallel/pipeline
// the throw is caught → null, as claude-code does). The prefix is NUL-free
// (the eval path uses a C string) and collision-proof via U+0001 framing.
globalThis.__WF_THROW_PREFIX = String.fromCharCode(1) + "__wf_throw__" + String.fromCharCode(1);
// NULL sentinel: a skipped / dead (failed/killed/errored) agent maps to this
// exact marker, which the pump resolves the agent() promise with `null` —
// claude-code returns `null` for such agents (not "" / not its text), so
// explicit `=== null` / `??` checks behave the same. Collision-proof, NUL-free,
// U+0001-framed like the throw prefix.
globalThis.__WF_NULL = String.fromCharCode(1) + "__wf_null__" + String.fromCharCode(1);
// The native runner captures these failures for the terminal workflow result.
// Keep the fallback so the prelude remains usable in standalone harnesses.
if (!('__wf_record_failure' in globalThis)) globalThis.__wf_record_failure = () => {};
globalThis.agent = (prompt, opts) => new Promise((res, rej) => { globalThis.__wf_queue.push({ prompt: String(prompt), opts: opts || {}, res, rej }); });
globalThis.__wf_error_info = (e) => {
  if (e && typeof e === "object") {
    return {
      name: e.name,
      msg: typeof e.message === "string" ? e.message : String(e),
    };
  }
  return { name: undefined, msg: String(e) };
};
globalThis.__wf_plural = (n, word) => n === 1 ? word : word + "s";
globalThis.__wf_pump = () => {
  const q = globalThis.__wf_queue;
  if (q.length > 0) {
    globalThis.__wf_queue = [];
    // Dispatch the batch as two parallel arrays: the prompts and the JSON-encoded
    // opts ({agentType, model, isolation, schema, label, phase, effort}). The host
    // runner maps the spawn-affecting opts onto each subagent request; host-only
    // behavior such as `throwOnError` is consumed by the runner itself.
    const results = globalThis.__wf_dispatch_batch(q.map((x) => x.prompt), q.map((x) => JSON.stringify(x.opts || {})));
    for (let i = 0; i < q.length; i++) {
      const r = results[i];
      if (typeof r === "string" && r.startsWith(globalThis.__WF_THROW_PREFIX)) {
        // Agent-cap, budget-ceiling, or caller-requested terminal failure → reject.
        const msg = r.slice(globalThis.__WF_THROW_PREFIX.length);
        const err = new Error(msg);
        if (msg.startsWith("Workflow token budget exceeded")) err.name = "WorkflowBudgetExceededError";
        else if (msg.startsWith("Workflow agent() call cap reached")) err.name = "WorkflowAgentCapError";
        q[i].rej(err);
      } else if (r === globalThis.__WF_NULL) {
        // skipped / dead agent → resolve with null (claude-code's contract).
        q[i].res(null);
      } else {
        // `agent({ schema })` returns the VALIDATED OBJECT, not a JSON string
        // (claude-code: `if (ne.schema) return p(he)`). The host serialises the
        // subagent's structured content to JSON; parse it back here so the script
        // can use `r.bugs` / `.flatMap(r => r.bugs)` directly — no manual parse.
        // A non-schema agent returns its final text verbatim (a string), even if
        // that text happens to look like JSON.
        const o = q[i].opts;
        if (o && o.schema && typeof r === "string" && r.length > 0) {
          try { q[i].res(JSON.parse(r)); } catch (e) { q[i].res(r); }
        } else {
          q[i].res(r);
        }
      }
    }
    return true;
  }
  return false;
};
// parallel(): start EVERY thunk first (so their agents queue together → one
// concurrent batch), then collect; a throwing thunk / rejected promise → null.
globalThis.parallel = async (thunks) => {
  if (!Array.isArray(thunks)) throw new TypeError("parallel() expects an array of functions");
  if (thunks.length > 4096) throw new Error("array length " + thunks.length + " exceeds the maximum of 4096 supported across the workflow VM boundary");
  for (let t of thunks) if (typeof t !== "function") throw new TypeError("parallel() expects an array of functions, not promises. Wrap each call: () => agent(...)");
  const ps = thunks.map((t) => { try { return Promise.resolve(t()); } catch (e) { return Promise.reject(e); } });
  const settled = await Promise.allSettled(ps);
  const out = [];
  let dropped = 0;
  for (let i = 0; i < settled.length; i++) {
    const entry = settled[i];
    if (entry.status === "fulfilled") {
      out.push(entry.value);
    } else {
      const { name, msg } = globalThis.__wf_error_info(entry.reason);
      if (name === "WorkflowBudgetExceededError") dropped++;
      else {
        globalThis.__wf_record_failure(`parallel[${i}] failed: ${msg}`);
        log(`parallel[${i}] failed: ${msg}`);
      }
      out.push(null);
    }
  }
  if (dropped > 0) globalThis.__wf_record_failure(`parallel: ${dropped} ${globalThis.__wf_plural(dropped, "slot")} dropped \u2014 token budget exceeded`);
  return out;
};
// pipeline(): each item runs its stage chain independently with NO barrier
// between stages — expressed as parallel() over per-item chains, so item A can
// be in a later stage while item B is still early, and each stage's agents
// batch. A throwing stage drops that item to null.
globalThis.pipeline = async (items, ...stages) => {
  if (!Array.isArray(items)) throw new TypeError("pipeline() expects an array as the first argument");
  if (items.length > 4096) throw new Error("array length " + items.length + " exceeds the maximum of 4096 supported across the workflow VM boundary");
  for (const s of stages) if (typeof s !== "function") throw new TypeError("pipeline() stages must be functions: pipeline(items, item => ..., result => ...)");
  const chain = async (item, idx) => {
    let v = item;
    for (const s of stages) {
      // Claude Code treats a null stage result as a dropped pipeline item and
      // does not invoke later stages with that sentinel.
      if (v === null) break;
      v = await s(v, item, idx);
    }
    return v;
  };
  const ps = items.map((it, i) => chain(it, i));
  const settled = await Promise.allSettled(ps);
  const out = [];
  let dropped = 0;
  for (let i = 0; i < settled.length; i++) {
    const entry = settled[i];
    if (entry.status === "fulfilled") {
      out.push(entry.value);
    } else {
      const { name, msg } = globalThis.__wf_error_info(entry.reason);
      if (name === "WorkflowBudgetExceededError") dropped++;
      else {
        globalThis.__wf_record_failure(`pipeline[${i}] failed: ${msg}`);
        log(`pipeline[${i}] failed: ${msg}`);
      }
      out.push(null);
    }
  }
  if (dropped > 0) globalThis.__wf_record_failure(`pipeline: ${dropped} ${globalThis.__wf_plural(dropped, "slot")} dropped \u2014 token budget exceeded`);
  return out;
};
// Default globals. The host OVERRIDES each (before this prelude runs) when it
// has a real value: `budget` (a WorkflowBudgetSource), `args` (the tool input),
// and `workflow()` (a real nested-run impl when nesting is allowed). These
// defaults apply otherwise: no-target budget; `undefined` args; and a
// `workflow()` that throws — the state inside a nested run, where claude-code's
// one-level nesting limit is reached.
if (!('budget' in globalThis)) globalThis.budget = { total: null, spent: () => 0, remaining: () => Infinity };
if (!('args' in globalThis)) globalThis.args = undefined;
if (!('workflow' in globalThis)) globalThis.workflow = async () => { throw new Error("workflow() cannot be called from within a child workflow — nesting is limited to one level. Inline the inner script or call its agents directly."); };
"#;

/// The NULL-sentinel result slot: when a batch runner returns this exact string
/// for an `agent()` call, the prelude resolves that promise with `null` (a
/// skipped / dead agent — claude-code's contract). MUST stay byte-identical to
/// the prelude's `globalThis.__WF_NULL`
/// (`String.fromCharCode(1)+"__wf_null__"+String.fromCharCode(1)`).
pub const WF_NULL_SENTINEL: &str = "\u{1}__wf_null__\u{1}";

/// Host-side refuse marker: when a batch runner returns this prefix plus a
/// message, the prelude rejects that `agent()` promise. Byte-identical to
/// `globalThis.__WF_THROW_PREFIX`.
pub const WF_THROW_PREFIX: &str = "\u{1}__wf_throw__\u{1}";

/// State of a `workflow_agent` progress event (oracle §8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    /// Agent has started execution.
    Start,
    /// Agent completed successfully.
    Done,
    /// Agent failed (error, stall, abort).
    Error,
    /// Agent result served from journal cache (not re-run).
    Cached,
}

impl AgentState {
    /// The state string as used in the serialised progress event.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Done => "done",
            Self::Error => "error",
            Self::Cached => "cached",
        }
    }
}

/// A progress event emitted by a running workflow script.
///
/// ## Structured shapes (oracle §8)
///
/// The three types map to the progress event shapes emitted by claude-code:
/// - `Phase { index, title }` → `{ type: "workflow_phase", index, title }` (`kind` omitted — always undefined in binary)
/// - `Log { message }` → `{ type: "workflow_log", message }`
/// - `Agent { … }` → `{ type: "workflow_agent", … }` with lifecycle state
///
/// ## Implementation note — PARTIAL subset
///
/// The `Agent` variant carries the MINIMAL FAITHFUL SUBSET:
/// `index`, `label`, `phase_index`, `phase_title`, `model`, `state`,
/// `agent_id` (present only for `Done`/`Error`/`Cached`), and
/// `tool_use_id` (= `workflow_agent_{index}_{agent_id_or_suffix}`).
///
/// Timestamp fields (`started_at`, `queued_at`, `last_progress_at`) and the
/// `progress` intermediate state are NOT emitted:
/// - `started_at` / `queued_at` / `last_progress_at`: the bridge dispatches
///   agents via a blocking sync channel on a dedicated thread; `std::time`
///   timestamps are available but the `Progress` channel is `Send + 'static`
///   and `SystemTime` is not `Eq`, which would break `#[derive(PartialEq, Eq)]`
///   on `Progress`. These are omitted to keep the enum `Eq` and the tests
///   straightforward. A future refactor that drops the `Eq` bound can add them.
/// - `queued` state: the bridge's `Plan` enum decides cached-vs-live in Phase A
///   BEFORE Phase B executes the spawns; emitting a per-agent queued event
///   requires coupling into Phase A, which would require threading the
///   `progress_tx` into the sequential plan loop on the async worker side.
///   This is non-trivial (the `on_progress` closure lives on the script thread)
///   so `queued` is omitted. `start` serves as the first lifecycle event.
/// - `progress` intermediate state: requires per-subagent progress streams from
///   the spawner, which are not exposed by the current `SubagentSpawner` trait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// `phase(title)` — starts a new progress group.
    /// Maps to `{ type: "workflow_phase", index, title }`.
    Phase {
        /// 1-based sequential phase index (matches oracle §8 `workflow_phase.index`).
        index: u32,
        /// The phase title string.
        title: String,
    },
    /// `log(message)` — a narrator line.
    /// Maps to `{ type: "workflow_log", message }`.
    Log {
        /// The log message string.
        message: String,
    },
    /// Per-agent lifecycle event.
    /// Maps to `{ type: "workflow_agent", … }`.
    Agent {
        /// Monotonically incrementing agent call ordinal (0-based).
        index: u64,
        /// Agent label: `opts.label ?? prompt.slice(0, 60)`.
        label: String,
        /// The phase index at the time this agent was dispatched (None if no phase set).
        phase_index: Option<u32>,
        /// The phase title at the time this agent was dispatched (None if no phase set).
        phase_title: Option<String>,
        /// Agent ID (UUID string from `SubagentResult`; None before completion).
        agent_id: Option<String>,
        /// Model string from opts or the default.
        model: Option<String>,
        /// Lifecycle state.
        state: AgentState,
        /// Failure/cancellation detail for `Error` events.
        error: Option<String>,
        /// `toolUseID = "workflow_agent_{index}_{agent_id_or_suffix}"`.
        tool_use_id: String,
    },
}

/// Errors from parsing or executing a workflow script.
#[derive(Debug)]
pub enum WorkflowError {
    /// The embedded engine failed to start.
    Engine(String),
    /// The script threw or failed to evaluate.
    Script(String),
}

impl std::fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Engine(e) => write!(f, "workflow engine error: {e}"),
            Self::Script(e) => write!(f, "workflow script error: {e}"),
        }
    }
}

impl std::error::Error for WorkflowError {}

/// The outcome of running a workflow script body.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// Progress events (`phase()` / `log()`) in emission order.
    pub progress: Vec<Progress>,
    /// The script's return value, JSON-serialised. claude-code returns the
    /// script's resolved value (e.g. `return { confirmed }`) as the Workflow
    /// tool result; this is `JSON.stringify(value)`. `None` when the script
    /// returns `undefined` (no `return`).
    pub result: Option<String>,
    /// Non-fatal per-item failures collected by `parallel()` / `pipeline()`.
    /// These are kept separate from progress logs so task notifications can
    /// surface them even when the script itself returns successfully.
    pub failures: Vec<String>,
}

/// Supplies the live token budget the script's `budget` global reflects: the
/// turn's target (`budget.total`) and the shared output-token spend
/// (`budget.spent()`), read on demand while the script runs. The host (the
/// composition root) backs this with the orchestrator's token target plus the
/// session's cumulative usage — the same shared pool the main loop and every
/// subagent add to. `None` leaves the no-target default (total `null`, spent
/// `0`, remaining `Infinity`).
pub trait WorkflowBudgetSource: Send + Sync {
    /// The turn's token target, or `None` if no `+Nk` target was set.
    fn total(&self) -> Option<u64>;
    /// Output tokens spent this turn across the shared pool (main loop + all
    /// workflows/subagents).
    fn spent(&self) -> u64;
}

/// Rewrite a leading `export const meta = …` / `export const meta=…` into a
/// plain `const meta = …` so the script body can be evaluated as a classic
/// script. claude-code reads `meta` from the module's export; here the meta
/// object is bound as a normal `const` (the executor reads it back via the
/// global scope). Only the `export ` keyword on the `meta` declaration is
/// stripped — every other statement is preserved verbatim.
#[must_use]
pub fn strip_meta_export(script: &str) -> String {
    let mut out = String::with_capacity(script.len());
    for (i, line) in script.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("export const meta") {
            let indent = &line[..line.len() - trimmed.len()];
            out.push_str(indent);
            out.push_str("const meta");
            out.push_str(rest);
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Validate the script's `meta` block by PARSING it (tree-sitter JS) and walking
/// the tree exactly as claude-code does (@202919276 `_Bp`/`OKa`/`PKa`/`yBp`/
/// `TBp`): `export const meta = {…}` must be the FIRST statement, the object a
/// PURE LITERAL (no spreads / computed keys / methods / accessors / runtime
/// values / template interpolation / reserved keys), and `meta.name` /
/// `meta.description` non-empty strings. Every error string is byte-exact; the
/// dynamic `${type}` slots are mapped back to the ESTree node names the binary
/// emits ([`estree_name`]).
///
/// # Errors
/// Returns [`WorkflowError::Script`] with the byte-exact message on violation.
pub fn validate_meta(script: &str) -> Result<(), WorkflowError> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|e| WorkflowError::Engine(format!("tree-sitter init: {e}")))?;
    let tree = parser
        .parse(script, None)
        .ok_or_else(|| WorkflowError::Engine("tree-sitter parse returned no tree".into()))?;
    let src = script.as_bytes();

    let Some(obj) = first_statement(tree.root_node()).and_then(|s| meta_object(s, src)) else {
        return Err(WorkflowError::Script(
            "`export const meta = { name, description, phases }` must be the FIRST statement in the script".into(),
        ));
    };
    if let Err(reason) = walk_object_literal(obj, src) {
        return Err(WorkflowError::Script(format!(
            "meta must be a pure literal: {reason}"
        )));
    }
    if !meta_string_field_nonempty(obj, src, "name") {
        return Err(WorkflowError::Script(
            "meta.name must be a non-empty string".into(),
        ));
    }
    if !meta_string_field_nonempty(obj, src, "description") {
        return Err(WorkflowError::Script(
            "meta.description must be a non-empty string".into(),
        ));
    }
    Ok(())
}

/// Compile the executable workflow body without running it. Claude Code does
/// this before registering the background task, so syntax errors are returned
/// synchronously and cannot create a task that fails later in the background.
#[must_use]
pub fn validate_body(script: &str) -> Result<(), WorkflowError> {
    use rquickjs::{Context, Runtime};

    let prepared = strip_meta_export(script);
    validate_body_language(&prepared)?;
    let runtime = Runtime::new().map_err(|e| WorkflowError::Engine(e.to_string()))?;
    let context = Context::full(&runtime).map_err(|e| WorkflowError::Engine(e.to_string()))?;
    let wrapped = format!("(async () => {{\n'use strict';\n{prepared}\n}})");
    context.with(|context| {
        context
            .eval::<rquickjs::Value, _>(wrapped.as_bytes())
            .map(|_| ())
            .map_err(|e| WorkflowError::Script(e.to_string()))
    })
}

/// Claude's workflow VM compiles the body as strict JavaScript and rejects a
/// small set of syntax forms before execution. QuickJS otherwise accepts some
/// of these in its default sloppy mode, so keep the source-level gate explicit
/// instead of relying on engine-specific parser behavior.
fn validate_body_language(script_body: &str) -> Result<(), WorkflowError> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|error| WorkflowError::Engine(format!("tree-sitter init: {error}")))?;
    let Some(tree) = parser.parse(script_body, None) else {
        return Ok(());
    };
    let source = script_body.as_bytes();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "with_statement" => {
                return Err(WorkflowError::Script(
                    "'with' statements are not supported in workflow scripts.".into(),
                ));
            }
            "call_expression" => {
                if let Some(function) = node.child_by_field_name("function") {
                    if node_text(function, source) == "import" {
                        return Err(WorkflowError::Script(
                            "import() is not available in workflow scripts.".into(),
                        ));
                    }
                }
            }
            "lexical_declaration" => {
                if node_text(node, source)
                    .trim_start()
                    .starts_with("await using")
                {
                    return Err(WorkflowError::Script(
                        "'await using' declarations are not supported in workflow scripts.".into(),
                    ));
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    Ok(())
}

fn node_text<'a>(node: tree_sitter::Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

/// Extract a `meta.<field>` string-literal value from a workflow script (used
/// for `meta.name` → claude-code `workflowName`). Returns the cooked string
/// value when the first statement is `export const meta = { … }` and the field
/// is a plain / non-interpolated string literal; `None` otherwise.
#[must_use]
pub fn meta_string_value(script: &str, field: &str) -> Option<String> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(script, None)?;
    let src = script.as_bytes();
    let obj = first_statement(tree.root_node()).and_then(|s| meta_object(s, src))?;
    let mut c = obj.walk();
    for m in obj.named_children(&mut c) {
        if m.kind() != "pair" {
            continue;
        }
        let Some(key) = m.child_by_field_name("key") else {
            continue;
        };
        let kname = match key.kind() {
            "property_identifier" => node_text(key, src).to_string(),
            "string" => string_inner(key, src),
            _ => continue,
        };
        if kname != field {
            continue;
        }
        let value = m.child_by_field_name("value")?;
        return match value.kind() {
            "string" => Some(string_inner(value, src)),
            "template_string" => {
                let mut cc = value.walk();
                let has_subst = value
                    .named_children(&mut cc)
                    .any(|n| n.kind() == "template_substitution");
                (!has_subst).then(|| string_inner(value, src))
            }
            _ => None,
        };
    }
    None
}

/// Extract the number of elements in a literal `meta.<field>` array. Claude
/// Code uses `meta.phases?.length ?? 0` for workflow telemetry; keeping this
/// parser-side avoids evaluating user code while preserving the literal-only
/// metadata contract enforced by [`validate_meta`].
#[must_use]
pub fn meta_array_len(script: &str, field: &str) -> Option<usize> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(script, None)?;
    let src = script.as_bytes();
    let obj = first_statement(tree.root_node()).and_then(|s| meta_object(s, src))?;
    let mut c = obj.walk();
    for pair in obj.named_children(&mut c) {
        if pair.kind() != "pair" {
            continue;
        }
        let Some(key) = pair.child_by_field_name("key") else {
            continue;
        };
        let key_name = match key.kind() {
            "property_identifier" => node_text(key, src).to_string(),
            "string" => string_inner(key, src),
            _ => continue,
        };
        if key_name == field {
            return array_value_len(pair, src);
        }
    }
    None
}

fn array_value_len(pair: tree_sitter::Node<'_>, src: &[u8]) -> Option<usize> {
    let value = pair.child_by_field_name("value")?;
    if value.kind() != "array" {
        return None;
    }
    // Claude sanitizes `meta.phases` before reading `.length`: only literal
    // objects containing a string `title` survive. Sparse arrays are rejected
    // by the metadata parser rather than counted as holes.
    let mut syntax_cursor = value.walk();
    let mut needs_value = true;
    for child in value.children(&mut syntax_cursor) {
        match child.kind() {
            "[" | "]" | "comment" => {}
            "," if needs_value => return None,
            "," => needs_value = true,
            _ if child.is_named() => needs_value = false,
            _ => {}
        }
    }
    let mut cursor = value.walk();
    Some(
        value
            .named_children(&mut cursor)
            .filter(|child| {
                child.kind() == "object" && object_string_field(*child, "title", src).is_some()
            })
            .count(),
    )
}

fn object_string_field(object: tree_sitter::Node<'_>, field: &str, src: &[u8]) -> Option<String> {
    let mut cursor = object.walk();
    let result = object.named_children(&mut cursor).find_map(|pair| {
        if pair.kind() != "pair" {
            return None;
        }
        let key = pair.child_by_field_name("key")?;
        let key_name = match key.kind() {
            "property_identifier" => node_text(key, src).to_string(),
            "string" => string_inner(key, src),
            _ => return None,
        };
        if key_name != field {
            return None;
        }
        let value = pair.child_by_field_name("value")?;
        (value.kind() == "string").then(|| string_inner(value, src))
    });
    result
}

/// The byte-exact message claude-code returns when an INLINE workflow `script`
/// uses a non-deterministic API (Claude Code 2.1.245 validateInput, errorCode 4).
pub const NON_DETERMINISTIC_MESSAGE: &str = "Workflow scripts must be deterministic: Date.now()/Math.random()/new Date() are unavailable (breaks resume). Stamp results after the workflow returns, or pass timestamps via args.";

/// Reject a script that uses a non-deterministic API — `Date.now()`,
/// `Math.random()`, or a zero-argument `new Date()` — which would break resume
/// (the journal replays prior `agent()` results, but re-runs the JS body). This
/// ports the binary's `HKa` AST walk (acorn `MemberExpression` for
/// `Date.now`/`Math.random` and zero-arg `NewExpression` for `new Date()`) onto
/// tree-sitter. claude-code applies this ONLY to an inline `script` input (not
/// to `scriptPath`/`name` files); the caller is responsible for that gating.
///
/// # Errors
/// Returns [`WorkflowError::Script`] with [`NON_DETERMINISTIC_MESSAGE`] when a
/// non-deterministic API is found.
pub fn check_determinism(script: &str) -> Result<(), WorkflowError> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|e| WorkflowError::Engine(format!("tree-sitter init: {e}")))?;
    // A parse failure must NOT block the script (binary `HKa` catches parse
    // errors and returns `false` = "no non-determinism found").
    let Some(tree) = parser.parse(script, None) else {
        return Ok(());
    };
    if uses_nondeterministic_api(tree.root_node(), script.as_bytes()) {
        return Err(WorkflowError::Script(NON_DETERMINISTIC_MESSAGE.to_string()));
    }
    Ok(())
}

/// Walk the tree for `Date.now` / `Math.random` member access (non-computed) or
/// a zero-argument `new Date()`.
fn uses_nondeterministic_api(node: tree_sitter::Node, src: &[u8]) -> bool {
    // `Date.now` / `Math.random` — a `member_expression` with identifier object
    // and identifier property (NOT a computed `subscript_expression`).
    if node.kind() == "member_expression" {
        if let (Some(obj), Some(prop)) = (
            node.child_by_field_name("object"),
            node.child_by_field_name("property"),
        ) {
            if obj.kind() == "identifier" && prop.kind() == "property_identifier" {
                let o = node_text(obj, src);
                let p = node_text(prop, src);
                if (o == "Date" && p == "now") || (o == "Math" && p == "random") {
                    return true;
                }
            }
        }
    }
    // `new Date()` with no arguments.
    if node.kind() == "new_expression" {
        if let Some(ctor) = node.child_by_field_name("constructor") {
            if ctor.kind() == "identifier" && node_text(ctor, src) == "Date" {
                let no_args = match node.child_by_field_name("arguments") {
                    None => true,
                    Some(args) => args.named_child_count() == 0,
                };
                if no_args {
                    return true;
                }
            }
        }
    }
    let mut c = node.walk();
    let found = node
        .named_children(&mut c)
        .any(|child| uses_nondeterministic_api(child, src));
    found
}

/// The program's first non-comment statement.
fn first_statement(program: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut c = program.walk();
    let first = program
        .named_children(&mut c)
        .find(|n| n.kind() != "comment");
    first
}

/// If `stmt` is `export const meta = {object}` (binary `_Bp`), return the object
/// node; else `None`.
fn meta_object<'t>(stmt: tree_sitter::Node<'t>, src: &[u8]) -> Option<tree_sitter::Node<'t>> {
    if stmt.kind() != "export_statement" {
        return None;
    }
    let decl = stmt.child_by_field_name("declaration")?;
    if decl.kind() != "lexical_declaration" || decl.child(0)?.kind() != "const" {
        return None;
    }
    let mut c = decl.walk();
    let mut declarators = decl
        .named_children(&mut c)
        .filter(|n| n.kind() == "variable_declarator");
    let d = declarators.next()?;
    if declarators.next().is_some() {
        return None; // const meta + other declarators ⇒ not the single-`meta` form
    }
    let name = d.child_by_field_name("name")?;
    if name.kind() != "identifier" || node_text(name, src) != "meta" {
        return None;
    }
    let value = d.child_by_field_name("value")?;
    (value.kind() == "object").then_some(value)
}

/// `OKa`: every member must be a plain, non-computed, `init` property with a
/// pure-literal value and a non-reserved key. Returns the bare error message
/// (the caller prefixes `meta must be a pure literal: `).
fn walk_object_literal(obj: tree_sitter::Node, src: &[u8]) -> Result<(), String> {
    let mut c = obj.walk();
    for m in obj.named_children(&mut c) {
        match m.kind() {
            "comment" => {}
            "pair" => {
                let key = m
                    .child_by_field_name("key")
                    .ok_or("only plain properties allowed in meta")?;
                if key.kind() == "computed_property_name" {
                    return Err("computed keys not allowed in meta".into());
                }
                let kname = key_name(key, src)?;
                if matches!(kname.as_str(), "__proto__" | "constructor" | "prototype") {
                    return Err(format!("reserved key name not allowed in meta: {kname}"));
                }
                let value = m
                    .child_by_field_name("value")
                    .ok_or("only plain properties allowed in meta")?;
                walk_value(value, src)?;
            }
            // An object method / getter / setter (ESTree Property method/kind≠init).
            "method_definition" => return Err("methods/accessors not allowed in meta".into()),
            // Shorthand `{name}` → the value is an Identifier (a runtime ref).
            "shorthand_property_identifier" => {
                return Err("non-literal node type in meta: Identifier".into());
            }
            // SpreadElement (`...x`) and anything else → not a plain Property.
            _ => return Err("only plain properties allowed in meta".into()),
        }
    }
    Ok(())
}

/// `yBp`: a property key's name. Identifier / string / number keys are allowed.
fn key_name(key: tree_sitter::Node, src: &[u8]) -> Result<String, String> {
    match key.kind() {
        "property_identifier" => Ok(node_text(key, src).to_string()),
        "string" => Ok(string_inner(key, src)),
        "number" => Ok(node_text(key, src).to_string()),
        other => Err(format!(
            "unsupported key type in meta: {}",
            estree_name(other)
        )),
    }
}

/// `PKa`: a property value must be a pure literal.
fn walk_value(v: tree_sitter::Node, src: &[u8]) -> Result<(), String> {
    match v.kind() {
        "string" | "number" | "true" | "false" | "null" => Ok(()),
        "object" => walk_object_literal(v, src),
        "array" => {
            let mut c = v.walk();
            let mut needs_value = true;
            for child in v.children(&mut c) {
                match child.kind() {
                    "[" | "]" => {}
                    "," => {
                        if needs_value {
                            return Err("sparse arrays not allowed in meta".into());
                        }
                        // A final comma is valid; another comma before an
                        // element is a hole and is rejected on the next comma
                        // or at the closing delimiter.
                        needs_value = true;
                    }
                    "comment" => {}
                    _ if child.is_named() => {
                        walk_value(child, src)?;
                        needs_value = false;
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        "spread_element" => Err("spread not allowed in meta".into()),
        "template_string" => {
            let mut c = v.walk();
            if v.named_children(&mut c)
                .any(|n| n.kind() == "template_substitution")
            {
                Err("template interpolation not allowed in meta".into())
            } else {
                Ok(())
            }
        }
        "unary_expression" => {
            let op = v.child_by_field_name("operator").map(|o| node_text(o, src));
            let arg_is_num = v
                .child_by_field_name("argument")
                .is_some_and(|a| a.kind() == "number");
            if op == Some("-") && arg_is_num {
                Ok(())
            } else {
                Err("only negative-number unary allowed in meta".into())
            }
        }
        other => Err(format!(
            "non-literal node type in meta: {}",
            estree_name(other)
        )),
    }
}

/// Map a tree-sitter node kind to the ESTree type name the binary embeds in its
/// `unsupported key type` / `non-literal node type` messages.
fn estree_name(kind: &str) -> &str {
    match kind {
        "identifier" => "Identifier",
        "call_expression" => "CallExpression",
        "member_expression" | "subscript_expression" => "MemberExpression",
        "arrow_function" => "ArrowFunctionExpression",
        "function" | "function_expression" | "generator_function" => "FunctionExpression",
        "binary_expression" => "BinaryExpression",
        "ternary_expression" => "ConditionalExpression",
        "new_expression" => "NewExpression",
        "await_expression" => "AwaitExpression",
        "object" => "ObjectExpression",
        "array" => "ArrayExpression",
        "string" | "number" | "regex" => "Literal",
        "template_string" => "TemplateLiteral",
        "unary_expression" => "UnaryExpression",
        "assignment_expression" => "AssignmentExpression",
        "parenthesized_expression" => "ParenthesizedExpression",
        other => other,
    }
}

/// The inner, COOKED text of a `string`/`template_string` node (delimiters
/// stripped, escape sequences resolved). claude-code reads `meta` via a real JS
/// parse (acorn), so the key/name/description values it compares are the cooked
/// string values — e.g. `"constructor"` is the reserved key `constructor`,
/// and `"x"` is a non-empty `name`. We resolve the standard JS string escapes
/// (`\n`, `\r`, `\t`, `\b`, `\f`, `\v`, `\0`, `\\`, the escaped quote/backtick,
/// plus `\xNN` and `\uXXXX` / `\u{...}`) so reserved-key detection and
/// non-emptiness match the parser.
fn string_inner(node: tree_sitter::Node, src: &[u8]) -> String {
    let t = node_text(node, src);
    let n = t.len();
    let raw = match t.chars().next() {
        Some('"' | '\'' | '`') if n >= 2 && t.is_char_boundary(n - 1) => &t[1..n - 1],
        _ => t,
    };
    cook_js_string(raw)
}

/// Resolve JS string-literal escape sequences in `raw` (delimiters already
/// stripped). Unknown escapes resolve to the escaped character itself (JS
/// semantics, e.g. `\q` → `q`).
fn cook_js_string(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(e) = chars.next() else {
            out.push('\\');
            break;
        };
        match e {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            '0' if !chars.peek().is_some_and(char::is_ascii_digit) => out.push('\0'),
            'x' => {
                let h: String = (0..2).filter_map(|_| chars.next()).collect();
                if let Some(ch) = u32::from_str_radix(&h, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                } else {
                    out.push('x');
                    out.push_str(&h);
                }
            }
            'u' => {
                if chars.peek() == Some(&'{') {
                    chars.next(); // consume '{'
                    let hex: String = chars.by_ref().take_while(|&ch| ch != '}').collect();
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    } else {
                        out.push('u');
                    }
                } else {
                    let h: String = (0..4).filter_map(|_| chars.next()).collect();
                    if let Some(ch) = u32::from_str_radix(&h, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    } else {
                        out.push('u');
                        out.push_str(&h);
                    }
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `TBp`: the object's `field` property exists and is a non-empty string literal
/// (a plain `string` or a `template_string` with no interpolation).
fn meta_string_field_nonempty(obj: tree_sitter::Node, src: &[u8], field: &str) -> bool {
    let mut c = obj.walk();
    for m in obj.named_children(&mut c) {
        if m.kind() != "pair" {
            continue;
        }
        let Some(key) = m.child_by_field_name("key") else {
            continue;
        };
        let kname = match key.kind() {
            "property_identifier" => node_text(key, src).to_string(),
            "string" => string_inner(key, src),
            _ => continue,
        };
        if kname != field {
            continue;
        }
        let Some(value) = m.child_by_field_name("value") else {
            return false;
        };
        return match value.kind() {
            "string" => !string_inner(value, src).is_empty(),
            "template_string" => {
                let mut cc = value.walk();
                let has_subst = value
                    .named_children(&mut cc)
                    .any(|n| n.kind() == "template_substitution");
                !has_subst && !string_inner(value, src).is_empty()
            }
            _ => false,
        };
    }
    false
}

/// Execute a workflow script's **synchronous** body, capturing `phase()` and
/// `log()` calls. The orchestration globals that require the async engine
/// (`agent`/`parallel`/`pipeline`) are not yet injected here — this validates
/// the engine + globals-injection + `meta` model the full executor extends.
///
/// # Errors
/// Returns [`WorkflowError`] if the engine fails to start or the script throws.
pub fn run_sync(script: &str) -> Result<RunOutcome, WorkflowError> {
    use rquickjs::{Context, Function, Runtime};

    let rt = Runtime::new().map_err(|e| WorkflowError::Engine(e.to_string()))?;
    let ctx = Context::full(&rt).map_err(|e| WorkflowError::Engine(e.to_string()))?;

    let progress: Rc<RefCell<Vec<Progress>>> = Rc::new(RefCell::new(Vec::new()));
    // Phase index counter: 1-based, incremented on each phase() call.
    let phase_counter: Rc<RefCell<u32>> = Rc::new(RefCell::new(0));
    let prepared = strip_meta_export(script);

    ctx.with(|ctx| -> Result<(), WorkflowError> {
        let globals = ctx.globals();

        let p_log = progress.clone();
        let log = Function::new(ctx.clone(), move |msg: String| {
            p_log.borrow_mut().push(Progress::Log { message: msg });
        })
        .map_err(|e| WorkflowError::Engine(e.to_string()))?;
        globals
            .set("log", log)
            .map_err(|e| WorkflowError::Engine(e.to_string()))?;

        let p_phase = progress.clone();
        let pc_phase = phase_counter.clone();
        let phase = Function::new(ctx.clone(), move |title: String| {
            let mut counter = pc_phase.borrow_mut();
            *counter += 1;
            let index = *counter;
            p_phase.borrow_mut().push(Progress::Phase { index, title });
        })
        .map_err(|e| WorkflowError::Engine(e.to_string()))?;
        globals
            .set("phase", phase)
            .map_err(|e| WorkflowError::Engine(e.to_string()))?;

        ctx.eval::<(), _>(prepared.as_bytes())
            .map_err(|e| WorkflowError::Script(e.to_string()))?;
        Ok(())
    })?;

    // The injected functions still hold `Rc` clones (the context owns them), so
    // clone the captured events out rather than unwrapping the `Rc`.
    let progress = progress.borrow().clone();
    // A classic (non-async) script cannot use top-level `return`, so there is
    // never a captured result on this path.
    Ok(RunOutcome {
        progress,
        result: None,
        failures: Vec::new(),
    })
}

/// Execute a workflow script's **async** body, capturing `phase()`/`log()` and
/// routing `agent(prompt)` through `agent_runner`.
///
/// The body is wrapped in an `async` IIFE so top-level `await` works (claude-code
/// runs the script as an `AsyncFunction`); QuickJS schedules the continuations as
/// microtasks, which we drive to completion via the runtime job queue. `agent()`
/// resolves synchronously through `agent_runner` (the real runtime blocks on a
/// subagent and returns its result), so sequential `await agent(...)` chains run
/// in order.
///
/// # Errors
/// Returns [`WorkflowError`] if the engine fails to start, the script throws, or
/// a job raises.
pub fn run<R>(script: &str, agent_runner: R) -> Result<RunOutcome, WorkflowError>
where
    R: FnMut(&[String], &[String]) -> Vec<String> + 'static,
{
    run_with_progress(
        script,
        agent_runner,
        |_: &Progress| {},
        None,
        false,
        None,
        None,
    )
}

/// Like [`run`], but also fires `on_progress` for each `phase()`/`log()` event
/// **as it happens** (live), in addition to collecting them into the returned
/// [`RunOutcome`]. The host forwards these so a running workflow's progress is
/// visible (e.g. spooled to the task output) before the script completes.
///
/// # Errors
/// Returns [`WorkflowError`] if the engine fails to start, the script throws, or
/// a job raises.
#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
pub fn run_with_progress<R, P>(
    script: &str,
    agent_runner: R,
    on_progress: P,
    budget: Option<Arc<dyn WorkflowBudgetSource>>,
    // When `true`, `workflow()` runs a nested workflow inline (the host runner
    // resolves+runs it via an `__wf_nested` agent() call); `false` (nested runs)
    // leaves the throwing default — claude-code's one-level nesting limit.
    allow_nested: bool,
    // The `args` global value as a JSON string (the Workflow tool's `args` input
    // / a `workflow()` call's args). `None` ⇒ `undefined`. JSON is a subset of JS
    // expressions, so it is a valid initializer.
    args: Option<String>,
    // Cooperative-cancel flag (claude-code's `abortController`). When set and the
    // host flips it to `true` (e.g. `Task::kill`), the embedded engine's
    // interrupt handler aborts the running script — including a pure-CPU/JS
    // infinite loop that never calls `agent()` — so killing a workflow does not
    // leak the QuickJS OS thread. `None` ⇒ no interrupt handler.
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<RunOutcome, WorkflowError>
where
    // `(prompts, opts_json) -> results`: the two parallel arrays the pump
    // dispatches — each `opts_json[i]` is `JSON.stringify(agent()'s opts)` for
    // `prompts[i]`. The host runner maps the spawn-affecting opts (agentType /
    // model / isolation) onto each subagent request.
    R: FnMut(&[String], &[String]) -> Vec<String> + 'static,
    // Fires for every `phase()`/`log()` as it is emitted (live).
    P: FnMut(&Progress) + 'static,
{
    use rquickjs::{Context, Function, Runtime};

    let rt = Runtime::new().map_err(|e| WorkflowError::Engine(e.to_string()))?;
    // Cooperative cancellation: QuickJS calls the interrupt handler periodically
    // during execution; returning `true` aborts with an uncatchable error. This
    // is what lets `Task::kill` stop a runaway pure-JS loop (no `agent()` call to
    // observe a dropped channel) instead of leaking the script thread.
    if let Some(flag) = cancel.clone() {
        rt.set_interrupt_handler(Some(Box::new(move || {
            flag.load(std::sync::atomic::Ordering::Relaxed)
        })));
    }
    let ctx = Context::full(&rt).map_err(|e| WorkflowError::Engine(e.to_string()))?;

    let progress: Rc<RefCell<Vec<Progress>>> = Rc::new(RefCell::new(Vec::new()));
    // Phase state: 1-based index counter + the current phase title.
    let phase_counter: Rc<RefCell<u32>> = Rc::new(RefCell::new(0));
    let current_phase: Rc<RefCell<Option<(u32, String)>>> = Rc::new(RefCell::new(None));
    let error: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let result_slot: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let failures: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let runner = Rc::new(RefCell::new(agent_runner));
    let on_progress = Rc::new(RefCell::new(on_progress));
    let prepared = strip_meta_export(script);
    // Wrap in an async IIFE; route a throw into `__wf_error` so it survives the
    // microtask boundary (a rejected top-level promise would otherwise be lost).
    // claude-code (V8) captures `(e && e.stack) || e`, but QuickJS's `e.stack`
    // omits the message line, so capture `String(e)` (the message) plus the
    // stack — the V8-vs-QuickJS stack frames differ inherently regardless.
    // The IIFE's resolved value is the script's `return` value; capture it via
    // `.then` into `__wf_result` (JSON-serialised) so the Workflow tool can
    // return it. `undefined` (no `return`) leaves the result unset. A throw is
    // already routed to `__wf_error` inside the `catch`, so the success handler
    // sees `undefined` and the rejection handler is a no-op.
    let wrapped = format!(
        "(async () => {{ 'use strict'; try {{\n{prepared}\n}} catch (e) {{ globalThis.__wf_error(String(e) + (e && e.stack ? \"\\n\" + e.stack : \"\")); }} }})().then((v) => {{ try {{ if (v !== undefined) globalThis.__wf_result(JSON.stringify(v)); }} catch (e) {{ globalThis.__wf_error(String(e)); }} }}, () => {{}});"
    );

    ctx.with(|ctx| -> Result<(), WorkflowError> {
        let globals = ctx.globals();
        let eng = |e: rquickjs::Error| WorkflowError::Engine(e.to_string());

        let p_log = progress.clone();
        let op_log = on_progress.clone();
        globals
            .set(
                "log",
                Function::new(ctx.clone(), move |msg: String| {
                    let prog = Progress::Log { message: msg };
                    (op_log.borrow_mut())(&prog);
                    p_log.borrow_mut().push(prog);
                })
                .map_err(eng)?,
            )
            .map_err(eng)?;

        let p_phase = progress.clone();
        let op_phase = on_progress.clone();
        let pc_phase = phase_counter.clone();
        let cp_phase = current_phase.clone();
        globals
            .set(
                "phase",
                Function::new(ctx.clone(), move |title: String| {
                    let mut counter = pc_phase.borrow_mut();
                    *counter += 1;
                    let index = *counter;
                    drop(counter);
                    // Track the current phase for agent events.
                    *cp_phase.borrow_mut() = Some((index, title.clone()));
                    let prog = Progress::Phase { index, title };
                    (op_phase.borrow_mut())(&prog);
                    p_phase.borrow_mut().push(prog);
                })
                .map_err(eng)?,
            )
            .map_err(eng)?;

        // Native batch dispatcher: the JS `__wf_pump` hands it every concurrently
        // pending agent prompt + its JSON-encoded opts at once; the runner
        // resolves them (the real runtime spawns the subagents in parallel).
        // We augment each opts_json with `__wf_phase: {index, title}` (current
        // phase at dispatch time) so the bridge can emit structured
        // `workflow_agent` progress events with the right phaseIndex/phaseTitle.
        // The bridge strips `__wf_phase` before forwarding opts to the spawner.
        let r = runner.clone();
        let cp_dispatch = current_phase.clone();
        globals
            .set(
                "__wf_dispatch_batch",
                Function::new(
                    ctx.clone(),
                    move |prompts: Vec<String>, opts_json: Vec<String>| -> Vec<String> {
                        // Augment each opts with the current phase snapshot.
                        let phase_snapshot = cp_dispatch.borrow().clone();
                        let augmented: Vec<String> = opts_json
                            .iter()
                            .map(|o| {
                                if let Some((idx, title)) = &phase_snapshot {
                                    // Inject __wf_phase into the opts object.
                                    // The opts is always a valid JSON object
                                    // (the prelude always passes {} for bare agent()).
                                    let mut v: serde_json::Value =
                                        serde_json::from_str(o).unwrap_or(serde_json::Value::Object(Default::default()));
                                    if let Some(obj) = v.as_object_mut() {
                                        obj.insert(
                                            "__wf_phase".to_string(),
                                            serde_json::json!({"index": idx, "title": title}),
                                        );
                                    }
                                    serde_json::to_string(&v).unwrap_or_else(|_| o.clone())
                                } else {
                                    o.clone()
                                }
                            })
                            .collect();
                        (r.borrow_mut())(&prompts, &augmented)
                    },
                )
                .map_err(eng)?,
            )
            .map_err(eng)?;

        let err = error.clone();
        globals
            .set(
                "__wf_error",
                Function::new(ctx.clone(), move |msg: String| {
                    *err.borrow_mut() = Some(msg);
                })
                .map_err(eng)?,
            )
            .map_err(eng)?;

        let failure_sink = failures.clone();
        globals
            .set(
                "__wf_record_failure",
                Function::new(ctx.clone(), move |message: String| {
                    failure_sink.borrow_mut().push(message);
                })
                .map_err(eng)?,
            )
            .map_err(eng)?;

        // Native sink for the script's JSON-serialised return value.
        let res = result_slot.clone();
        globals
            .set(
                "__wf_result",
                Function::new(ctx.clone(), move |v: String| {
                    *res.borrow_mut() = Some(v);
                })
                .map_err(eng)?,
            )
            .map_err(eng)?;

        // Real token budget (when the host supplies a source): set
        // `globalThis.budget` BEFORE the prelude so its `if (!('budget' in
        // globalThis))` default is skipped. `budget.total` snapshots the turn's
        // target; `spent()`/`remaining()` read the shared pool live.
        if let Some(src) = budget.as_ref() {
            let s_total = src.clone();
            globals
                .set(
                    "__wf_budget_total",
                    Function::new(ctx.clone(), move || -> Option<f64> {
                        s_total.total().map(|t| t as f64)
                    })
                    .map_err(eng)?,
                )
                .map_err(eng)?;
            let s_spent = src.clone();
            globals
                .set(
                    "__wf_budget_spent",
                    Function::new(ctx.clone(), move || -> f64 { s_spent.spent() as f64 })
                        .map_err(eng)?,
                )
                .map_err(eng)?;
            ctx.eval::<(), _>(
                b"globalThis.budget = { total: __wf_budget_total(), spent: () => __wf_budget_spent(), remaining: () => { const t = globalThis.budget.total; return (t === null || t === undefined) ? Infinity : Math.max(0, t - __wf_budget_spent()); } };" as &[u8],
            )
            .map_err(|e| WorkflowError::Engine(e.to_string()))?;
        }

        // `args` global (the Workflow tool's `args` input / a `workflow()` call's
        // args). Set BEFORE the prelude so its `if (!('args' in globalThis))`
        // default is skipped.
        //
        // Parsed with `JSON.parse`, NOT interpolated as an object literal. The
        // two are not equivalent: in a literal, `__proto__` is a PROTOTYPE
        // SETTER, not an own property. Splicing caller JSON in directly meant
        // `{"__proto__": {"x": 1}}` gave an `args` whose `Object.keys` and
        // `hasOwnProperty` both showed nothing while `args.x` still read back
        // `1` — so a script that allow-lists its input by enumerating keys
        // passes values it never saw. `JSON.parse` treats the key as ordinary
        // data, so enumeration and property access agree again.
        //
        // The JSON is re-encoded as a JS STRING literal, which also removes the
        // "is this JSON also a valid JS expression" question the old comment
        // rested on.
        if let Some(args_json) = &args {
            let literal = serde_json::to_string(args_json).map_err(|e| {
                WorkflowError::Engine(format!("could not encode workflow args: {e}"))
            })?;
            ctx.eval::<(), _>(format!("globalThis.args = JSON.parse({literal});").as_bytes())
                .map_err(|e| WorkflowError::Engine(e.to_string()))?;
        }

        // Top-level `workflow()`: run a nested workflow INLINE, in this same
        // runtime (claude-code shares the parent's runtime state — concurrency
        // cap, agent counter, abort, budget). It (1) resolves the script via the
        // host (an `agent('', { __wf_resolve })` call the runner reads as a file
        // request, returning the source) and (2) evaluates that source as an
        // async function in THIS context, so its `agent()`/`parallel()`/`budget`
        // all share the parent's globals/queue. A depth counter throws on a
        // nested `workflow()` (claude-code's one-level limit); `args` is swapped
        // to the call's value for the nested body. (`agent` is resolved lazily at
        // call time, so defining this before the prelude is fine.)
        if allow_nested {
            ctx.eval::<(), _>(
                "globalThis.__wf_depth = 0; globalThis.workflow = async (nameOrRef, a) => { if (globalThis.__wf_depth >= 1) throw new Error(\"workflow() cannot be called from within a child workflow \u{2014} nesting is limited to one level. Inline the inner script or call its agents directly.\"); const spec = (typeof nameOrRef === 'string') ? { name: nameOrRef } : nameOrRef; const src = await agent('', { __wf_resolve: JSON.stringify(spec) }); if (src === '') throw new Error(\"workflow(): could not resolve the nested workflow\"); globalThis.__wf_depth += 1; const savedArgs = globalThis.args; globalThis.args = a; try { return await (new Function('return (async () => {\\n' + src + '\\n})();'))(); } finally { globalThis.__wf_depth -= 1; globalThis.args = savedArgs; } };".as_bytes(),
            )
            .map_err(|e| WorkflowError::Engine(e.to_string()))?;
        }

        // Orchestration prelude: agent() (deferred-promise) + parallel() /
        // pipeline() + budget/args/workflow defined in JS, with the byte-locked
        // argument validation. The native __wf_dispatch_batch (above) receives
        // each concurrent batch the pump produces.
        ctx.eval::<(), _>(WORKFLOW_PRELUDE.as_bytes())
            .map_err(|e| WorkflowError::Engine(e.to_string()))?;

        // Eval the IIFE (returns a pending promise we drive below).
        ctx.eval::<rquickjs::Value, _>(wrapped.as_bytes())
            .map_err(|e| WorkflowError::Script(e.to_string()))?;
        Ok(())
    })?;

    // Drive the runtime: drain microtasks, then pump the agent queue (dispatch
    // every concurrently-pending agent as ONE batch + resolve), and repeat until
    // the workflow settles (no jobs and no queued agents).
    loop {
        while rt
            .execute_pending_job()
            .map_err(|e| WorkflowError::Script(format!("{e:?}")))?
        {}
        let pumped = ctx.with(|ctx| -> Result<bool, WorkflowError> {
            let pump: Function = ctx
                .globals()
                .get("__wf_pump")
                .map_err(|e| WorkflowError::Engine(e.to_string()))?;
            pump.call::<_, bool>(())
                .map_err(|e| WorkflowError::Script(e.to_string()))
        })?;
        if !pumped {
            break;
        }
    }

    if let Some(e) = error.borrow().clone() {
        return Err(WorkflowError::Script(e));
    }
    let progress = progress.borrow().clone();
    let result = result_slot.borrow().clone();
    let failures = failures.borrow().clone();
    Ok(RunOutcome {
        progress,
        result,
        failures,
    })
}

#[cfg(test)]
mod meta_validation_tests {
    use super::*;

    fn err(script: &str) -> String {
        match validate_meta(script) {
            Err(WorkflowError::Script(m)) => m,
            other => panic!("expected Script error, got {other:?}"),
        }
    }

    #[test]
    fn accepts_a_valid_pure_literal_meta() {
        validate_meta(
            "export const meta = { name: 'x', description: 'd', phases: [{ title: 'A' }] };\nreturn 1;",
        )
        .expect("valid meta");
        // numbers, negative numbers, booleans, null, nested objects/arrays, and
        // non-interpolated templates are all pure literals.
        validate_meta(
            "export const meta = { name: `n`, description: 'd', n: -3, b: true, z: null, o: { k: [1, 2] } };",
        )
        .expect("valid literals");
    }

    #[test]
    fn validate_body_compiles_async_body_without_running_it() {
        validate_body(
            "export const meta = { name: 'x', description: 'd' };\nawait agent('p'); return 1;",
        )
        .expect("valid async body");
        assert!(
            validate_body("export const meta = { name: 'x', description: 'd' };\nif (").is_err()
        );
        assert!(validate_body(
            "export const meta = { name: 'x', description: 'd' };\nwith ({x: 1}) { log(x); }"
        )
        .is_err());
        assert!(validate_body(
            "export const meta = { name: 'x', description: 'd' };\nawait import('x');"
        )
        .is_err());
    }

    #[test]
    fn first_statement_must_be_export_const_meta() {
        let m = "`export const meta = { name, description, phases }` must be the FIRST statement in the script";
        assert_eq!(
            err("const x = 1; export const meta = { name: 'a', description: 'b' };"),
            m
        );
        assert_eq!(err("log('hi'); return 1;"), m);
        assert_eq!(err("const meta = { name: 'a', description: 'b' };"), m); // not exported
        assert_eq!(err("export let meta = { name: 'a', description: 'b' };"), m); // let, not const
        assert_eq!(err("export const other = { name: 'a' };"), m); // wrong name
    }

    #[test]
    fn name_and_description_must_be_nonempty_strings() {
        assert_eq!(
            err("export const meta = { description: 'd' };"),
            "meta.name must be a non-empty string"
        );
        assert_eq!(
            err("export const meta = { name: '', description: 'd' };"),
            "meta.name must be a non-empty string"
        );
        assert_eq!(
            err("export const meta = { name: 5, description: 'd' };"),
            "meta.name must be a non-empty string"
        );
        assert_eq!(
            err("export const meta = { name: 'x' };"),
            "meta.description must be a non-empty string"
        );
        assert_eq!(
            err("export const meta = { name: 'x', description: '' };"),
            "meta.description must be a non-empty string"
        );
    }

    #[test]
    fn meta_array_len_matches_literal_phase_count() {
        let script = "export const meta = { name: 'x', description: 'd', phases: [{ title: 'A' }, { title: 'B' }] };";
        assert_eq!(meta_array_len(script, "phases"), Some(2));
        assert_eq!(meta_array_len(script, "missing"), None);
        assert_eq!(
            meta_array_len(
                "export const meta = { name: 'x', description: 'd', phases: 'nope' };",
                "phases"
            ),
            None
        );
        assert_eq!(
            meta_array_len(
                "export const meta = { name: 'x', description: 'd', phases: [, ,] };",
                "phases"
            ),
            None
        );
        assert_eq!(
            meta_array_len(
                "export const meta = { name: 'x', description: 'd', phases: [1, { foo: 'x' }, { title: 'A' }] };",
                "phases"
            ),
            Some(1)
        );
        assert_eq!(
            meta_array_len(
                "export const meta = { name: 'x', description: 'd', phases: [] };",
                "phases"
            ),
            Some(0)
        );
    }

    #[test]
    fn pure_literal_violations_are_byte_exact() {
        let p = |reason: &str| format!("meta must be a pure literal: {reason}");
        assert_eq!(
            err("export const meta = { name: 'x', description: 'd', ...rest };"),
            p("only plain properties allowed in meta")
        );
        assert_eq!(
            err("export const meta = { ['a' + 'b']: 1, name: 'x', description: 'd' };"),
            p("computed keys not allowed in meta")
        );
        assert_eq!(
            err("export const meta = { foo() {}, name: 'x', description: 'd' };"),
            p("methods/accessors not allowed in meta")
        );
        assert_eq!(
            err("export const meta = { name: 'x', description: 'd', v: someVar };"),
            p("non-literal node type in meta: Identifier")
        );
        assert_eq!(
            err("export const meta = { name: 'x', description: 'd', v: f() };"),
            p("non-literal node type in meta: CallExpression")
        );
        assert_eq!(
            err("export const meta = { name: 'x', description: 'd', v: `a${b}c` };"),
            p("template interpolation not allowed in meta")
        );
        assert_eq!(
            err("export const meta = { name: 'x', description: 'd', v: [...a] };"),
            p("spread not allowed in meta")
        );
        assert_eq!(
            err("export const meta = { name: 'x', description: 'd', constructor: 1 };"),
            p("reserved key name not allowed in meta: constructor")
        );
        assert_eq!(
            err("export const meta = { name: 'x', description: 'd', v: +1 };"),
            p("only negative-number unary allowed in meta")
        );
        // shorthand property references a runtime identifier.
        assert_eq!(
            err("export const meta = { name, description: 'd' };"),
            p("non-literal node type in meta: Identifier")
        );
    }

    #[test]
    fn string_keys_and_values_are_escape_cooked() {
        // cook_js_string resolves the standard JS escapes.
        assert_eq!(cook_js_string(r"constructor"), "constructor");
        assert_eq!(cook_js_string(r"a\x62c"), "abc");
        assert_eq!(cook_js_string(r"x\u{1F600}"), "x\u{1F600}");
        assert_eq!(cook_js_string(r"a\tb\nc"), "a\tb\nc");
        assert_eq!(cook_js_string("plain"), "plain");
        // A quoted reserved key is still detected.
        assert_eq!(
            err(r#"export const meta = { name: 'x', description: 'd', "constructor": 1 };"#),
            "meta must be a pure literal: reserved key name not allowed in meta: constructor"
        );
        // An escape-bearing name/description cooks to non-empty and passes.
        validate_meta(r#"export const meta = { name: "abc", description: "d\tx" };"#)
            .expect("escaped strings cook to non-empty");
    }
}

#[cfg(test)]
mod determinism_tests {
    use super::*;

    #[test]
    fn rejects_date_now_math_random_and_bare_new_date() {
        for s in [
            "export const meta = { name: 'a', description: 'b' };\nconst t = Date.now();",
            "log(Math.random());",
            "const d = new Date();",
            "const d = new Date;",
        ] {
            let err = check_determinism(s).expect_err("should reject");
            match err {
                WorkflowError::Script(m) => assert_eq!(m, NON_DETERMINISTIC_MESSAGE),
                other => panic!("got {other:?}"),
            }
        }
    }

    #[test]
    fn allows_deterministic_scripts_and_dated_new_date() {
        // `new Date(args.ts)` (with an argument) is allowed — only the zero-arg
        // form reads the wall clock.
        for s in [
            "export const meta = { name: 'a', description: 'b' };\nawait agent('x');",
            "const d = new Date(args.ts);",
            "const d = new Date(1700000000000);",
            "log('date now is fine in a string: Date.now()');",
        ] {
            check_determinism(s).unwrap_or_else(|e| panic!("should allow {s:?}: {e:?}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Batch runner for scripts that never call `agent()` (the batch is always
    /// empty, so this is never actually invoked).
    fn no_agents(prompts: &[String], _opts: &[String]) -> Vec<String> {
        prompts.iter().map(|_| String::new()).collect()
    }

    #[test]
    fn schema_agent_result_is_parsed_into_an_object() {
        // A schema run's result is JSON; agent({schema}) resolves with the parsed
        // object so the script can use `r.bugs` directly (claude-code contract).
        let script = r#"
            const r = await agent('find', { schema: { type: 'object' } });
            log('count=' + r.bugs.length + ' first=' + r.bugs[0]);
            const r2 = await agent('plain');
            log('plain=' + r2);
        "#;
        let out = run(script, |prompts: &[String], opts: &[String]| {
            prompts
                .iter()
                .zip(opts)
                .map(|(_, o)| {
                    if o.contains("schema") {
                        r#"{"bugs":["x","y"]}"#.to_string()
                    } else {
                        "raw text".to_string()
                    }
                })
                .collect()
        })
        .unwrap();
        assert_eq!(
            out.progress,
            vec![
                Progress::Log {
                    message: "count=2 first=x".into()
                },
                Progress::Log {
                    message: "plain=raw text".into()
                },
            ]
        );
    }

    #[test]
    fn cancel_flag_aborts_a_runaway_loop() {
        use std::sync::atomic::AtomicBool;
        // A pure-CPU loop with no agent() calls would never observe a dropped
        // channel; the interrupt handler (driven by the cancel flag) is what
        // stops it. Pre-set the flag so the very first interrupt check aborts.
        let cancel = Arc::new(AtomicBool::new(true));
        let err = run_with_progress(
            "let s = 0; for (let i = 0; i < 1e12; i++) { s += i; } log('done');",
            no_agents,
            |_: &Progress| {},
            None,
            false,
            None,
            Some(cancel),
        )
        .unwrap_err();
        // The interrupt aborts the run (the exact phase it fires in — prelude eval
        // vs. a pumped job — determines Engine vs. Script; both are terminal).
        let msg = err.to_string();
        assert!(
            msg.contains("QuickJS") || msg.contains("nterrupt") || msg.contains("xception"),
            "expected an interrupt/abort error, got {err:?}"
        );
    }

    #[test]
    fn null_sentinel_resolves_agent_to_null() {
        // A dead/skipped agent (host returns WF_NULL_SENTINEL) resolves to null,
        // not "" — so `=== null` checks and `??` behave per the contract.
        let script = r#"
            const r = await agent('dead');
            log('isNull=' + (r === null));
            const rs = await parallel([() => agent('a'), () => agent('dead')]);
            log('filtered=' + rs.filter(Boolean).join(','));
        "#;
        let out = run(script, |prompts: &[String], _opts: &[String]| {
            prompts
                .iter()
                .map(|p| {
                    if p == "dead" {
                        WF_NULL_SENTINEL.to_string()
                    } else {
                        format!("ok:{p}")
                    }
                })
                .collect()
        })
        .unwrap();
        assert_eq!(
            out.progress,
            vec![
                Progress::Log {
                    message: "isNull=true".into()
                },
                Progress::Log {
                    message: "filtered=ok:a".into()
                },
            ]
        );
    }

    #[test]
    fn engine_evaluates_js() {
        // (kept as a fast smoke test of the embedded engine)
        let out = run_sync("log(String(1 + 2 * 3))").unwrap();
        assert_eq!(
            out.progress,
            vec![Progress::Log {
                message: "7".into()
            }]
        );
    }

    #[test]
    fn strip_meta_export_only_touches_the_meta_decl() {
        let src = "export const meta = { name: 'x' }\nlog('hi')";
        assert_eq!(
            strip_meta_export(src),
            "const meta = { name: 'x' }\nlog('hi')"
        );
        // Indentation preserved; unrelated `export` lines untouched.
        assert_eq!(
            strip_meta_export("  export const meta={}"),
            "  const meta={}"
        );
        assert_eq!(strip_meta_export("export default 1"), "export default 1");
    }

    #[test]
    fn runs_a_workflow_style_script_capturing_phase_and_log() {
        let script = r#"
export const meta = {
  name: 'demo',
  description: 'foundation smoke test',
  phases: [{ title: 'Scan' }, { title: 'Report' }],
}
phase('Scan')
const items = ['a', 'b', 'c']
log(`${items.length} items to scan`)
phase('Report')
log(`done: ${items.join(',')}`)
"#;
        let out = run_sync(script).unwrap();
        assert_eq!(
            out.progress,
            vec![
                Progress::Phase {
                    index: 1,
                    title: "Scan".into()
                },
                Progress::Log {
                    message: "3 items to scan".into()
                },
                Progress::Phase {
                    index: 2,
                    title: "Report".into()
                },
                Progress::Log {
                    message: "done: a,b,c".into()
                },
            ]
        );
    }

    #[test]
    fn script_error_is_surfaced() {
        let err = run_sync("log(undefinedThing.x)").unwrap_err();
        assert!(matches!(err, WorkflowError::Script(_)), "got {err:?}");
    }

    #[test]
    fn runs_async_script_with_sequential_agents() {
        // `await agent(...)` chains run in order; the runner sees the prompts
        // sequentially and its results flow back into the script.
        let script = r#"
export const meta = { name: 'a', description: 'async smoke' }
phase('Work')
const a = await agent('first')
log('got: ' + a)
const b = await agent('second')
log('got: ' + b)
"#;
        let mut calls = 0;
        let out = run(
            script,
            move |prompts: &[String], _opts: &[String]| -> Vec<String> {
                prompts
                    .iter()
                    .map(|prompt| {
                        calls += 1;
                        format!("[r{calls}:{prompt}]")
                    })
                    .collect()
            },
        )
        .unwrap();
        assert_eq!(
            out.progress,
            vec![
                Progress::Phase {
                    index: 1,
                    title: "Work".into()
                },
                Progress::Log {
                    message: "got: [r1:first]".into()
                },
                Progress::Log {
                    message: "got: [r2:second]".into()
                },
            ]
        );
    }

    #[test]
    fn async_script_uses_agent_result_in_logic() {
        // The result of an awaited agent drives subsequent control flow.
        let script = r#"
const n = Number(await agent('count'))
for (let i = 0; i < n; i++) log('item ' + i)
"#;
        let out = run(script, |prompts: &[String], _opts: &[String]| {
            prompts.iter().map(|_| "3".to_string()).collect()
        })
        .unwrap();
        assert_eq!(
            out.progress,
            vec![
                Progress::Log {
                    message: "item 0".into()
                },
                Progress::Log {
                    message: "item 1".into()
                },
                Progress::Log {
                    message: "item 2".into()
                },
            ]
        );
    }

    #[test]
    fn async_script_throw_surfaces_after_await() {
        let err = run(
            "await agent('x'); throw new Error('boom')",
            |prompts: &[String], _opts: &[String]| {
                prompts.iter().map(|_| "ok".to_string()).collect()
            },
        )
        .unwrap_err();
        match err {
            WorkflowError::Script(s) => assert!(s.contains("boom"), "got: {s}"),
            other => panic!("expected Script error, got {other:?}"),
        }
    }

    #[test]
    fn parallel_collects_results_in_order() {
        let script = r#"
const rs = await parallel([
  () => agent('a'),
  () => agent('b'),
  () => agent('c'),
])
log(rs.join('|'))
"#;
        let mut n = 0;
        let out = run(
            script,
            move |prompts: &[String], _opts: &[String]| -> Vec<String> {
                prompts
                    .iter()
                    .map(|p| {
                        n += 1;
                        format!("{p}{n}")
                    })
                    .collect()
            },
        )
        .unwrap();
        assert_eq!(
            out.progress,
            vec![Progress::Log {
                message: "a1|b2|c3".into()
            }]
        );
    }

    #[test]
    fn parallel_throwing_thunk_becomes_null() {
        let script = r#"
const rs = await parallel([
  () => agent('ok'),
  () => { throw new Error('boom') },
])
log(String(rs[0]) + ',' + String(rs[1]))
"#;
        let out = run(script, |prompts: &[String], _opts: &[String]| {
            prompts.iter().map(|_| "OK".to_string()).collect()
        })
        .unwrap();
        assert_eq!(
            out.progress,
            vec![
                Progress::Log {
                    message: "parallel[1] failed: boom".into()
                },
                Progress::Log {
                    message: "OK,null".into()
                },
            ]
        );
    }

    #[test]
    fn parallel_logs_budget_drop_summary_instead_of_per_item_failures() {
        let script = r#"
const rs = await parallel([
  () => agent('a'),
  () => agent('b'),
])
log(rs.map(value => String(value)).join(','))
"#;
        let msg = "Workflow token budget exceeded (1 / 1 output tokens). Stopping further agent() calls. In-flight agents will complete; their results are preserved.";
        let out = run(script, move |_prompts: &[String], _opts: &[String]| {
            vec![
                format!("{}{msg}", super::WF_THROW_PREFIX),
                format!("{}{msg}", super::WF_THROW_PREFIX),
            ]
        })
        .unwrap();
        assert_eq!(
            out.progress,
            vec![Progress::Log {
                message: "null,null".into()
            }]
        );
        assert_eq!(
            out.failures,
            vec!["parallel: 2 slots dropped \u{2014} token budget exceeded"]
        );
    }

    #[test]
    fn pipeline_logs_budget_drop_summary_for_a_single_item() {
        let script = r#"
const rs = await pipeline(
  ['only'],
  (item) => agent(item),
)
log(String(rs[0]))
"#;
        let msg = "Workflow token budget exceeded (1 / 1 output tokens). Stopping further agent() calls. In-flight agents will complete; their results are preserved.";
        let out = run(script, move |_prompts: &[String], _opts: &[String]| {
            vec![format!("{}{msg}", super::WF_THROW_PREFIX)]
        })
        .unwrap();
        assert_eq!(
            out.progress,
            vec![Progress::Log {
                message: "null".into()
            }]
        );
        assert_eq!(
            out.failures,
            vec!["pipeline: 1 slot dropped \u{2014} token budget exceeded"]
        );
    }

    #[test]
    fn pipeline_logs_item_failures_and_keeps_other_items_running() {
        let script = r#"
const rs = await pipeline(
  ['drop', 'keep'],
  (item) => { if (item === 'drop') throw new Error('boom'); return item; },
  (value) => value + '!'
)
log(rs.map(value => String(value)).join('|'))
"#;
        let out = run(script, no_agents).unwrap();
        assert_eq!(
            out.progress,
            vec![
                Progress::Log {
                    message: "pipeline[0] failed: boom".into()
                },
                Progress::Log {
                    message: "null|keep!".into()
                },
            ]
        );
        assert_eq!(out.failures, vec!["pipeline[0] failed: boom"]);
    }

    #[test]
    fn parallel_dispatches_concurrent_agents_as_one_batch() {
        // parallel() starts every thunk before awaiting → the agents queue
        // together and the driver dispatches them as a SINGLE concurrent batch.
        let script = r#"
const rs = await parallel([() => agent('a'), () => agent('b'), () => agent('c')])
log(rs.join('|'))
"#;
        let batches: Rc<RefCell<Vec<usize>>> = Rc::new(RefCell::new(Vec::new()));
        let b = batches.clone();
        let out = run(
            script,
            move |prompts: &[String], _opts: &[String]| -> Vec<String> {
                b.borrow_mut().push(prompts.len());
                prompts.iter().map(|p| format!("R:{p}")).collect()
            },
        )
        .unwrap();
        assert_eq!(
            out.progress,
            vec![Progress::Log {
                message: "R:a|R:b|R:c".into()
            }]
        );
        assert_eq!(
            *batches.borrow(),
            vec![3],
            "all three agents dispatched in ONE concurrent batch"
        );
    }

    #[test]
    fn sequential_awaits_are_separate_batches() {
        // A sequential `await agent()` chain dispatches one prompt at a time.
        let script = r#"
const a = await agent('a')
const b = await agent('b')
log(a + b)
"#;
        let batches: Rc<RefCell<Vec<usize>>> = Rc::new(RefCell::new(Vec::new()));
        let b = batches.clone();
        let out = run(
            script,
            move |prompts: &[String], _opts: &[String]| -> Vec<String> {
                b.borrow_mut().push(prompts.len());
                prompts.iter().map(|p| p.to_uppercase()).collect()
            },
        )
        .unwrap();
        assert_eq!(
            out.progress,
            vec![Progress::Log {
                message: "AB".into()
            }]
        );
        assert_eq!(
            *batches.borrow(),
            vec![1, 1],
            "two sequential awaits → two batches of one"
        );
    }

    #[test]
    fn pipeline_stages_batch_across_items() {
        // pipeline runs items independently, so each stage's agents across all
        // items dispatch together: stage 1 over [x,y] is one batch, stage 2 is
        // the next.
        let script = r#"
const rs = await pipeline(
  ['x', 'y'],
  (it) => agent('s1:' + it),
  (prev) => agent('s2:' + prev),
)
log(rs.join('|'))
"#;
        let batches: Rc<RefCell<Vec<usize>>> = Rc::new(RefCell::new(Vec::new()));
        let b = batches.clone();
        let out = run(
            script,
            move |prompts: &[String], _opts: &[String]| -> Vec<String> {
                b.borrow_mut().push(prompts.len());
                prompts.iter().map(|p| format!("[{p}]")).collect()
            },
        )
        .unwrap();
        assert_eq!(
            out.progress,
            vec![Progress::Log {
                message: "[s2:[s1:x]]|[s2:[s1:y]]".into()
            }]
        );
        assert_eq!(
            *batches.borrow(),
            vec![2, 2],
            "stage 1 over both items is one batch, stage 2 the next"
        );
    }

    #[test]
    fn pipeline_runs_each_item_through_all_stages() {
        let script = r#"
const rs = await pipeline(
  [1, 2, 3],
  (x) => x + 1,
  (x) => x * 10,
)
log(rs.join(','))
"#;
        let out = run(script, no_agents).unwrap();
        assert_eq!(
            out.progress,
            vec![Progress::Log {
                message: "20,30,40".into()
            }]
        );
    }

    #[test]
    fn pipeline_skips_remaining_stages_after_a_null_result() {
        // Claude Code stops an item's chain as soon as a stage resolves to
        // null; later stages must not be invoked with that sentinel.
        let script = r#"
const rs = await pipeline(
  ['drop', 'keep'],
  (item) => item === 'drop' ? null : item,
  (value) => { log('stage2:' + value); return value + '!'; },
)
log(rs.map(value => String(value)).join('|'))
"#;
        let out = run(script, no_agents).unwrap();
        assert_eq!(
            out.progress,
            vec![
                Progress::Log {
                    message: "stage2:keep".into()
                },
                Progress::Log {
                    message: "null|keep!".into()
                },
            ]
        );
    }

    #[test]
    fn pipeline_rejects_non_array_first_arg() {
        let err = run("await pipeline('nope', x => x)", no_agents).unwrap_err();
        match err {
            WorkflowError::Script(s) => {
                assert!(
                    s.contains("pipeline() expects an array as the first argument"),
                    "got: {s}"
                );
                // Binary throws `TypeError` (not generic `Error`) for this
                // validator, matching parallel()'s validators.
                assert!(s.contains("TypeError"), "must be a TypeError; got: {s}");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn pipeline_rejects_non_function_stage() {
        let err = run("await pipeline([1], 'notafn')", no_agents).unwrap_err();
        match err {
            WorkflowError::Script(s) => assert!(
                s.contains("pipeline() stages must be functions"),
                "got: {s}"
            ),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn budget_args_workflow_globals_present() {
        let script = r#"
log('total=' + String(budget.total))
log('remaining=' + String(budget.remaining()))
log('spent=' + String(budget.spent()))
log('args=' + String(args))
log('wf=' + (typeof workflow))
"#;
        let out = run(script, no_agents).unwrap();
        assert_eq!(
            out.progress,
            vec![
                Progress::Log {
                    message: "total=null".into()
                },
                Progress::Log {
                    message: "remaining=Infinity".into()
                },
                Progress::Log {
                    message: "spent=0".into()
                },
                Progress::Log {
                    message: "args=undefined".into()
                },
                Progress::Log {
                    message: "wf=function".into()
                },
            ]
        );
    }

    #[test]
    fn nested_workflow_call_throws_clearly() {
        let err = run("await workflow('child')", no_agents).unwrap_err();
        match err {
            WorkflowError::Script(s) => {
                assert!(
                    s.contains("workflow() cannot be called from within a child workflow"),
                    "got: {s}"
                );
                assert!(s.contains("nesting is limited to one level"), "got: {s}");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parallel_rejects_non_array() {
        let err = run("await parallel('notanarray')", no_agents).unwrap_err();
        match err {
            WorkflowError::Script(s) => assert!(
                s.contains("parallel() expects an array of functions"),
                "got: {s}"
            ),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parallel_rejects_non_function_items() {
        // Passing a promise (a non-function) in the array should trigger throw 2.
        let err = run("await parallel([Promise.resolve('x')])", no_agents).unwrap_err();
        match err {
            WorkflowError::Script(s) => assert!(
                s.contains("parallel() expects an array of functions, not promises"),
                "got: {s}"
            ),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn captures_the_scripts_return_value_as_json() {
        // The script's `return` value is the Workflow tool's result.
        let out = run("return { ok: 1, items: [2, 3] };", no_agents).unwrap();
        assert_eq!(out.result.as_deref(), Some(r#"{"ok":1,"items":[2,3]}"#));
    }

    #[test]
    fn no_return_leaves_the_result_unset() {
        let out = run("log('side effect only');", no_agents).unwrap();
        assert_eq!(out.result, None);
        assert_eq!(
            out.progress,
            vec![Progress::Log {
                message: "side effect only".into()
            }]
        );
    }

    #[test]
    fn captures_a_return_value_built_from_agent_results() {
        let script = r#"
            const rs = await parallel([() => agent('a'), () => agent('b')]);
            return { confirmed: rs };
        "#;
        let out = run(script, |prompts: &[String], _opts: &[String]| {
            prompts.iter().map(|p| format!("{p}!")).collect()
        })
        .unwrap();
        assert_eq!(out.result.as_deref(), Some(r#"{"confirmed":["a!","b!"]}"#));
    }

    #[test]
    fn agent_opts_reach_the_runner_as_json() {
        // `agent(prompt, opts)` — the opts object is JSON-encoded and handed to
        // the runner alongside each prompt (parallel arrays). A bare `agent(p)`
        // yields `{}`.
        let captured: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let cap = captured.clone();
        let script = r#"
            await agent('p1', { agentType: 'reviewer', model: 'opus', isolation: 'worktree' });
            await agent('p2');
        "#;
        run(script, move |prompts: &[String], opts: &[String]| {
            cap.borrow_mut().extend(opts.iter().cloned());
            prompts.iter().map(|_| "ok".to_string()).collect()
        })
        .unwrap();
        let got = captured.borrow().clone();
        assert_eq!(got.len(), 2);
        assert!(
            got[0].contains(r#""agentType":"reviewer""#),
            "got: {}",
            got[0]
        );
        assert!(got[0].contains(r#""model":"opus""#));
        assert!(got[0].contains(r#""isolation":"worktree""#));
        assert_eq!(got[1], "{}", "a bare agent(p) carries empty opts");
    }

    #[test]
    fn run_with_progress_fires_live_callbacks() {
        let seen: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let s = seen.clone();
        let script = r#"
            phase('A');
            log('one');
            phase('B');
            log('two');
        "#;
        let out = run_with_progress(
            script,
            no_agents,
            move |p: &Progress| {
                s.borrow_mut().push(match p {
                    Progress::Phase { title: t, .. } => format!("phase:{t}"),
                    Progress::Log { message: m } => format!("log:{m}"),
                    Progress::Agent { .. } => return,
                });
            },
            None,
            false,
            None,
            None,
        )
        .unwrap();
        // Callbacks fired live, in emission order.
        assert_eq!(
            *seen.borrow(),
            vec!["phase:A", "log:one", "phase:B", "log:two"]
        );
        // The same events are still collected in the outcome.
        assert_eq!(out.progress.len(), 4);
    }

    #[test]
    fn real_budget_source_drives_the_budget_globals() {
        struct Src;
        impl WorkflowBudgetSource for Src {
            fn total(&self) -> Option<u64> {
                Some(500_000)
            }
            fn spent(&self) -> u64 {
                120_000
            }
        }
        let script = r#"
            log('total=' + String(budget.total));
            log('spent=' + String(budget.spent()));
            log('remaining=' + String(budget.remaining()));
        "#;
        let out = run_with_progress(
            script,
            no_agents,
            |_: &Progress| {},
            Some(Arc::new(Src) as Arc<dyn WorkflowBudgetSource>),
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            out.progress,
            vec![
                Progress::Log {
                    message: "total=500000".into()
                },
                Progress::Log {
                    message: "spent=120000".into()
                },
                Progress::Log {
                    message: "remaining=380000".into()
                },
            ]
        );
    }

    #[test]
    fn args_global_reflects_the_passed_value() {
        let out = run_with_progress(
            "log('x=' + args.x); log('len=' + args.items.length);",
            no_agents,
            |_: &Progress| {},
            None,
            false,
            Some(r#"{ "x": 7, "items": [1, 2, 3] }"#.to_string()),
            None,
        )
        .unwrap();
        assert_eq!(
            out.progress,
            vec![
                Progress::Log {
                    message: "x=7".into()
                },
                Progress::Log {
                    message: "len=3".into()
                }
            ]
        );
    }

    #[test]
    fn workflow_runs_nested_inline_when_allowed() {
        // allow_nested=true → workflow() resolves the nested SOURCE via an
        // `__wf_resolve` agent() call, then evaluates it IN THIS runtime. The
        // nested body's `return` flows back; its `args` is the call's value.
        let out = run_with_progress(
            r#"
                const r = await workflow('child', { n: 1 });
                log('ok=' + r.ok + ' n=' + r.n);
                return r;
            "#,
            |_prompts: &[String], opts_json: &[String]| {
                opts_json
                    .iter()
                    .map(|o| {
                        if o.contains("__wf_resolve") {
                            // The nested workflow's SOURCE (sees the swapped args).
                            "return { ok: true, n: args.n };".to_string()
                        } else {
                            String::new()
                        }
                    })
                    .collect()
            },
            |_: &Progress| {},
            None,
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            out.progress,
            vec![Progress::Log {
                message: "ok=true n=1".into()
            }]
        );
        assert_eq!(out.result.as_deref(), Some(r#"{"ok":true,"n":1}"#));
    }
}

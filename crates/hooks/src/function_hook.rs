//! The sandbox plugin-supplied function hooks run in.
//!
//! # Why this exists
//!
//! claude-code 2.1.267 runs function hooks in a bundled worker
//! (`HOOKS_WORKER_URL` → `src/plugins/functionHooks/hooks-worker/hooks-worker.js`).
//! That worker is packed separately from the main bundle and compiled to
//! bytecode: it is NOT in the extracted chunks, and searching the 200MB binary
//! finds `self.postMessage` twice, both in bundled node-forge. **Upstream's
//! isolation model cannot be read, so it cannot be ported.** This is a designed
//! sandbox, and the design is stated here so it can be argued with.
//!
//! # Threat model
//!
//! A function hook is *plugin-supplied code that runs automatically*, without a
//! prompt, on an event the user did not initiate. That is strictly more
//! dangerous than the workflow engine's JS, which runs a script the user wrote
//! and explicitly asked to run. ⛔ Do not reuse the workflow engine here on the
//! grounds that "the port already runs JS".
//!
//! # The model: deny by default, and nothing is granted
//!
//! QuickJS was chosen for one property: **a bare context has no host bindings.**
//! There is no `fs`, no `net`, no `process`, no `env`, no module loader and no
//! timer unless something binds them. So the sandbox does not *construct*
//! isolation — it declines to hand any out:
//!
//! | capability | status |
//! |---|---|
//! | filesystem / network / process / env | never bound — unreachable, not filtered |
//! | host callbacks into Rust | none registered |
//! | module import / `require` | no loader installed |
//! | ambient state across calls | fresh `Runtime` + `Context` per invocation |
//! | wall-clock | [`Sandbox::budget`], enforced by an interrupt handler |
//! | memory | [`Sandbox::memory_limit`], enforced by the runtime |
//!
//! The hook sees exactly one thing — its payload, as a JSON value bound to
//! `input` — and returns exactly one thing: a JSON-serialisable result.
//!
//! ⛔ **Adding a host binding here widens the blast radius of every plugin on
//! the machine.** If one is ever needed, it belongs behind an explicit,
//! per-capability opt-in in the plugin manifest, reviewed on its own terms —
//! not added to this context because one hook found it convenient.
//!
//! # 🚨 What the in-engine budget does NOT cover
//!
//! QuickJS polls the interrupt handler only from its BYTECODE INTERPRETER.
//! `libregexp.c` has no timeout callback at all — its only host hook is a
//! stack-overflow check. So a catastrophic-backtracking regexp runs to
//! completion no matter what [`Sandbox::budget`] says, and because it allocates
//! almost nothing, [`Sandbox::memory_limit`] does not catch it either.
//! Measured: `/^(a+)+$/.test('a'.repeat(30)+'!')` ran 50s under a 100ms budget
//! and returned `Ok`, never `TimedOut`.
//!
//! That is why [`Sandbox::eval_abandonable`] exists and why the executor uses
//! it: the in-engine budget is a cooperative cap, and the OUTER deadline is the
//! real one. ⛔ Do not "simplify" the executor back to a plain `eval` on a
//! blocking-pool thread — roughly sixty bytes of plugin JS would then hang the
//! turn forever and consume a pool thread other work needs.
//!
//! Denial of service within those bounds is out of scope: a hook that burns its
//! whole budget every time is a slow session, not a compromised one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default wall-clock budget for one function-hook invocation.
///
/// Upstream carries a per-hook `budgetMs`; until the manifest surface exists
/// this is the single cap. Small on purpose: a hook is a decision function, not
/// a place to do work.
pub const DEFAULT_FUNCTION_HOOK_BUDGET: Duration = Duration::from_millis(1_000);

/// Default heap cap for one function-hook invocation (8 MiB).
pub const DEFAULT_FUNCTION_HOOK_MEMORY: usize = 8 * 1024 * 1024;

/// Hard ceiling on a hook's wall-clock budget.
///
/// 🚨 `budgetMs` comes from PLUGIN-supplied config. Without this clamp it is a
/// knob FOR the plugin rather than a cap ON it: `{"budgetMs": 86400000}` would
/// block every matching event for a day. Clamped, not rejected, so a generous
/// value degrades to the ceiling instead of dropping the hook.
pub const MAX_FUNCTION_HOOK_BUDGET: Duration = Duration::from_secs(5);

/// Why a function hook did not produce a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FunctionHookError {
    /// The hook body did not parse or threw.
    Threw(String),
    /// The hook exceeded [`Sandbox::budget`].
    TimedOut,
    /// The hook exceeded [`Sandbox::memory_limit`], or the engine refused to
    /// allocate. QuickJS surfaces this as an ordinary exception, so it is
    /// reported as [`Self::Threw`] unless the interrupt fired first.
    OutOfMemory,
    /// The hook returned something that is not JSON-serialisable.
    NotSerialisable(String),
    /// The engine itself failed to start.
    EngineUnavailable(String),
}

impl std::fmt::Display for FunctionHookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Threw(message) => write!(f, "function hook threw: {message}"),
            Self::TimedOut => write!(f, "function hook exceeded its time budget"),
            Self::OutOfMemory => write!(f, "function hook exceeded its memory limit"),
            Self::NotSerialisable(message) => {
                write!(
                    f,
                    "function hook returned a non-serialisable value: {message}"
                )
            }
            Self::EngineUnavailable(message) => {
                write!(f, "function hook engine unavailable: {message}")
            }
        }
    }
}

/// A single-use sandbox for one function-hook invocation.
#[derive(Debug, Clone, Copy)]
pub struct Sandbox {
    /// Wall-clock budget.
    pub budget: Duration,
    /// Heap cap in bytes.
    pub memory_limit: usize,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self {
            budget: DEFAULT_FUNCTION_HOOK_BUDGET,
            memory_limit: DEFAULT_FUNCTION_HOOK_MEMORY,
        }
    }
}

impl Sandbox {
    /// Clamp this sandbox's budget to [`MAX_FUNCTION_HOOK_BUDGET`].
    #[must_use]
    pub fn clamped(mut self) -> Self {
        self.budget = self.budget.min(MAX_FUNCTION_HOOK_BUDGET);
        self
    }

    /// Evaluate on a DEDICATED thread and give up on it after the budget.
    ///
    /// The in-engine interrupt cannot stop a runaway regexp (see the module
    /// doc), so this is the deadline that actually holds. When it fires, the
    /// worker thread is ABANDONED — it keeps running until the regexp finishes,
    /// which may be never.
    ///
    /// ⛔ A dedicated thread, not `spawn_blocking`: an abandoned blocking-pool
    /// thread is one fewer for every other blocking task in the process, so a
    /// hostile plugin firing on a frequent event could starve the pool. Leaking
    /// an OS thread is the lesser harm, and it is bounded by how often the hook
    /// fires rather than by a fixed pool size.
    pub fn eval_abandonable(
        self,
        source: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, FunctionHookError> {
        let sandbox = self.clamped();
        // A small grace so the in-engine interrupt gets to report `TimedOut`
        // (with its cheaper cleanup) before the outer deadline gives up.
        let deadline = sandbox.budget + Duration::from_millis(250);
        let (tx, rx) = std::sync::mpsc::channel();
        let source = source.to_string();
        let input = input.clone();
        std::thread::Builder::new()
            .name("function-hook".into())
            .spawn(move || {
                // The receiver is gone on the timeout path; dropping the result
                // is the intended outcome, not an error.
                let _ = tx.send(sandbox.eval(&source, &input));
            })
            .map_err(|error| FunctionHookError::EngineUnavailable(error.to_string()))?;
        rx.recv_timeout(deadline)
            .unwrap_or(Err(FunctionHookError::TimedOut))
    }

    /// Evaluate `source` with `input` bound, and return its completion value.
    ///
    /// The body is evaluated as an expression-producing script: the last
    /// expression's value is the result, so both `({deny: true})` and a longer
    /// body ending in a value work.
    pub fn eval(
        self,
        source: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, FunctionHookError> {
        let runtime = rquickjs::Runtime::new()
            .map_err(|error| FunctionHookError::EngineUnavailable(error.to_string()))?;
        runtime.set_memory_limit(self.memory_limit);
        // Bound guest recursion before it can exhaust the native worker stack.
        runtime.set_max_stack_size(256 * 1024);

        // Wall-clock enforcement. QuickJS calls the interrupt handler
        // periodically during execution, so an infinite loop in a hook ends the
        // invocation instead of the turn.
        let deadline = Instant::now() + self.budget;
        let tripped = Arc::new(AtomicBool::new(false));
        let tripped_handler = Arc::clone(&tripped);
        runtime.set_interrupt_handler(Some(Box::new(move || {
            if Instant::now() >= deadline {
                tripped_handler.store(true, Ordering::SeqCst);
                return true;
            }
            false
        })));

        let context = rquickjs::Context::full(&runtime)
            .map_err(|error| FunctionHookError::EngineUnavailable(error.to_string()))?;

        // `input` is marshalled as JSON TEXT and parsed inside the sandbox
        // rather than bridged value-by-value: it keeps the host/guest boundary
        // to one string and leaves no Rust callback reachable from the guest.
        let payload = serde_json::to_string(input)
            .map_err(|error| FunctionHookError::NotSerialisable(error.to_string()))?;
        let program = format!(
            "(function(){{\nconst input = JSON.parse({});\nreturn JSON.stringify((function(){{\n{}\n}})());\n}})()",
            json_string_literal(&payload),
            source
        );

        context.with(|ctx| {
            // QuickJS 0.14 adds a guest microtask scheduler to a full context.
            // Hooks are synchronous; keep their advertised global surface fixed.
            ctx.globals()
                .remove("queueMicrotask")
                .map_err(|error| FunctionHookError::EngineUnavailable(error.to_string()))?;
            match ctx.eval::<Option<String>, _>(program.as_str()) {
                Ok(Some(text)) => serde_json::from_str(&text)
                    .map_err(|error| FunctionHookError::NotSerialisable(error.to_string())),
                // `JSON.stringify(undefined)` is `undefined`: the hook declined
                // to answer, which is a valid "no opinion", not an error.
                Ok(None) => Ok(serde_json::Value::Null),
                Err(error) => {
                    if tripped.load(Ordering::SeqCst) {
                        return Err(FunctionHookError::TimedOut);
                    }
                    let detail = ctx
                        .catch()
                        .as_exception()
                        .and_then(|exception| exception.message())
                        .unwrap_or_else(|| error.to_string());
                    if detail.contains("out of memory") {
                        return Err(FunctionHookError::OutOfMemory);
                    }
                    Err(FunctionHookError::Threw(detail))
                }
            }
        })
    }
}

/// Encode `value` as a JS string literal safe to splice into source.
fn json_string_literal(value: &str) -> String {
    // `serde_json` already escapes quotes/backslashes/control chars. The two
    // line separators below are valid JSON but NOT valid inside a JS string
    // literal, so they must be escaped again or the program fails to parse.
    serde_json::Value::String(value.to_string())
        .to_string()
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(source: &str) -> Result<serde_json::Value, FunctionHookError> {
        Sandbox::default().eval(source, &json!({"tool": "Bash"}))
    }

    /// 🚨 THE security test. Every name below is a host capability that must be
    /// unreachable — not filtered, not shimmed, ABSENT. If any becomes defined,
    /// a plugin can reach the machine and this whole design is void.
    ///
    /// Asserting `typeof X === "undefined"` rather than "the call failed": a
    /// binding that exists but errors today is one refactor away from working.
    #[test]
    fn no_host_capability_is_reachable_from_a_hook() {
        for name in [
            "require",
            "process",
            "globalThis.process",
            "Deno",
            "Bun",
            "fetch",
            "XMLHttpRequest",
            "WebSocket",
            "navigator",
            "fs",
            "readFile",
            "open",
            "child_process",
            "spawn",
            "setTimeout",
            "setInterval",
            "queueMicrotask",
            "importScripts",
            "postMessage",
            "self",
        ] {
            let out = run(&format!("return typeof {name};")).expect(name);
            assert_eq!(
                out,
                json!("undefined"),
                "`{name}` is reachable from a function hook — plugin code must \
                 not be able to touch the host"
            );
        }
    }

    /// No module loader is installed, so a hook cannot import its way out.
    ///
    /// ⚠️ The obvious test does NOT work and is recorded here so it is not
    /// rewritten that way: `try { import('fs'); return 'resolved' } catch ...`
    /// always returns `'resolved'`, because `import()` yields a PROMISE rather
    /// than throwing. It passes identically whether or not a loader exists —
    /// it proves nothing. Settling the promise would need a job pump, and this
    /// sandbox is deliberately synchronous.
    ///
    /// So assert the two things that ARE observable: `import()` hands back a
    /// promise rather than a module, and a STATIC import is rejected outright
    /// because the body is evaluated as a script, not a module.
    #[test]
    fn a_hook_cannot_import_a_module() {
        let kind = run("const p = import('fs'); return p && typeof p.then;").unwrap();
        assert_eq!(
            kind,
            json!("function"),
            "import() must yield a promise, never a module object"
        );
        let module_object = run("const p = import('fs'); return typeof p.readFileSync;").unwrap();
        assert_eq!(module_object, json!("undefined"));

        // A static import is a syntax error in script context.
        assert!(matches!(
            run("import fs from 'fs'; return 1;"),
            Err(FunctionHookError::Threw(_))
        ));
    }

    /// 🚨 The finding the whole `eval_abandonable` path exists for.
    ///
    /// QuickJS polls the interrupt handler only from its bytecode interpreter;
    /// `libregexp.c` has no timeout callback. So catastrophic backtracking
    /// ignores `budget` entirely, allocates almost nothing (so `memory_limit`
    /// misses it too), and returns `Ok` rather than `TimedOut`. Measured on the
    /// plain `eval`: ~50s under a 100ms budget at n=30, unbounded at n=40.
    ///
    /// ⛔ If this test starts taking tens of seconds, someone routed the
    /// executor back through `eval` — in production that is the turn hanging
    /// forever, not a slow test.
    #[test]
    fn a_runaway_regexp_cannot_outlive_the_outer_deadline() {
        let sandbox = Sandbox {
            budget: Duration::from_millis(100),
            ..Sandbox::default()
        };
        let started = Instant::now();
        let result =
            sandbox.eval_abandonable("return /^(a+)+$/.test('a'.repeat(40) + '!');", &json!({}));
        assert_eq!(result, Err(FunctionHookError::TimedOut));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the outer deadline must fire; took {:?}",
            started.elapsed()
        );
    }

    /// 🚨 `budgetMs` is PLUGIN-supplied. Unclamped it is a knob FOR the plugin
    /// rather than a cap ON it: `{"budgetMs": 86400000}` blocks every matching
    /// event for a day.
    #[test]
    fn a_plugin_cannot_grant_itself_an_unbounded_budget() {
        let greedy = Sandbox {
            budget: Duration::from_secs(86_400),
            ..Sandbox::default()
        };
        assert_eq!(greedy.clamped().budget, MAX_FUNCTION_HOOK_BUDGET);

        let started = Instant::now();
        let result = greedy.eval_abandonable("while (true) {}", &json!({}));
        assert_eq!(result, Err(FunctionHookError::TimedOut));
        assert!(
            started.elapsed() < MAX_FUNCTION_HOOK_BUDGET + Duration::from_secs(2),
            "a 24h budget must be clamped, not honoured; took {:?}",
            started.elapsed()
        );
    }

    /// The clamp is a ceiling, not a floor — a modest budget is left alone.
    #[test]
    fn a_budget_under_the_ceiling_is_left_alone() {
        let modest = Sandbox {
            budget: Duration::from_millis(250),
            ..Sandbox::default()
        };
        assert_eq!(modest.clamped().budget, Duration::from_millis(250));
    }

    /// A hook that loops forever ends its own invocation, not the turn.
    #[test]
    fn an_infinite_loop_hits_the_budget_instead_of_hanging() {
        let sandbox = Sandbox {
            budget: Duration::from_millis(80),
            ..Sandbox::default()
        };
        let started = Instant::now();
        let result = sandbox.eval("while (true) {}", &json!({}));
        assert_eq!(result, Err(FunctionHookError::TimedOut));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the interrupt handler must actually stop execution"
        );
    }

    /// Runaway allocation is capped rather than taking the process with it.
    #[test]
    fn runaway_allocation_is_capped() {
        let sandbox = Sandbox {
            memory_limit: 1024 * 1024,
            budget: Duration::from_millis(500),
            ..Sandbox::default()
        };
        let result = sandbox.eval(
            "const a = []; while (true) { a.push('x'.repeat(4096)); } ",
            &json!({}),
        );
        // ⚠️ `is_err()` is NOT enough and used to be all this asserted: the
        // test also sets a budget, so it passed identically when the memory
        // limit was ignored entirely (audited with a 200 MiB override). Name
        // the variant, or this proves only "something went wrong".
        assert_eq!(
            result,
            Err(FunctionHookError::OutOfMemory),
            "the MEMORY limit must be what stops this, not the clock"
        );
    }

    #[test]
    fn recursion_fails_without_exhausting_the_native_stack() {
        let error = run("function recurse() { return recurse(); } return recurse();").unwrap_err();
        assert!(
            matches!(error, FunctionHookError::Threw(ref detail) if detail.contains("stack")),
            "unexpected recursion result: {error:?}"
        );
    }

    /// Each invocation gets a fresh runtime: one hook cannot leave state for
    /// the next, which is how a hook would otherwise build a covert channel.
    #[test]
    fn no_state_survives_between_invocations() {
        let _ = run("globalThis.__leak = 'from the first hook'; return 1;").unwrap();
        let second = run("return typeof globalThis.__leak;").unwrap();
        assert_eq!(second, json!("undefined"));
    }

    #[test]
    fn the_payload_arrives_and_the_result_comes_back() {
        let out = Sandbox::default()
            .eval(
                "return {seen: input.tool, ok: true};",
                &json!({"tool": "Bash"}),
            )
            .unwrap();
        assert_eq!(out, json!({"seen": "Bash", "ok": true}));
    }

    /// A hook with no opinion returns nothing; that is not an error.
    #[test]
    fn returning_nothing_is_a_valid_no_opinion() {
        assert_eq!(run("return;").unwrap(), serde_json::Value::Null);
    }

    #[test]
    fn a_throwing_hook_reports_its_message() {
        let error = run("throw new Error('nope');").unwrap_err();
        match error {
            FunctionHookError::Threw(message) => assert!(message.contains("nope"), "{message}"),
            other => panic!("expected Threw, got {other:?}"),
        }
    }

    /// U+2028/U+2029 in a payload must survive intact.
    ///
    /// ⚠️ This does NOT test the re-escaping in `json_string_literal`, and an
    /// earlier version of this comment claimed it did. A mutation audit deleted
    /// both `.replace` calls and this test stayed green: QuickJS is ES2019+,
    /// where raw U+2028/U+2029 are LEGAL inside a string literal, so the
    /// re-escaping is unobservable here. The escaping is kept because it costs
    /// nothing and the guarantee is engine-independent, but ⛔ do not read this
    /// test as covering it — nothing does.
    ///
    /// What it does cover is real: a payload carrying these characters round
    /// trips without corrupting the generated program.
    #[test]
    fn a_line_separator_in_the_payload_survives() {
        let out = Sandbox::default()
            .eval(
                "return input.text.length;",
                &json!({"text": "a\u{2028}b\u{2029}c"}),
            )
            .unwrap();
        assert_eq!(out, json!(5));
        let round_tripped = Sandbox::default()
            .eval("return input.text;", &json!({"text": "a\u{2028}b"}))
            .unwrap();
        assert_eq!(
            round_tripped,
            json!("a\u{2028}b"),
            "the separator must survive verbatim"
        );
    }

    /// A payload that looks like source must not become source.
    #[test]
    fn a_payload_cannot_inject_code() {
        let hostile = json!({"text": "\"); globalThis.__pwned = true; (\""});
        let out = Sandbox::default()
            .eval("return typeof globalThis.__pwned;", &hostile)
            .unwrap();
        assert_eq!(out, json!("undefined"));
    }
}

#[cfg(test)]
mod config_tests {
    use crate::definition::HookExecutor;

    /// 🚨 The reachability check for this whole feature. A plugin declares a
    /// function hook in config; if that config does not parse into
    /// `HookExecutor::Function`, every part of AG-15 is code nobody can invoke.
    #[test]
    fn a_function_hook_declared_in_config_becomes_a_function_executor() {
        let json = serde_json::json!({
            "type": "function",
            "source": "return {decision: 'block', reason: 'no'};",
            "budgetMs": 250
        });
        let entry: crate::loader::HookEntry =
            serde_json::from_value(json).expect("a function entry must deserialize");
        let (name, executor) =
            crate::loader::build_executor(&entry).expect("it must map to an executor");
        assert_eq!(name, "function");
        match executor {
            HookExecutor::Function { source, budget_ms } => {
                assert!(source.contains("decision"));
                assert_eq!(budget_ms, Some(250));
            }
            other => panic!("expected a Function executor, got {other:?}"),
        }
    }

    /// A `function` entry with no body is dropped, matching how a `command`
    /// entry with no command is dropped — not defaulted to an empty script that
    /// would run and return nothing on every event.
    #[test]
    fn a_function_hook_without_a_body_is_skipped() {
        let entry: crate::loader::HookEntry =
            serde_json::from_value(serde_json::json!({"type": "function"})).unwrap();
        assert!(crate::loader::build_executor(&entry).is_none());
    }
}

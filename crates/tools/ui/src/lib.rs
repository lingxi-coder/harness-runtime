//! UI / interaction tools: AskUserQuestion, Brief, ReportFindings, SendMessage,
//! Sleep, StructuredOutput (the `SyntheticOutputTool` struct, wire name
//! `StructuredOutput`). Extracted in M8-P7. Cross-platform.
#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::match_wildcard_for_single_variants,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::format_collect,
    clippy::similar_names,
    clippy::doc_markdown,
    clippy::manual_let_else
)]
pub mod artifact;
pub mod ask_user_question;
pub mod brief;
pub mod list_agents;
pub mod observer_report;
pub mod push_notification;
pub mod report_findings;
pub mod send_message;
pub mod sleep;
pub mod synthetic_output;

/// ONE lock for every test that mutates `platform_api::live_sessions`'s process
/// globals (`set_process_dir` / `set_process_session_id` / `set_process_name`).
///
/// It lives at crate level because those globals are per-PROCESS and the whole
/// crate's unit tests share one process. `list_agents.rs` and `send_message.rs`
/// each used to declare their own `process_lock()` with its own `static LOCK` —
/// two distinct mutexes guarding one resource, so each file serialized against
/// itself and against nothing else. A `send_message` test could call
/// `set_process_dir` into its own TempDir while a `list_agents` test was
/// reading, and the listing lost the peer entry it had just written.
///
/// The failure was load-dependent: `-p tool-ui --lib` alone passed 119/119 four
/// times over (including `--test-threads=1`), and only went red inside a larger
/// multi-crate run, where more binaries competing for cores changed the
/// interleaving. A file-local lock cannot fix that; the lock has to be as wide
/// as the state it guards.
#[cfg(test)]
pub(crate) fn process_globals_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}
pub use artifact::ArtifactTool;
pub use ask_user_question::AskUserQuestionTool;
pub use brief::BriefTool;
pub use list_agents::ListAgentsTool;
pub use push_notification::PushNotificationTool;
pub use report_findings::ReportFindingsTool;
pub use send_message::SendMessageTool;
pub use sleep::SleepTool;
pub use synthetic_output::SyntheticOutputTool;
pub use tool_api::ask_user_question::AskUserQuestionExchange;

use std::sync::Arc;
/// Register the UI tools against `reg` (the full set, including the builtin
/// `SendMessage`). This is the default-session path.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    register_with_options(reg, ctx, true, true, None);
}

/// Register UI tools for a host with no user-interaction transport.
/// `AskUserQuestion` is omitted from the advertised schema rather than exposed
/// with a resolver that could fabricate an answer.
pub fn register_all_without_ask_user_question(
    reg: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
) {
    register_with_options(reg, ctx, true, false, None);
}

/// Register the UI tools EXCEPT the builtin `SendMessage`.
///
/// A coordinator-capable session registers the richer
/// `coordinator::tool_send_message::SendMessageTool` IN PLACE OF this builtin
/// (it carries the swarm routing surface — broadcast / name-resolution /
/// shutdown + plan-approval handshake). Because the registry's `find_by_name`
/// is builtin-first, both must not be present or the earlier one would
/// silently shadow the later — so the engine skips this builtin in coordinator
/// mode (see `engine-desktop::register_desktop_tools`).
pub fn register_all_except_send_message(
    reg: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
) {
    register_with_options(reg, ctx, false, true, None);
}

/// Coordinator-mode registration for a host with no questionnaire transport.
pub fn register_all_except_send_message_without_ask_user_question(
    reg: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
) {
    register_with_options(reg, ctx, false, false, None);
}

/// Register the full UI tool set but force `AskUserQuestion` to use the given
/// resolver for this registry instance.
pub fn register_all_with_ask_resolver(
    reg: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
    resolver: Arc<dyn ask_user_question::AskUserQuestionResolver>,
) {
    register_with_options(reg, ctx, true, true, Some(resolver));
}

/// Coordinator-mode variant of [`register_all_with_ask_resolver`]: skip the
/// builtin `SendMessage` while still injecting a custom AskUserQuestion
/// resolver.
pub fn register_all_except_send_message_with_ask_resolver(
    reg: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
    resolver: Arc<dyn ask_user_question::AskUserQuestionResolver>,
) {
    register_with_options(reg, ctx, false, true, Some(resolver));
}

fn register_with_options(
    reg: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
    include_send_message: bool,
    include_ask_user_question: bool,
    ask_resolver: Option<Arc<dyn ask_user_question::AskUserQuestionResolver>>,
) {
    reg.register_builtin(Arc::new(SleepTool::new(ctx.clone())));
    if include_send_message {
        reg.register_builtin(Arc::new(SendMessageTool::new(ctx.clone())));
    }
    // 2.1.270 `ObserverReport`. Registered unconditionally, like the oracle's
    // `isEnabled(){return!0}`: reachability belongs to the PAIRING, not to
    // registration. An agent that is not an observer calling this gets a
    // reason ("only available to an observer agent") rather than an unknown
    // tool, which is what tells the model it asked the wrong question instead
    // of that the tool does not exist.
    reg.register_builtin(Arc::new(observer_report::ObserverReportTool::new(
        None, None,
    )));
    // 2.1.232 `ListAgents` (`zy`, alias `ListPeers`). `is_enabled` follows
    // harbor-kite / a live-session process dir so headless fixtures stay empty.
    reg.register_builtin(Arc::new(ListAgentsTool::new(ctx.clone())));
    // AskUserQuestion must NOT auto-continue by default (oracle 2.1.201): the
    // production resolver honors `askUserQuestionTimeout` (default `never` ⇒
    // block on the user). A non-interactive (`--print`) session does not expose
    // this schema; a defensive call reports `InteractionRequired` and never
    // manufactures a selection. See `ask_user_question::DefaultTimeoutResolver`.
    //
    // (M-15) The live `askUserQuestionTimeout` settings value rides on
    // `ctx.ask_user_question_timeout` (populated at the composition roots from
    // the merged settings.json). Parse it via `parse_or_default` — mirroring the
    // zod `.catch(void 0)` + `?? "never"` chain, so an absent / unparsable value
    // falls back to `Never` (block). The non-interactive `--print` auto-first
    // behavior is unchanged: `DefaultTimeoutResolver::resolve` short-circuits to
    // first-option whenever `non_interactive` is set, regardless of the window.
    if include_ask_user_question {
        let ask_timeout = ask_user_question::AskUserQuestionTimeout::parse_or_default(
            ctx.ask_user_question_timeout.as_deref(),
        );
        let ask_resolver = ask_resolver.unwrap_or_else(|| {
            Arc::new(ask_user_question::DefaultTimeoutResolver::new(ask_timeout))
        });
        reg.register_builtin(Arc::new(AskUserQuestionTool::with_resolver(
            ctx.clone(),
            ask_resolver,
        )));
    }
    reg.register_builtin(Arc::new(BriefTool::new(ctx.clone())));
    // PARITY: the `PushNotification` tool (binary `Wzp`). Registered always; its
    // `is_enabled` (flag `tengu_kairos_push_notifications`, default off) gates
    // exposure — so by default it is invisible to the model, exactly like the
    // shipped binary.
    reg.register_builtin(Arc::new(PushNotificationTool::new(ctx.clone())));
    // PARITY (2.1.238 `pJf`): `ReportFindings`. The oracle tool object defines
    // NO `isEnabled`, so `es()`'s default `isEnabled:()=>!0` applies and the
    // schema is advertised in every session — registered unconditionally here.
    reg.register_builtin(Arc::new(ReportFindingsTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(SyntheticOutputTool::new(ctx)));
    // NOTE: `ArtifactTool` lives in this crate but is registered by the DESKTOP
    // composition root (`harness_runtime::desktop::register_desktop_tools`), not here — it
    // is a first-party/claude.ai feature gated on `tengu_cobalt_plinth` +
    // first-party auth, so it is desktop-only (and stays out of the mobile tool
    // set). Its `is_enabled` (CC `dY()`) keeps it invisible to the model until
    // the Statsig gate is available.
}

// =============================================================================
// Tests — M-15: `askUserQuestionTimeout` threaded from `BuiltinToolContext`
// into the registered `AskUserQuestion` tool's resolver.
// =============================================================================
#[cfg(test)]
mod ask_timeout_wiring_tests {
    use super::*;
    use mobile_linux_api::ProcessOutput;
    use serde_json::json;
    use std::sync::Arc;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};
    use tool_api::ToolError;

    fn dummy_out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    fn one_question() -> serde_json::Value {
        json!({
            "questions": [{
                "question": "Pick one?",
                "header": "Choice",
                "options": [
                    { "label": "Alpha", "description": "the a" },
                    { "label": "Beta", "description": "the b" }
                ]
            }]
        })
    }

    /// Register the UI tools with the given `askUserQuestionTimeout` carrier and
    /// return the registered `AskUserQuestion` tool.
    fn register_and_get_ask(timeout: Option<&str>) -> Arc<dyn tool_api::tool_trait::Tool> {
        let mut ctx = shell_test_ctx(dummy_out());
        ctx.ask_user_question_timeout = timeout.map(str::to_string);
        let mut reg = tool_api::ToolRegistry::new();
        register_all(&mut reg, ctx);
        reg.find_by_name("AskUserQuestion")
            .expect("AskUserQuestion must be registered")
    }

    #[tokio::test]
    async fn none_carrier_blocks_interactive_as_never() {
        // No setting ⇒ `never` ⇒ an interactive call blocks (does NOT auto-pick).
        let tool = register_and_get_ask(None);
        let err = tool
            .call(one_question(), fresh_ctx(), fresh_tx())
            .await
            .expect_err("never must block an interactive call");
        assert!(matches!(err, ToolError::InteractionRequired(_)));
    }

    #[tokio::test]
    async fn bogus_carrier_falls_back_to_never() {
        // Unparsable value ⇒ zod `.catch` fallback to `never` (blocks).
        let tool = register_and_get_ask(Some("bogus"));
        let err = tool
            .call(one_question(), fresh_ctx(), fresh_tx())
            .await
            .expect_err("bogus must fall back to never and block");
        assert!(matches!(err, ToolError::InteractionRequired(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn duration_carrier_without_ui_requires_interaction() {
        let tool = register_and_get_ask(Some("60s"));
        let err = tool
            .call(one_question(), fresh_ctx(), fresh_tx())
            .await
            .expect_err("a resolver without a UI cannot invent answers");
        assert!(matches!(err, tool_api::ToolError::InteractionRequired(_)));
    }

    #[tokio::test]
    async fn duration_carrier_headless_requires_interaction() {
        let tool = register_and_get_ask(Some("5m"));
        let mut ctx = fresh_ctx();
        ctx.options.is_non_interactive_session = true;
        let err = tool
            .call(one_question(), ctx, fresh_tx())
            .await
            .expect_err("headless cannot answer on the user's behalf");
        assert!(matches!(err, tool_api::ToolError::InteractionRequired(_)));
    }
}

#[cfg(test)]
mod process_globals_lock_gate {
    /// Both files that mutate `platform_api::live_sessions`'s per-process globals must
    /// route through ONE lock. They previously each declared a `process_lock()`
    /// backed by its own `static LOCK`, so each serialized against itself and
    /// against nothing else, and the suite went red only under load.
    ///
    /// This is a SOURCE gate rather than a behavioural one on purpose: the race
    /// it guards is load-dependent and does not reproduce on demand, so a
    /// behavioural test would be green whether or not the bug was present. What
    /// is deterministic is the structure — a file-local `static` under
    /// `fn process_lock` is the defect itself, so that is what this asserts.
    const SHARED: &str = "crate::process_globals_lock()";

    fn process_lock_body(source: &str, file: &str) -> String {
        let start = source
            .find("fn process_lock()")
            .unwrap_or_else(|| panic!("{file} no longer declares `fn process_lock()`; if it was renamed, retarget this gate rather than deleting it"));
        let rest = &source[start..];
        let end = rest
            .find("\n    }")
            .unwrap_or_else(|| panic!("{file}: could not find the end of `fn process_lock()`"));
        rest[..end].to_string()
    }

    #[test]
    fn neither_file_reintroduces_a_private_process_lock() {
        for (file, source) in [
            ("list_agents.rs", include_str!("list_agents.rs")),
            ("send_message.rs", include_str!("send_message.rs")),
        ] {
            let body = process_lock_body(source, file);
            assert!(
                body.contains(SHARED),
                "{file}'s `process_lock()` does not delegate to `{SHARED}`. Its body is:\n{body}\n\n                 A file-local `static LOCK` here guards only this file. `list_agents.rs` and \
                 `send_message.rs` both call `platform_api::live_sessions::set_process_dir` / \
                 `set_process_session_id` / `set_process_name`, which are PER-PROCESS, so two \
                 mutexes let a `send_message` test repoint the process dir while a `list_agents` \
                 test is reading it — the listing then loses the peer entry it just wrote."
            );
            assert!(
                !body.contains("static LOCK"),
                "{file}'s `process_lock()` still declares its own `static LOCK`"
            );
        }
    }
}

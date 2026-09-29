//! `ConversationOrchestrator` composition must publish the process-global
//! session-mode flag, and must source it from `interactive_session` — NOT from
//! `interactive_permissions`.
//!
//! The two are deliberately separate: a session can be interactive while
//! permission prompting stays headless (an embedding host that answers
//! permission requests itself). Sourcing the global from
//! `interactive_permissions` would mark such a session non-interactive and
//! silently change what leaf consumers see — `agent/src/runner.rs`,
//! `tool-api/src/tool_invoker_impl.rs` and `llm-runtime/src/service.rs` all read
//! it through `effective_non_interactive_session()` for callers with no
//! per-call context handle.
//!
//! # Why this test has a file to itself
//!
//! `conversation/wiring.rs` calls
//! `session_flags::set_non_interactive_session(!config.interactive_session)` on
//! EVERY construction, into one `AtomicBool` shared by the whole process. The
//! orchestrator lib test binary constructs ~130 orchestrators, nearly all with
//! the `interactive_session: false` default, on parallel threads — so a
//! read-back there races every one of them and fails intermittently. It did:
//! this assertion lived in `conversation/tests/turn_recovery_tests.rs` and
//! flaked roughly one run in two.
//!
//! Each integration test file is its own process, so the orchestrator built
//! below is the only writer of that global. **Do not add another
//! orchestrator-constructing test to this file** — that reintroduces the race.
//! The race-free half of the original test (the system-prompt bullet, which
//! reads `config.interactive_session` per-instance rather than the global)
//! stays in the lib module as
//! `interactive_session_flag_drives_prompt_guidance`.

use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

fn orchestrator_with(interactive_session: bool) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig {
            // Headless permission prompting throughout: the point is that it
            // does NOT decide the session-mode flag.
            interactive_permissions: false,
            interactive_session,
            ..OrchestratorConfig::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

#[tokio::test]
async fn composition_publishes_session_mode_from_interactive_session() {
    let prior = lingxi_core::host::session_flags::is_non_interactive_session();

    // Both polarities, so the assertion cannot pass by the flag happening to
    // already hold the expected value. Sequential, single-threaded: this file
    // has exactly one test for exactly this reason.
    let _interactive = orchestrator_with(true);
    let published_when_interactive = lingxi_core::host::session_flags::is_non_interactive_session();

    let _headless = orchestrator_with(false);
    let published_when_headless = lingxi_core::host::session_flags::is_non_interactive_session();

    lingxi_core::host::session_flags::set_non_interactive_session(prior);

    assert!(
        !published_when_interactive,
        "interactive_session=true must publish an INTERACTIVE session flag even when \
         permission prompting stays headless; sourcing it from interactive_permissions \
         (false here) would publish non-interactive instead"
    );
    assert!(
        published_when_headless,
        "interactive_session=false must publish a NON-INTERACTIVE session flag"
    );
}

//! STUB.6 — faithful-stub audit + partition regression test.
//!
//! ROADMAP §7 quick-win 3: "lock the intentionally-stubbed commands as
//! correct-by-design + partition test. Prevents wasted implementation of
//! TS-stubbed commands."
//!
//! Among the slash commands the Rust port leaves on the shared
//! [`UnimplementedCommandHandler`], this test pins down the subset that is
//! **correct-by-design** — i.e. claude-code *itself* disables / hides /
//! feature-gates-OFF / ships a literal `name: 'stub'` for them — so the Rust
//! stub is **faithful**, not a parity gap. That set lives in
//! [`command_api::builtin_support::CORRECT_BY_DESIGN_STUBS`].
//!
//! CRITICAL boundary this test defends (the STUB.6 warning): it must NOT
//! mislabel **host-bound-deferred** commands — those that claude-code
//! IMPLEMENTS via an interactive TUI/JSX dialog or SDK control-request, which
//! the Rust port defers only for lack of host infra — as correct-by-design.
//! Those are a *different*, genuine-gap partition
//! ([`command_api::builtin_support::HOST_BOUND_DEFERRED_GAPS`]). The two sets
//! are asserted disjoint here so a future contributor cannot quietly move a
//! real (worth-implementing) command into the "faithful, leave-alone" bucket.
//!
//! The dynamic half of the test is the load-bearing regression guard: it
//! registers the **full** builtin command surface (every batch, exactly as the
//! CLI does) and asserts each correct-by-design name STILL dispatches to the
//! locked stub literal `"{name}: not implemented in v0.6.0 (M5)"`. If someone
//! turns a correct-by-design stub into a half-implementation (a handler that
//! returns anything else), this test fails — which is the whole point.

use std::collections::HashSet;
use std::sync::Arc;

use command_api::builtin_support::names::{
    BUILTIN_COMMAND_NAMES, BUILTIN_CORE_NAMES, CORRECT_BY_DESIGN_STUBS, HOST_BOUND_DEFERRED_GAPS,
    INTENTIONALLY_DISABLED_COMMANDS,
};
use command_api::builtin_support::unimplemented::UnimplementedCommandHandler;
use command_api::{CommandRegistry, RegistrySlashDispatcher};
use command_core::register_all_builtin_commands;
use platform_api::{SlashCommandDispatcher, SlashDispatchResult};
use tokio::sync::RwLock;

fn names_of(table: &[(&'static str, &'static str)]) -> HashSet<&'static str> {
    table.iter().map(|(n, _)| *n).collect()
}

// ============================================================================
// Static partition invariants (data-driven, self-documenting).
// ============================================================================

#[test]
fn correct_by_design_set_is_locked_at_3() {
    assert_eq!(
        CORRECT_BY_DESIGN_STUBS.len(),
        3,
        "only three faithful stubs remain after the 2.1.252 stale-name audit"
    );
}

#[test]
fn host_bound_deferred_set_is_empty_after_btw_wiring() {
    assert_eq!(
        HOST_BOUND_DEFERRED_GAPS.len(),
        0,
        "all public host-bound command gaps should be wired"
    );
}

#[test]
fn correct_by_design_and_host_bound_deferred_are_disjoint() {
    // The central STUB.6 invariant: a command is EITHER a faithful
    // correct-by-design stub OR a genuine deferred gap — never both.
    let cbd = names_of(CORRECT_BY_DESIGN_STUBS);
    let gaps = names_of(HOST_BOUND_DEFERRED_GAPS);
    assert!(
        cbd.is_disjoint(&gaps),
        "correct-by-design and host-bound-deferred partitions overlap: {:?}",
        cbd.intersection(&gaps).collect::<Vec<_>>()
    );
}

#[test]
fn correct_by_design_excludes_every_host_bound_name() {
    // Belt-and-braces against regression: none of the two implemented
    // commands may appear in the faithful-by-design set.
    let cbd = names_of(CORRECT_BY_DESIGN_STUBS);
    for name in ["btw", "reload-plugins"] {
        assert!(
            !cbd.contains(name),
            "'{name}' is implemented by claude-code and must NOT be marked correct-by-design"
        );
    }
}

#[test]
fn partition_union_equals_legacy_intentionally_disabled() {
    // With `/btw` implemented, the legacy disabled table contains only
    // correct-by-design entries.
    let mut union = names_of(CORRECT_BY_DESIGN_STUBS);
    union.extend(names_of(HOST_BOUND_DEFERRED_GAPS));
    assert_eq!(
        union,
        names_of(INTENTIONALLY_DISABLED_COMMANDS),
        "partition union must equal the legacy INTENTIONALLY_DISABLED_COMMANDS set"
    );
}

#[test]
fn every_partitioned_name_is_a_real_builtin_and_not_core() {
    let full: HashSet<&str> = BUILTIN_COMMAND_NAMES.iter().copied().collect();
    let core: HashSet<&str> = BUILTIN_CORE_NAMES.iter().copied().collect();
    for (name, reason) in CORRECT_BY_DESIGN_STUBS
        .iter()
        .chain(HOST_BOUND_DEFERRED_GAPS)
    {
        assert!(full.contains(name), "'{name}' is not a real builtin");
        assert!(
            !core.contains(name),
            "'{name}' is a wired core command and cannot be stubbed"
        );
        assert!(!reason.trim().is_empty(), "'{name}' must document a reason");
    }
}

// ============================================================================
// Dynamic regression guard — the load-bearing half of STUB.6.
//
// Register the FULL builtin surface exactly as the CLI does, then assert that
// every correct-by-design name still resolves to the locked stub literal. This
// fails the moment a correct-by-design stub is converted into a
// half-implementation (any handler that returns something other than the stub
// literal).
// ============================================================================

fn full_dispatcher() -> RegistrySlashDispatcher {
    let mut reg = CommandRegistry::new();
    register_all_builtin_commands(&mut reg);
    RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
}

#[tokio::test]
async fn every_correct_by_design_stub_still_returns_the_locked_literal() {
    let d = full_dispatcher();
    for (name, reason) in CORRECT_BY_DESIGN_STUBS {
        let expected = UnimplementedCommandHandler::stub_literal(name);
        match d.dispatch(&format!("/{name}")).await {
            SlashDispatchResult::Handled { display } => {
                assert_eq!(
                    display, expected,
                    "/{name} ({reason}) is a correct-by-design stub and must still return \
                     the locked literal; if it changed, it was turned into a half-implementation"
                );
            }
            other => panic!("/{name} must dispatch to Handled(stub literal), got {other:?}"),
        }
    }
}

#[tokio::test]
async fn host_bound_deferred_gaps_also_currently_stub_but_are_not_locked_as_faithful() {
    // Kept as a generic guard for future host-bound gaps; currently empty.
    let d = full_dispatcher();
    for (name, _reason) in HOST_BOUND_DEFERRED_GAPS {
        let expected = UnimplementedCommandHandler::stub_literal(name);
        match d.dispatch(&format!("/{name}")).await {
            SlashDispatchResult::Handled { display } => {
                assert_eq!(
                    display, expected,
                    "/{name} currently stubbed (deferred host infra); flips when implemented"
                );
            }
            other => panic!("/{name} must dispatch to Handled, got {other:?}"),
        }
    }
}

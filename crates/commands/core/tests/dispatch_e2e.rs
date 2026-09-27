//! End-to-end integration test: build a `CommandRegistry`, register all current
//! wire a dispatcher, and exercise the full surface from the public API.
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-09-commands-surface.md`
//! Task 7.

use command_api::CommandRegistry;
use command_api::RegistrySlashDispatcher;
use command_core::register_all_builtin_commands;
use platform_api::{SlashCommandDispatcher, SlashDispatchResult};
use std::sync::Arc;
use tokio::sync::RwLock;

fn build_dispatcher() -> RegistrySlashDispatcher {
    let mut reg = CommandRegistry::new();
    register_all_builtin_commands(&mut reg);
    command_core::register_bundled_skills(&mut reg, false);
    RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
}

#[tokio::test]
async fn dataviz_dispatches_bundled_body_and_optional_user_request() {
    let d = build_dispatcher();
    let outcome = d
        .dispatch("/dataviz make the revenue chart keyboard accessible")
        .await;
    let SlashDispatchResult::RunAsTurn { prompt } = outcome else {
        panic!("expected /dataviz to run as a prompt turn, got {outcome:?}");
    };
    assert!(prompt.starts_with("# Data Visualization\n"));
    assert!(prompt.contains("references/palette.md"));
    assert!(prompt.contains("references/marks-and-anatomy.md"));
    assert!(prompt.contains("references/interaction.md"));
    assert!(prompt.ends_with("## User Request\n\nmake the revenue chart keyboard accessible"));
}

#[tokio::test]
async fn happy_path_known_core_command() {
    let d = build_dispatcher();
    let outcome = d.dispatch("/help").await;
    match outcome {
        SlashDispatchResult::Handled { display } => {
            assert_eq!(display, "help: not implemented in v0.6.0 (M5)");
        }
        other => panic!("expected Handled, got {other:?}"),
    }
}

#[tokio::test]
async fn happy_path_known_unimplemented_command() {
    let d = build_dispatcher();
    // `x402` left the 105-name surface with the re-lock; `teleport` is a
    // still-registered stub.
    let outcome = d.dispatch("/teleport").await;
    match outcome {
        SlashDispatchResult::Handled { display } => {
            assert_eq!(display, "teleport: not implemented in v0.6.0 (M5)");
        }
        other => panic!("expected Handled, got {other:?}"),
    }
}

#[tokio::test]
async fn known_command_with_args_ignored_by_stub() {
    let d = build_dispatcher();
    let outcome = d.dispatch("/exit --force --soon=now").await;
    match outcome {
        SlashDispatchResult::Handled { display } => {
            assert_eq!(display, "exit: not implemented in v0.6.0 (M5)");
        }
        other => panic!("expected Handled, got {other:?}"),
    }
}

#[tokio::test]
async fn unknown_command_path() {
    let d = build_dispatcher();
    let outcome = d.dispatch("/zzz-fake").await;
    match outcome {
        SlashDispatchResult::Unknown { name, display } => {
            assert_eq!(name, "zzz-fake");
            assert_eq!(display, "Unknown command: /zzz-fake");
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[tokio::test]
async fn non_slash_input_path() {
    let d = build_dispatcher();
    let outcome = d.dispatch("hello /clear world").await;
    assert!(matches!(outcome, SlashDispatchResult::NotASlashCommand));
}

#[tokio::test]
async fn many_dispatches_against_one_registry() {
    let d = build_dispatcher();
    for name in ["clear", "compact", "teleport", "version", "help"] {
        let raw = format!("/{name}");
        let outcome = d.dispatch(&raw).await;
        let expected = format!("{name}: not implemented in v0.6.0 (M5)");
        match outcome {
            SlashDispatchResult::Handled { display } => assert_eq!(display, expected),
            other => panic!("/{name} unexpected: {other:?}"),
        }
    }
}

#[tokio::test]
async fn concurrent_dispatches_against_one_registry() {
    let d = Arc::new(build_dispatcher());
    let mut handles = vec![];
    for name in ["agents", "config", "version", "doctor", "hooks", "init"] {
        let dispatcher = d.clone();
        let raw = format!("/{name}");
        let expected = format!("{name}: not implemented in v0.6.0 (M5)");
        handles.push(tokio::spawn(async move {
            let outcome = dispatcher.dispatch(&raw).await;
            match outcome {
                SlashDispatchResult::Handled { display } => assert_eq!(display, expected),
                other => panic!("/{name} {other:?}"),
            }
        }));
    }
    for h in handles {
        h.await.expect("task panic");
    }
}

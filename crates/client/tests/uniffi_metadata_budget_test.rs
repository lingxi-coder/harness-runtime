//! The gate that keeps `client::protocol` buildable for iOS and Android.
//!
//! # The failure mode this exists to catch
//!
//! `#[derive(uniffi::Enum)]` / `#[derive(uniffi::Record)]` fold a type's whole
//! description — module path, every variant name, every field name, every field
//! type tag, and every `///` docstring on the type, its variants and their
//! fields — into ONE compile-time `uniffi_core::MetadataBuffer`. That buffer is
//! a fixed `[u8; BUF_SIZE]` with `BUF_SIZE` hardcoded to 32768 in
//! `uniffi_core`, and the writers `assert!` on overflow **in a `const`
//! context**.
//!
//! So overflow is not a warning and not a truncation. It is
//! `error[E0080]: evaluation of constant value failed`, `client::protocol
//! --features uniffi` stops compiling, and every mobile client — `ios-framework`,
//! `android-aar`, `engine-mobile` — becomes unbuildable. On 2026-08-27
//! Under UniFFI 0.28, `ClientCommand` crossed its 16384-byte limit at 20156 bytes and the iOS and Android
//! apps could not be built at all.
//!
//! # Why nothing caught it
//!
//! Every gate on the desktop side (`cargo test -p bridge-server`, the electron
//! suites, `cargo check` at the workspace root) builds `client::protocol` with
//! DEFAULT features, where `uniffi` is off and none of this metadata exists.
//! The feature that breaks is the one nothing enabled. Hence a test that
//! *only* exists under `--features uniffi`, plus the `mobile-ffi-surface` CI
//! job that actually turns the feature on.
//!
//! # Why a budget rather than just "does it compile"
//!
//! By the time the hard limit is hit, the crate no longer compiles, so no test
//! can report anything — the only signal is a const-eval panic pointing at a
//! `derive` in someone else's crate. This test fires 4096 bytes EARLIER, while
//! the code still builds, and says which type is running out of room and what
//! to do about it.

#![cfg(feature = "uniffi")]

use client::protocol::commands::UNIFFI_META_CLIENT_ENUM_CLIENTCOMMAND;
use client::protocol::events::UNIFFI_META_CLIENT_ENUM_CLIENTEVENT;
use client::protocol::local_apps::{
    UNIFFI_META_CLIENT_ENUM_APPEVENTDTO, UNIFFI_META_CLIENT_ENUM_PLUGINCOMMANDDTO,
};

/// `uniffi_core::metadata::BUF_SIZE` — the hard, hardcoded ceiling every
/// `MetadataBuffer` asserts against. Verified against `uniffi_core 0.32.2`
/// (`src/metadata.rs:100`); `uniffi_core_pin_is_still_the_one_this_budget_was_measured_against`
/// below fails if that pin moves, because a different release may choose a
/// different `BUF_SIZE`.
const UNIFFI_METADATA_BUF_SIZE: usize = 32_768;

/// How much room the budget deliberately leaves between "this test goes red"
/// and "the crate stops compiling". 4 KiB is roughly a third of a full
/// `ClientCommand`, i.e. enough slack to land a considered fix instead of an
/// emergency one.
const REQUIRED_RESERVE: usize = 4_096;

/// The largest metadata buffer any single type in this crate may occupy.
///
/// Raising this toward `UNIFFI_METADATA_BUF_SIZE` defeats the entire point and
/// is refused by
/// `budget_keeps_a_usable_reserve_below_the_hard_limit`. If a type is over
/// budget, shrink the type — do not move the line.
const PER_TYPE_METADATA_BUDGET: usize = 12_288;

/// The uniffi release `UNIFFI_METADATA_BUF_SIZE` was read from.
const PINNED_UNIFFI_CORE_VERSION: &str = "0.32.2";

fn assert_within_budget(type_name: &str, source_path: &str, metadata: &[u8]) {
    let actual = metadata.len();
    assert!(
        actual <= PER_TYPE_METADATA_BUDGET,
        "`{type_name}`'s UniFFI metadata is {actual} bytes — {over} bytes over the \
         {PER_TYPE_METADATA_BUDGET}-byte budget, and only {to_wall} bytes below \
         uniffi_core {PINNED_UNIFFI_CORE_VERSION}'s hard {UNIFFI_METADATA_BUF_SIZE}-byte \
         MetadataBuffer limit.\n\
         \n\
         Crossing that hard limit is NOT a warning: the `uniffi::Enum` derive fails \
         const-evaluation (error[E0080]), `cargo check -p client --features uniffi` \
         stops compiling, and ios-framework / android-aar / engine-mobile cannot be built \
         at all. The desktop build will not notice, because it never enables `uniffi`.\n\
         \n\
         The buffer holds the module path, every variant NAME, every field NAME, every \
         field TYPE tag, AND every `///` DOCSTRING on the type, its variants and their \
         fields. Docstrings are normally the bulk of it: when `ClientCommand` overflowed on \
         2026-08-27 its structure was 3939 bytes and its prose was 16217.\n\
         \n\
         To get back under budget, in {source_path}:\n\
         (a) demote per-variant / per-field `///` prose to ordinary `//` comments — the text \
         stays exactly where it is, and no wire tag, field name, Rust API or command changes; \
         or\n\
         (b) move a variant's fields into their own `#[derive(uniffi::Record)]` struct, which \
         gets a separate {UNIFFI_METADATA_BUF_SIZE}-byte buffer of its own.\n\
         \n\
         Do NOT raise PER_TYPE_METADATA_BUDGET.",
        over = actual - PER_TYPE_METADATA_BUDGET,
        to_wall = UNIFFI_METADATA_BUF_SIZE.saturating_sub(actual),
    );
}

/// `ClientCommand` is the type that actually overflowed. It is the biggest
/// enum a client can send and the one every new command lands in.
#[test]
fn client_command_uniffi_metadata_stays_within_budget() {
    assert_within_budget(
        "ClientCommand",
        "client/src/protocol/commands.rs",
        &UNIFFI_META_CLIENT_ENUM_CLIENTCOMMAND,
    );
}

/// `ClientEvent` is the other half of the contract and the next one at risk —
/// it is materially closer to the ceiling than `ClientCommand` is.
#[test]
fn client_event_uniffi_metadata_stays_within_budget() {
    assert_within_budget(
        "ClientEvent",
        "client/src/protocol/events.rs",
        &UNIFFI_META_CLIENT_ENUM_CLIENTEVENT,
    );
}

/// `AppEventDto` is the enum every create-flow event rides in — same shape of
/// risk as `ClientEvent`, just unwatched until now.
#[test]
fn app_event_dto_uniffi_metadata_stays_within_budget() {
    assert_within_budget(
        "AppEventDto",
        "client/src/protocol/local_apps.rs",
        &UNIFFI_META_CLIENT_ENUM_APPEVENTDTO,
    );
}

/// `PluginCommandDto` is the plugin-facing analogue of `ClientCommand` — same
/// shape of risk, just unwatched until now.
#[test]
fn plugin_command_dto_uniffi_metadata_stays_within_budget() {
    assert_within_budget(
        "PluginCommandDto",
        "client/src/protocol/local_apps.rs",
        &UNIFFI_META_CLIENT_ENUM_PLUGINCOMMANDDTO,
    );
}

/// The budget is only useful if it fires while the code still compiles.
/// Someone "fixing" a red run by nudging `PER_TYPE_METADATA_BUDGET` up to
/// 16383 would turn this file into a no-op that reports the overflow at the
/// same instant the compiler already does. This test refuses that.
#[test]
fn budget_keeps_a_usable_reserve_below_the_hard_limit() {
    assert!(
        PER_TYPE_METADATA_BUDGET < UNIFFI_METADATA_BUF_SIZE,
        "PER_TYPE_METADATA_BUDGET ({PER_TYPE_METADATA_BUDGET}) must be below uniffi's hard \
         {UNIFFI_METADATA_BUF_SIZE}-byte MetadataBuffer limit."
    );
    let reserve = UNIFFI_METADATA_BUF_SIZE - PER_TYPE_METADATA_BUDGET;
    assert!(
        reserve >= REQUIRED_RESERVE,
        "PER_TYPE_METADATA_BUDGET ({PER_TYPE_METADATA_BUDGET}) leaves only {reserve} bytes \
         before uniffi's hard {UNIFFI_METADATA_BUF_SIZE}-byte limit, but this gate is only \
         actionable if it goes red at least {REQUIRED_RESERVE} bytes early — once the limit \
         is reached the crate no longer compiles and no test can run at all. Shrink the \
         offending type instead of raising the budget."
    );
}

/// `UNIFFI_METADATA_BUF_SIZE` is a constant copied out of someone else's crate.
/// It is private there, so nothing links the two except this pin: if the uniffi
/// dependency moves, the copied ceiling has to be re-read from the new source
/// before this gate can be trusted again.
#[test]
fn uniffi_core_pin_is_still_the_one_this_budget_was_measured_against() {
    let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock"))
        .expect("workspace Cargo.lock is readable from client::protocol/");
    let mut versions = Vec::new();
    let mut in_uniffi_core = false;
    for line in lock.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            in_uniffi_core = false;
        } else if line == r#"name = "uniffi_core""# {
            in_uniffi_core = true;
        } else if in_uniffi_core {
            if let Some(rest) = line.strip_prefix("version = ") {
                versions.push(rest.trim_matches('"').to_owned());
                in_uniffi_core = false;
            }
        }
    }
    assert_eq!(
        versions,
        vec![PINNED_UNIFFI_CORE_VERSION.to_owned()],
        "This gate's {UNIFFI_METADATA_BUF_SIZE}-byte hard limit was read out of \
         uniffi_core {PINNED_UNIFFI_CORE_VERSION}'s `src/metadata.rs` (`const BUF_SIZE`). \
         Cargo.lock now resolves uniffi_core to {versions:?}. Re-read `BUF_SIZE` in that \
         release and update UNIFFI_METADATA_BUF_SIZE / PINNED_UNIFFI_CORE_VERSION together \
         — until then the budget in this file is measured against the wrong ceiling."
    );
}

//! `client-protocol` — the versioned, transport-agnostic DTO contract shared by
//! every M10 native-app client transport (bridge-server WebSocket/JSON-RPC for
//! Electron, `UniFFI` for iOS/Android).
//!
//! This crate is **pure contract**: it defines the [`commands`], [`events`],
//! [`message`], [`permission`], [`computer_access`], [`listings`], and
//! [`error`] DTOs plus the [`version`] constant. It contains NO engine logic —
//! the engine→DTO lowering lives in the separate `client-adapter` crate
//! (governing decision §0.2).
//!
//! Governing constraints frozen here (see the M10 foundation plan §0):
//! - Tool payloads are JSON **Strings** on the wire (`input_json`/`result_json`,
//!   `effective_json`/`provenance_json`); `serde_json::Value` is NOT a dependency
//!   of this crate because it is not UniFFI-representable (§0.4).
//! - The SAME DTOs compile plain (bridge-server) and, under the `uniffi`
//!   feature, as `UniFFI` types (mobile) — the feature is declared here and lit
//!   up in F3 once `uniffi` is vendored/pinned offline (F3-00).
//!
//! At F1-00 the modules are empty stubs; subsequent F1-* tasks fill them in.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 69 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]

pub mod ask_user_question;
pub mod audio;
pub mod commands;
pub mod computer_access;
pub mod controls;
pub mod error;
pub mod events;
pub mod listings;
pub mod local_apps;
pub mod message;
pub mod permission;
pub mod tool_display;
pub mod version;

// UniFFI scaffolding (F3-01). Under the `uniffi` feature the DTOs above gain
// `#[derive(uniffi::Enum/Record/Error)]` and this macro emits the per-crate
// metadata + initialization the bindgen reads. It is a no-op for the default
// (bridge-server) build, which never compiles this in. The aggregating cdylib
// crate (ios-framework / android-aar) re-exports this scaffolding so all
// component symbols land in the final library.
#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

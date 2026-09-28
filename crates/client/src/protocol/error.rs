//! `ClientError` — the flat, `#[derive(uniffi::Error)]`-ready result error.
//!
//! `ClientError` is the typed error returned across a `Result` boundary by a
//! client transport — most directly the mobile `submit(command) -> Result<(),
//! ClientError>` FFI entry point (plan F3-05). It is DISTINCT from
//! [`crate::protocol::events::ErrorKindDto`]: that enum is the coarse class carried by the
//! streaming, fire-and-forget [`crate::protocol::events::ClientEvent::Error`]; this enum
//! is the *call-result* error a synchronous request resolves to.
//!
//! ## Flat by design (decision §0.4, plan F1-07)
//!
//! Every variant is FLAT — each carries a single `message: String` field with NO
//! nested non-FFI payloads. `serde_json::Value` and other non-UniFFI-representable
//! types are deliberately excluded so the `#[derive(uniffi::Error)]` (added in
//! F3-01, once `uniffi` is vendored/pinned offline in F3-00) lands without a
//! shape change. F1-07 freezes the shape and the `thiserror` `Display` strings;
//! F3-01 turns the FFI-flatness into a real compile-time check by enabling the
//! derive under the `uniffi` feature.
//!
//! A *struct* variant (`{ message }`) rather than a tuple variant is used so the
//! type honors the frozen §0.1 serde convention `#[serde(tag = "type")]`: serde
//! cannot internally-tag a newtype variant wrapping a primitive `String`, but it
//! can tag a struct variant. The shape stays flat (one `String` field) and
//! UniFFI-flat-error compatible.
//!
//! The derive is feature-gated with `#[cfg_attr(feature = "uniffi", …)]` so the
//! SAME type compiles plain (bridge-server) and as a `UniFFI` error (mobile). The
//! `cfg_attr` line is wired in F3-01; at F1-07 the `uniffi` package is not yet a
//! dependency (it is commented out in `Cargo.toml` until F3-00), so adding the
//! derive now would break the stubbed `--features uniffi` build. The type is
//! kept derive-*ready* instead.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A flat, FFI-ready error returned across a client transport boundary.
///
/// Variants mirror the command failure modes a client must branch on:
/// - [`Transport`](ClientError::Transport): the connection/IPC layer failed.
/// - [`Protocol`](ClientError::Protocol): a frame violated the wire contract.
/// - [`Rejected`](ClientError::Rejected): the request was refused (e.g. a
///   permission denial, or an out-of-lifecycle command).
/// - [`NotFound`](ClientError::NotFound): a named target (session, task, model)
///   does not exist.
/// - [`Internal`](ClientError::Internal): any other server-side failure.
///
/// Internally tagged on `type` with `snake_case` variant names (the frozen serde
/// convention, §0.1), so it round-trips byte-stably alongside every other DTO.
/// `#[non_exhaustive]` so a future variant is an additive (non-breaking) change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClientError {
    /// The connection / IPC transport failed (dropped socket, write error,
    /// failed handshake).
    #[error("transport error: {message}")]
    Transport {
        /// Human-readable cause of the transport failure.
        message: String,
    },

    /// A frame violated the wire contract (malformed JSON, unknown variant,
    /// version mismatch).
    #[error("protocol error: {message}")]
    Protocol {
        /// The protocol-violation detail.
        message: String,
    },

    /// The request was refused — e.g. a permission denial, or a command issued
    /// out of the session lifecycle.
    #[error("request rejected: {message}")]
    Rejected {
        /// The reason the request was refused.
        message: String,
    },

    /// A named target (session id, task id, model name) does not exist.
    #[error("not found: {message}")]
    NotFound {
        /// Names the missing target.
        message: String,
    },

    /// Any other server-side / internal failure.
    #[error("internal error: {message}")]
    Internal {
        /// The internal-failure detail.
        message: String,
    },
}

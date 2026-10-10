//! Remote-drive **wire protocol** types (M8-P13).
//!
//! These are the handshake + framing DTOs for M9's remote-driving work (a
//! desktop engine driven by a mobile/remote client over a bridge transport).
//! M8 ships only the type definitions + serde derives — no transport, no auth
//! flow, no server loop (those land in M9, see `// M9:` markers).
//!
//! The module is named `wire` (not `protocol`) deliberately: `bridge` depends
//! on the `protocol` crate, so a local `mod protocol` would shadow it. The
//! public types are re-exported at the crate root, so consumers write
//! `bridge::ServerHello` regardless.

use client::protocol::audio::AudioCapabilitySnapshotDto;
use client::protocol::computer_access::ComputerAccessRequestDto;
use client::protocol::events::ClientEvent;
use client::protocol::permission::PermissionRequest;
use client::protocol::version::CLIENT_PROTOCOL_VERSION;
use serde::{Deserialize, Serialize};

/// Bridge wire-protocol version this build speaks.
///
/// Bumped `0.1.0` → `0.2.0` in M10-F2 for the server-push event [`Frame`] and
/// the `client_protocol_version` carry on [`Capabilities`]. This is the
/// **envelope** version (framing/handshake), distinct from the contract-level
/// [`client::protocol::version::CLIENT_PROTOCOL_VERSION`] exchanged independently
/// inside [`Capabilities`] (governing decision §0.10).
pub const BRIDGE_PROTOCOL_VERSION: &str = "0.2.0";

/// Capability flags advertised in the handshake.
#[allow(clippy::struct_excessive_bools)] // mirrors the wire protocol's flag list verbatim
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Actual connection-scoped native Agent realtime controller is installed.
    #[serde(default)]
    pub realtime_audio: bool,
    /// Endpoint can stream incremental turn events.
    pub supports_streaming: bool,
    /// Endpoint exposes the tool surface.
    pub supports_tools: bool,
    /// Endpoint exposes the skill surface.
    pub supports_skills: bool,
    /// Endpoint exposes the slash-command surface.
    pub supports_commands: bool,
    /// The `client-protocol` DTO contract version this endpoint speaks
    /// (`client::protocol::version::CLIENT_PROTOCOL_VERSION`). Exchanged
    /// INDEPENDENTLY of [`BRIDGE_PROTOCOL_VERSION`]: the wire envelope and the
    /// DTO contract are versioned separately (governing decision §0.10).
    pub client_protocol_version: String,
    /// Device audio support/readiness snapshot. `None` means the endpoint has
    /// not reported its device-local AudioService capabilities yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioCapabilitySnapshotDto>,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            realtime_audio: false,
            supports_streaming: true,
            supports_tools: true,
            supports_skills: true,
            supports_commands: true,
            client_protocol_version: CLIENT_PROTOCOL_VERSION.to_string(),
            audio: None,
        }
    }
}

/// Decide whether a `remote` semver version is COMPATIBLE with the `local` one
/// for handshake purposes (M10-F2-07).
///
/// The rule is the structural-diff convention of governing decision §0.10: a
/// **major**-version bump signals a breaking change (a removed / renamed /
/// retyped wire entry), so two peers are compatible **iff they share the same
/// major version**. Minor / patch differences are additive (new variant / new
/// optional field) and remain compatible.
///
/// This is applied INDEPENDENTLY to both versioned numbers exchanged in the
/// handshake — [`BRIDGE_PROTOCOL_VERSION`] (the wire envelope, carried on
/// [`ClientHello::protocol_version`]) and
/// [`client::protocol::version::CLIENT_PROTOCOL_VERSION`] (the DTO contract,
/// carried on [`Capabilities::client_protocol_version`]) — and a mismatch in
/// EITHER refuses the connection.
///
/// **Fail-closed**: a version string that does not parse as `major[.minor…]`
/// (empty, non-numeric major) is treated as INCOMPATIBLE rather than silently
/// accepted, so a malformed handshake cannot slip past the guard.
#[must_use]
pub fn version_compatible(local: &str, remote: &str) -> bool {
    match (major_of(local), major_of(remote)) {
        (Some(a), Some(b)) => a == b,
        // An unparseable version on either side is refused (fail-closed).
        _ => false,
    }
}

/// Parse the leading `major` component of a `major.minor.patch` string. Returns
/// `None` if the major component is missing or not a base-10 integer.
fn major_of(version: &str) -> Option<u64> {
    version.split('.').next()?.trim().parse::<u64>().ok()
}

/// Client → server opening handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    /// Wire-protocol version the client speaks.
    pub protocol_version: String,
    /// Human-readable client identifier (e.g. `"lingxi-ios/0.9.0"`).
    pub client_name: String,
    /// What the client can handle.
    pub capabilities: Capabilities,
}

/// Server → client handshake reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    /// Wire-protocol version the server speaks.
    pub protocol_version: String,
    /// Human-readable server identifier (e.g. `"lingxi-engine-desktop/0.9.0"`).
    pub server_name: String,
    /// What the server can provide.
    pub capabilities: Capabilities,
}

/// A framed JSON-RPC-style request from client to server.
///
/// M9 maps `method`/`params` onto `lingxi_core::types::Effect` execution; M8 keeps the
/// envelope generic (a `serde_json::Value` payload) so the wire shape is locked
/// without coupling to the concrete effect enum yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BridgeRequest {
    /// Correlation id (echoed in the matching [`BridgeResponse`]).
    pub id: u64,
    /// Method name (e.g. `"run_turn"`, `"dispatch_tool"`).
    pub method: String,
    /// Method parameters. // M9: typed against `lingxi_core::types::Effect`.
    pub params: serde_json::Value,
}

/// A framed reply to a [`BridgeRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BridgeResponse {
    /// Correlation id of the originating request.
    pub id: u64,
    /// Success payload. // M9: typed against `lingxi_core::types::EffectResult`.
    pub result: Option<serde_json::Value>,
    /// Error payload (mutually exclusive with `result`).
    pub error: Option<BridgeWireError>,
}

/// Wire-level error inside a [`BridgeResponse`] (distinct from the engine-side
/// runtime `transport::BridgeError`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeWireError {
    /// Numeric error code (JSON-RPC convention).
    pub code: i32,
    /// Log-safe error message.
    pub message: String,
}

/// Server → client auth challenge. // M9: JWT/nonce flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthChallenge {
    /// Opaque nonce the client signs.
    pub nonce: String,
}

/// Client → server auth reply. // M9: signed token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthResponse {
    /// Bearer token / signed nonce.
    pub token: String,
}

/// The post-handshake **frame** carried over the WebSocket text channel
/// (M10-F2 wire v0.2).
///
/// The request/response wire alone cannot express an UNSOLICITED server push,
/// which the streaming turn feed requires. `Frame` is the tagged union that adds
/// that third arm:
///
/// - [`Frame::Request`] — a client→server command. `params` is a
///   [`client::protocol::commands::ClientCommand`] serialized as JSON; `id`
///   correlates the matching [`Frame::Response`].
/// - [`Frame::Response`] — the server→client reply to a request, echoing its
///   `id`. `result` is a [`ClientEvent`]/reply payload as JSON.
/// - [`Frame::Event`] — an UNSOLICITED server→client [`ClientEvent`] push
///   (streamed turn events, listing updates). Events carry **no** `id` — they
///   are not correlated to any request.
/// - [`Frame::PermissionRequest`] — an UNSOLICITED server→client
///   [`PermissionRequest`] push (F2-06). [`PermissionRequest`] is a STANDALONE
///   frozen DTO in [`client::protocol::permission`], NOT a [`ClientEvent`]
///   variant, so it cannot ride on [`Frame::Event`]. It gets its own arm here
///   (additive — the enum is `#[non_exhaustive]`, so no `client-protocol`
///   snapshot changes). Like an event it carries **no** envelope `id`; the inner
///   [`PermissionRequest::request_id`] is the correlator the client echoes back
///   in the matching [`ClientCommand::ApprovePermission`]/`DenyPermission`
///   (which travel as [`Frame::Request`]).
/// - [`Frame::ComputerAccessRequest`] — an UNSOLICITED server→client
///   [`ComputerAccessRequestDto`] push, the SAME shape as
///   [`Frame::PermissionRequest`] one level down: the `computer` tool's
///   `request_access` prompt can't be expressed as a
///   [`crate::wire::Frame::PermissionRequest`] (per-app checkboxes, a tier, and
///   independent capability flags — see
///   `permission::computer_access`'s own doc comment for why it bypasses
///   the generic permission gate), so it gets its own additive arm. Carries
///   **no** envelope `id`; the inner
///   [`ComputerAccessRequestDto::request_id`] is the correlator the client
///   echoes back in the matching
///   [`ClientCommand::ApproveComputerAccess`]/`DenyComputerAccess`
///   (which travel as [`Frame::Request`]).
///
/// **Adjacently** tagged on `type` (`"request"` / `"response"` / `"event"`,
/// `snake_case`) with the payload under `payload`. Adjacent (not internal)
/// tagging is required because the inner [`ClientEvent`] is ITSELF internally
/// tagged on `type` — nesting it under `payload` keeps the frame discriminator
/// from colliding with the event's own `type` field. `#[non_exhaustive]` so a
/// future frame kind is additive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Frame {
    /// A client→server command envelope (carries a correlation `id`).
    Request(BridgeRequest),
    /// A server→client reply (echoes the originating request's `id`).
    Response(BridgeResponse),
    /// An unsolicited server→client [`ClientEvent`] push (no `id`).
    Event(ClientEvent),
    /// An unsolicited server→client [`PermissionRequest`] push (F2-06, no `id`).
    /// Answered by a separate [`Frame::Request`] carrying
    /// `ApprovePermission`/`DenyPermission` correlated by
    /// [`PermissionRequest::request_id`].
    PermissionRequest(PermissionRequest),
    /// An unsolicited server→client [`ComputerAccessRequestDto`] push (no
    /// `id`). Answered by a separate [`Frame::Request`] carrying
    /// `ApproveComputerAccess`/`DenyComputerAccess` correlated by
    /// [`ComputerAccessRequestDto::request_id`].
    ComputerAccessRequest(ComputerAccessRequestDto),
}

#[cfg(test)]
mod tests {
    use super::version_compatible;

    #[test]
    fn same_major_is_compatible() {
        // Identical versions, and minor/patch drift within the same major, are
        // additive (new variant / new optional field) ⇒ compatible (§0.10).
        assert!(version_compatible("0.2.0", "0.2.0"));
        assert!(version_compatible("0.2.0", "0.2.7"));
        assert!(version_compatible("1.0.0", "1.4.2"));
        assert!(version_compatible("2.5.0", "2.0.9"));
    }

    #[test]
    fn different_major_is_incompatible() {
        // A major bump signals a breaking (removed/renamed/retyped) change.
        assert!(!version_compatible("0.2.0", "1.0.0"));
        assert!(!version_compatible("1.0.0", "2.0.0"));
        assert!(!version_compatible("1.4.2", "99.0.0"));
    }

    #[test]
    fn unparseable_version_is_refused_fail_closed() {
        assert!(!version_compatible("1.0.0", ""));
        assert!(!version_compatible("", "1.0.0"));
        assert!(!version_compatible("1.0.0", "abc"));
        assert!(!version_compatible("not-a-version", "1.0.0"));
    }
}

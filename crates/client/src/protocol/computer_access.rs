//! `computer` tool `request_access` DTOs — the wire shape for the Electron/
//! bridge-server path, sourced from
//! `permission::computer_access::{ComputerAccessRequest, ComputerAccessResponse}`.
//!
//! Mirrors [`crate::protocol::permission`]'s conventions exactly:
//! - the outbound request/response STRUCTS carry no own `type` tag — like
//!   [`crate::protocol::permission::PermissionRequest`], the tag lives one level up on the
//!   transport envelope ([`bridge::wire::Frame`]'s adjacently-tagged
//!   `Frame::ComputerAccessRequest` arm), not on this DTO itself;
//! - every optional field uses
//!   `#[serde(default, skip_serializing_if = "Option::is_none")]`.
//!
//! [`AccessTierDto`] is the ONE deliberate departure from this crate's usual
//! internally-tagged enum convention (`#[serde(tag = "type", …)]`, see
//! [`crate::protocol::permission::PermissionResponseDto`]): it carries no per-variant
//! payload and rides as a bare wire STRING (`"read"` / `"click"` / `"full"`),
//! byte-identical to the source `permission::computer_access::AccessTier::
//! as_str()` — a plain `#[serde(rename_all = "snake_case")]` fieldless enum
//! serializes exactly that way (no `tag` attribute ⇒ no wrapping object).
//!
//! `serde_json::Value` never enters this crate (decision §0.4) — the DTOs here
//! are already flat scalar/`Vec`/`Option` fields, so no JSON-String collapse is
//! needed (unlike `tool_input_json` on [`crate::protocol::permission::PermissionKindDto`]).

use serde::{Deserialize, Serialize};

/// One requested application — mirrors
/// `permission::computer_access::RequestedApp`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct RequestedAppDto {
    /// Display label (the resolved bundle id when available, else the raw
    /// caller-supplied name).
    pub label: String,
}

/// The per-app capability tier requested — mirrors
/// `permission::computer_access::AccessTier`. A bare wire STRING (see the
/// module doc for why this is not internally tagged like this crate's other
/// enums): `"read"` / `"click"` / `"full"`, byte-identical to `AccessTier::as_str()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AccessTierDto {
    /// Visible in screenshots only — no clicks or typing.
    Read,
    /// Plain clicks, scroll, drag, cursor queries.
    Click,
    /// Full interaction: right-click, modifier-clicks, typing, key presses.
    Full,
}

/// Which macOS TCC permissions are missing — mirrors
/// `permission::computer_access::TccState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TccStateDto {
    /// Whether Accessibility is granted.
    pub accessibility: bool,
    /// Whether Screen Recording is granted.
    pub screen_recording: bool,
}

/// Outbound `request_access` request — the wire lowering of
/// `permission::computer_access::ComputerAccessRequest`. Correlated by
/// `request_id` (assigned by the connection-scoped broker, mirroring
/// [`crate::protocol::permission::PermissionRequest::request_id`]); echoed back in the
/// matching `ApproveComputerAccess`/`DenyComputerAccess`
/// ([`crate::protocol::commands::ClientCommand`]).
///
/// Carries NO own `type` tag (see the module doc) — the tag lives on the
/// transport envelope that wraps this DTO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ComputerAccessRequestDto {
    /// Connection-scoped correlator assigned by the broker; echoed back in the
    /// matching `ApproveComputerAccess`/`DenyComputerAccess`.
    pub request_id: u64,
    /// One-sentence explanation shown to the user (`request_access`'s `reason`
    /// field, verbatim).
    pub reason: String,
    /// Apps requested for this grant.
    pub apps: Vec<RequestedAppDto>,
    /// The tier requested for `apps`.
    pub tier: AccessTierDto,
    /// Whether `clipboardRead` was requested.
    pub clipboard_read: bool,
    /// Whether `clipboardWrite` was requested.
    pub clipboard_write: bool,
    /// Whether `systemKeyCombos` was requested.
    pub system_key_combos: bool,
    /// `Some` when a required macOS permission (Accessibility / Screen
    /// Recording) is missing. Optional + skipped when absent (§0.1
    /// convention).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcc_state: Option<TccStateDto>,
}

/// The user's resolution — the wire lowering of
/// `permission::computer_access::ComputerAccessResponse`. An empty
/// `granted_apps` with every flag `false` means "denied"; there is no separate
/// boolean (mirrors the source type's own doc comment).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ComputerAccessResponseDto {
    /// The subset of the request's `apps` (by label) the user left checked.
    pub granted_apps: Vec<String>,
    /// Whether `clipboardRead` was granted.
    pub clipboard_read: bool,
    /// Whether `clipboardWrite` was granted.
    pub clipboard_write: bool,
    /// Whether `systemKeyCombos` was granted.
    pub system_key_combos: bool,
}

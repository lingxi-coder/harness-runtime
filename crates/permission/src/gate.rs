//! Re-export shim — workspace-wide single source of truth lives in
//! [`platform_api::permission_gate`] and [`platform_api::prompting_gate`].
//! `lingxi-permission` re-exports so downstream crates only need to depend on
//! `lingxi-permission`, not on the platform-api crate directly.
#![forbid(unsafe_code)]

pub use platform_api::permission_gate::{
    AutoModePrompt, HandoffReview, MatchedAskRule, PermissionAbort, PermissionCheckContext,
    PermissionDecision, PermissionDecisionSource, PermissionGate, PermissionOutcome,
    PermissionRequestSource, PermissionResolution, PromptWorker,
};
pub use platform_api::prompting_gate::{
    PermissionRequest, PermissionResponse, PromptDecision, PromptDefault, PromptError,
    PromptingGate,
};

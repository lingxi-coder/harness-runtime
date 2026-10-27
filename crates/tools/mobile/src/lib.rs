//! `tool-mobile` — native personal-device services exposed as typed builtin tools.
//!
//! Folds the native non-media device tools into a single crate of submodules.
//! Every tool here is pure Rust that routes to an
//! `Arc<dyn …Control>` capability carried in [`tool_api::BuiltinToolContext`].
//!
//! ## Two composition roots, two entry points
//!
//! Media capture, recording, speech, playback, and realtime workflows live in
//! `tool-media`, shared by desktop and mobile. This crate retains the other
//! mobile-exclusive device tools.

#![forbid(unsafe_code)]

pub mod calendar;
pub mod clipboard;
pub mod contacts;
pub mod device_status;
mod device_support;
pub mod haptics;
pub mod location;
pub mod notification;
pub mod open_url;
pub mod share;

pub use calendar::CalendarTool;
pub use clipboard::ClipboardTool;
pub use contacts::ContactsTool;
pub use device_status::DeviceStatusTool;
pub use haptics::HapticsTool;
pub use location::LocationTool;
pub use notification::NotificationTool;
pub use open_url::OpenUrlTool;
pub use share::ShareTool;

/// Register the non-media mobile device tools backed by this host into `reg`.
///
/// Replaces the former per-crate `tool_<name>::register_all` calls the
/// composition root made; the ordering matches the previous wiring.
///
/// Each tool appears only when its corresponding host control is configured.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    use std::sync::Arc;

    if ctx.notifications.is_some() {
        notification::register_all(reg, ctx.clone());
    }
    if ctx.clipboard.is_some() {
        clipboard::register_all(reg, ctx.clone());
    }
    if ctx.share.is_some() {
        share::register_all(reg, ctx.clone());
    }
    if ctx.location.is_some() {
        reg.register_builtin(Arc::new(LocationTool::new(ctx.clone())));
    }
    if ctx.device_status.is_some() {
        reg.register_builtin(Arc::new(DeviceStatusTool::new(ctx.clone())));
    }
    if ctx.haptics.is_some() {
        reg.register_builtin(Arc::new(HapticsTool::new(ctx.clone())));
    }
    if ctx.deep_link.is_some() {
        reg.register_builtin(Arc::new(OpenUrlTool::new(ctx.clone())));
    }
    if ctx.calendar.is_some() {
        reg.register_builtin(Arc::new(CalendarTool::new(ctx.clone())));
    }
    if ctx.contacts.is_some() {
        reg.register_builtin(Arc::new(ContactsTool::new(ctx)));
    }
}

#[cfg(test)]
mod device_tools_tests;

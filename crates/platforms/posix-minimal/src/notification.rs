//! Notification sink helper for desktop hosts.
//!
//! There is no `NotificationSink` trait in `lingxi-traits` yet — this module
//! ships a thin helper the cli-demo (and Plan 17's hooks runner) can use to
//! route human-facing notifications to stderr. Once a real trait lands
//! upstream, this becomes the canonical impl.

/// Write a notification line to stderr, prefixed with `[notify]`.
pub fn notify(message: &str) {
    eprintln!("[notify] {message}");
}

//! macOS TCC permission probes (Accessibility + Screen Recording).
//!
//! Parity target: `cu.tcc.checkAccessibility()` / `checkScreenRecording()`
//! (Swift, via a private-ish but Apple-documented pair of public C functions).
//! Both are boolean TCC queries, so a raw `extern "C"` binding
//! is simpler and lighter than pulling in a whole accessibility crate for two
//! functions.
//!
//! `request_access` uses the request variants so macOS can register the app
//! in System Settings. Effects use only read-only queries. The trust
//! result describes authorization; target availability is checked separately
//! by the operation, as in the upstream adapter.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGSessionCopyCurrentDictionary() -> core_foundation::dictionary::CFDictionaryRef;
    /// `AXIsProcessTrustedWithOptions(options)` — `ApplicationServices/
    /// HIServices`. With `kAXTrustedCheckOptionPrompt` set to `true` in
    /// `options`, this prompts (and registers the process in System
    /// Settings → Accessibility) the first time it's ever called for this
    /// process; later calls just report the current status.
    ///
    /// Declared returning `u8`, NOT Rust `bool`: the real header return type
    /// is `Boolean` (`MacTypes.h`'s `typedef unsigned char Boolean`), a
    /// C `unsigned char` — not C99 `_Bool`. Binding it as Rust `bool` would
    /// be UB per `bool`'s validity invariant (only bit patterns `0x00`/`0x01`
    /// are valid) if the ABI-level byte the callee returns is ever anything
    /// else in the unused high bits of the underlying register/byte.
    fn AXIsProcessTrustedWithOptions(options: core_foundation::dictionary::CFDictionaryRef) -> u8;
    fn AXIsProcessTrusted() -> u8;

}

/// Refuse operations when Quartz cannot establish a console session or reports a lock.
pub fn screen_locked() -> Result<bool, lingxi_core::host::computer_control::ComputerError> {
    use core_foundation::{
        base::{CFType, TCFType},
        boolean::CFBoolean,
        dictionary::CFDictionary,
        string::CFString,
    };
    let raw = unsafe { CGSessionCopyCurrentDictionary() };
    if raw.is_null() {
        return Err(
            lingxi_core::host::computer_control::ComputerError::PermissionDenied(
                "No active Quartz session".into(),
            ),
        );
    }
    let session: CFDictionary<CFString, CFType> =
        unsafe { CFDictionary::wrap_under_create_rule(raw) };
    let key = CFString::from_static_string("CGSSessionScreenIsLocked");
    Ok(session
        .find(&key)
        .and_then(|value| value.downcast::<CFBoolean>())
        .is_some_and(bool::from))
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    /// `CGRequestScreenCaptureAccess()` — macOS 11.0+. Prompts (and
    /// registers the process in System Settings → Screen & System Audio
    /// Recording) the first time it's ever called for this process; later
    /// calls just report the current status without re-prompting. Genuinely
    /// a C99 `bool` in the real header, unlike `AXIsProcessTrustedWithOptions`'s
    /// legacy `Boolean` — Rust `bool` is the correct ABI type here.
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGPreflightScreenCaptureAccess() -> bool;

}

/// Read-only TCC state, matching Claude Code's native `tcc` checks.
///
/// AX hit testing is target-dependent, so it belongs to target validation, not
/// global permission detection. In particular, NotImplemented/ CannotComplete
/// from an application must not override an already granted TCC permission.
#[must_use]
pub fn query_os_permissions(
) -> Result<(bool, bool), lingxi_core::host::computer_control::ComputerError> {
    Ok((accessibility_granted(), unsafe {
        CGPreflightScreenCaptureAccess()
    }))
}

/// Read only the current Accessibility grant. Target AX failures use this
/// query to distinguish unavailable target messaging from an actual OS denial.
/// This never requests, registers or prompts for permission.
#[must_use]
pub(super) fn accessibility_granted() -> bool {
    (unsafe { AXIsProcessTrusted() }) != 0
}

/// `(accessibility_granted, screen_recording_granted)`.
#[must_use]
pub fn check_os_permissions() -> Option<(bool, bool)> {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;

    // `{ kAXTrustedCheckOptionPrompt: true }` — the exact one-entry options
    // dictionary `AXIsProcessTrustedWithOptions` documents for "prompt if
    // not yet decided". The key is the literal string `AXTrustedCheckOptionPrompt`
    // (Apple headers define `kAXTrustedCheckOptionPrompt` as that CFString
    // constant), so building it by value here needs no extra linkage.
    let key = CFString::from_static_string("AXTrustedCheckOptionPrompt");
    let value = CFBoolean::true_value();
    let options: CFDictionary<CFType, CFType> =
        CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);

    // Safety: both functions take no arguments beyond a validly-constructed
    // (non-null, well-formed) CFDictionaryRef for the first, return a plain
    // boolean-ish value, and have no other documented preconditions —
    // trivially safe to call from any thread at any time. `options` outlives
    // the call (it's a local binding dropped after this statement, not
    // before).
    let _ = unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) };
    let _ = unsafe { CGRequestScreenCaptureAccess() };
    query_os_permissions().ok()
}

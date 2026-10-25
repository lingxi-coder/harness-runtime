//! macOS TCC permission probes (Accessibility + Screen Recording).
//!
//! Parity target: `cu.tcc.checkAccessibility()` / `checkScreenRecording()`
//! (Swift, via a private-ish but Apple-documented pair of public C functions).
//! Both are simple no-argument boolean queries, so a raw `extern "C"` binding
//! is simpler and lighter than pulling in a whole accessibility crate for two
//! functions.
//!
//! `request_access` uses the request variants so macOS can register the app
//! in System Settings. Effects use only read-only queries. A nominal trust
//! result is not sufficient evidence of working AX messaging, so Accessibility
//! additionally needs a bounded real message before reporting access as available.
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
    fn AXUIElementCreateSystemWide() -> core_foundation::base::CFTypeRef;
    fn AXUIElementSetMessagingTimeout(
        element: core_foundation::base::CFTypeRef,
        seconds: f32,
    ) -> i32;
    fn AXUIElementCopyAttributeValue(
        element: core_foundation::base::CFTypeRef,
        attribute: core_foundation::string::CFStringRef,
        value: *mut core_foundation::base::CFTypeRef,
    ) -> i32;
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

/// Read-only live TCC queries. Never registers/prompts for access during effects.
#[must_use]
pub fn query_os_permissions(
) -> Result<(bool, bool), lingxi_core::host::computer_control::ComputerError> {
    Ok((query_accessibility()?, unsafe {
        CGPreflightScreenCaptureAccess()
    }))
}

fn query_unavailable(
    stage: &str,
    status: Option<i32>,
    has_value: Option<bool>,
) -> lingxi_core::host::computer_control::ComputerError {
    // Status and presence are safe diagnostic facts: never include the AX value
    // itself, window titles, application content or any user-controlled string.
    lingxi_core::host::computer_control::ComputerError::PermissionDenied(format!(
        "Could not verify macOS Accessibility access. Retry when the accessibility service is available. (AX probe stage={stage}, status={status:?}, hasValue={has_value:?})"
    ))
}

fn accessibility_query_result(
    trusted: bool,
    status: i32,
    has_value: bool,
) -> Result<bool, lingxi_core::host::computer_control::ComputerError> {
    if !trusted || status == -25211 {
        return Ok(false);
    }
    match (status, has_value) {
        (0, true) | (-25212, _) => Ok(true), // No focused application is not a TCC revocation.
        _ => Err(query_unavailable(
            "focused_application",
            Some(status),
            Some(has_value),
        )),
    }
}

fn query_accessibility() -> Result<bool, lingxi_core::host::computer_control::ComputerError> {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::string::CFString;

    let trusted = unsafe { AXIsProcessTrusted() } != 0;
    if !trusted {
        return Ok(false);
    }
    let raw = unsafe { AXUIElementCreateSystemWide() };
    if raw.is_null() {
        return Err(query_unavailable("create_systemwide", None, Some(false)));
    }
    let system = unsafe { CFType::wrap_under_create_rule(raw) };
    let timeout_status =
        unsafe { AXUIElementSetMessagingTimeout(system.as_concrete_TypeRef(), 0.25) };
    if timeout_status != 0 {
        return Err(query_unavailable("set_timeout", Some(timeout_status), None));
    }
    let attribute = CFString::from_static_string("AXFocusedApplication");
    let mut value = std::ptr::null();
    let status = unsafe {
        AXUIElementCopyAttributeValue(
            system.as_concrete_TypeRef(),
            attribute.as_concrete_TypeRef(),
            &mut value,
        )
    };
    let has_value = !value.is_null();
    if has_value {
        // Copy ownership must be released even on an unexpected error/value pair.
        let _value = unsafe { CFType::wrap_under_create_rule(value) };
    }
    // In particular, kAXErrorAPIDisabled (-25211) overrides a nominal trust=true result.
    // Busy/unknown failures block only this operation; they must not latch a
    // fabricated permission revocation or tell the user to reauthorize.
    accessibility_query_result(trusted, status, has_value)
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

#[cfg(test)]
mod tests {
    use super::{accessibility_query_result, query_unavailable};

    #[test]
    fn cached_trust_cannot_override_disabled_or_unavailable_ax_messaging() {
        assert!(accessibility_query_result(true, 0, true).unwrap());
        assert!(!accessibility_query_result(false, 0, true).unwrap());
        assert!(!accessibility_query_result(true, -25211, false).unwrap());
        assert!(!accessibility_query_result(true, -25211, true).unwrap());
        assert!(accessibility_query_result(true, -25212, false).unwrap());
        assert!(accessibility_query_result(true, 0, false).is_err());
        for status in [-25204, -1, 1] {
            assert!(
                accessibility_query_result(true, status, true).is_err(),
                "AX status {status}"
            );
            assert!(
                accessibility_query_result(true, status, false).is_err(),
                "AX status {status}"
            );
        }
    }

    #[test]
    fn unavailable_queries_report_only_the_bounded_probe_stage_and_raw_status() {
        let error = accessibility_query_result(true, -25204, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("stage=focused_application"));
        assert!(error.contains("status=Some(-25204)"));
        assert!(error.contains("hasValue=Some(false)"));
        let error = query_unavailable("set_timeout", Some(-25202), None).to_string();
        assert!(error.contains("stage=set_timeout"));
        assert!(error.contains("status=Some(-25202)"));
        assert!(error.contains("hasValue=None"));
        let error = query_unavailable("create_systemwide", None, Some(false)).to_string();
        assert!(error.contains("stage=create_systemwide"));
        assert!(error.contains("status=None"));
        assert!(error.contains("hasValue=Some(false)"));
    }
}

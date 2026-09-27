//! macOS TCC permission probes (Accessibility + Screen Recording).
//!
//! Parity target: `cu.tcc.checkAccessibility()` / `checkScreenRecording()`
//! (Swift, via a private-ish but Apple-documented pair of public C functions).
//! Both are simple no-argument boolean queries, so a raw `extern "C"` binding
//! is simpler and lighter than pulling in a whole accessibility crate for two
//! functions.
//!
//! Both bindings use the REQUEST variant, not the read-only preflight/query
//! variant, even though this function is conceptually a "check" — found via
//! live interactive testing of the `request_access` TCC panel: with the
//! read-only `CGPreflightScreenCaptureAccess()` this crate originally used,
//! nothing ever actually asks the OS for Screen Recording access, so macOS
//! never adds this process to System Settings → Screen & System Audio
//! Recording — the panel's "Open System Settings → Screen Recording" row
//! sends the user to a list with no entry to toggle on at all, a dead end.
//! Both `AXIsProcessTrustedWithOptions` (with the prompt option) and
//! `CGRequestScreenCaptureAccess` are documented by Apple as idempotent
//! after the first decision: once the user has answered (via the native
//! prompt this triggers, or a manual System Settings toggle), later calls
//! just return the current status without re-prompting — so it is safe to
//! call this on every single `request_access` invocation, not just once.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
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
}

/// `(accessibility_granted, screen_recording_granted)`.
#[must_use]
pub fn check_os_permissions() -> (bool, bool) {
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
    let accessibility =
        unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) } != 0;
    let screen_recording = unsafe { CGRequestScreenCaptureAccess() };
    (accessibility, screen_recording)
}

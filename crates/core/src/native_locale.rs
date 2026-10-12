//! Narrow, read-only access to the active native locale.

/// Return the process's current `LC_CTYPE` locale on non-Apple Unix.
///
/// This only queries the locale already selected by the host process. It does
/// not read locale environment variables or change the process locale. Other
/// platforms return `None` so callers can use their platform locale provider.
#[cfg(all(unix, not(target_vendor = "apple"), not(target_os = "android")))]
#[allow(unsafe_code)]
pub fn current_lc_ctype() -> Option<String> {
    // SAFETY: A null locale argument asks libc to return the current value
    // without changing it. The returned NUL-terminated string is copied into
    // an owned Rust string immediately and the pointer is not retained.
    let locale = unsafe { libc::setlocale(libc::LC_CTYPE, std::ptr::null()) };
    if locale.is_null() {
        return None;
    }

    // SAFETY: A successful setlocale query returns a pointer to a
    // NUL-terminated locale name. It is read immediately and copied.
    Some(
        unsafe { std::ffi::CStr::from_ptr(locale) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// Return the process's current `LC_CTYPE` locale on non-Apple Unix.
///
/// This only queries the locale already selected by the host process. It does
/// not read locale environment variables or change the process locale. Other
/// platforms return `None` so callers can use their platform locale provider.
#[cfg(not(all(unix, not(target_vendor = "apple"), not(target_os = "android"))))]
pub fn current_lc_ctype() -> Option<String> {
    None
}

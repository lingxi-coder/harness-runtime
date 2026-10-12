//! Read-only access to the active Unix locale selected for the C locale APIs.

/// Copy the active `LC_CTYPE` locale identifier without changing process locale.
///
/// Native's Unix `platformUserPreferredLanguages` path queries this category
/// with `setlocale(LC_CTYPE, nullptr)`. Copying the returned identifier here
/// lets callers perform the same mapping without exporting a libc pointer.
#[cfg(all(unix, not(target_vendor = "apple"), not(target_os = "android")))]
#[allow(unsafe_code)]
pub fn current_lc_ctype() -> Option<String> {
    use std::ffi::CStr;

    // SAFETY: POSIX specifies a null locale argument as a query of the current
    // category; this call does not change the process locale.
    let value = unsafe { libc::setlocale(libc::LC_CTYPE, std::ptr::null()) };
    if value.is_null() {
        return None;
    }

    // SAFETY: a non-null setlocale result is a NUL-terminated locale name. Copy
    // it immediately so no pointer into libc's storage escapes this function.
    let bytes = unsafe { CStr::from_ptr(value) }.to_bytes();
    Some(String::from_utf8_lossy(bytes).into_owned())
}

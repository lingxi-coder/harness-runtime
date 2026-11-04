//! Minimum supported Rust version: 1.28

use std::num::NonZeroUsize;

#[cfg_attr(any(target_os = "linux", target_os = "android"), path = "linux.rs")]
#[cfg_attr(target_os = "freebsd", path = "freebsd.rs")]
#[cfg_attr(
    all(
        any(target_os = "macos", target_os = "ios"),
        target_pointer_width = "64"
    ),
    path = "apple.rs"
)]
#[cfg_attr(target_os = "aix", path = "aix.rs")]
mod imp;

/// Obtain the number of threads currently part of the active process. Returns `None` if the number
/// of threads cannot be determined.
///
/// On Apple platforms, a translated process or a process whose translation status cannot be
/// determined returns `None` instead of estimating the number of application threads.
pub fn num_threads() -> Option<NonZeroUsize> {
    imp::num_threads()
}

/// Determine if the current process is single-threaded. Returns `None` if the number of threads
/// cannot be determined.
pub fn is_single_threaded() -> Option<bool> {
    is_single_threaded_from_count(num_threads())
}

fn is_single_threaded_from_count(count: Option<NonZeroUsize>) -> Option<bool> {
    count.map(|n| n.get() == 1)
}

#[cfg(test)]
mod tests {
    use super::is_single_threaded_from_count;
    use std::num::NonZeroUsize;

    #[test]
    fn indeterminate_count_does_not_claim_single_threaded() {
        assert_eq!(is_single_threaded_from_count(None), None);
        assert_eq!(
            is_single_threaded_from_count(NonZeroUsize::new(1)),
            Some(true)
        );
        assert_eq!(
            is_single_threaded_from_count(NonZeroUsize::new(2)),
            Some(false)
        );
    }
}

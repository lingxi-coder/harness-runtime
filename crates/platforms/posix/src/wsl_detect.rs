//! WSL kernel detection.
//!
//! claude-code's sandbox runtime supports WSL2 but refuses WSL1.
//! Detection reads `/proc/version`:
//! - `microsoft-standard` substring (lowercase) OR explicit `WSL2` marker →
//!   `WslKind::WslTwo`. claude-code treats both as WSL2.
//! - Capital-`M` `Microsoft` substring without the above → `WslKind::WslOne`.
//! - Neither → `WslKind::NotWsl` (pure Linux).
//!
//! Live read happens in [`detect`]; the inner parser ([`parse_wsl_kind`]) is
//! exposed for unit testing with synthetic `/proc/version` payloads.

/// Result of WSL-kind inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WslKind {
    /// Host is WSL2 — bwrap sandbox is supported.
    WslTwo,
    /// Host is WSL1 — sandbox is refused.
    WslOne,
    /// Host is not WSL (regular Linux or non-Linux OS).
    NotWsl,
}

/// Detect WSL kind by reading `/proc/version`.
///
/// On non-Linux hosts (or where `/proc/version` is unreadable), returns
/// [`WslKind::NotWsl`]. This is the conservative default — callers running on
/// macOS / Windows / Linux without `/proc` correctly treat themselves as
/// not-WSL.
#[must_use]
pub fn detect() -> WslKind {
    match std::fs::read_to_string("/proc/version") {
        Ok(contents) => parse_wsl_kind(&contents),
        Err(_) => WslKind::NotWsl,
    }
}

/// Pure-function inner parser exposed for unit testing.
#[must_use]
pub fn parse_wsl_kind(proc_version: &str) -> WslKind {
    let has_wsl2_marker =
        proc_version.contains("microsoft-standard") || proc_version.contains("WSL2");
    if has_wsl2_marker {
        return WslKind::WslTwo;
    }
    if proc_version.contains("Microsoft") {
        return WslKind::WslOne;
    }
    WslKind::NotWsl
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microsoft_standard_lowercase_is_wsl2() {
        let s = "Linux version 5.15.90.1-microsoft-standard-WSL2 (oe-user@oe-host)";
        assert_eq!(parse_wsl_kind(s), WslKind::WslTwo);
    }

    #[test]
    fn explicit_wsl2_marker_is_wsl2() {
        let s = "Linux version 4.19.128-microsoft-standard #1 SMP Tue WSL2";
        assert_eq!(parse_wsl_kind(s), WslKind::WslTwo);
    }

    #[test]
    fn capital_microsoft_only_is_wsl1() {
        // WSL1 kernels report as "Linux version 4.4.0-19041-Microsoft" without
        // any of the WSL2 markers.
        let s = "Linux version 4.4.0-19041-Microsoft (Microsoft@Microsoft.com)";
        assert_eq!(parse_wsl_kind(s), WslKind::WslOne);
    }

    #[test]
    fn pure_linux_is_none() {
        let s = "Linux version 6.5.0-1015-aws (buildd@lcy02-amd64-002)";
        assert_eq!(parse_wsl_kind(s), WslKind::NotWsl);
    }

    #[test]
    fn empty_string_is_none() {
        assert_eq!(parse_wsl_kind(""), WslKind::NotWsl);
    }
}

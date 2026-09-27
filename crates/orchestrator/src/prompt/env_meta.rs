//! Pure `<env>`-block metadata lookups for the production system-prompt
//! builder: marketing name + knowledge cutoff per model id, and the
//! `uname -sr` OS-version string.
//!
//! Ported 1:1 from claude-code:
//! - `getMarketingNameForModel` (`utils/model/model.ts:570-614`)
//! - `getKnowledgeCutoff` (`constants/prompts.ts:712-730`)
//! - `getUnameSR` (`constants/prompts.ts:745-756`)
//!
//! The [`env_block`](crate::prompt::env_block) formatter is unchanged; it
//! consumes whatever values the builder places in
//! [`SystemPromptContext`](crate::prompt::SystemPromptContext). Previously
//! the builder stubbed `model_marketing_name`/`knowledge_cutoff` to `None`
//! and `os_version` to `"<os> <arch>"`; these helpers feed the real values.
#![forbid(unsafe_code)]

use std::sync::OnceLock;

const WINDOWS_NT: &str = "Windows_NT";
const WINDOWS_POWERSHELL_EXE: &str = "powershell.exe";
const WINDOWS_CMD_EXE: &str = "cmd.exe";

/// Marketing name for a model id, e.g. `claude-opus-4-6` -> `Opus 4.6`.
///
/// Mirrors `getMarketingNameForModel`. The TS resolves the id to a canonical
/// short name first (`getCanonicalName`, which unwraps Bedrock/Vertex ARNs);
/// without that provider-resolution layer we substring-match the lowercased
/// id directly, which is equivalent for first-party ids. The `[1m]` 1M-context
/// suffix is detected on the lowercased id (TS `modelId.toLowerCase()`).
///
/// Order is significant — every later needle is a substring of an earlier one,
/// so the most specific suffix must be checked first. Returns `None` for an
/// unknown model (TS falls back to `You are powered by the model {id}.`).
#[must_use]
pub fn marketing_name_for_model(model_id: &str) -> Option<&'static str> {
    let original = model_id.to_ascii_lowercase();
    let canonical = platform_api::model_capabilities::normalize_model_id(model_id);
    let has_1m = original.contains("[1m]");

    if canonical == "claude-fable-5-1" {
        return Some("Fable 5.1");
    }
    if canonical == "claude-mythos-5-1" {
        return Some("Mythos 5.1");
    }
    if canonical == "claude-opus-5" {
        return Some(if has_1m {
            "Opus 5 (1M context)"
        } else {
            "Opus 5"
        });
    }
    if canonical == "claude-opus-4-8" {
        return Some(if has_1m {
            "Opus 4.8 (1M context)"
        } else {
            "Opus 4.8"
        });
    }
    if canonical == "claude-opus-4-7" {
        return Some(if has_1m {
            "Opus 4.7 (1M context)"
        } else {
            "Opus 4.7"
        });
    }
    if canonical == "claude-opus-4-6" {
        return Some(if has_1m {
            "Opus 4.6 (1M context)"
        } else {
            "Opus 4.6"
        });
    }
    if canonical.starts_with("claude-opus-4-5") {
        return Some("Opus 4.5");
    }
    if canonical.starts_with("claude-opus-4-1") {
        return Some("Opus 4.1");
    }
    if canonical == "claude-opus-4" || canonical.starts_with("claude-opus-4-") {
        return Some("Opus 4");
    }
    // sonnet-5 before the sonnet-4-x arms (2.1.198 registry display_name
    // "Sonnet 5"; mutually exclusive substrings).
    if canonical == "claude-sonnet-5" {
        return Some(if has_1m {
            "Sonnet 5 (1M context)"
        } else {
            "Sonnet 5"
        });
    }
    if canonical == "claude-sonnet-4-6" {
        return Some(if has_1m {
            "Sonnet 4.6 (1M context)"
        } else {
            "Sonnet 4.6"
        });
    }
    if canonical.starts_with("claude-sonnet-4-5") {
        return Some(if has_1m {
            "Sonnet 4.5 (1M context)"
        } else {
            "Sonnet 4.5"
        });
    }
    if canonical == "claude-sonnet-4" || canonical.starts_with("claude-sonnet-4-") {
        return Some(if has_1m {
            "Sonnet 4 (1M context)"
        } else {
            "Sonnet 4"
        });
    }
    if canonical.starts_with("claude-3-7-sonnet") {
        return Some("Claude 3.7 Sonnet");
    }
    if canonical.starts_with("claude-3-5-sonnet") {
        return Some("Claude 3.5 Sonnet");
    }
    if canonical.starts_with("claude-haiku-4-5") {
        return Some("Haiku 4.5");
    }
    if canonical.starts_with("claude-3-5-haiku") {
        return Some("Claude 3.5 Haiku");
    }
    None
}

/// Knowledge-cutoff string for a model id, e.g. `claude-opus-4-6` ->
/// `May 2025`. Mirrors `getKnowledgeCutoff`. `None` for unknown models (TS
/// omits the `Assistant knowledge cutoff is ...` sentence entirely).
#[must_use]
pub fn knowledge_cutoff_for_model(model_id: &str) -> Option<&'static str> {
    let canonical = platform_api::model_capabilities::normalize_model_id(model_id);
    if canonical == "claude-opus-5" {
        Some("May 2026")
    } else if canonical == "claude-fable-5-1" || canonical == "claude-mythos-5-1" {
        Some("June 2026")
    } else if canonical == "claude-sonnet-5" {
        Some("January 2026")
    } else if canonical == "claude-opus-4-8" || canonical == "claude-opus-4-7" {
        Some("January 2026")
    } else if canonical == "claude-sonnet-4-6" {
        Some("August 2025")
    } else if canonical == "claude-opus-4-6" || canonical.starts_with("claude-opus-4-5") {
        Some("May 2025")
    } else if canonical.starts_with("claude-haiku-4") {
        Some("February 2025")
    } else if canonical == "claude-opus-4"
        || canonical.starts_with("claude-opus-4-")
        || canonical == "claude-sonnet-4"
        || canonical.starts_with("claude-sonnet-4-")
    {
        Some("January 2025")
    } else {
        None
    }
}

fn collapse_posix_shell(raw: &str) -> String {
    if raw.contains("zsh") {
        "zsh".into()
    } else if raw.contains("bash") {
        "bash".into()
    } else {
        raw.to_string()
    }
}

fn basename_or_raw(raw: &str) -> String {
    raw.rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(raw)
        .to_string()
}

pub(crate) fn detect_shell_with(
    is_windows: bool,
    shell_env: Option<&str>,
    comspec_env: Option<&str>,
    prefer_powershell: bool,
) -> String {
    if is_windows {
        if prefer_powershell {
            return WINDOWS_POWERSHELL_EXE.into();
        }
        return comspec_env
            .filter(|s| !s.is_empty())
            .map(basename_or_raw)
            .unwrap_or_else(|| WINDOWS_CMD_EXE.into());
    }

    collapse_posix_shell(shell_env.filter(|s| !s.is_empty()).unwrap_or("unknown"))
}

#[must_use]
/// Environment-block shell string. On Windows, Claude prefers `powershell.exe`
/// over the often-empty `$SHELL`; elsewhere it collapses `$SHELL` to
/// `zsh`/`bash` by substring and otherwise returns the raw value.
pub fn detect_shell() -> String {
    detect_shell_with(
        cfg!(windows),
        std::env::var("SHELL").ok().as_deref(),
        std::env::var("ComSpec")
            .ok()
            .or_else(|| std::env::var("COMSPEC").ok())
            .as_deref(),
        cfg!(windows),
    )
}

fn parse_uname_sr(stdout: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn parse_windows_ver_release(output: &str) -> Option<String> {
    let start = output.find(|c: char| c.is_ascii_digit())?;
    let version = output[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>();
    let mut parts = version.split('.').filter(|part| !part.is_empty());
    let major = parts.next()?;
    let minor = parts.next()?;
    let build = parts.next()?;
    Some(format!("{major}.{minor}.{build}"))
}

pub(crate) fn os_version_string_with<F, G>(
    is_windows: bool,
    mut uname_sr: F,
    mut windows_ver: G,
    fallback_os: &str,
    fallback_arch: &str,
) -> String
where
    F: FnMut() -> Option<String>,
    G: FnMut() -> Option<String>,
{
    if is_windows {
        if let Some(release) = windows_ver().and_then(|s| parse_windows_ver_release(&s)) {
            return format!("{WINDOWS_NT} {release}");
        }
        return format!("{WINDOWS_NT} {fallback_arch}");
    }

    if let Some(uname) = uname_sr() {
        return uname;
    }
    format!("{fallback_os} {fallback_arch}")
}

/// `uname -sr`-style OS version string, e.g. `Darwin 25.3.0` / `Linux 6.6.4`.
///
/// Mirrors `getUnameSR`: on POSIX, `os.type()` + `os.release()` is byte-equal
/// to `uname -s -r`, so we shell out to that (one space-joined line) and trim.
/// On Windows, Claude reports `Windows_NT ${os.release()}`; `cmd /C ver` exposes
/// the release number without adding a new dependency. Any probe failure falls
/// back to a non-empty stub.
#[must_use]
pub fn os_version_string() -> String {
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            os_version_string_with(
                cfg!(windows),
                || {
                    if let Ok(out) = std::process::Command::new("uname")
                        .arg("-s")
                        .arg("-r")
                        .output()
                    {
                        if out.status.success() {
                            return parse_uname_sr(&out.stdout);
                        }
                    }
                    None
                },
                || {
                    if let Ok(out) = std::process::Command::new("cmd")
                        .args(["/C", "ver"])
                        .output()
                    {
                        if out.status.success() {
                            return Some(String::from_utf8_lossy(&out.stdout).trim().to_string());
                        }
                    }
                    None
                },
                std::env::consts::OS,
                std::env::consts::ARCH,
            )
        })
        .clone()
}

/// Local-date string in `YYYY-MM-DD` form, e.g. `2026-06-20`.
///
/// 1:1 with claude-code `Rtt` (binary offset ~197026183), which builds
/// `${getFullYear()}-${getMonth()+1 padStart2}-${getDate() padStart2}` from a
/// `new Date()` — i.e. the LOCAL date (not UTC). This is the `${date}` body of
/// the `currentDate` additional-context entry (`WNi`: `Today's date is
/// ${date}.`).
#[must_use]
pub fn current_date_string() -> String {
    use chrono::Datelike;
    let now = chrono::Local::now();
    format!("{:04}-{:02}-{:02}", now.year(), now.month(), now.day())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_date_is_iso_local() {
        let d = current_date_string();
        // YYYY-MM-DD: 10 chars, dashes at 4 and 7.
        assert_eq!(d.len(), 10, "got {d}");
        assert_eq!(&d[4..5], "-");
        assert_eq!(&d[7..8], "-");
        assert!(d[..4].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn marketing_names_match_ts_map() {
        assert_eq!(
            marketing_name_for_model("claude-fable-5-1"),
            Some("Fable 5.1")
        );
        assert_eq!(
            marketing_name_for_model("claude-mythos-5-1"),
            Some("Mythos 5.1")
        );
        assert_eq!(
            marketing_name_for_model("claude-opus-4-8"),
            Some("Opus 4.8")
        );
        assert_eq!(
            marketing_name_for_model("claude-opus-4-8-20260101[1m]"),
            Some("Opus 4.8 (1M context)")
        );
        assert_eq!(
            marketing_name_for_model("claude-opus-4-7"),
            Some("Opus 4.7")
        );
        assert_eq!(
            marketing_name_for_model("claude-opus-4-7-20251101[1m]"),
            Some("Opus 4.7 (1M context)")
        );
        assert_eq!(
            marketing_name_for_model("claude-opus-4-6"),
            Some("Opus 4.6")
        );
        assert_eq!(
            marketing_name_for_model("claude-opus-4-6-20251101[1m]"),
            Some("Opus 4.6 (1M context)")
        );
        assert_eq!(
            marketing_name_for_model("claude-opus-4-5"),
            Some("Opus 4.5")
        );
        assert_eq!(
            marketing_name_for_model("claude-opus-4-1"),
            Some("Opus 4.1")
        );
        assert_eq!(marketing_name_for_model("claude-opus-4-0"), Some("Opus 4"));
        assert_eq!(
            marketing_name_for_model("claude-sonnet-4-5[1m]"),
            Some("Sonnet 4.5 (1M context)")
        );
        assert_eq!(
            marketing_name_for_model("claude-sonnet-4-5"),
            Some("Sonnet 4.5")
        );
        assert_eq!(
            marketing_name_for_model("claude-sonnet-5"),
            Some("Sonnet 5")
        );
        assert_eq!(
            marketing_name_for_model("claude-sonnet-5[1m]"),
            Some("Sonnet 5 (1M context)")
        );
        assert_eq!(
            marketing_name_for_model("claude-haiku-4-5"),
            Some("Haiku 4.5")
        );
        assert_eq!(
            marketing_name_for_model("claude-3-7-sonnet"),
            Some("Claude 3.7 Sonnet")
        );
        // Unknown / unmapped -> None (TS bare-id fallback).
        assert_eq!(marketing_name_for_model("gpt-4o"), None);
    }

    #[test]
    fn knowledge_cutoffs_match_ts_map() {
        assert_eq!(
            knowledge_cutoff_for_model("claude-fable-5-1"),
            Some("June 2026")
        );
        assert_eq!(
            knowledge_cutoff_for_model("claude-mythos-5-1"),
            Some("June 2026")
        );
        assert_eq!(
            knowledge_cutoff_for_model("claude-opus-4-8"),
            Some("January 2026")
        );
        assert_eq!(
            knowledge_cutoff_for_model("claude-opus-4-7"),
            Some("January 2026")
        );
        assert_eq!(
            knowledge_cutoff_for_model("claude-sonnet-5"),
            Some("January 2026")
        );
        assert_eq!(
            knowledge_cutoff_for_model("claude-sonnet-4-6"),
            Some("August 2025")
        );
        assert_eq!(
            knowledge_cutoff_for_model("claude-opus-4-6"),
            Some("May 2025")
        );
        assert_eq!(
            knowledge_cutoff_for_model("claude-opus-4-5"),
            Some("May 2025")
        );
        assert_eq!(
            knowledge_cutoff_for_model("claude-haiku-4-5"),
            Some("February 2025")
        );
        assert_eq!(
            knowledge_cutoff_for_model("claude-opus-4-1"),
            Some("January 2025")
        );
        assert_eq!(
            knowledge_cutoff_for_model("claude-sonnet-4-0"),
            Some("January 2025")
        );
        assert_eq!(
            knowledge_cutoff_for_model("claude-sonnet-4-5"),
            Some("January 2025")
        );
        assert_eq!(knowledge_cutoff_for_model("gpt-4o"), None);
    }

    #[test]
    fn os_version_is_populated() {
        // Shells out to `uname` on this POSIX host; never empty (fallback
        // guarantees a value even on spawn failure).
        assert!(!os_version_string().is_empty());
    }

    #[test]
    fn detect_shell_prefers_powershell_on_windows() {
        let got = detect_shell_with(true, None, Some(r"C:\Windows\System32\cmd.exe"), true);
        assert_eq!(got, "powershell.exe");
    }

    #[test]
    fn detect_shell_uses_comspec_basename_without_powershell() {
        let got = detect_shell_with(true, None, Some(r"C:\Windows\System32\cmd.exe"), false);
        assert_eq!(got, "cmd.exe");
    }

    #[test]
    fn detect_shell_collapses_posix_shell_names() {
        assert_eq!(
            detect_shell_with(false, Some("/bin/zsh"), None, false),
            "zsh"
        );
        assert_eq!(
            detect_shell_with(false, Some("/usr/local/bin/bash"), None, false),
            "bash"
        );
        assert_eq!(
            detect_shell_with(false, Some("/opt/fish/bin/fish"), None, false),
            "/opt/fish/bin/fish"
        );
    }

    #[test]
    fn windows_os_version_uses_windows_nt_release() {
        let got = os_version_string_with(
            true,
            || Some("ignored".into()),
            || Some("Microsoft Windows [Version 10.0.22631.5335]".into()),
            "windows",
            "x86_64",
        );
        assert_eq!(got, "Windows_NT 10.0.22631");
    }

    #[test]
    fn windows_os_version_falls_back_when_ver_is_unparseable() {
        let got = os_version_string_with(
            true,
            || None,
            || Some("garbled".into()),
            "windows",
            "x86_64",
        );
        assert_eq!(got, "Windows_NT x86_64");
    }
}

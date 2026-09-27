//! Shell discovery shared by shell and task tools without a sibling-tool dependency.

/// Linux/WSL shell path.
pub const BASH_SHELL_LINUX: &str = "/bin/bash";
/// macOS shell path.
pub const BASH_SHELL_MACOS: &str = "/bin/zsh";

static DISCOVERY_LOGGER: std::sync::OnceLock<fn(bool, &str)> = std::sync::OnceLock::new();

/// Install host diagnostic forwarding once. `true` means warning, `false` info.
/// Logging stays outside this API crate's dependency graph.
pub fn set_shell_discovery_logger(logger: fn(bool, &str)) {
    let _ = DISCOVERY_LOGGER.set(logger);
}

fn discovery_log(warning: bool, message: &str) {
    if let Some(logger) = DISCOVERY_LOGGER.get() {
        logger(warning, message);
    }
}

/// Resolve the shell binary to spawn under.
///
/// Mirrors `findSuitableShell()` in `src/utils/Shell.ts`: if
/// `LINGXI_SHELL` is set to a non-empty value that contains `"bash"` or
/// `"zsh"`, return it verbatim (no executable-check — that matches the TS
/// behaviour which only validates that the path exists/is-executable, not
/// that it runs successfully). Fall back to the compile-time OS default when
/// the env var is absent, empty, or names an unsupported shell.
///
/// The return value is either the env-var string (leaked to `'static` so the
/// signature stays `&'static str`) or a compile-time constant. Unique env
/// values are leaked at most once (tests may mutate `LINGXI_SHELL`).
#[must_use]
pub fn resolve_shell_path() -> &'static str {
    static CACHE: std::sync::Mutex<Option<(String, &'static str)>> = std::sync::Mutex::new(None);
    let env_key = std::env::var("LINGXI_SHELL").unwrap_or_default();
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((k, v)) = cache.as_ref() {
        if k == &env_key {
            return v;
        }
    }
    let resolved = resolve_shell_path_uncached(&env_key);
    *cache = Some((env_key, resolved));
    resolved
}

fn resolve_shell_path_uncached(env_key: &str) -> &'static str {
    if !env_key.is_empty() && (env_key.contains("bash") || env_key.contains("zsh")) {
        return Box::leak(env_key.to_string().into_boxed_str());
    }
    // Windows: Git Bash discovery (cc 2.1.219 `MQ`/`P6n`) — env override with
    // validation, then Program Files probes, then git-on-PATH. Falls through to
    // the compile-time default when nothing resolves (the `P6n` "Git Bash not
    // found" case; the unavailable line is logged inside `git_bash_path`).
    if cfg!(windows) {
        if let Some(p) = git_bash_path() {
            return p;
        }
    }
    if cfg!(target_os = "macos") {
        BASH_SHELL_MACOS
    } else {
        BASH_SHELL_LINUX
    }
}

// ===== BASH.GITBASH — Windows Git Bash resolution (cc 2.1.219 `MQ`/`P6n`) ===

/// Verdict on a `CLAUDE_CODE_GIT_BASH_PATH` override (cc 2.1.219 `MQ` head).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitBashOverride {
    /// Basename is a bash/sh binary AND the file exists — use it verbatim.
    Valid,
    /// Basename is acceptable but the file does not exist.
    NotFound,
    /// Basename is not `bash.exe`/`sh.exe`/`bash`/`sh` (existence is NOT
    /// probed — the oracle short-circuits `o && e(v)` before the filesystem).
    NotBashBinary,
}

/// Classify an override path: `basename(v).toLowerCase()` must be in
/// `["bash.exe","sh.exe","bash","sh"]`, and only then is existence probed.
/// Pure (existence injected) so the matrix is unit-testable on every OS.
pub fn classify_git_bash_override(path: &str, exists: &dyn Fn(&str) -> bool) -> GitBashOverride {
    // Node `path.basename` on win32 splits on both separators.
    let basename = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    if !matches!(basename.as_str(), "bash.exe" | "sh.exe" | "bash" | "sh") {
        return GitBashOverride::NotBashBinary;
    }
    if exists(path) {
        GitBashOverride::Valid
    } else {
        GitBashOverride::NotFound
    }
}

/// The byte-exact `MQ` rejection warning:
/// `` CLAUDE_CODE_GIT_BASH_PATH "{v}" {not found|is not a bash/sh binary}; falling back to auto-detection ``.
/// `var` is the env spelling that supplied the value (`LINGXI_GIT_BASH_PATH`
/// is accepted as the rebrand twin; the `CLAUDE_CODE_` spelling reproduces the
/// oracle bytes).
#[must_use]
pub fn git_bash_override_warning(var: &str, value: &str, verdict: GitBashOverride) -> String {
    let reason = match verdict {
        // `${o?"not found":"is not a bash/sh binary"}` — o = basename valid,
        // so reaching the warning with a valid basename means the probe failed.
        GitBashOverride::NotFound => "not found",
        _ => "is not a bash/sh binary",
    };
    format!("{var} \"{value}\" {reason}; falling back to auto-detection")
}

/// Resolve the Git Bash binary (cc 2.1.219 `MQ` body, dependency-injected):
/// validated env override first (invalid → warn + auto-detect), then the two
/// Program Files installs, then git-on-PATH `join(git, "..","..","bin",
/// "bash.exe")`.
pub fn resolve_git_bash_path_with(
    env_override: Option<(&str, &str)>,
    exists: &dyn Fn(&str) -> bool,
    which_git: &dyn Fn() -> Option<std::path::PathBuf>,
) -> Option<String> {
    if let Some((var, value)) = env_override {
        match classify_git_bash_override(value, exists) {
            GitBashOverride::Valid => return Some(value.to_string()),
            verdict => {
                discovery_log(true, &git_bash_override_warning(var, value, verdict));
            }
        }
    }
    for candidate in [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files (x86)\Git\bin\bash.exe",
    ] {
        if exists(candidate) {
            return Some(candidate.to_string());
        }
    }
    if let Some(git) = which_git() {
        // `WMe.join(git, "..", "..", "bin", "bash.exe")` — git.exe lives in
        // `Git\cmd\` (or `Git\bin\`), so two `..` from the FILE path land on
        // the install root.
        let candidate = git_bash_beside_git(&git.to_string_lossy());
        if exists(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// `WMe.join(git, "..", "..", "bin", "bash.exe")` where `WMe` is
/// `R(require("path/win32"))` (@226607353) — Node's `path.win32.join`
/// NORMALIZES, so the two `..` are collapsed and
/// `C:\Custom\Git\cmd\git.exe` resolves to `C:\Custom\Git\bin\bash.exe`.
///
/// Deliberately string-level rather than `PathBuf::join`, which appends `..`
/// verbatim: the result is not just probed, it is what `resolve_shell_path`
/// returns, what `P6n` (@226606409) exports as `SHELL` to every child, and
/// what the `Using bash path: "…"` line prints. Off Windows `std::path` also
/// sees a backslash path as a SINGLE component, so it has nothing to pop.
#[doc(hidden)]
pub fn git_bash_beside_git(git: &str) -> String {
    let is_sep = |c: char| c == '\\' || c == '/';
    let root_len = win32_root_len(git);
    let (root, rest) = git.split_at(root_len);
    let rooted = root.ends_with(['\\', '/']);

    let mut comps: Vec<&str> = rest
        .split(is_sep)
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    // The two `..`. A rooted path swallows an over-pop at its root; a relative
    // one keeps the leftovers as leading `..` (Node's `normalizeString`).
    let mut deficit = 0;
    for _ in 0..2 {
        if comps.pop().is_none() {
            deficit += 1;
        }
    }

    let mut parts: Vec<&str> = Vec::new();
    if !rooted {
        parts.extend(std::iter::repeat_n("..", deficit));
    }
    parts.extend(comps);
    parts.push("bin");
    parts.push("bash.exe");
    // `path/win32` renders every separator as a backslash.
    format!("{}{}", root.replace('/', "\\"), parts.join("\\"))
}

/// Length of the win32 root prefix that `..` may not climb past: `\\` (UNC or
/// `\\?\`), a drive spec (`C:` / `C:\`), or a bare leading separator.
fn win32_root_len(path: &str) -> usize {
    let b = path.as_bytes();
    let sep = |c: u8| c == b'\\' || c == b'/';
    if b.len() >= 2 && sep(b[0]) && sep(b[1]) {
        return 2;
    }
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return if b.len() >= 3 && sep(b[2]) { 3 } else { 2 };
    }
    usize::from(!b.is_empty() && sep(b[0]))
}

/// Locate `git` on `PATH` (the `O6n("git")` which-alike used by `MQ`).
/// Windows executable extensions only — this auto-detection chain is
/// windows-only in the oracle.
fn which_git_on_path() -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for name in ["git.exe", "git.cmd", "git"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Memoized process-wide Git Bash path (`MQ` is memoized; `P6n` runs once at
/// startup). On success the path is exported as `SHELL` and logged
/// (`Using bash path: "{p}"`); on failure the `P6n` unavailable line is
/// logged. Consulted by [`resolve_shell_path`] on Windows.
///
/// Env override: `LINGXI_GIT_BASH_PATH` first, then the upstream
/// `CLAUDE_CODE_GIT_BASH_PATH` spelling; empty values count as unset.
#[must_use]
pub fn git_bash_path() -> Option<&'static str> {
    static RESOLVED: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    RESOLVED
        .get_or_init(|| {
            let override_owned = ["LINGXI_GIT_BASH_PATH", "CLAUDE_CODE_GIT_BASH_PATH"]
                .iter()
                .find_map(|var| {
                    std::env::var(var)
                        .ok()
                        .filter(|v| !v.is_empty())
                        .map(|v| (*var, v))
                });
            let resolved = resolve_git_bash_path_with(
                override_owned.as_ref().map(|(var, v)| (*var, v.as_str())),
                &|p| std::path::Path::new(p).exists(),
                &which_git_on_path,
            );
            // `P6n` side effects belong to the real windows runtime only —
            // resolution stays testable everywhere.
            if cfg!(windows) {
                match &resolved {
                    Some(p) => {
                        std::env::set_var("SHELL", p);
                        discovery_log(false, &format!("Using bash path: \"{p}\""));
                    }
                    None => discovery_log(true, "Git Bash not found; BashTool will be unavailable"),
                }
            }
            resolved
        })
        .as_deref()
}

/// Human-readable duration — claude-code's `qs()` in its default (no-options)
/// form: a sub-minute value is `"<floor(seconds)>s"`; otherwise the largest
/// units down, `"Xd Yh Zm"` / `"Yh Zm Ws"` / `"Zm Ws"` / `"Ws"`, with the
/// seconds field rounded and 60→carry normalization (`60s→+1m`, `60m→+1h`,
/// `24h→+1d`). Used for the timed-out-command annotation (94 call sites in the
/// binary; ported for the one the Bash tool needs).
#[must_use]
pub fn format_duration_ms(ms: u64) -> String {
    if ms < 60_000 {
        return format!("{}s", ms / 1000);
    }
    let mut days = ms / 86_400_000;
    let mut hours = (ms % 86_400_000) / 3_600_000;
    let mut mins = (ms % 3_600_000) / 60_000;
    let mut secs = ((ms % 60_000) as f64 / 1000.0).round() as u64;
    if secs == 60 {
        secs = 0;
        mins += 1;
    }
    if mins == 60 {
        mins = 0;
        hours += 1;
    }
    if hours == 24 {
        hours = 0;
        days += 1;
    }
    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m {secs}s")
    } else if mins > 0 {
        format!("{mins}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn format_duration_ms_matches_qs() {
        // Sub-minute → floored seconds.
        assert_eq!(format_duration_ms(0), "0s");
        assert_eq!(format_duration_ms(200), "0s");
        assert_eq!(format_duration_ms(5_000), "5s");
        assert_eq!(format_duration_ms(59_999), "59s");
        // Minute+ → largest units down, seconds rounded, 60→carry.
        assert_eq!(format_duration_ms(60_000), "1m 0s");
        assert_eq!(format_duration_ms(120_000), "2m 0s");
        assert_eq!(format_duration_ms(119_600), "2m 0s"); // 1m 59.6s → round → 2m 0s
        assert_eq!(format_duration_ms(3_600_000), "1h 0m 0s");
        assert_eq!(format_duration_ms(3_661_000), "1h 1m 1s");
        assert_eq!(format_duration_ms(90_061_000), "1d 1h 1m"); // days form drops seconds
    }
    #[test]
    fn git_bash_classifier_matrix() {
        let all_exist = |_: &str| true;
        let none_exist = |_: &str| false;
        // Valid basenames, case-insensitive, both separators.
        for p in [
            r"C:\tools\bash.exe",
            r"C:\tools\BASH.EXE",
            "C:/git/bin/sh.exe",
            r"D:\x\bash",
            "/usr/bin/sh",
        ] {
            assert_eq!(
                classify_git_bash_override(p, &all_exist),
                GitBashOverride::Valid,
                "{p}"
            );
            assert_eq!(
                classify_git_bash_override(p, &none_exist),
                GitBashOverride::NotFound,
                "{p}"
            );
        }
        // Invalid basenames never probe existence (`o && e(v)` short-circuit).
        let must_not_probe = |p: &str| -> bool { panic!("existence probed for {p}") };
        for p in [r"C:\tools\pwsh.exe", r"C:\Git\bin\bash.exe.bak", "cmd.exe"] {
            assert_eq!(
                classify_git_bash_override(p, &must_not_probe),
                GitBashOverride::NotBashBinary,
                "{p}"
            );
        }
    }
    #[test]
    fn git_bash_warning_is_byte_exact() {
        // `CLAUDE_CODE_GIT_BASH_PATH "${v}" ${o?"not found":"is not a bash/sh
        // binary"}; falling back to auto-detection`
        assert_eq!(
            git_bash_override_warning(
                "CLAUDE_CODE_GIT_BASH_PATH",
                r"C:\x\bash.exe",
                GitBashOverride::NotFound
            ),
            "CLAUDE_CODE_GIT_BASH_PATH \"C:\\x\\bash.exe\" not found; falling back to auto-detection"
        );
        assert_eq!(
            git_bash_override_warning(
                "CLAUDE_CODE_GIT_BASH_PATH",
                r"C:\x\pwsh.exe",
                GitBashOverride::NotBashBinary
            ),
            "CLAUDE_CODE_GIT_BASH_PATH \"C:\\x\\pwsh.exe\" is not a bash/sh binary; falling back to auto-detection"
        );
    }
    #[test]
    fn git_bash_resolution_order() {
        // 1) Valid override wins verbatim.
        let exists_override = |p: &str| p == r"D:\portable\bash.exe";
        assert_eq!(
            resolve_git_bash_path_with(
                Some(("CLAUDE_CODE_GIT_BASH_PATH", r"D:\portable\bash.exe")),
                &exists_override,
                &|| None,
            )
            .as_deref(),
            Some(r"D:\portable\bash.exe")
        );
        // 2) Invalid override falls back to the Program Files probes.
        let exists_pf = |p: &str| p == r"C:\Program Files\Git\bin\bash.exe";
        assert_eq!(
            resolve_git_bash_path_with(
                Some(("CLAUDE_CODE_GIT_BASH_PATH", r"D:\missing\bash.exe")),
                &exists_pf,
                &|| None,
            )
            .as_deref(),
            Some(r"C:\Program Files\Git\bin\bash.exe")
        );
        // 3) (x86) probe is second.
        let exists_x86 = |p: &str| p == r"C:\Program Files (x86)\Git\bin\bash.exe";
        assert_eq!(
            resolve_git_bash_path_with(None, &exists_x86, &|| None).as_deref(),
            Some(r"C:\Program Files (x86)\Git\bin\bash.exe")
        );
        // 4) git-on-PATH: join(git, "..", "..", "bin", "bash.exe"). The
        // expectation is the LITERAL normalized path, not a recomputation of
        // the code under test — `path.win32.join` collapses the two `..`, so
        // no `..` may survive into the returned string.
        let git = std::path::PathBuf::from(r"C:\Custom\Git\cmd\git.exe");
        let exists_git = |p: &str| p == r"C:\Custom\Git\bin\bash.exe";
        assert_eq!(
            resolve_git_bash_path_with(None, &exists_git, &|| Some(git.clone())).as_deref(),
            Some(r"C:\Custom\Git\bin\bash.exe")
        );
        // 5) Nothing anywhere -> None.
        assert_eq!(resolve_git_bash_path_with(None, &|_| false, &|| None), None);
    }
    #[test]
    fn git_bash_candidate_is_win32_normalized() {
        assert_eq!(
            git_bash_beside_git(r"C:\Custom\Git\cmd\git.exe"),
            r"C:\Custom\Git\bin\bash.exe"
        );
        assert!(!git_bash_beside_git(r"C:\Custom\Git\cmd\git.exe").contains(".."));
        // Forward slashes in a PATH entry still render as backslashes.
        assert_eq!(
            git_bash_beside_git("C:/Custom/Git/cmd/git.exe"),
            r"C:\Custom\Git\bin\bash.exe"
        );
        // `..` cannot climb past the drive root.
        assert_eq!(git_bash_beside_git(r"C:\Git\git.exe"), r"C:\bin\bash.exe");
        // UNC share root is preserved.
        assert_eq!(
            git_bash_beside_git(r"\\srv\share\Git\cmd\git.exe"),
            r"\\srv\share\Git\bin\bash.exe"
        );
        // Relative PATH entry keeps the leftover `..` (Node's normalizeString).
        assert_eq!(git_bash_beside_git(r".\git.exe"), r"..\bin\bash.exe");
    }
}

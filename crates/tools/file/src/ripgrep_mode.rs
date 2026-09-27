//! `USE_BUILTIN_RIPGREP` ripgrep execution-mode resolver.
//!
//! 1:1 port of claude-code 2.1.207's ripgrep-config resolver `A3r`:
//!
//! ```js
//! A3r=Or(()=>{
//!   if(ou(process.env.USE_BUILTIN_RIPGREP)){
//!     let{cmd:r}=MYn("rg",[]);
//!     if(r!=="rg")return{mode:"system",command:r,args:[]}
//!   }
//!   ... return {mode:"embedded", ... argv0:"rg"}
//! })
//! function ou(e){                                   // = isEnvDefinedFalsy
//!   if(e===void 0)return false;
//!   if(typeof e==="boolean")return !e;
//!   let t=String(e).toLowerCase().trim();
//!   return["0","false","no","off"].includes(t)
//! }
//! ```
//!
//! When `USE_BUILTIN_RIPGREP` is EXPLICITLY falsy (`0`/`false`/`no`/`off` —
//! [`platform_api::env::is_env_defined_falsy`], the byte-faithful `ou()` port) AND a
//! system `rg` resolves on `$PATH` to a concrete path (`MYn("rg",[])` returning
//! a `cmd !== "rg"`), claude-code switches Grep to that system binary. Otherwise
//! it uses the embedded ripgrep. The env var keeps its un-prefixed
//! `USE_BUILTIN_RIPGREP` name (it is not a `CLAUDE_*` name, so the LingXi rename
//! does not apply).
//!
//! LingXi's Grep is an IN-PROCESS engine on the `grep-*`/`ignore` crates (the
//! same libraries ripgrep itself is built on), which IS claude-code's default
//! "embedded" mode. This module ports the mode DECISION (the env + `$PATH`
//! surface) so `USE_BUILTIN_RIPGREP` is consumed faithfully.
//!
//! RESIDUAL: the actual system-`rg` SUBPROCESS execution backend (spawning the
//! resolved binary and reformatting its output to the tool's model-facing
//! `content` shape) is NOT ported — the in-process engine remains the executor
//! for both modes. Swapping in a subprocess backend that reproduces the tool's
//! output format is a separate, larger change.

use std::path::PathBuf;

/// The resolved ripgrep execution mode (binary `A3r`'s `{mode, …}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RipgrepMode {
    /// The default embedded ripgrep (here: LingXi's in-process `grep-*` engine).
    Embedded,
    /// A system `rg` selected via `USE_BUILTIN_RIPGREP` opt-out; carries the
    /// resolved absolute path (binary `{mode:"system", command:r, args:[]}`).
    System {
        /// Absolute path to the resolved system `rg` binary.
        command: PathBuf,
    },
}

/// `USE_BUILTIN_RIPGREP` env var name (kept verbatim — not a `CLAUDE_*` name).
pub const USE_BUILTIN_RIPGREP_ENV: &str = "USE_BUILTIN_RIPGREP";

/// The `rg` executable base name.
const RG_BIN: &str = "rg";

/// Resolve the ripgrep execution mode from the ambient process environment.
///
/// Reads `USE_BUILTIN_RIPGREP` and delegates to [`resolve_ripgrep_mode_from`].
#[must_use]
pub fn resolve_ripgrep_mode() -> RipgrepMode {
    let env = std::env::var(USE_BUILTIN_RIPGREP_ENV).ok();
    resolve_ripgrep_mode_from(env.as_deref(), find_rg_on_path)
}

/// The pure `A3r` core: given the captured `USE_BUILTIN_RIPGREP` value and a
/// `$PATH` `rg` resolver, decide the mode.
///
/// `opt_out` (binary `ou()`) is [`platform_api::env::is_env_defined_falsy`]: undefined
/// or empty ⇒ not opted out; otherwise the lowercased/trimmed value must be one
/// of `0`/`false`/`no`/`off`. Only when opted out is `$PATH` consulted (`MYn`);
/// a concrete resolved path yields [`RipgrepMode::System`], else
/// [`RipgrepMode::Embedded`].
#[must_use]
pub fn resolve_ripgrep_mode_from(
    env_value: Option<&str>,
    resolve_rg: impl FnOnce() -> Option<PathBuf>,
) -> RipgrepMode {
    if platform_api::env::is_env_defined_falsy(env_value) {
        // `MYn("rg",[])` → `{cmd:r}`; `if(r!=="rg")` ⇒ a concrete path was found.
        if let Some(command) = resolve_rg() {
            return RipgrepMode::System { command };
        }
    }
    RipgrepMode::Embedded
}

/// `MYn("rg",[])` — resolve `rg` on `$PATH`. Returns the first executable `rg`
/// (`rg.exe` on Windows) found, or `None` (the binary's `cmd === "rg"` "not
/// resolved" case).
#[must_use]
pub fn find_rg_on_path() -> Option<PathBuf> {
    let bin = if cfg!(target_os = "windows") {
        "rg.exe"
    } else {
        RG_BIN
    };
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(bin);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// A regular file that is executable (on unix, any of the exec bits). On
/// non-unix, `is_file()` is the best available proxy.
fn is_executable_file(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_or_normal_is_embedded_without_touching_path() {
        // Not opted out ⇒ Embedded, and the `$PATH` resolver is NOT consulted
        // (it would panic if called).
        let panic_resolver = || -> Option<PathBuf> { panic!("PATH must not be scanned") };
        assert_eq!(
            resolve_ripgrep_mode_from(None, panic_resolver),
            RipgrepMode::Embedded
        );
        assert_eq!(
            resolve_ripgrep_mode_from(Some(""), || panic!("no scan")),
            RipgrepMode::Embedded
        );
        // Truthy / out-of-set values are NOT opt-outs.
        for v in ["1", "true", "yes", "on", "enabled", "2"] {
            assert_eq!(
                resolve_ripgrep_mode_from(Some(v), || panic!("no scan for {v}")),
                RipgrepMode::Embedded,
                "{v:?} must not opt out"
            );
        }
    }

    #[test]
    fn defined_falsy_with_system_rg_selects_system() {
        let rg = PathBuf::from("/usr/bin/rg");
        for v in ["0", "false", "FALSE", " no ", "Off"] {
            let mode = resolve_ripgrep_mode_from(Some(v), || Some(rg.clone()));
            assert_eq!(
                mode,
                RipgrepMode::System {
                    command: rg.clone()
                },
                "{v:?} should opt out to system rg"
            );
        }
    }

    #[test]
    fn defined_falsy_without_system_rg_falls_back_to_embedded() {
        // Opted out but no `rg` on `$PATH` (binary `cmd === "rg"`) ⇒ Embedded.
        assert_eq!(
            resolve_ripgrep_mode_from(Some("false"), || None),
            RipgrepMode::Embedded
        );
    }
}

//! Path validation for the Glob and Grep search tools.
//!
//! ST-04 / ST-05 (claude-code 2.1.238): the two tools' `validateInput({path})`
//! are NOT the same function — the port's earlier "1:1 with v2.1.183's
//! identical validator on both tools" claim stopped holding upstream. The
//! oracle (`cc-238.js`; 2.1.220 is byte-identical) defines them side by side:
//!
//! **Glob** (`lhe = es({name:Bm,...})`, @226421955) — directory-ONLY:
//! ```js
//! async validateInput({pattern:e,path:t}){
//!   let r=h0i(Bm,[["pattern",e],["path",t]]); if(r)return r;      // null-byte guard (ST-06, unported)
//!   if(t){let n=Ar(),o=Zi(t);
//!     if(aU(o))return{result:!0};                                  // UNC skip
//!     let i;
//!     try{i=await n.stat(o)}
//!     catch(s){ if(ur(s)){                                         // ENOENT
//!       let a=await YDe(o), l=`Directory does not exist: ${t}. ${U_e} ${er()}.`;
//!       if(a)l+=` Did you mean ${a}?`;
//!       return{result:!1,message:l,errorCode:1}} throw s}
//!     if(!i.isDirectory())return{result:!1,message:`Path is not a directory: ${t}`,errorCode:2}}
//!   return{result:!0}
//! }
//! ```
//!
//! **Grep** (`Uve = es({name:Am,...})`, @226429938) — accepts a FILE too, and
//! words the missing-path error differently. There is NO `isDirectory()` arm:
//! Grep's own schema calls `path` "File or directory to search in (rg PATH)",
//! and `rg` happily searches a single file.
//! ```js
//! async validateInput({pattern:e,path:t,glob:r,type:n,head_limit:o,offset:i}){
//!   ...                                                            // null-byte + integer guards (ST-06/ST-07, unported)
//!   if(t){let a=Ar(),l=Zi(t);
//!     if(aU(l))return{result:!0};                                  // UNC skip
//!     try{await a.stat(l)}
//!     catch(c){ if(ur(c)){                                         // ENOENT
//!       let u=await YDe(l), d=`Path does not exist: ${t}. ${U_e} ${er()}.`;
//!       if(u)d+=` Did you mean ${u}?`;
//!       return{result:!1,message:d,errorCode:1}} throw c}}
//!   return{result:!0}
//! }
//! ```
//!
//! where `U_e="Note: your current working directory is"`, `Zi(e)` resolves a
//! relative path against the cwd, `er()` is the cwd and `YDe` is the
//! "did you mean" sibling probe.
//!
//! Divergences (documented): claude-code's `errorCode` 1/2 is not carried —
//! [`ValidationError`] is message-only, so only the model-visible message is
//! reproduced. A non-ENOENT `stat` error (claude-code `throw o`) is allowed to
//! fall through to the tool's own `canonicalize_and_validate` rather than
//! erroring here (the rare permission-on-stat case). The `h0i` null-byte guard
//! and Grep's `head_limit`/`offset` integer guards are separate, unported
//! findings (ST-06 / ST-07).

use std::path::{Path, PathBuf};
use tool_api::tool_trait::ValidationError;

/// claude-code `U_e` — the sentence between the missing path and the cwd in
/// both tools' ENOENT message.
const CWD_NOTE_PREFIX: &str = "Note: your current working directory is";

/// ST-06 — 1:1 port of claude-code `h0i(toolName, fields)` (oracle 2.1.238
/// @289924033), the FIRST guard in both search tools' `validateInput`:
///
/// ```js
/// function h0i(e,t){let r=t.find(([,n])=>n?.includes("\x00"));
///   if(r)return{result:!1,message:`${e} ${r[0]} cannot contain null bytes (\\0). Remove the null byte and try again.`,errorCode:2};
///   return null}
/// ```
///
/// `fields` is the tool's ORDERED field list — Glob passes
/// `[("pattern",…),("path",…)]`, Grep
/// `[("pattern",…),("path",…),("glob",…),("type",…)]` — and only the first
/// offender is reported (JS `find`). Non-string values can't hold a NUL, which
/// is why the caller passes `Value::as_str` results. The rendered message keeps
/// the JS template's literal backslash-zero: `… null bytes (\0). …`.
/// (errorCode 2 is dropped — [`ValidationError`] is message-only.)
pub(crate) fn validate_no_null_bytes(
    tool_name: &str,
    fields: &[(&str, Option<&str>)],
) -> Result<(), ValidationError> {
    for (field, value) in fields {
        if value.is_some_and(|v| v.contains('\0')) {
            return Err(ValidationError(format!(
                "{tool_name} {field} cannot contain null bytes (\\0). Remove the null byte and try again."
            )));
        }
    }
    Ok(())
}

/// Glob's `validateInput({path})` — the path must be an existing DIRECTORY.
/// `cwd` is the tool's effective working directory (`er()` analogue). Returns
/// the byte-exact claude-code error when the path is missing (errorCode 1,
/// `Directory does not exist: …`) or is a non-directory (errorCode 2,
/// `Path is not a directory: …`); `Ok(())` when the path is a UNC path, a real
/// directory, or fails `stat` for a non-ENOENT reason.
pub(crate) fn validate_glob_directory(path: &str, cwd: &Path) -> Result<(), ValidationError> {
    let (cwd, resolved) = match resolve_for_validation(path, cwd) {
        Some(pair) => pair,
        // UNC / network path — validation skipped entirely.
        None => return Ok(()),
    };
    // `stat` follows symlinks, matching JS `fs.stat`.
    match std::fs::metadata(&resolved) {
        Ok(meta) => {
            if !meta.is_dir() {
                // errorCode 2. Glob ONLY — Grep has no such arm.
                return Err(ValidationError(format!("Path is not a directory: {path}")));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ValidationError(enoent_message(
            "Directory does not exist",
            path,
            &resolved,
            &cwd,
        ))),
        // claude-code `throw o`; here we defer to the tool's own path guard.
        Err(_) => Ok(()),
    }
}

/// Grep's `validateInput({path})` — the path may be a file OR a directory
/// (`rg PATH`), so the ONLY rejection is ENOENT, and its wording is
/// `Path does not exist: …` (not Glob's `Directory does not exist: …`).
pub(crate) fn validate_grep_path(path: &str, cwd: &Path) -> Result<(), ValidationError> {
    let (cwd, resolved) = match resolve_for_validation(path, cwd) {
        Some(pair) => pair,
        None => return Ok(()),
    };
    match std::fs::metadata(&resolved) {
        // No `isDirectory()` check upstream: a file path is accepted and
        // searched directly.
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ValidationError(enoent_message(
            "Path does not exist",
            path,
            &resolved,
            &cwd,
        ))),
        Err(_) => Ok(()),
    }
}

/// Shared prologue of both validators: canonicalize the cwd, resolve `path`
/// against it (`Zi(e)`), and short-circuit UNC paths (`aU`). `None` means "UNC
/// — return `{result:!0}` now".
fn resolve_for_validation(path: &str, cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    // claude-code `er()` (Node `process.cwd()`) is the realpath, and `YDe`
    // realpath's the candidate's dirname — so both sides of its containment
    // check are canonical. Canonicalize the cwd here so they agree even when the
    // workspace path traverses a symlink (e.g. macOS `/var` → `/private/var`);
    // the "current working directory" note then also shows the realpath, as
    // claude-code does.
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    // `Zi(e)`: resolve a relative path against the cwd.
    let resolved = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };
    // `n.startsWith("\\\\")||n.startsWith("//")` — UNC / network paths skip
    // validation entirely.
    {
        let resolved_str = resolved.to_string_lossy();
        if resolved_str.starts_with("\\\\") || resolved_str.starts_with("//") {
            return None;
        }
    }
    Some((cwd, resolved))
}

/// The shared errorCode-1 body: `{lead}: {path}. {U_e} {cwd}.` plus the
/// optional `" Did you mean {x}?"` suffix. `lead` is the only byte that differs
/// between the two tools (`Directory does not exist` vs `Path does not exist`).
/// Note the message uses the RAW input `path`, not the resolved one
/// (claude-code `${t}`), and the cwd from `er()`.
fn enoent_message(lead: &str, path: &str, resolved: &Path, cwd: &Path) -> String {
    let mut message = format!("{lead}: {path}. {CWD_NOTE_PREFIX} {}.", cwd.display());
    if let Some(suggestion) = suggest_sibling_dir(resolved, cwd) {
        message.push_str(&format!(" Did you mean {}?", suggestion.display()));
    }
    message
}

/// 1:1 port of claude-code `moe(e)` (binary @ the search-tool region): a
/// "did you mean" suggestion that detects a path given relative to the cwd's
/// PARENT and points at the same name relative to the cwd. Returns the candidate
/// absolute path iff it exists.
///
/// ```js
/// function moe(e){
///   let t=Pt(), n=pf.dirname(t), r=e;
///   try{ let u=await realpath(pf.dirname(e)); r=pf.join(u, pf.basename(e)) }catch{}
///   let o=n===pf.sep?pf.sep:n+pf.sep;
///   if(!r.startsWith(o) || r.startsWith(t+pf.sep) || r===t) return;
///   let l=pf.relative(n,r), c=pf.join(t,l);
///   try{ return await stat(c), c }catch{ return }
/// }
/// ```
/// (the win32 `toLowerCase` case-fold is dropped — posix.)
fn suggest_sibling_dir(resolved: &Path, cwd: &Path) -> Option<PathBuf> {
    // `n = dirname(t)` — the cwd's parent. No parent (cwd is root) ⇒ no suggestion.
    let cwd_parent = cwd.parent()?;
    // `r = join(realpath(dirname(e)), basename(e))`, falling back to `e` when the
    // dirname can't be realpath'd (claude-code's `catch{}`).
    let resolved_dir = resolved.parent();
    let r = match (
        resolved_dir.and_then(|d| std::fs::canonicalize(d).ok()),
        resolved.file_name(),
    ) {
        (Some(real_parent), Some(base)) => real_parent.join(base),
        _ => resolved.to_path_buf(),
    };
    // Bail unless `r` is strictly under the cwd's parent AND not under the cwd
    // itself AND not the cwd.
    if !r.starts_with(cwd_parent) || r == cwd_parent || r.starts_with(cwd) || r == cwd {
        return None;
    }
    // `l = relative(parent, r); c = join(cwd, l)`.
    let l = r.strip_prefix(cwd_parent).ok()?;
    let candidate = cwd.join(l);
    std::fs::metadata(&candidate).ok().map(|_| candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn absent_path_is_ok() {
        // No path argument is handled by the caller (only calls when present);
        // an existing directory passes for both tools.
        let tmp = TempDir::new().unwrap();
        assert!(validate_glob_directory(tmp.path().to_str().unwrap(), tmp.path()).is_ok());
        assert!(validate_grep_path(tmp.path().to_str().unwrap(), tmp.path()).is_ok());
    }

    #[test]
    fn glob_nonexistent_directory_message_is_byte_exact() {
        let tmp = TempDir::new().unwrap();
        let err = validate_glob_directory("no_such_dir", tmp.path()).unwrap_err();
        // The note shows the canonical cwd (matching claude-code's realpath er()).
        let canon_cwd = std::fs::canonicalize(tmp.path()).unwrap();
        let expected = format!(
            "Directory does not exist: no_such_dir. Note: your current working directory is {}.",
            canon_cwd.display()
        );
        // No sibling suggestion for a fresh tmp (its parent has no `no_such_dir`).
        assert_eq!(err.0, expected);
    }

    /// ST-05: Grep's ENOENT arm words the lead differently — `Path does not
    /// exist:` where Glob says `Directory does not exist:` (oracle
    /// `Uve.validateInput` @226429938 vs `lhe.validateInput` @226421955).
    #[test]
    fn grep_nonexistent_path_message_is_byte_exact() {
        let tmp = TempDir::new().unwrap();
        let err = validate_grep_path("no_such_dir", tmp.path()).unwrap_err();
        let canon_cwd = std::fs::canonicalize(tmp.path()).unwrap();
        assert_eq!(
            err.0,
            format!(
                "Path does not exist: no_such_dir. Note: your current working directory is {}.",
                canon_cwd.display()
            )
        );
    }

    #[test]
    fn glob_path_is_a_file_message_is_byte_exact() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let err = validate_glob_directory(file.to_str().unwrap(), tmp.path()).unwrap_err();
        assert_eq!(
            err.0,
            format!("Path is not a directory: {}", file.display())
        );
    }

    /// ST-04: the oracle's Grep `validateInput` has NO `isDirectory()` arm — a
    /// FILE path is accepted and handed to `rg` as a single-file search root.
    #[test]
    fn grep_accepts_a_file_path() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(validate_grep_path(file.to_str().unwrap(), tmp.path()).is_ok());
    }

    #[test]
    fn unc_path_skips_validation() {
        let tmp = TempDir::new().unwrap();
        assert!(validate_glob_directory("//server/share", tmp.path()).is_ok());
        assert!(validate_grep_path("//server/share", tmp.path()).is_ok());
    }

    #[test]
    fn did_you_mean_suggests_sibling_relative_to_cwd() {
        // Layout: parent/{cwd, sib}. A query for "../sib" resolves under parent
        // (not under cwd), and `sib` ALSO exists as join(cwd-name?) — actually
        // the heuristic maps parent/sib → cwd/sib. So create cwd/sib too.
        let parent = TempDir::new().unwrap();
        let cwd = parent.path().join("cwd");
        std::fs::create_dir(&cwd).unwrap();
        // The path the user typed, resolving (relative to cwd) to parent/sib:
        std::fs::create_dir(parent.path().join("sib")).unwrap();
        // The candidate the heuristic points at: cwd/sib.
        std::fs::create_dir(cwd.join("sib")).unwrap();
        // User typed "../sib" → resolved parent/sib EXISTS, so validate passes;
        // to exercise the suggestion we query a NON-existent parent/sib name that
        // maps to an existing cwd/<name>. Use "../nope" with cwd/nope present:
        std::fs::create_dir(cwd.join("nope")).unwrap();
        let path = parent.path().join("nope");
        let path = path.to_str().unwrap();
        // Both tools carry the same `YDe` suffix, only the lead differs.
        let glob_err = validate_glob_directory(path, &cwd).unwrap_err();
        assert!(
            glob_err.0.contains("Did you mean") && glob_err.0.contains("nope"),
            "got: {}",
            glob_err.0
        );
        let grep_err = validate_grep_path(path, &cwd).unwrap_err();
        assert!(
            grep_err.0.starts_with("Path does not exist:")
                && grep_err.0.contains("Did you mean")
                && grep_err.0.contains("nope"),
            "got: {}",
            grep_err.0
        );
    }
}

//! Trusted-directory enforcement for every file-touching tool.
//!
//! `canonicalize_and_validate(path, &trusted_dirs)` returns the canonical
//! form of `path` IFF it lives inside one of the canonicalised
//! `trusted_dirs`. Otherwise returns [`PathValidationError::Outside`]; the
//! caller is expected to emit the `tengu_file_path_blocked` event via
//! [`emit_blocked_event`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use telemetry::pii::{PiiTagged, Verified};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::AnalyticsBus;
use thiserror::Error;

/// Event-name lock for path-blocked rejections.
///
/// This event does NOT live in `telemetry::tengu::tool` (it is
/// cross-cutting, fired from this module, not from a specific tool).
/// Declared inline as a string literal so M4-01 can ship without a
/// telemetry-schema bump; M5 may relocate to `tengu/tool.rs`.
pub const PATH_BLOCKED_EVENT: &str = "tengu_file_path_blocked";

/// Failure modes for [`canonicalize_and_validate`].
#[derive(Debug, Error)]
pub enum PathValidationError {
    /// Canonical form of `path` is not under any trusted directory.
    ///
    /// The `Display` string is byte-locked: `"File path {path} is outside
    /// trusted directories"` (spec §5).
    #[error("File path {} is outside trusted directories", path.display())]
    Outside {
        /// The path that was rejected (canonicalised if possible, otherwise
        /// as supplied).
        path: PathBuf,
    },
    /// Underlying I/O failure during canonicalisation (e.g. ENOENT).
    #[error("io error while validating path: {0}")]
    Io(String),
}

/// Canonicalise `path` and assert the result lives under at least one of
/// `trusted_dirs`.
///
/// `trusted_dirs` SHOULD be pre-canonicalised by the caller (this helper
/// canonicalises them defensively on every call — acceptable since the
/// list is small, ~2-5 entries). Returns the canonical absolute path on
/// success.
///
/// # Errors
/// Returns [`PathValidationError::Outside`] if `path` resolves outside the
/// trusted set, or [`PathValidationError::Io`] on canonicalisation failure.
pub fn canonicalize_and_validate(
    path: &Path,
    trusted_dirs: &[PathBuf],
) -> Result<PathBuf, PathValidationError> {
    let canon = canonicalize_with_fallback(path)?;
    let trusted_canon: Vec<PathBuf> = trusted_dirs
        .iter()
        .filter_map(|d| std::fs::canonicalize(d).ok())
        .collect();
    if trusted_canon.iter().any(|t| canon.starts_with(t)) {
        Ok(canon)
    } else {
        Err(PathValidationError::Outside { path: canon })
    }
}

/// Resolve `path` against `cwd` when it is relative, leaving an already
/// absolute path untouched.
///
/// `std::fs::canonicalize` (used internally by
/// [`canonicalize_and_validate`]) resolves a RELATIVE path against the
/// process's actual `current_dir()` — the real OS working directory, which
/// is set once at process boot and is NOT what `EnterWorktree`/`ExitWorktree`
/// swap. Callers that want a relative `file_path`/`path` tool argument to
/// track the session's CURRENT [`crate::session_cwd::SessionCwd`] (so it
/// lands under a worktree after a swap, not the frozen boot cwd) must
/// resolve it through this function — using `ctx.cwd()` — BEFORE handing the
/// path to [`canonicalize_and_validate`] or any other OS-relative-resolving
/// call.
///
/// An absolute input is returned unchanged, so this is a no-op for every
/// existing absolute-path caller (Read/Write/Edit document `file_path` as
/// "must be an absolute path"; this only changes behavior for the relative
/// case those docs advise against but do not enforce).
#[must_use]
pub fn resolve_against_cwd(path: PathBuf, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// Apply the filesystem's model-path translation (mobile-linux guest paths →
/// their host-backed twins) to an already cwd-resolved tool path.
///
/// Every file tool calls this between [`resolve_against_cwd`] and
/// [`canonicalize_and_validate`], so a guest path canonicalizes and
/// containment-checks as the host directory that actually backs it. On the
/// default filesystem (`translate_model_path` → `Ok(None)`) this returns the
/// input unchanged — desktop behavior is byte-identical. `Err` carries the
/// filesystem's fence message (unbacked guest space, read-only mount) for the
/// tool to surface to the model.
pub fn translate_model_path(
    fs: &std::sync::Arc<dyn platform_api::FileSystem>,
    path: PathBuf,
    write: bool,
) -> Result<PathBuf, String> {
    match fs.translate_model_path(&path.to_string_lossy(), write) {
        Ok(None) => Ok(path),
        Ok(Some(host)) => Ok(PathBuf::from(host)),
        Err(error) => Err(error.to_string()),
    }
}

fn canonicalize_with_fallback(path: &Path) -> Result<PathBuf, PathValidationError> {
    if let Ok(p) = std::fs::canonicalize(path) {
        Ok(p)
    } else {
        // Target does not exist — canonicalise the parent and re-join.
        let parent = path
            .parent()
            .ok_or_else(|| PathValidationError::Io(format!("no parent for {path:?}")))?;
        let file = path
            .file_name()
            .ok_or_else(|| PathValidationError::Io(format!("no file_name for {path:?}")))?;
        let parent_canon =
            std::fs::canonicalize(parent).map_err(|e| PathValidationError::Io(e.to_string()))?;
        Ok(parent_canon.join(file))
    }
}

/// Emit [`PATH_BLOCKED_EVENT`] through `bus` carrying the (PII-tagged) path
/// and the (PII-safe) tool name. Callers fire this right after a
/// [`canonicalize_and_validate`] failure.
pub async fn emit_blocked_event(bus: &AnalyticsBus, tool_name: &str, path: &Path) {
    let mut metadata: LogEventMetadata = HashMap::new();
    metadata.insert(
        "tool_name".to_string(),
        AnalyticsValue::String(Verified::assert_safe(tool_name.to_string()).into_inner()),
    );
    metadata.insert(
        "_PROTO_path".to_string(),
        AnalyticsValue::String(
            PiiTagged::assert_pii_tagged_column(path.display().to_string()).into_inner(),
        ),
    );
    bus.log_event(PATH_BLOCKED_EVENT, metadata).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use telemetry::InMemorySink;
    use tempfile::TempDir;

    #[test]
    fn event_name_byte_locked() {
        assert_eq!(PATH_BLOCKED_EVENT, "tengu_file_path_blocked");
    }

    #[test]
    fn outside_display_byte_locked() {
        let e = PathValidationError::Outside {
            path: PathBuf::from("/etc/passwd"),
        };
        assert_eq!(
            e.to_string(),
            "File path /etc/passwd is outside trusted directories"
        );
    }

    #[tokio::test]
    async fn accepts_path_inside_trusted_dir() {
        let tmp = TempDir::new().unwrap();
        let trusted = vec![tmp.path().to_path_buf()];
        let target = tmp.path().join("foo.txt");
        std::fs::write(&target, "hi").unwrap();
        let canon = canonicalize_and_validate(&target, &trusted).unwrap();
        let trusted_canon = std::fs::canonicalize(tmp.path()).unwrap();
        assert!(canon.starts_with(&trusted_canon));
    }

    #[tokio::test]
    async fn rejects_path_outside_trusted_dir() {
        let tmp = TempDir::new().unwrap();
        let trusted = vec![tmp.path().to_path_buf()];
        let target = PathBuf::from("/etc/hosts");
        let err = canonicalize_and_validate(&target, &trusted).unwrap_err();
        assert!(
            matches!(err, PathValidationError::Outside { .. }),
            "expected Outside, got {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_path_traversal_outside_trusted() {
        // Deviation from plan: original test relied on canonicalize
        // collapsing `..` segments through non-existent intermediate
        // dirs, which fails ENOENT on macOS/Linux (canonicalize requires
        // every path component to exist). We construct the traversal via
        // a real symlink inside the tempdir pointing outward, which
        // exercises the same code path (canonicalize follows the symlink
        // to /etc/hosts, which is outside trusted).
        if !std::path::Path::new("/etc/hosts").exists() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let trusted = vec![tmp.path().to_path_buf()];
        let link = tmp.path().join("link_to_etc_hosts");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hosts", &link).unwrap();
        let err = canonicalize_and_validate(&link, &trusted).unwrap_err();
        assert!(
            matches!(err, PathValidationError::Outside { .. }),
            "expected Outside, got {err:?}"
        );
    }

    #[tokio::test]
    async fn nonexistent_file_uses_parent_canonical() {
        let tmp = TempDir::new().unwrap();
        let trusted = vec![tmp.path().to_path_buf()];
        let target = tmp.path().join("future.txt");
        let canon = canonicalize_and_validate(&target, &trusted).unwrap();
        let trusted_canon = std::fs::canonicalize(tmp.path()).unwrap();
        assert!(canon.starts_with(&trusted_canon));
        assert!(canon.ends_with("future.txt"));
    }

    #[test]
    fn resolve_against_cwd_leaves_absolute_path_untouched() {
        let cwd = PathBuf::from("/some/worktree");
        let abs = PathBuf::from("/etc/passwd");
        assert_eq!(resolve_against_cwd(abs.clone(), &cwd), abs);
    }

    #[test]
    fn resolve_against_cwd_joins_relative_path_onto_cwd() {
        let cwd = PathBuf::from("/some/worktree");
        let rel = PathBuf::from("src/main.rs");
        assert_eq!(
            resolve_against_cwd(rel, &cwd),
            PathBuf::from("/some/worktree/src/main.rs")
        );
    }

    #[tokio::test]
    async fn emit_blocked_event_lands_in_sink() {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::default());
        bus.attach_sink(sink.clone()).await;
        emit_blocked_event(&bus, "Read", Path::new("/etc/passwd")).await;
        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "tengu_file_path_blocked");
        let metadata = &events[0].metadata;
        assert!(metadata.contains_key("tool_name"));
        assert!(metadata.contains_key("_PROTO_path"));
        match metadata.get("tool_name").unwrap() {
            AnalyticsValue::String(s) => assert_eq!(s, "Read"),
            other => panic!("expected String, got {other:?}"),
        }
        match metadata.get("_PROTO_path").unwrap() {
            AnalyticsValue::String(s) => assert_eq!(s, "/etc/passwd"),
            other => panic!("expected String, got {other:?}"),
        }
    }
}

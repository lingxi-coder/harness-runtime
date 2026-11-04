//! Direct session-directory links written by `/add-dir` (Claude Code 2.1.307).
//!
//! A session stored in project directory A is made discoverable from B by
//! appending A's complete project-directory path to B's `.session-aliases`.
//! Links are directional and are never followed recursively.

use super::path::project_dir_name;
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use tokio::io::AsyncWriteExt;
use unicode_normalization::UnicodeNormalization;

/// Filename used by the native session-directory link format.
pub const SESSION_ALIASES_FILE: &str = ".session-aliases";

/// Exact transcript links created by `/cd`, separate from directory-wide
/// `/add-dir` links. Entries contain one source relative to the projects
/// directory, one JSON object per line.
pub const SESSION_CWD_ALIASES_FILE: &str = ".session-cwd-aliases";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CwdAlias {
    source: String,
    canonical_home: bool,
}

fn valid_cwd_alias(relative: &Path) -> bool {
    let mut components = relative.components();
    let (Some(Component::Normal(bucket)), Some(Component::Normal(filename)), None) =
        (components.next(), components.next(), components.next())
    else {
        return false;
    };
    let Some(bucket) = bucket.to_str() else {
        return false;
    };
    if bucket.is_empty()
        || !bucket
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return false;
    }
    let filename = Path::new(filename);
    filename.extension().and_then(|s| s.to_str()) == Some("jsonl")
        && filename
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(super::validate_uuid)
}

/// Read only direct `/cd` links, rejecting traversal and malformed entries.
/// A link is only a discovery hint; the loader checks the transcript's fixed
/// identity and latest relocation cwd before showing it in this directory.
pub(crate) async fn read_session_cwd_aliases(home: &Path, project_dir: &Path) -> Vec<PathBuf> {
    let Ok(content) = tokio::fs::read_to_string(project_dir.join(SESSION_CWD_ALIASES_FILE)).await
    else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let canonical_home = std::fs::canonicalize(home).ok();
    content
        .split_inclusive('\n')
        .filter_map(|line| line.strip_suffix('\n'))
        .filter_map(|line| serde_json::from_str::<CwdAlias>(line).ok())
        .filter_map(|entry| {
            let relative = PathBuf::from(entry.source);
            if !valid_cwd_alias(&relative) {
                return None;
            }
            let base = if entry.canonical_home {
                canonical_home.as_deref()?
            } else {
                home
            };
            let path = base.join("projects").join(relative);
            seen.insert(path.clone()).then_some(path)
        })
        .collect()
}

/// Durably record one selected source for discovery after `/cd`. The caller
/// must still persist the relocation marker before publishing its new cwd.
/// Separate sessions targeting one directory serialize their alias updates.
pub async fn record_session_cwd_alias(
    home: &Path,
    source_path: &Path,
    new_cwd: &Path,
) -> io::Result<bool> {
    // Capture only the trusted home root. Never canonicalize the source's
    // projects/bucket components, which would change its selected authority.
    let canonical_home = std::fs::canonicalize(home)?;
    let (source, uses_canonical_home) = match source_path.strip_prefix(home.join("projects")) {
        Ok(source) => (source.to_path_buf(), false),
        Err(_) => (
            source_path
                .strip_prefix(canonical_home.join("projects"))
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "session source is outside projects",
                    )
                })?
                .to_path_buf(),
            true,
        ),
    };
    if !valid_cwd_alias(&source) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid session source path",
        ));
    }
    let target = alias_target_directory(new_cwd);
    let bucket = project_dir_name(&target.to_string_lossy());
    if source.parent() == Some(Path::new(&bucket)) {
        return Ok(false);
    }
    let home = canonical_home;
    tokio::task::spawn_blocking(move || {
        use lingxi_core::host::{rooted_fs, FsError};
        let map_error = |error: FsError| io::Error::other(error.to_string());
        let root = rooted_fs::root_identity(&home).map_err(map_error)?;
        let dir = Path::new("projects").join(bucket);
        rooted_fs::ensure_directory_identity_pinned(&home, &dir, &root).map_err(map_error)?;
        let _lock = rooted_fs::lock_exclusive_pinned(
            &home,
            &dir.join(".session-cwd-aliases.lock"),
            0o700,
            0o600,
            Some(&root),
        )
        .map_err(map_error)?;
        let alias_path = dir.join(SESSION_CWD_ALIASES_FILE);
        let content = match rooted_fs::read_to_string_pinned(&home, &alias_path, Some(&root)) {
            Ok(content) => content,
            Err(FsError::NotFound(_)) => String::new(),
            Err(error) => return Err(map_error(error)),
        };
        if !content.is_empty() && !content.ends_with('\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete session cwd alias file",
            ));
        }
        let mut file = rooted_fs::open_append_file_pinned(&home, &alias_path, Some(&root))
            .map_err(map_error)?;
        rooted_fs::validate_open_file_single_link(&file).map_err(map_error)?;
        let already_present = content
            .lines()
            .filter_map(|line| serde_json::from_str::<CwdAlias>(line).ok())
            .any(|entry| {
                Path::new(&entry.source) == source.as_path()
                    && entry.canonical_home == uses_canonical_home
            });
        if !already_present {
            let entry = serde_json::to_string(&CwdAlias {
                source: source.to_string_lossy().into_owned(),
                canonical_home: uses_canonical_home,
            })
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
                + "\n";
            let previous_len = file.metadata()?.len();
            if let Err(error) = file
                .write_all(entry.as_bytes())
                .and_then(|()| file.sync_all())
            {
                return match file.set_len(previous_len).and_then(|()| file.sync_all()) {
                    Ok(()) => Err(error),
                    Err(rollback) => Err(io::Error::other(format!(
                        "{error}; alias rollback failed: {rollback}"
                    ))),
                };
            }
        } else {
            // A previous attempt may have synced the file but failed to sync a
            // directory entry. Re-establish the full durability boundary.
            file.sync_all()?;
        }
        // Persist both the file entry and any newly created bucket/projects
        // entries before the caller can publish the matching cwd marker.
        rooted_fs::sync_parent_pinned(&home, &alias_path, Some(&root)).map_err(map_error)?;
        rooted_fs::sync_parent_pinned(&home, &dir, Some(&root)).map_err(map_error)?;
        rooted_fs::sync_parent_pinned(&home, Path::new("projects"), Some(&root))
            .map_err(map_error)?;
        Ok(!already_present)
    })
    .await
    .map_err(io::Error::other)?
}

/// Read direct linked project directories in their first-seen order.
///
/// Native `W0e` splits only on LF, drops only empty strings, and deduplicates
/// exact strings. In particular, CR and spaces are part of a path, not trim
/// characters. Missing or unreadable link files behave as an empty list.
pub async fn read_session_aliases(project_dir: &Path) -> Vec<PathBuf> {
    let Ok(content) = tokio::fs::read_to_string(project_dir.join(SESSION_ALIASES_FILE)).await
    else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    content
        .split('\n')
        .filter(|line| !line.is_empty() && seen.insert(*line))
        .map(PathBuf::from)
        .collect()
}

/// Native resume's wS normalizes both resolved and retained paths to NFC.
pub(crate) fn normalized_directory_path(path: &Path) -> PathBuf {
    let resolved = match std::fs::canonicalize(path) {
        Ok(resolved) => resolved,
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::debug!(path = %path.display(), %error, "session alias realpath failed");
            }
            path.to_path_buf()
        }
    };
    PathBuf::from(resolved.to_string_lossy().nfc().collect::<String>())
}

/// Native OPr applies NFC inside the successful realpath branch. A failed
/// lookup retains the original path bytes, including decomposed characters.
fn alias_target_directory(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(resolved) => PathBuf::from(resolved.to_string_lossy().nfc().collect::<String>()),
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::debug!(path = %path.display(), %error, "session alias realpath failed");
            }
            path.to_path_buf()
        }
    }
}

/// Link the current session's authoritative transcript directory from an added
/// working directory. Returns whether a new line was appended.
///
/// `source_project_dir` is the actual transcript's parent, rather than a bucket
/// guessed from the current tool cwd: worktree entry and session relocation can
/// make those paths differ. Callers with transcript persistence disabled do not
/// invoke this operation. The source path is persisted verbatim, followed by LF.
/// A successful append has completed before this operation returns.
pub async fn record_session_alias(
    lingxi_home: &Path,
    source_project_dir: &Path,
    added_dir: &Path,
) -> io::Result<bool> {
    let added_dir = alias_target_directory(added_dir);
    let project_dir = lingxi_home
        .join("projects")
        .join(project_dir_name(&added_dir.to_string_lossy()));
    if source_project_dir.as_os_str() == project_dir.as_os_str() {
        return Ok(false);
    }
    // The write path only treats ENOENT as an absent file. Other read errors
    // stop the update; the read-only W0e-style API above remains best-effort.
    let content = match tokio::fs::read_to_string(project_dir.join(SESSION_ALIASES_FILE)).await {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let source = source_project_dir.to_string_lossy();
    if content.split('\n').any(|line| line == source.as_ref()) {
        return Ok(false);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&project_dir)?;
    }
    #[cfg(not(unix))]
    tokio::fs::create_dir_all(&project_dir).await?;

    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(project_dir.join(SESSION_ALIASES_FILE)).await?;
    let line = format!("{source}\n");
    file.write_all(line.as_bytes()).await?;
    // Tokio write_all may only enqueue the blocking write. Native awaits
    // appendFile, so wait for completion before readers or a duplicate check.
    file.flush().await?;
    Ok(true)
}

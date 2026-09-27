//! `bashEditDiffEnabled` — the diff of files a Bash command changed (CLI-5).
//!
//! Oracle: 2.1.269's `y0r` (gate), `x0r` (tree snapshot), `b0r` (diff) in the
//! 2.1.270 binary. The contract is written out at the head of [`crate::bash`];
//! this module implements it.
//!
//! # The user's repository is never touched
//!
//! Every git invocation runs against a SHADOW git directory —
//! `GIT_DIR=<shadow>`, `GIT_WORK_TREE=<the real worktree>` — whose
//! `objects/info/alternates` points at the real object store so blobs are
//! shared without being copied. The shadow owns its own index and its own
//! `refs/heads/snapshot`.
//!
//! ⛔ This is the safety property, not a performance trick. `update-index`
//! against the real `.git` would stage the user's entire working tree as a side
//! effect of running a shell command, and `write-tree` would then silently
//! rewrite their staged state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// `x2s = 20_000_000` — a file at or above this is never hashed into the
/// snapshot tree. Its `(size, mtime)` goes to a side ledger instead, so a
/// multi-gigabyte artefact cannot make a Bash command wait on a full hash.
pub const LARGE_FILE_BYTES: u64 = 20_000_000;

/// `E2s = 32_000` — pathspec bytes per `ls-files` batch.
const PATHSPEC_BATCH_BYTES: usize = 32_000;

/// `QKe = 5` — files that carry hunks in the rendered diff.
pub const MAX_DIFF_FILES: usize = 5;

/// `iTe = 200` — paths listed as changed (the hook's `tool_response` list).
pub const MAX_CHANGED_PATHS: usize = 200;

/// `k2s = 400` — total hunk lines kept for one file.
pub const MAX_FILE_HUNK_LINES: usize = 400;

/// `w2s = 64_000` — total hunk characters kept for one file.
pub const MAX_FILE_HUNK_CHARS: usize = 64_000;

/// `h0r = 2` — consecutive failures before a repository is given up on.
pub const FAILURE_THRESHOLD: u32 = 2;

/// The empty-blob object ids — oracle `c0r`.
///
/// 🚨 NOT a binary check, which is the natural misreading. `A` with an empty
/// DESTINATION is "created an empty file" and `D` with an empty SOURCE is
/// "deleted an empty file"; both render with no hunks because there is no
/// content to show.
const EMPTY_BLOB_SHA1: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
const EMPTY_BLOB_SHA256: &str = "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813";

/// Is `sha` (possibly abbreviated) the empty blob?
#[must_use]
pub fn is_empty_blob(sha: &str) -> bool {
    !sha.is_empty() && (EMPTY_BLOB_SHA1.starts_with(sha) || EMPTY_BLOB_SHA256.starts_with(sha))
}

/// The git subcommands that move the worktree wholesale — the alternation
/// inside oracle `A2s`, consumed by `_0r`.
const BRANCH_MOVING_GIT_SUBCOMMANDS: [&str; 11] = [
    "checkout",
    "switch",
    "stash",
    "pull",
    "merge",
    "rebase",
    "reset",
    "restore",
    "clean",
    "cherry-pick",
    "revert",
];

/// Oracle `_0r(command)` / `A2s` — is this a bare `git <branch-moving>` call?
///
/// A command that matches gets NO snapshot: its diff would be the whole branch
/// delta, which tells the model nothing about what the command did.
///
/// 🚨 The pattern is deliberately NARROW, and the narrowness is the point. Every
/// argument must start with `[A-Za-z0-9._/@~^]`, so a flag (`git reset --hard`)
/// does NOT match and its diff IS taken; so does anything with a quote, a pipe
/// or a `&&`, because the regex is anchored at both ends. Widening this to "any
/// command containing `git checkout`" would silently drop the diff for
/// compound commands that also edit files.
#[must_use]
pub fn is_branch_moving_git(command: &str) -> bool {
    let rest = command.trim_start_matches([' ', '\t']);
    let rest = rest
        .strip_prefix("sudo")
        .and_then(|r| {
            let trimmed = r.trim_start_matches([' ', '\t']);
            (trimmed.len() < r.len()).then_some(trimmed)
        })
        .unwrap_or(rest);
    let Some(rest) = rest.strip_prefix("git") else {
        return false;
    };
    let rest = rest.trim_start_matches([' ', '\t']);
    if rest.len() == command.trim_start_matches([' ', '\t']).len() {
        // `git` was not followed by at least one space or tab.
        return false;
    }
    let Some(rest) = BRANCH_MOVING_GIT_SUBCOMMANDS
        .iter()
        .find_map(|sub| rest.strip_prefix(sub))
    else {
        return false;
    };
    // `{0,16}` plain arguments, then only trailing whitespace.
    let mut remaining = rest;
    for _ in 0..16 {
        let after_gap = remaining.trim_start_matches([' ', '\t']);
        if after_gap.len() == remaining.len() {
            break; // no separator ⇒ no further argument
        }
        let mut chars = after_gap.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphanumeric() || "._/@~^".contains(c) => {}
            _ => break,
        }
        let end = after_gap
            .find(|c: char| !(c.is_ascii_alphanumeric() || "._/@~^-".contains(c)))
            .unwrap_or(after_gap.len());
        remaining = &after_gap[end..];
    }
    remaining.trim().is_empty() && remaining.chars().all(char::is_whitespace)
}

/// Whether a path from git's output is one this port will act on — oracle `p$`.
///
/// Rejects absolute paths, control characters, and any `.git` / `.` / `..`
/// segment. A path that fails this is dropped rather than sanitised: it should
/// not have come out of `status` in the first place, and guessing what it meant
/// is how a diff ends up naming a file the command never touched.
#[must_use]
pub fn is_safe_relative_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') {
        return false;
    }
    if path
        .chars()
        .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}')
    {
        return false;
    }
    path.split('/').all(|segment| {
        if segment.is_empty() || segment == "." || segment == ".." {
            return false;
        }
        let lowered = segment.to_lowercase();
        let trimmed = lowered.trim_end_matches(['.', ' ']);
        trimmed != ".git" && !is_git_short_name(trimmed)
    })
}

/// `git~1`-style 8.3 short names for `.git`.
fn is_git_short_name(segment: &str) -> bool {
    segment
        .strip_prefix("git~")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

/// What `git status --porcelain=v2 -z` said — oracle `H2s`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StatusScan {
    /// Paths with content changes (tracked, modified/added, or untracked files).
    pub changed: Vec<String>,
    /// Paths git reports as deleted from the worktree.
    pub deleted: Vec<String>,
    /// Untracked DIRECTORIES, which must be expanded through `ls-files`.
    pub untracked_directories: Vec<String>,
}

/// Parse `--porcelain=v2 -z` output.
///
/// `? path` is untracked; a trailing `/` marks a directory. `1 ` (ordinary) and
/// `u ` (unmerged) records carry the path after 8 resp. 10 space-separated
/// fields; `XY` at offset 2..4 gives the status, and `.` in the WORKTREE column
/// means "no worktree change" — those are skipped, because the snapshot is
/// about what is on disk, not what is staged.
#[must_use]
pub fn parse_status_v2(stdout: &str) -> StatusScan {
    let mut scan = StatusScan::default();
    for record in stdout.split('\0') {
        if let Some(rest) = record.strip_prefix("? ") {
            let is_dir = rest.ends_with('/');
            let bare = if is_dir {
                &rest[..rest.len() - 1]
            } else {
                rest
            };
            if is_safe_relative_path(bare) {
                if is_dir {
                    scan.untracked_directories.push(rest.to_string());
                } else {
                    scan.changed.push(rest.to_string());
                }
            }
            continue;
        }
        let ordinary = record.starts_with("1 ");
        if !ordinary && !record.starts_with("u ") {
            continue;
        }
        let fields = if ordinary { 9 } else { 11 };
        let mut offset = 0usize;
        for _ in 0..fields - 1 {
            match record[offset..].find(' ') {
                Some(index) => offset += index + 1,
                None => {
                    offset = record.len();
                    break;
                }
            }
        }
        if offset >= record.len() {
            continue;
        }
        let path = &record[offset..];
        // Byte 3 is the worktree column of `XY`.
        let worktree_status = record.as_bytes().get(3).copied().unwrap_or(b'.');
        if worktree_status == b'.' || !is_safe_relative_path(path) {
            continue;
        }
        if worktree_status == b'D' {
            scan.deleted.push(path.to_string());
        } else {
            scan.changed.push(path.to_string());
        }
    }
    scan
}

/// Split pathspecs into `ls-files` batches under the byte cap — oracle `B2s`.
#[must_use]
pub fn pathspec_batches(paths: &[String]) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut batch: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    for path in paths {
        let cost = path.len() + 1;
        if !batch.is_empty() && bytes + cost > PATHSPEC_BATCH_BYTES {
            out.push(std::mem::take(&mut batch));
            bytes = 0;
        }
        batch.push(path.clone());
        bytes += cost;
    }
    if !batch.is_empty() {
        out.push(batch);
    }
    out
}

/// A shadow git directory for one worktree.
#[derive(Debug, Clone)]
pub struct ShadowRepo {
    /// The real worktree the snapshot describes.
    pub work_tree: PathBuf,
    /// The private git directory. Never the repository's own `.git`.
    pub git_dir: PathBuf,
}

impl ShadowRepo {
    /// Prepare (or reuse) the shadow for `work_tree`, whose real git directory
    /// is `real_git_dir`.
    ///
    /// # Errors
    /// Any failure to create the directory, initialise it, or point its
    /// alternates at the real object store.
    pub async fn prepare(
        work_tree: &Path,
        real_git_dir: &Path,
        shadow_root: &Path,
    ) -> Result<Self, String> {
        let git_dir = shadow_root.to_path_buf();
        let objects = git_dir.join("objects");
        let alternates = objects.join("info").join("alternates");
        let real_objects = real_git_dir.join("objects");

        if !git_dir.join("HEAD").exists() {
            tokio::fs::create_dir_all(&git_dir)
                .await
                .map_err(|e| format!("shadow mkdir: {e}"))?;
            // Oracle `eNt` creates every shadow directory with mode 448 (0o700).
            // The shadow's index names every path in the user's worktree, so it
            // must not be readable by other local users — the same lesson as the
            // plugin-extraction fix (MCP-2). `create_dir_all` applies the
            // process umask, which is commonly 022.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&git_dir, std::fs::Permissions::from_mode(0o700))
                    .await
                    .map_err(|e| format!("shadow chmod: {e}"))?;
            }
            let out = Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(&git_dir)
                .stdin(Stdio::null())
                .output()
                .await
                .map_err(|e| format!("shadow init: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "shadow init exit {:?}",
                    out.status.code().unwrap_or(-1)
                ));
            }
        }
        tokio::fs::create_dir_all(objects.join("info"))
            .await
            .map_err(|e| format!("shadow objects/info: {e}"))?;
        // Rewritten every time: a repo that moved leaves a stale alternate,
        // and a stale alternate makes `write-tree` succeed against objects
        // that are no longer reachable from the real repo.
        tokio::fs::write(&alternates, format!("{}\n", real_objects.display()))
            .await
            .map_err(|e| format!("shadow alternates: {e}"))?;
        Ok(Self {
            work_tree: work_tree.to_path_buf(),
            git_dir,
        })
    }

    /// Run one git command against the shadow.
    async fn git(&self, args: &[&str], stdin: Option<&str>) -> Result<(i32, String), String> {
        let mut command = Command::new("git");
        command
            .arg("--literal-pathspecs")
            .args(args)
            .env("GIT_DIR", &self.git_dir)
            .env("GIT_WORK_TREE", &self.work_tree)
            // The shadow must not inherit the user's hooks, aliases, or
            // credential helpers: it runs on every Bash call.
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.git_dir.join("config.global"))
            .env("GIT_OPTIONAL_LOCKS", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.work_tree)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|e| format!("git spawn: {e}"))?;
        if let Some(input) = stdin {
            use tokio::io::AsyncWriteExt as _;
            if let Some(mut pipe) = child.stdin.take() {
                pipe.write_all(input.as_bytes())
                    .await
                    .map_err(|e| format!("git stdin: {e}"))?;
                pipe.shutdown().await.ok();
            }
        }
        let out = child
            .wait_with_output()
            .await
            .map_err(|e| format!("git wait: {e}"))?;
        Ok((
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        ))
    }

    /// Snapshot the worktree as a tree object — oracle `x0r`.
    ///
    /// Returns the tree id, and the side ledger of files too large (or
    /// unreadable) to hash, keyed by path with a `"{size} {mtime_ms}"` stamp.
    /// Those files are NOT in the tree: a change to one shows up as a changed
    /// PATH without a diff, which is the trade upstream makes rather than
    /// hashing gigabytes on every shell command.
    ///
    /// # Errors
    /// Any git step that fails; the caller treats an error as "no diff this
    /// time" and counts it toward the per-repo circuit breaker.
    pub async fn snapshot(&self) -> Result<(String, HashMap<String, String>), String> {
        // A crashed git can leave the shadow's own lock behind; it is ours to
        // clear, unlike the real repository's.
        let _ = tokio::fs::remove_file(self.git_dir.join("index.lock")).await;

        let (code, stdout) = self
            .git(
                &[
                    "status",
                    "--porcelain=v2",
                    "-z",
                    "--untracked-files=normal",
                    "--ignored=no",
                    "--no-renames",
                    "--ignore-submodules=all",
                ],
                None,
            )
            .await?;
        if code != 0 {
            return Err(format!("status exit {code}"));
        }
        let mut scan = parse_status_v2(&stdout);

        // Untracked directories become their files.
        for batch in pathspec_batches(&scan.untracked_directories) {
            let mut args: Vec<&str> =
                vec!["ls-files", "-z", "--others", "--exclude-standard", "--"];
            args.extend(batch.iter().map(String::as_str));
            let (code, stdout) = self.git(&args, None).await?;
            if code != 0 {
                return Err(format!("ls-files exit {code}"));
            }
            scan.changed.extend(
                stdout
                    .split('\0')
                    .filter(|p| !p.is_empty() && !p.ends_with('/') && is_safe_relative_path(p))
                    .map(str::to_string),
            );
        }

        // Split off the files that must not be hashed.
        let mut hashable: Vec<String> = Vec::new();
        let mut oversized: HashMap<String, String> = HashMap::new();
        for path in &scan.changed {
            match tokio::fs::symlink_metadata(self.work_tree.join(path)).await {
                Ok(meta) if meta.is_file() && meta.len() < LARGE_FILE_BYTES => {
                    hashable.push(path.clone());
                }
                Ok(meta) if meta.is_file() => {
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_millis());
                    oversized.insert(path.clone(), format!("{} {mtime}", meta.len()));
                }
                // A symlink, a directory, or a vanished path: not content.
                Ok(_) => {}
                Err(_) => {
                    oversized.insert(path.clone(), "unreadable".to_string());
                }
            }
        }

        for (paths, flags) in [
            (&scan.deleted, vec!["--force-remove"]),
            (&hashable, vec!["--add", "--remove"]),
        ] {
            if paths.is_empty() {
                continue;
            }
            let mut args: Vec<&str> = vec!["update-index"];
            args.extend(flags);
            args.extend(["-z", "--stdin"]);
            let mut input = paths.join("\0");
            input.push('\0');
            let (code, _) = self.git(&args, Some(&input)).await?;
            if code != 0 {
                return Err(format!("update-index exit {code}"));
            }
        }

        let (code, stdout) = self.git(&["write-tree"], None).await?;
        let tree = stdout.trim().to_string();
        if code != 0 || !is_object_id(&tree) {
            return Err(format!("write-tree exit {code}"));
        }
        // Pin it: an unreferenced tree is a GC candidate, and the diff needs
        // BOTH trees to still exist when the command finishes.
        self.pin(&tree).await?;
        Ok((tree, oversized))
    }

    /// Keep `tree` reachable — oracle `C0r`, which commits it and points the
    /// shadow's own branch at the commit.
    async fn pin(&self, tree: &str) -> Result<(), String> {
        let mut command = Command::new("git");
        command
            .args(["commit-tree", tree, "-m", "snapshot"])
            .env("GIT_DIR", &self.git_dir)
            .env("GIT_WORK_TREE", &self.work_tree)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.git_dir.join("config.global"))
            // Fixed identity and date so the commit id is a function of the
            // tree alone: re-pinning an unchanged tree must not create a new
            // object every shell command.
            .env("GIT_AUTHOR_NAME", "bash-edit-diff")
            .env("GIT_AUTHOR_EMAIL", "bash-edit-diff@localhost")
            .env("GIT_AUTHOR_DATE", "1000000000 +0000")
            .env("GIT_COMMITTER_NAME", "bash-edit-diff")
            .env("GIT_COMMITTER_EMAIL", "bash-edit-diff@localhost")
            .env("GIT_COMMITTER_DATE", "1000000000 +0000")
            .current_dir(&self.work_tree)
            .stdin(Stdio::null());
        let out = command
            .output()
            .await
            .map_err(|e| format!("commit-tree: {e}"))?;
        let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || !is_object_id(&commit) {
            return Err("commit-tree failed".to_string());
        }
        let refs = self.git_dir.join("refs").join("heads");
        tokio::fs::create_dir_all(&refs)
            .await
            .map_err(|e| format!("shadow refs: {e}"))?;
        tokio::fs::write(refs.join("snapshot"), format!("{commit}\n"))
            .await
            .map_err(|e| format!("shadow ref write: {e}"))
    }

    /// `diff-tree` between two snapshots, already capped — oracle `b0r`.
    ///
    /// # Errors
    /// A failed `diff-tree`.
    pub async fn diff(&self, before: &str, after: &str) -> Result<BashEditDiff, String> {
        if before == after {
            return Ok(BashEditDiff::default());
        }
        let (code, stdout) = self
            .git(
                &[
                    "diff-tree",
                    "-r",
                    "--raw",
                    "-p",
                    "--no-color",
                    "--no-renames",
                    before,
                    after,
                ],
                None,
            )
            .await?;
        if code != 0 {
            return Err(format!("diff-tree exit {code}"));
        }
        Ok(parse_diff_tree(&stdout))
    }
}

/// A 40- or 64-hex object id.
#[must_use]
fn is_object_id(candidate: &str) -> bool {
    (candidate.len() == 40 || candidate.len() == 64)
        && candidate.chars().all(|c| c.is_ascii_hexdigit())
}

/// One changed file in the rendered diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffFile {
    /// Repository-relative path.
    pub path: String,
    /// Hunk bodies, `@@` header first. Empty for a created/deleted empty file.
    pub hunks: Vec<Vec<String>>,
    /// The file did not exist before.
    pub created: bool,
    /// The file no longer exists.
    pub deleted: bool,
}

/// What the Bash tool attaches to its result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BashEditDiff {
    /// Files shown with hunks, capped at [`MAX_DIFF_FILES`].
    pub files: Vec<DiffFile>,
    /// Files changed beyond the ones shown.
    pub more_files: usize,
    /// Every changed path, capped at [`MAX_CHANGED_PATHS`] — this is what a
    /// `PostToolUse` hook receives in `tool_response`.
    pub changed_files: Vec<String>,
}

/// Parse `diff-tree -r --raw -p` output into the capped result.
#[must_use]
pub fn parse_diff_tree(output: &str) -> BashEditDiff {
    let mut statuses: Vec<(String, char, String, String)> = Vec::new();
    for line in output.lines() {
        let Some(parsed) = parse_raw_line(line) else {
            continue;
        };
        statuses.push(parsed);
    }
    let hunks = parse_patches(output);

    let mut files: Vec<DiffFile> = Vec::new();
    // Created/deleted EMPTY files first: they have no patch to draw hunks from,
    // so a hunk-driven walk would drop them entirely.
    for (path, status, src, dst) in &statuses {
        if files.len() >= MAX_DIFF_FILES {
            break;
        }
        let empty_add = *status == 'A' && is_empty_blob(dst);
        let empty_delete = *status == 'D' && is_empty_blob(src);
        if empty_add || empty_delete {
            files.push(DiffFile {
                path: path.clone(),
                hunks: Vec::new(),
                created: empty_add,
                deleted: empty_delete,
            });
        }
    }
    for (path, file_hunks) in &hunks {
        if files.len() >= MAX_DIFF_FILES {
            break;
        }
        if !within_hunk_caps(file_hunks) {
            continue;
        }
        let Some((_, status, _, _)) = statuses.iter().find(|(p, _, _, _)| p == path) else {
            continue;
        };
        files.push(DiffFile {
            path: path.clone(),
            // `lines.filter(l => l !== "")` — a bare empty line carries nothing
            // and costs a row.
            hunks: file_hunks
                .iter()
                .map(|h| h.iter().filter(|l| !l.is_empty()).cloned().collect())
                .collect(),
            created: *status == 'A',
            deleted: *status == 'D',
        });
    }

    let total = statuses.len().max(hunks.len());
    let mut changed_files: Vec<String> = statuses.iter().map(|(p, _, _, _)| p.clone()).collect();
    changed_files.truncate(MAX_CHANGED_PATHS);
    BashEditDiff {
        more_files: total.saturating_sub(files.len()),
        files,
        changed_files,
    }
}

/// `^:\d{6} \d{6} (\S+) (\S+) ([A-Z])\t(.+)$` → `(path, status, src, dst)`.
fn parse_raw_line(line: &str) -> Option<(String, char, String, String)> {
    let rest = line.strip_prefix(':')?;
    let (fields, path) = rest.split_once('\t')?;
    let parts: Vec<&str> = fields.split(' ').collect();
    if parts.len() < 5 {
        return None;
    }
    let [src_mode, dst_mode, src, dst, status] = [parts[0], parts[1], parts[2], parts[3], parts[4]];
    if src_mode.len() != 6 || dst_mode.len() != 6 {
        return None;
    }
    let status = status.chars().next().filter(char::is_ascii_uppercase)?;
    let path = unquote_c_path(path);
    is_safe_relative_path(&path).then_some((path, status, src.to_string(), dst.to_string()))
}

/// git C-quotes a path containing unusual bytes; undo it.
#[must_use]
pub fn unquote_c_path(path: &str) -> String {
    let Some(inner) = path
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return path.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Hunks per path, from the `-p` half of the output.
fn parse_patches(output: &str) -> Vec<(String, Vec<Vec<String>>)> {
    let mut out: Vec<(String, Vec<Vec<String>>)> = Vec::new();
    for section in output.split("\ndiff --git ").skip(1) {
        let mut lines = section.lines();
        let Some(header) = lines.next() else { continue };
        let Some(path) = path_from_diff_header(header) else {
            continue;
        };
        let mut hunks: Vec<Vec<String>> = Vec::new();
        let mut current: Option<Vec<String>> = None;
        for line in lines {
            if line.starts_with("@@") {
                if let Some(done) = current.take() {
                    hunks.push(done);
                }
                current = Some(vec![line.to_string()]);
            } else if let Some(hunk) = current.as_mut() {
                if line.starts_with("diff --git ") {
                    break;
                }
                hunk.push(line.to_string());
            }
        }
        if let Some(done) = current.take() {
            hunks.push(done);
        }
        if !hunks.is_empty() {
            out.push((path, hunks));
        }
    }
    out
}

/// `a/<path> b/<path>` — the two halves are equal for a `--no-renames` diff.
fn path_from_diff_header(header: &str) -> Option<String> {
    let header = header.strip_prefix("diff --git ").unwrap_or(header);
    let (a, b) = header.split_once(' ')?;
    let a = unquote_c_path(a);
    let b = unquote_c_path(b);
    let a = a.strip_prefix("a/")?;
    let b = b.strip_prefix("b/")?;
    (a == b && is_safe_relative_path(a)).then(|| a.to_string())
}

/// `qDt` — a file is kept only while its hunks fit BOTH caps.
#[must_use]
pub fn within_hunk_caps(hunks: &[Vec<String>]) -> bool {
    let lines: usize = hunks.iter().map(Vec::len).sum();
    let chars: usize = hunks.iter().flat_map(|h| h.iter()).map(String::len).sum();
    lines <= MAX_FILE_HUNK_LINES && chars <= MAX_FILE_HUNK_CHARS
}

/// Locate the repository containing `cwd`: `(work_tree, git_dir)`.
///
/// `None` when `cwd` is not in a git repository, which is the common case for
/// a shell command and must cost one cheap `rev-parse` rather than an error.
pub async fn discover_repo(cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel", "--absolute-git-dir"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let work_tree = PathBuf::from(lines.next()?.trim());
    let git_dir = PathBuf::from(lines.next()?.trim());
    (work_tree.is_dir() && git_dir.is_dir()).then_some((work_tree, git_dir))
}

/// One repository's shadow directory name — the worktree path, hashed, so two
/// checkouts of the same project never share a snapshot.
#[must_use]
pub fn shadow_dir_for(shadow_root: &Path, work_tree: &Path) -> PathBuf {
    // FNV-1a: this only has to separate two checkouts, not resist anything, and
    // pulling a hash crate into this tool for a directory name is not a trade
    // worth making.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in work_tree.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    shadow_root.join(format!("{hash:016x}"))
}

/// Consecutive failures per work tree, and the repositories already given up
/// on — oracle `h0r`.
///
/// Keyed by the WORK TREE rather than the shadow, so a repo that is retried
/// under a different shadow root still counts as the same repo. Only reached
/// on the failure paths and once per Bash call otherwise.
static REPO_FAILURES: std::sync::Mutex<Option<HashMap<PathBuf, u32>>> = std::sync::Mutex::new(None);

fn with_failures<T>(f: impl FnOnce(&mut HashMap<PathBuf, u32>) -> T) -> T {
    let mut guard = REPO_FAILURES.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

/// Has this work tree failed [`FAILURE_THRESHOLD`] times in a row?
fn repo_given_up(work_tree: &Path) -> bool {
    with_failures(|map| map.get(work_tree).is_some_and(|n| *n >= FAILURE_THRESHOLD))
}

fn note_failure(work_tree: &Path) {
    let count = with_failures(|map| {
        let slot = map.entry(work_tree.to_path_buf()).or_insert(0);
        *slot = slot.saturating_add(1);
        *slot
    });
    if count == FAILURE_THRESHOLD {
        tracing::debug!(
            work_tree = %work_tree.display(),
            "bashEditDiff: disabled for this repository after {FAILURE_THRESHOLD} consecutive failures"
        );
    }
}

fn note_success(work_tree: &Path) {
    with_failures(|map| map.remove(work_tree));
}

/// Forget this work tree's recorded failures. Test-only.
///
/// ⚠️ Deliberately per-repo rather than "clear everything": the ledger is
/// process-global, and a test that wiped it could clear a CONCURRENT test's
/// entries mid-run. Each fixture owns a fresh temp dir, so a per-key reset
/// cannot reach a neighbour.
#[cfg(test)]
fn forget_failures(work_tree: &Path) {
    with_failures(|map| map.remove(work_tree));
}

/// Take the BEFORE snapshot for a command about to run in `cwd`.
///
/// Every failure answers `None`: a diff is a convenience, and a shell command
/// must never fail because git did. Two consecutive failures for one work tree
/// stop the feature for that repository ([`FAILURE_THRESHOLD`]) — a repo git
/// cannot snapshot must not pay the cost on every subsequent command.
pub async fn snapshot_before(shadow_root: &Path, cwd: &Path) -> Option<(ShadowRepo, String)> {
    let (work_tree, git_dir) = discover_repo(cwd).await?;
    if repo_given_up(&work_tree) {
        return None;
    }
    let shadow = ShadowRepo::prepare(
        &work_tree,
        &git_dir,
        &shadow_dir_for(shadow_root, &work_tree),
    )
    .await
    .map_err(|error| tracing::debug!(%error, "bashEditDiff: shadow unavailable"))
    .ok();
    let Some(shadow) = shadow else {
        note_failure(&work_tree);
        return None;
    };
    let snapshot = shadow
        .snapshot()
        .await
        .map_err(|error| tracing::debug!(%error, "bashEditDiff: snapshot failed"))
        .ok();
    let Some((tree, _oversized)) = snapshot else {
        note_failure(&work_tree);
        return None;
    };
    note_success(&work_tree);
    Some((shadow, tree))
}

/// Take the AFTER snapshot and diff it against `before`. `None` when nothing
/// changed or anything went wrong.
///
/// ⚠️ "Nothing changed" is NOT a failure. Only a git error counts towards
/// [`FAILURE_THRESHOLD`] — folding the identical-tree case in would disable the
/// feature after two commands that happened not to touch anything, which is the
/// common case.
pub async fn diff_after(shadow: &ShadowRepo, before: &str) -> Option<BashEditDiff> {
    let snapshot = shadow
        .snapshot()
        .await
        .map_err(|error| tracing::debug!(%error, "bashEditDiff: post-snapshot failed"))
        .ok();
    let Some((after, _oversized)) = snapshot else {
        note_failure(&shadow.work_tree);
        return None;
    };
    if after == before {
        note_success(&shadow.work_tree);
        return None;
    }
    let diff = shadow
        .diff(before, &after)
        .await
        .map_err(|error| tracing::debug!(%error, "bashEditDiff: diff failed"))
        .ok();
    let Some(diff) = diff else {
        note_failure(&shadow.work_tree);
        return None;
    };
    note_success(&shadow.work_tree);
    (!diff.changed_files.is_empty()).then_some(diff)
}

/// Render the diff for the model, appended after the command's output.
#[must_use]
pub fn render(diff: &BashEditDiff) -> String {
    let mut out = String::from("\nFiles changed by this command:\n");
    for file in &diff.files {
        let tag = if file.created {
            " (created)"
        } else if file.deleted {
            " (deleted)"
        } else {
            ""
        };
        out.push_str(&format!("{}{tag}\n", file.path));
        for hunk in &file.hunks {
            for line in hunk {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    if diff.more_files > 0 {
        out.push_str(&format!(
            "… and {} more changed {}\n",
            diff.more_files,
            if diff.more_files == 1 {
                "file"
            } else {
                "files"
            }
        ));
    }
    out
}

/// The gate — oracle `y0r(mode)`, with every input passed in so it is testable
/// without process-global state.
///
/// `trusted_tier` is the value from the USER / FLAG / POLICY tiers only
/// (`Bk(...)[0]`); `merged` is the fully-merged value. The split is the
/// security-relevant half: outside `auto` / `bypassPermissions` only those three
/// tiers may turn the feature ON, so a checked-in project settings file cannot
/// make Claude start reading and diffing the repository's files.
///
/// 🚨 `rollout` (`Aot()`) defaults FALSE without server configuration, so the
/// "on by default in auto mode" arm is dead in an unconfigured install — the
/// same shape as MEM-4's `tengu_onyx_plover`. A faithful port is opt-in.
#[must_use]
pub fn enabled(
    env_override: Option<bool>,
    trusted_tier: Option<bool>,
    merged: Option<bool>,
    permissive_mode: bool,
    rollout: bool,
) -> bool {
    if let Some(value) = env_override {
        return value;
    }
    if trusted_tier == Some(false) || merged == Some(false) {
        return false;
    }
    if trusted_tier == Some(true) {
        return true;
    }
    permissive_mode && rollout
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as SyncCommand;

    fn git(cwd: &Path, args: &[&str]) {
        let out = SyncCommand::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo with one committed file, plus the shadow next to it.
    ///
    /// Everything below resolves `git` from `PATH` — both these fixtures and,
    /// through [`ShadowRepo`], the code under test. The `powershell` tests in
    /// this same binary REPLACE `PATH` (one of them with `""`), so the guard
    /// returned here has to outlive the whole test or the spawn intermittently
    /// fails as `NotFound`. See `crate::test_path_env`.
    async fn repo() -> (
        std::sync::RwLockReadGuard<'static, ()>,
        tempfile::TempDir,
        PathBuf,
        ShadowRepo,
    ) {
        let path_guard = crate::test_path_env::read();
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path().join("repo");
        std::fs::create_dir_all(&wt).unwrap();
        git(&wt, &["init", "-q", "-b", "main"]);
        git(&wt, &["config", "user.email", "t@example.invalid"]);
        git(&wt, &["config", "user.name", "t"]);
        std::fs::write(wt.join("kept.txt"), "one\ntwo\nthree\n").unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "seed"]);
        // 🚨 Build the shadow from the work tree git REPORTS, exactly as
        // `snapshot_before` does. On macOS a temp dir is `/var/...` and git
        // reports `/private/var/...`; a fixture that passed the raw path would
        // give `ShadowRepo::work_tree` a different value from the one the
        // failure ledger is keyed by, and the ledger tests would then assert
        // against a key nothing ever writes.
        let (wt, git_dir) = discover_repo(&wt).await.expect("the fixture is a repo");
        let shadow = ShadowRepo::prepare(&wt, &git_dir, &tmp.path().join("shadow"))
            .await
            .expect("shadow");
        (path_guard, tmp, wt, shadow)
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn an_unchanged_worktree_snapshots_to_the_same_tree() {
        let (_path, _tmp, _wt, shadow) = repo().await;
        let (a, _) = shadow.snapshot().await.expect("first");
        let (b, _) = shadow.snapshot().await.expect("second");
        assert_eq!(a, b, "nothing changed, so the tree id must not move");
        assert_eq!(
            shadow.diff(&a, &b).await.expect("diff"),
            BashEditDiff::default(),
            "identical trees produce no diff at all"
        );
    }

    /// 🔒 The safety property. A snapshot must leave the user's repository
    /// exactly as it found it — same index, same refs, same status.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn snapshotting_does_not_touch_the_real_repository() {
        let (_path, _tmp, wt, shadow) = repo().await;
        std::fs::write(wt.join("scratch.txt"), "untracked\n").unwrap();

        let before_index = std::fs::read(wt.join(".git").join("index")).expect("index");
        let status_before = SyncCommand::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&wt)
            .output()
            .unwrap()
            .stdout;
        let head_before = SyncCommand::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&wt)
            .output()
            .unwrap()
            .stdout;

        shadow.snapshot().await.expect("snapshot");

        assert_eq!(
            std::fs::read(wt.join(".git").join("index")).expect("index"),
            before_index,
            "the real index was written — a shell command must never stage the user's tree"
        );
        assert_eq!(
            SyncCommand::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&wt)
                .output()
                .unwrap()
                .stdout,
            status_before
        );
        assert_eq!(
            SyncCommand::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&wt)
                .output()
                .unwrap()
                .stdout,
            head_before
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn an_edit_to_a_tracked_file_produces_its_hunks() {
        let (_path, _tmp, wt, shadow) = repo().await;
        let (before, _) = shadow.snapshot().await.expect("before");
        std::fs::write(wt.join("kept.txt"), "one\nCHANGED\nthree\n").unwrap();
        let (after, _) = shadow.snapshot().await.expect("after");
        assert_ne!(before, after);

        let diff = shadow.diff(&before, &after).await.expect("diff");
        assert_eq!(diff.changed_files, vec!["kept.txt".to_string()]);
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path, "kept.txt");
        assert!(!diff.files[0].created && !diff.files[0].deleted);
        let body = diff.files[0].hunks.concat().join("\n");
        assert!(body.contains("-two"), "{body}");
        assert!(body.contains("+CHANGED"), "{body}");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn an_untracked_new_file_is_part_of_the_snapshot() {
        // Without this, every file a command CREATES would be invisible — which
        // is most of what a shell command does.
        let (_path, _tmp, wt, shadow) = repo().await;
        let (before, _) = shadow.snapshot().await.expect("before");
        std::fs::write(wt.join("fresh.txt"), "hello\n").unwrap();
        let (after, _) = shadow.snapshot().await.expect("after");
        let diff = shadow.diff(&before, &after).await.expect("diff");
        assert_eq!(diff.changed_files, vec!["fresh.txt".to_string()]);
        assert!(diff.files[0].created);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_created_empty_file_is_reported_with_no_hunks() {
        // The `c0r` arm. A hunk-driven walk would drop it: an empty file has
        // no patch body at all.
        let (_path, _tmp, wt, shadow) = repo().await;
        let (before, _) = shadow.snapshot().await.expect("before");
        std::fs::write(wt.join("empty.txt"), "").unwrap();
        let (after, _) = shadow.snapshot().await.expect("after");
        let diff = shadow.diff(&before, &after).await.expect("diff");
        assert_eq!(diff.files.len(), 1, "{diff:?}");
        assert_eq!(diff.files[0].path, "empty.txt");
        assert!(diff.files[0].created);
        assert!(diff.files[0].hunks.is_empty());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_deleted_file_is_reported_as_deleted() {
        let (_path, _tmp, wt, shadow) = repo().await;
        let (before, _) = shadow.snapshot().await.expect("before");
        std::fs::remove_file(wt.join("kept.txt")).unwrap();
        let (after, _) = shadow.snapshot().await.expect("after");
        let diff = shadow.diff(&before, &after).await.expect("diff");
        assert_eq!(diff.changed_files, vec!["kept.txt".to_string()]);
        assert!(diff.files[0].deleted);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_gitignored_file_is_not_a_change() {
        let (_path, _tmp, wt, shadow) = repo().await;
        std::fs::write(wt.join(".gitignore"), "build/\n").unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "ignore"]);
        let (before, _) = shadow.snapshot().await.expect("before");
        std::fs::create_dir_all(wt.join("build")).unwrap();
        std::fs::write(wt.join("build").join("out.o"), "junk\n").unwrap();
        let (after, _) = shadow.snapshot().await.expect("after");
        assert_eq!(
            before, after,
            "an ignored artefact must not show as a change"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_file_over_the_size_cap_is_never_hashed() {
        // It becomes a changed PATH with no content — the trade that keeps a
        // shell command from hashing gigabytes.
        let (_path, _tmp, wt, shadow) = repo().await;
        let big = wt.join("big.bin");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(LARGE_FILE_BYTES + 1).unwrap();
        drop(file);
        let (_tree, oversized) = shadow.snapshot().await.expect("snapshot");
        assert!(
            oversized.contains_key("big.bin"),
            "the large file must land in the side ledger: {oversized:?}"
        );
    }

    #[test]
    fn the_empty_blob_check_is_not_a_binary_check() {
        assert!(is_empty_blob("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"));
        assert!(is_empty_blob("e69de29"), "abbreviated ids are matched too");
        assert!(is_empty_blob(
            "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813"
        ));
        assert!(!is_empty_blob("0000000000000000000000000000000000000000"));
        assert!(!is_empty_blob(""), "an empty sha is not the empty blob");
    }

    #[test]
    fn a_path_that_reaches_into_git_is_refused() {
        assert!(is_safe_relative_path("src/main.rs"));
        assert!(!is_safe_relative_path("/etc/passwd"));
        assert!(!is_safe_relative_path("../escape"));
        assert!(!is_safe_relative_path(".git/config"));
        assert!(!is_safe_relative_path(".GIT/config"), "case-folded");
        assert!(!is_safe_relative_path("git~1/config"), "8.3 short name");
        assert!(!is_safe_relative_path(".git./config"), "trailing dot");
        assert!(!is_safe_relative_path("a\u{0}b"));
        assert!(!is_safe_relative_path(""));
    }

    #[test]
    fn status_v2_skips_records_with_no_worktree_change() {
        // `XY` with `.` in the worktree column is staged-only; the snapshot is
        // about what is on disk.
        let scan = parse_status_v2(
            "1 M. N... 100644 100644 100644 aaa bbb staged-only.txt\0\
             1 .M N... 100644 100644 100644 aaa bbb edited.txt\0\
             1 .D N... 100644 100644 100644 aaa bbb gone.txt\0\
             ? fresh.txt\0? subdir/\0",
        );
        assert_eq!(scan.changed, vec!["edited.txt", "fresh.txt"]);
        assert_eq!(scan.deleted, vec!["gone.txt"]);
        assert_eq!(scan.untracked_directories, vec!["subdir/"]);
    }

    #[test]
    fn the_hunk_caps_reject_on_either_axis() {
        let short = vec![vec!["@@".to_string(), "+x".to_string()]];
        assert!(within_hunk_caps(&short));
        let many_lines = vec![vec!["x".to_string(); MAX_FILE_HUNK_LINES + 1]];
        assert!(!within_hunk_caps(&many_lines), "line cap");
        let long_lines = vec![vec!["x".repeat(MAX_FILE_HUNK_CHARS + 1)]];
        assert!(!within_hunk_caps(&long_lines), "char cap");
    }

    #[test]
    fn pathspec_batches_stay_under_the_byte_cap() {
        let paths: Vec<String> = (0..5_000).map(|i| format!("dir{i}/file")).collect();
        let batches = pathspec_batches(&paths);
        assert!(batches.len() > 1, "5000 paths must not be one argv");
        for batch in &batches {
            let bytes: usize = batch.iter().map(|p| p.len() + 1).sum();
            assert!(bytes <= PATHSPEC_BATCH_BYTES + 64, "{bytes}");
        }
        assert_eq!(batches.concat(), paths, "no path may be dropped");
    }

    #[test]
    fn a_c_quoted_path_is_unquoted() {
        assert_eq!(unquote_c_path("plain.txt"), "plain.txt");
        assert_eq!(unquote_c_path(r#""with space.txt""#), "with space.txt");
        assert_eq!(unquote_c_path(r#""tab\there.txt""#), "tab\there.txt");
    }

    #[test]
    fn the_gate_is_opt_in_and_a_project_cannot_turn_it_on() {
        // Env wins outright, in both directions.
        assert!(enabled(Some(true), Some(false), Some(false), false, false));
        assert!(!enabled(Some(false), Some(true), Some(true), true, true));
        // A `false` from either the trusted tier or the merged value wins.
        assert!(!enabled(None, Some(false), None, true, true));
        assert!(!enabled(None, None, Some(false), true, true));
        // Only the trusted tier may turn it on outside a permissive mode…
        assert!(enabled(None, Some(true), Some(true), false, false));
        // …a merged-only `true` (i.e. project/local settings) may not.
        assert!(!enabled(None, None, Some(true), false, false));
        // 🚨 And the auto-mode arm needs the rollout flag, which defaults off:
        // unconfigured, the feature is OFF.
        assert!(!enabled(None, None, None, true, false));
        assert!(enabled(None, None, None, true, true));
    }

    /// Oracle `_0r` / `A2s`. The NARROWNESS is the property under test: the
    /// regex is anchored at both ends and every argument must begin with a
    /// non-`-` character, so only a bare `git <subcommand> <refs…>` matches.
    #[test]
    fn only_a_bare_branch_moving_git_call_suppresses_the_diff() {
        for command in [
            "git checkout main",
            "  git switch feature/x",
            "sudo git reset",
            "git\tstash",
            "git cherry-pick abc123 def456  ",
            "git restore src/lib.rs",
        ] {
            assert!(
                is_branch_moving_git(command),
                "{command:?} moves the worktree wholesale"
            );
        }
        for command in [
            // A FLAG starts with `-`, which the argument pattern rejects — so a
            // `git reset --hard` still gets its diff.
            "git reset --hard",
            "git checkout -b new",
            // Anchored at both ends: a compound command may also edit files.
            "git checkout main && echo hi",
            "echo hi; git checkout main",
            "git checkout main | tee log",
            // A quoted argument is not a plain ref.
            "git checkout \"my branch\"",
            // Not one of the eleven subcommands.
            "git commit -am wip",
            "git status",
            // Not git at all.
            "gitk",
            "mygit checkout main",
            "",
        ] {
            assert!(
                !is_branch_moving_git(command),
                "{command:?} must still be diffed"
            );
        }
    }

    /// Oracle `eNt` creates the shadow with mode 448. The shadow index lists
    /// every path in the user's worktree; another local user must not be able
    /// to read it.
    #[cfg(unix)]
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn the_shadow_directory_is_private_to_this_user() {
        use std::os::unix::fs::PermissionsExt;
        let (_path, _tmp, _wt, shadow) = repo().await;
        let mode = std::fs::metadata(&shadow.git_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "the shadow git directory must be 0700, not {mode:o}"
        );
    }

    /// `h0r` — two consecutive failures stop the feature for that repository.
    ///
    /// The failure is produced by making the shadow ROOT a regular file, so
    /// `ShadowRepo::prepare` cannot create the shadow directory under it. The
    /// third call then answers `None` even with a perfectly good shadow root,
    /// which is the whole point: the ledger, not the current attempt, decides.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn two_consecutive_failures_give_up_on_the_repository() {
        let (_path, tmp, wt, _shadow) = repo().await;

        let blocked = tmp.path().join("not-a-dir");
        std::fs::write(&blocked, b"").unwrap();
        assert!(
            snapshot_before(&blocked, &wt).await.is_none(),
            "a shadow root that is a file cannot produce a snapshot"
        );
        assert!(!repo_given_up(&wt), "one failure is not enough to give up");
        assert!(snapshot_before(&blocked, &wt).await.is_none());
        assert!(
            repo_given_up(&wt),
            "{FAILURE_THRESHOLD} consecutive failures must disable this repository"
        );

        let good = tmp.path().join("shadow-2");
        assert!(
            snapshot_before(&good, &wt).await.is_none(),
            "the repository is given up on, so a usable shadow root changes nothing"
        );
        forget_failures(&wt);
        assert!(
            snapshot_before(&good, &wt).await.is_some(),
            "and the same call succeeds once the ledger is cleared, proving the \
             give-up was what refused it rather than the shadow root"
        );
    }

    /// Oracle `x0r` ends `…, XKe.set(n,pe), ZKe.delete(e), pe` — a successful
    /// snapshot CLEARS the count. Without that, the threshold counts lifetime
    /// failures rather than consecutive ones, and a repo that hiccuped twice in
    /// an hour-long session is disabled for the rest of it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_success_clears_the_failure_count() {
        let (_path, tmp, wt, _shadow) = repo().await;
        let blocked = tmp.path().join("not-a-dir");
        std::fs::write(&blocked, b"").unwrap();
        let good = tmp.path().join("shadow-ok");

        assert!(snapshot_before(&blocked, &wt).await.is_none(), "failure 1");
        assert!(snapshot_before(&good, &wt).await.is_some(), "a success");
        assert!(snapshot_before(&blocked, &wt).await.is_none(), "failure 2");
        assert!(
            !repo_given_up(&wt),
            "two failures either side of a SUCCESS are not consecutive"
        );
        forget_failures(&wt);
    }

    /// 🚨 A command that changed nothing is the COMMON case. Counting it as a
    /// failure would disable the feature after two harmless commands.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn an_unchanged_tree_is_not_counted_as_a_failure() {
        let (_path, _tmp, wt, shadow) = repo().await;
        let (before, _) = shadow.snapshot().await.expect("before");

        for _ in 0..(FAILURE_THRESHOLD + 2) {
            assert!(
                diff_after(&shadow, &before).await.is_none(),
                "nothing changed, so there is no diff"
            );
        }
        assert!(
            !repo_given_up(&wt),
            "an identical tree is not a git failure and must not count towards the threshold"
        );
    }
}

//! WIZARD-06 — the recon gather's real I/O (2.1.220).
//!
//! The producers in [`crate::auto_mode_producers`] are pure; this module is
//! where they actually touch the disk and spawn subprocesses. Keeping the split
//! means the rendering logic is tested without a filesystem, and the containment
//! rules live in exactly one place.
//!
//! Everything here reads a repository that may be hostile, so three rules hold
//! throughout:
//!
//! * **Never follow a symlink.** Reads use `O_NOFOLLOW`, and
//!   [`contained_read`] additionally requires the canonicalised target to be
//!   exactly `root/relative` — so an intermediate symlinked directory is
//!   refused even though each individual component resolved.
//! * **Never let the repo run code.** Every `git` invocation carries
//!   [`crate::auto_mode_producers::GIT_HARDENING_FLAGS`], which disables hooks,
//!   the filesystem monitor and the credential prompt.
//! * **Never block forever.** Subprocesses are killed at
//!   [`crate::auto_mode_producers::SUBPROCESS_TIMEOUT_MS`], and reads are capped.
//!
//! The glob-backed scans run IN-PROCESS on the `ignore` crate — claude-code's
//! own "embedded ripgrep" mode, and what `tools/file`'s Grep already does here.
//! Spawning `rg` instead would make every glob-backed recon section depend on a
//! binary being on `PATH`, and a missing binary renders "nothing found" rather
//! than "could not look" — the exact confusion the rest of this subsystem
//! works to prevent.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::auto_mode_producers::{
    DocSource, LocalSettingsSource, RepoFactsSource, SettingsReconSource, WalkLimit,
    DOC_GLOB_LIMIT, DOC_GLOB_MAX_DEPTH, DOC_READ_CAP_LINGXI_MD, GIT_HARDENING_FLAGS,
    SUBPROCESS_TIMEOUT_MS,
};

/// `s$s` — the longest path `eNd` will keep.
const MAX_GLOB_PATH_LEN: usize = 256;

/// `jIe`'s truncation suffix. Distinct from `Z1d`'s: this one reports the
/// character cut AND the file's real size, so a truncated read can never pass
/// for a whole one.
fn truncated_suffix(cap: usize, size: u64) -> String {
    format!("\n\u{2026}[truncated at {cap} chars of {size} bytes]")
}

/// `jIe` — open with `O_NOFOLLOW`, require a regular file, read up to `cap`
/// characters, and mark the result when it was cut.
///
/// Returns `None` for anything that is not a readable regular file, including a
/// symlinked final component.
#[must_use]
pub fn secure_read_capped(path: &Path, cap: usize, require_nlink1: bool) -> Option<String> {
    use std::io::Read;

    #[cfg(unix)]
    let opened = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    };
    #[cfg(not(unix))]
    let opened = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => return None,
        Ok(_) => std::fs::File::open(path),
        Err(_) => return None,
    };

    let file = opened.ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    #[cfg(unix)]
    if require_nlink1 {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return None;
        }
    }
    #[cfg(not(unix))]
    let _ = require_nlink1;

    let size = meta.len();
    // `Math.min(size, cap + 1)` — one past the cap is enough to detect a cut.
    let want = size.min(cap as u64 + 1);
    let mut bytes = Vec::new();
    (&file).take(want).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();

    // The cut is by CHARACTERS, not bytes, and never splits one.
    if text.chars().count() > cap {
        let head: String = text.chars().take(cap).collect();
        return Some(format!("{head}{}", truncated_suffix(cap, size)));
    }
    Some(text)
}

/// `jhr` — read `relative` from inside `root`, refusing any indirection.
///
/// Both paths are canonicalised; the target must land strictly under the root
/// AND be exactly `root/relative`. That second check is what rejects a
/// symlinked intermediate directory: each component resolves fine, but the
/// resolved path is no longer the one that was asked for.
#[must_use]
pub fn contained_read(root: &Path, relative: &str, cap: usize) -> Option<String> {
    let root_c = std::fs::canonicalize(root).ok()?;
    let target_c = std::fs::canonicalize(root.join(relative)).ok()?;
    let rel = target_c.strip_prefix(&root_c).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    if target_c != root_c.join(relative) {
        return None;
    }
    secure_read_capped(&target_c, cap, false)
}

/// Does any component of `root/relative` not exist, or is any of them a symlink?
///
/// `None` when a component could not be stat'd at all (which the caller reads
/// as "absent"), `Some(true)` when one is a symlink.
#[must_use]
pub fn path_symlink_check(root: &Path, relative: &str) -> Option<bool> {
    let mut at = root.to_path_buf();
    for part in relative.split('/') {
        at.push(part);
        let meta = std::fs::symlink_metadata(&at).ok()?;
        if meta.file_type().is_symlink() {
            return Some(true);
        }
    }
    Some(false)
}

/// Run a command, killing it at `timeout`. `None` only when it could not be
/// spawned at all.
///
/// On timeout the child is killed and whatever it had already written is
/// returned with `timed_out = true` — a streaming scan needs its partial
/// results AND the fact that they are partial.
///
/// stdout is drained on its own thread: an 8 MB pipe would otherwise fill and
/// deadlock the child while we sat polling for its exit.
fn run_capped(cmd: Command, timeout: Duration) -> Option<(i32, String, bool)> {
    run_capped_full(cmd, timeout).map(|(c, out, _err, t)| (c, out, t))
}

/// As [`run_capped`], but also returns stderr — `gh`'s availability check reads it.
fn run_capped_full(mut cmd: Command, timeout: Duration) -> Option<(i32, String, String, bool)> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    use std::io::Read;
    let drain = |mut pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(p) = pipe.as_mut() {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout: Option<Box<dyn std::io::Read + Send>> = child
        .stdout
        .take()
        .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>);
    let stderr: Option<Box<dyn std::io::Read + Send>> = child
        .stderr
        .take()
        .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>);
    let reader = drain(stdout);
    let err_reader = drain(stderr);

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    let partial = reader.join().unwrap_or_default();
                    let errs = err_reader.join().unwrap_or_default();
                    return Some((
                        -1,
                        String::from_utf8_lossy(&partial).into_owned(),
                        String::from_utf8_lossy(&errs).into_owned(),
                        true,
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = reader.join();
                let _ = err_reader.join();
                return None;
            }
        }
    };

    let bytes = reader.join().ok()?;
    let errs = err_reader.join().unwrap_or_default();
    Some((
        status.code().unwrap_or(-1),
        String::from_utf8_lossy(&bytes).into_owned(),
        String::from_utf8_lossy(&errs).into_owned(),
        false,
    ))
}

fn recon_timeout() -> Duration {
    Duration::from_millis(SUBPROCESS_TIMEOUT_MS)
}

/// `hFt` — `git -C <root> <hardening flags> <args>`; stdout with one trailing
/// newline removed, or `""` on any failure or non-zero exit.
#[must_use]
pub fn git_output(root: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root);
    cmd.args(GIT_HARDENING_FLAGS);
    cmd.args(args);
    match run_capped(cmd, recon_timeout()) {
        Some((0, out, false)) => out.strip_suffix('\n').unwrap_or(&out).to_string(),
        _ => String::new(),
    }
}

/// `Vsy` — how many lines the command produced; `0` unless it exited cleanly
/// with output.
#[must_use]
pub fn git_line_count(root: &Path, args: &[&str]) -> usize {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root);
    cmd.args(GIT_HARDENING_FLAGS);
    cmd.args(args);
    match run_capped(cmd, recon_timeout()) {
        Some((0, out, false)) if !out.is_empty() => out.matches('\n').count(),
        _ => 0,
    }
}

/// `eNd` — de-duplicate, drop over-long paths, sort, cap.
fn finalize_glob(mut paths: Vec<String>, limit: usize) -> Vec<String> {
    paths.retain(|p| p.len() <= MAX_GLOB_PATH_LEN);
    paths.sort_unstable();
    paths.dedup();
    paths.truncate(limit);
    paths
}

/// `RPo` — list files under `root` matching `patterns`.
///
/// Runs IN-PROCESS on the `ignore` crate rather than spawning `rg`. That is
/// claude-code's own "embedded ripgrep" mode, and it is what `tools/file`'s
/// Grep already does here — `ignore` is the same library ripgrep is built on.
/// Spawning instead would make every glob-backed recon section depend on a
/// binary being on `PATH`, and when it is not there the section renders
/// "nothing found" rather than "could not look", which is precisely the
/// confusion the rest of this subsystem works to avoid.
///
/// `.git` and `node_modules` are excluded, the walk is depth-limited, hidden
/// files are included, and `.gitignore` is respected — matching
/// `rg --files --hidden --max-depth N -g '!.git' -g '!node_modules'`.
#[must_use]
pub fn rg_files(
    root: &Path,
    patterns: &[&str],
    limit: usize,
    depth: usize,
    keep: Option<&regex::Regex>,
) -> Vec<String> {
    let Some(overrides) = build_overrides(root, patterns) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .max_depth(Some(depth))
        .overrides(overrides)
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if keep.is_none_or(|re| re.is_match(&rel)) {
            out.push(rel);
        }
    }
    finalize_glob(out, limit)
}

/// Build the `-g` override set: the two standing exclusions plus `patterns`.
fn build_overrides(root: &Path, patterns: &[&str]) -> Option<ignore::overrides::Override> {
    let mut builder = ignore::overrides::OverrideBuilder::new(root);
    builder.add("!.git").ok()?;
    builder.add("!node_modules").ok()?;
    for p in patterns {
        builder.add(p).ok()?;
    }
    builder.build().ok()
}

// ── network paths ────────────────────────────────────────────────────────────

/// `tu` — a UNC path (`\\server\share` or `//server/share`).
fn is_unc(path: &str) -> bool {
    let mut chars = path.chars();
    matches!(chars.next(), Some('/' | '\\')) && matches!(chars.next(), Some('/' | '\\'))
}

/// `em` — a WSL path (`\\wsl$\…`, `\\wsl.localhost\…`).
///
/// UNC-SHAPED but local, which is why it is carved out of [`is_network_path`].
fn is_wsl_path(path: &str) -> bool {
    if !is_unc(path) {
        return false;
    }
    let rest = &path[2..];
    let lower = rest.to_lowercase();
    let after = if let Some(a) = lower.strip_prefix("wsl$") {
        a
    } else if let Some(a) = lower.strip_prefix("wsl.localhost") {
        a
    } else {
        return false;
    };
    after.starts_with('/') || after.starts_with('\\')
}

/// `vEi`/`d_e` — an autofs `/net/<host>/…` path.
fn is_autofs_net(path: &str) -> bool {
    if !path.starts_with('/') {
        return false;
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            parts.pop();
            continue;
        }
        parts.push(part);
        if parts.len() == 2 && parts[0].to_lowercase() == "net" {
            return true;
        }
    }
    false
}

/// `K7e` — would touching this path reach a NETWORK host?
///
/// A UNC share or an autofs `/net/<host>` mount is not merely slow: merely
/// stat-ing one authenticates to, or at minimum resolves, the named host. The
/// recon refuses to walk such a path at all rather than attempting it and
/// reporting the result — see
/// [`crate::auto_mode_gates::HOME_REPOS_NETWORK_HOME`].
///
/// WSL paths are UNC-shaped but local, so they are explicitly not network.
#[must_use]
pub fn is_network_path(path: &str) -> bool {
    (is_unc(path) && !is_wsl_path(path)) || is_autofs_net(path)
}

// ── tail reads ───────────────────────────────────────────────────────────────

/// The outcome of a tail read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailRead {
    /// The file is not there — skip it silently.
    Absent,
    /// It exists but could not be read; the caller marks the gather PARTIAL
    /// rather than pretending the file held nothing.
    Unreadable,
    /// The last `cap` bytes, and whether the head was cut off.
    Read {
        /// The decoded content.
        content: String,
        /// The file was longer than `cap`.
        truncated: bool,
    },
}

/// `jIe`'s `fromTail` branch — read the LAST `cap` bytes of a file.
///
/// Shell history grows at the end, so the recent commands are the tail. When
/// the read starts mid-file the first partial line is dropped, so a command is
/// never reported with its head sliced off.
#[must_use]
pub fn secure_read_tail(path: &Path, cap: u64, require_nlink1: bool) -> TailRead {
    use std::io::{Read, Seek, SeekFrom};

    #[cfg(unix)]
    let opened = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    };
    #[cfg(not(unix))]
    let opened = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => return TailRead::Unreadable,
        Ok(_) => std::fs::File::open(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return TailRead::Absent,
        Err(_) => return TailRead::Unreadable,
    };

    let mut file = match opened {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return TailRead::Absent,
        Err(_) => return TailRead::Unreadable,
    };
    let Ok(meta) = file.metadata() else {
        return TailRead::Unreadable;
    };
    if !meta.is_file() {
        return TailRead::Unreadable;
    }
    #[cfg(unix)]
    if require_nlink1 {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return TailRead::Unreadable;
        }
    }
    #[cfg(not(unix))]
    let _ = require_nlink1;

    let size = meta.len();
    let from = size.saturating_sub(cap);
    if file.seek(SeekFrom::Start(from)).is_err() {
        return TailRead::Unreadable;
    }
    let mut bytes = Vec::new();
    if file.take(size - from).read_to_end(&mut bytes).is_err() {
        return TailRead::Unreadable;
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();

    if from == 0 {
        return TailRead::Read {
            content: text,
            truncated: false,
        };
    }
    // Started mid-file: drop the partial first line.
    let content = match text.find('\n') {
        Some(i) => text[i + 1..].to_string(),
        None => String::new(),
    };
    TailRead::Read {
        content,
        truncated: true,
    }
}

// ── read-deny across path aliases ────────────────────────────────────────────

/// `esy` — re-express `path` as if it sat under `to` instead of `from`.
///
/// `None` when `path` is not under `from` at all. Comparison is
/// case-insensitive on Windows, matching the platform's own path semantics.
#[must_use]
pub fn rebase_path(path: &str, from: &str, to: &str, windows: bool) -> Option<String> {
    let norm = |s: &str| {
        if windows {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };
    let (p, f) = (norm(path), norm(from));
    if p == f {
        return Some(to.to_string());
    }
    let sep = if windows { '\\' } else { '/' };
    let prefix = if f.ends_with(sep) {
        f
    } else {
        format!("{f}{sep}")
    };
    if !p.starts_with(&prefix) {
        return None;
    }
    Some(format!("{to}{}", &path[from.len()..]))
}

/// `Smt` — is `path` read-denied under EITHER spelling of a directory that has
/// two names?
///
/// A home directory routinely has two: the one the environment reports and the
/// one `realpath` resolves it to (`/home/u` vs `/mnt/data/u`, `/var/...` vs
/// `/private/var/...`). A `permissions.deny` rule is written against one of
/// them. Checking only the path as given would let the other spelling walk
/// straight past a rule the user wrote — so the path is rebased between the two
/// and the deny predicate is asked about each.
///
/// `is_denied` is injected because the deny set lives with the session policy,
/// not with the gatherer.
#[must_use]
pub fn denied_under_either_alias(
    path: &str,
    dir_a: &str,
    dir_b: &str,
    is_denied: &dyn Fn(&str) -> bool,
    windows: bool,
) -> bool {
    if is_denied(path) {
        return true;
    }
    if dir_a == dir_b {
        return false;
    }
    for (from, to) in [(dir_a, dir_b), (dir_b, dir_a)] {
        if let Some(alias) = rebase_path(path, from, to, windows) {
            if is_denied(&alias) {
                return true;
            }
        }
    }
    false
}

// ── the assembled gather ──────────────────────────────────────────────────────

/// Every recon producer wired to the real filesystem.
///
/// The seven producers with real I/O run; the four whose I/O is not wired yet
/// (the home walk, the projects-root enumeration and the two `gh` capabilities)
/// fall back to [`crate::auto_mode_pregather::ReconProducers`]'s default, which
/// renders the failure marker. That is the designed degradation path: those
/// sections report "data unavailable" rather than an empty result.
///
/// Note which ones those are — all four are GATED. With the conservative
/// answers (`scope=project`, `depth=here`) none of them is even reached, so the
/// block is complete.
pub struct FsReconProducers {
    root: PathBuf,
    user_config_dir: PathBuf,
    transcript_dir: PathBuf,
    classify_all_shell: bool,
    /// Q2 = `all` — whether the org repo split may be fetched.
    org_split: bool,
    /// Whether outbound `gh` traffic is permitted at all.
    nonessential_traffic_allowed: bool,
}

impl FsReconProducers {
    /// Wire the producers for `root`, with the user config and transcript
    /// directories they read outside it.
    #[must_use]
    pub fn new(
        root: impl Into<PathBuf>,
        user_config_dir: impl Into<PathBuf>,
        transcript_dir: impl Into<PathBuf>,
        classify_all_shell: bool,
    ) -> Self {
        Self {
            root: root.into(),
            user_config_dir: user_config_dir.into(),
            transcript_dir: transcript_dir.into(),
            classify_all_shell,
            org_split: false,
            // `gh` reaches github.com, which is exactly the outbound traffic
            // `LINGXI_DISABLE_NONESSENTIAL_TRAFFIC` exists to stop. Resolve it
            // from the live setting rather than assuming allowed: a host that
            // opted out must not have the wizard contact GitHub on its behalf.
            nonessential_traffic_allowed: !platform_api::traffic_mode::is_essential_traffic_only(),
        }
    }

    /// Set the Q2 = `all` org-split gate. Defaults to closed, so a caller that
    /// forgets to thread the answer through fetches LESS, not more.
    #[must_use]
    pub fn with_org_split(mut self, allowed: bool) -> Self {
        self.org_split = allowed;
        self
    }

    /// Override whether outbound `gh` traffic is allowed at all.
    ///
    /// Defaults to the live `LINGXI_DISABLE_NONESSENTIAL_TRAFFIC` setting; this
    /// exists for tests and for hosts that gate it differently.
    #[must_use]
    pub fn with_nonessential_traffic(mut self, allowed: bool) -> Self {
        self.nonessential_traffic_allowed = allowed;
        self
    }
}

impl crate::auto_mode_pregather::ReconProducers for FsReconProducers {
    fn project_docs(&self) -> Result<String, ()> {
        Ok(crate::auto_mode_producers::project_docs_section(
            &FsDocSource::new(&self.root, &self.user_config_dir),
        ))
    }
    fn repo_facts(&self) -> Result<String, ()> {
        Ok(
            crate::auto_mode_producers::repo_facts_section(&FsRepoFactsSource::new(&self.root))
                .body,
        )
    }
    fn existing_settings(&self) -> Result<String, ()> {
        let (local, _) = crate::auto_mode_producers::local_settings_block(
            &FsLocalSettingsSource::new(&self.root),
        );
        crate::auto_mode_producers::existing_settings_section(&FsSettingsReconSource::new(
            self.user_config_dir.join("settings.json"),
            local,
            self.classify_all_shell,
        ))
    }
    fn project_usage(&self) -> Result<String, ()> {
        Ok(crate::auto_mode_producers::project_usage_section(
            &FsProjectUsageSource::new(&self.transcript_dir),
        ))
    }
    fn config_scans(&self) -> Result<String, ()> {
        Ok(crate::auto_mode_producers::config_scans_section(
            &FsConfigScanSource::new(&self.root),
        ))
    }
    fn default_labels(&self) -> Result<String, ()> {
        Ok(crate::auto_mode_producers::default_labels_section())
    }
    fn shell_history(&self) -> Result<String, ()> {
        Ok(crate::auto_mode_producers::shell_history_section(
            &FsShellHistorySource::from_process_env(),
        ))
    }
    fn home_repos(&self) -> Result<String, ()> {
        let home = home_dir().ok_or(())?;
        let (repos, limit) = walk_home_repos(&home);
        Ok(crate::auto_mode_producers::home_repos_body(&repos, limit))
    }
    fn all_projects_usage(&self) -> Result<String, ()> {
        // The transcript dir is `<projects_root>/<this project>`; the sweep
        // covers its siblings and excludes this one.
        let projects_root = self.transcript_dir.parent().ok_or(())?;
        Ok(crate::auto_mode_producers::all_projects_usage_section(
            &FsAllProjectsSource::new(projects_root, Some(self.transcript_dir.clone())),
        ))
    }
    fn repo_visibility(&self) -> Result<String, ()> {
        let outcome = crate::auto_mode_producers::repo_visibility_section(
            &GhCli::new(&self.root),
            self.org_split,
            self.nonessential_traffic_allowed,
        );
        if outcome.failures.any() {
            telemetry::emit_auto_mode_pregather("visibility_gh_failed");
        }
        if outcome.org_list_parse_failed {
            telemetry::emit_auto_mode_pregather("org_list_gh_parse_failed");
        }
        Ok(outcome.body)
    }
    fn sibling_docs(&self) -> Result<String, ()> {
        use crate::auto_mode_producers::GhSource;
        let gh = GhCli::new(&self.root);
        // The org and this repo's name come from the SAME origin remote the
        // visibility section parses, so an unrecognised remote yields no
        // sibling lookups rather than a guessed org.
        let reduced = gh
            .origin_remote()
            .and_then(|url| crate::auto_mode_producers::remote_to_host_org_repo(url.trim(), None))
            .ok_or(())?;
        let parts: Vec<&str> = reduced.split('/').collect();
        let [_, org, repo] = parts.as_slice() else {
            return Err(());
        };
        Ok(crate::auto_mode_producers::sibling_docs_body(
            org, repo, &gh,
        ))
    }
}

/// The user's home directory, from the process environment.
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

// ── concrete sources ─────────────────────────────────────────────────────────

/// Real-filesystem [`DocSource`] rooted at a project directory.
pub struct FsDocSource {
    root: PathBuf,
    user_config_dir: PathBuf,
    doc_glob: regex::Regex,
}

impl FsDocSource {
    /// Build a source for `root`, with the user's config directory for the
    /// user-level `LINGXI.md`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, user_config_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            user_config_dir: user_config_dir.into(),
            // `^\.lingxi\/(skills|rules|agents)\/`
            doc_glob: regex::Regex::new(r"^\.lingxi/(skills|rules|agents)/").expect("static regex"),
        }
    }
}

impl DocSource for FsDocSource {
    fn user_lingxi_md(&self) -> Option<String> {
        secure_read_capped(
            &self.user_config_dir.join("LINGXI.md"),
            DOC_READ_CAP_LINGXI_MD,
            false,
        )
    }
    fn project_file(&self, relative: &str, cap: usize) -> Option<String> {
        contained_read(&self.root, relative, cap)
    }
    fn lingxi_doc_paths(&self) -> Vec<String> {
        rg_files(
            &self.root,
            &["SKILL.md", "*.md"],
            DOC_GLOB_LIMIT,
            DOC_GLOB_MAX_DEPTH,
            Some(&self.doc_glob),
        )
    }
}

/// Real-filesystem [`RepoFactsSource`].
pub struct FsRepoFactsSource {
    root: PathBuf,
}

impl FsRepoFactsSource {
    /// Build a source for the repository at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl RepoFactsSource for FsRepoFactsSource {
    fn git(&self, args: &[&str]) -> String {
        git_output(&self.root, args)
    }
    fn git_line_count(&self, args: &[&str]) -> usize {
        git_line_count(&self.root, args)
    }
    fn path_has_symlink_component(&self, relative: &str) -> Option<bool> {
        path_symlink_check(&self.root, relative)
    }
    fn read_file(&self, relative: &str, cap: usize) -> Option<String> {
        contained_read(&self.root, relative, cap)
    }
    fn repo_path(&self) -> String {
        self.root.display().to_string()
    }
}

/// Real-filesystem [`crate::auto_mode_producers::ConfigScanSource`].
pub struct FsConfigScanSource {
    root: PathBuf,
}

impl FsConfigScanSource {
    /// Build a source for the repository at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl crate::auto_mode_producers::ConfigScanSource for FsConfigScanSource {
    fn scan_files(&self, globs: &[&str], path_filter: Option<&regex::Regex>) -> Vec<String> {
        use crate::auto_mode_producers::{
            CONFIG_SCAN_FILE_LIMIT, CONFIG_SCAN_READ_CAP, DOC_GLOB_MAX_DEPTH,
        };
        rg_files(
            &self.root,
            globs,
            CONFIG_SCAN_FILE_LIMIT,
            DOC_GLOB_MAX_DEPTH,
            path_filter,
        )
        .into_iter()
        .filter_map(|p| contained_read(&self.root, &p, CONFIG_SCAN_READ_CAP))
        .collect()
    }

    fn list_paths(
        &self,
        globs: &[&str],
        limit: usize,
        depth: usize,
        filter: Option<&regex::Regex>,
    ) -> Vec<String> {
        rg_files(&self.root, globs, limit, depth, filter)
    }

    fn package_json(&self) -> Option<String> {
        contained_read(
            &self.root,
            "package.json",
            crate::auto_mode_producers::PACKAGE_JSON_READ_CAP,
        )
    }

    fn bucket_scan(&self) -> Option<crate::auto_mode_producers::BucketScan> {
        use crate::auto_mode_producers::{
            bucket_prefix_clusters, extract_bucket_names, BucketCount, BucketScan,
            BUCKET_SCAN_DISTINCT_CAP, BUCKET_SCAN_GLOBS, BUCKET_SCAN_MAX_FILESIZE_BYTES,
            BUCKET_SCAN_TIMEOUT_MS, DOC_GLOB_MAX_DEPTH, FLAGGED_LIST_CAP,
        };

        // In-process, like `rg_files` — see the note there.
        let overrides = build_overrides(&self.root, &BUCKET_SCAN_GLOBS)?;
        let deadline = Instant::now() + Duration::from_millis(BUCKET_SCAN_TIMEOUT_MS);

        struct Tally {
            occurrences: usize,
            files: usize,
            last_file: String,
        }
        let mut tallies: std::collections::HashMap<String, Tally> =
            std::collections::HashMap::new();
        let mut hit_cap = false;
        let mut timed_out = false;

        let walker = ignore::WalkBuilder::new(&self.root)
            .hidden(false)
            .max_depth(Some(DOC_GLOB_MAX_DEPTH))
            .overrides(overrides)
            .build();

        'files: for entry in walker.flatten() {
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            // `--max-filesize`: an oversized config file is skipped, not read.
            if entry
                .metadata()
                .is_ok_and(|m| m.len() > BUCKET_SCAN_MAX_FILESIZE_BYTES)
            {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(&self.root) else {
                continue;
            };
            let path = rel.to_string_lossy().replace('\\', "/");
            let Ok(content) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            for name in extract_bucket_names(&content) {
                if name.len() > 256 {
                    continue;
                }
                if let Some(t) = tallies.get_mut(&name) {
                    t.occurrences += 1;
                    if t.last_file != path {
                        t.files += 1;
                        t.last_file = path.clone();
                    }
                } else {
                    if tallies.len() >= BUCKET_SCAN_DISTINCT_CAP {
                        hit_cap = true;
                        break 'files;
                    }
                    tallies.insert(
                        name,
                        Tally {
                            occurrences: 1,
                            files: 1,
                            last_file: path.clone(),
                        },
                    );
                }
            }
        }

        let mut top: Vec<(String, BucketCount)> = tallies
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    BucketCount {
                        occurrences: v.occurrences,
                        files: v.files,
                    },
                )
            })
            .collect();
        top.sort_by(|a, b| {
            b.1.occurrences
                .cmp(&a.1.occurrences)
                .then_with(|| a.0.cmp(&b.0))
        });
        let distinct = top.len();
        let names: Vec<String> = top.iter().map(|(k, _)| k.clone()).collect();
        top.truncate(FLAGGED_LIST_CAP);

        Some(BucketScan {
            top,
            distinct,
            clusters: bucket_prefix_clusters(names.iter()),
            truncated: hit_cap || timed_out,
        })
    }
}

/// Real-filesystem [`crate::auto_mode_producers::ProjectUsageSource`].
pub struct FsProjectUsageSource {
    transcript_dir: PathBuf,
}

impl FsProjectUsageSource {
    /// Build a source for this project's transcript directory.
    #[must_use]
    pub fn new(transcript_dir: impl Into<PathBuf>) -> Self {
        Self {
            transcript_dir: transcript_dir.into(),
        }
    }
}

impl crate::auto_mode_producers::ProjectUsageSource for FsProjectUsageSource {
    fn transcripts(&self) -> Option<Vec<crate::auto_mode_producers::TranscriptFile>> {
        use crate::auto_mode_producers::{TranscriptFile, TRANSCRIPT_FILE_LIMIT};
        // An unreadable directory is "no history", matching the oracle's catch.
        let entries = std::fs::read_dir(&self.transcript_dir).ok()?;
        let mut found: Vec<(std::time::SystemTime, TranscriptFile)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            found.push((
                mtime,
                TranscriptFile {
                    path,
                    size: meta.len(),
                },
            ));
        }
        // Newest first, then capped -- so a busy project's OLD transcripts are
        // what falls off, not its recent ones.
        found.sort_by(|a, b| b.0.cmp(&a.0));
        found.truncate(TRANSCRIPT_FILE_LIMIT);
        Some(found.into_iter().map(|(_, f)| f).collect())
    }

    fn read_transcript(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
}

/// Real-filesystem [`LocalSettingsSource`] for `.lingxi/settings.local.json`.
pub struct FsLocalSettingsSource {
    root: PathBuf,
}

impl FsLocalSettingsSource {
    /// Build a source for the project at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    fn dir(&self) -> PathBuf {
        self.root.join(".lingxi")
    }
    fn file(&self) -> PathBuf {
        self.dir().join("settings.local.json")
    }
}

impl LocalSettingsSource for FsLocalSettingsSource {
    fn lingxi_dir(&self) -> Option<bool> {
        let meta = std::fs::symlink_metadata(self.dir()).ok()?;
        Some(meta.is_dir())
    }
    fn local_file(&self) -> Option<(bool, u64, u64)> {
        let meta = std::fs::symlink_metadata(self.file()).ok()?;
        #[cfg(unix)]
        let nlink = {
            use std::os::unix::fs::MetadataExt;
            meta.nlink()
        };
        #[cfg(not(unix))]
        let nlink = 1u64;
        Some((meta.is_file(), nlink, meta.len()))
    }
    fn read_local(&self) -> Option<String> {
        secure_read_capped(
            &self.file(),
            crate::auto_mode_producers::SETTINGS_READ_CAP,
            true,
        )
    }
    fn tracked_in_git(&self) -> bool {
        !git_output(
            &self.root,
            &["ls-files", "--", ".lingxi/settings.local.json"],
        )
        .is_empty()
    }
}

/// Real-filesystem [`SettingsReconSource`].
pub struct FsSettingsReconSource {
    settings_path: PathBuf,
    local_block: String,
    classify_all_shell: bool,
}

impl FsSettingsReconSource {
    /// Build a source for `settings_path`, with the already-rendered
    /// project-local sub-block and the `classifyAllShell` flag.
    #[must_use]
    pub fn new(
        settings_path: impl Into<PathBuf>,
        local_block: String,
        classify_all_shell: bool,
    ) -> Self {
        Self {
            settings_path: settings_path.into(),
            local_block,
            classify_all_shell,
        }
    }
}

impl SettingsReconSource for FsSettingsReconSource {
    fn user_settings(&self) -> Result<Option<String>, ()> {
        match std::fs::symlink_metadata(&self.settings_path) {
            // Absent is fine; anything else present-but-unreadable is not.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(()),
            Ok(_) => secure_read_capped(
                &self.settings_path,
                crate::auto_mode_producers::SETTINGS_READ_CAP,
                false,
            )
            .map(Some)
            .ok_or(()),
        }
    }
    fn local_block(&self) -> String {
        self.local_block.clone()
    }
    fn classify_all_shell(&self) -> bool {
        self.classify_all_shell
    }
}

/// Real-subprocess [`GhSource`] — the `gh` calls `x1d` and `Xsy` make.
///
/// Every invocation runs with [`gh_env_overrides`] applied. That is the
/// security-relevant part: it pins `GH_HOST` to github.com, always clears the
/// ENTERPRISE tokens, and — when the user's own `GH_HOST` named a DIFFERENT
/// server — clears `GH_TOKEN`/`GITHUB_TOKEN` too, because those credentials
/// belong to that server and this call is about to go to github.com.
pub struct GhCli {
    root: PathBuf,
    overrides: Vec<(&'static str, Option<String>)>,
}

impl GhCli {
    /// Build a runner for the repository at `root`, resolving the token
    /// overrides against the caller's `GH_HOST`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let current_host = std::env::var("GH_HOST").ok();
        Self {
            root: root.into(),
            overrides: crate::auto_mode_producers::gh_env_overrides(current_host.as_deref()),
        }
    }

    fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        for (key, value) in &self.overrides {
            match value {
                Some(v) => cmd.env(key, v),
                None => cmd.env_remove(key),
            };
        }
        cmd
    }
}

impl crate::auto_mode_producers::GhSource for GhCli {
    fn origin_remote(&self) -> Option<String> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.root);
        cmd.args(GIT_HARDENING_FLAGS);
        cmd.args(["remote", "get-url", "origin"]);
        let (code, stdout, _) = run_capped(
            cmd,
            Duration::from_millis(crate::auto_mode_producers::GH_TIMEOUT_MS),
        )?;
        (code == 0).then_some(stdout)
    }

    fn gh(&self, args: &[&str], max_buffer: usize) -> crate::auto_mode_producers::GhResult {
        use crate::auto_mode_producers::{GhResult, GH_TIMEOUT_MS};
        let mut cmd = self.command("gh");
        cmd.args(args);
        match run_capped_full(cmd, Duration::from_millis(GH_TIMEOUT_MS)) {
            Some((code, mut stdout, stderr, _timed_out)) => {
                cap_at_char_boundary(&mut stdout, max_buffer);
                GhResult {
                    code,
                    stdout,
                    stderr,
                }
            }
            // `gh` not on PATH spawns nothing. 127 is the shell's own
            // "command not found", which `gh_is_unavailable` reads as an
            // ordinary environment fact rather than a breakage worth recording.
            None => GhResult {
                code: 127,
                stdout: String::new(),
                stderr: String::new(),
            },
        }
    }
}

impl crate::auto_mode_producers::SiblingDocsSource for GhCli {
    fn list_org_repos(&self, org: &str) -> Option<String> {
        use crate::auto_mode_producers::GhSource;
        let res = self.gh(
            &[
                "repo",
                "list",
                org,
                "--limit",
                "100",
                "--json",
                "name,visibility,pushedAt",
            ],
            256_000,
        );
        (res.code == 0).then_some(res.stdout)
    }

    fn fetch_doc(&self, org: &str, repo: &str, doc: &str) -> Option<String> {
        use crate::auto_mode_producers::{is_valid_repo_name, GhSource};
        // `org` and `repo` are interpolated into an API path, so both must
        // survive the name shape before the call is made — `..` here would be
        // path traversal against the GitHub API.
        if !is_valid_repo_name(org) || !is_valid_repo_name(repo) {
            return None;
        }
        let res = self.gh(
            &[
                "api",
                &format!("repos/{org}/{repo}/contents/{doc}"),
                "--jq",
                ".content",
            ],
            262_144,
        );
        if res.code != 0 {
            return None;
        }
        // `contents` returns base64 with embedded newlines.
        let packed: String = res.stdout.split_whitespace().collect();
        if packed.is_empty() {
            return None;
        }
        base64_decode(&packed).and_then(|b| String::from_utf8(b).ok())
    }
}

/// Truncate `s` to at most `max_bytes`, cutting only at a character boundary.
///
/// The cut lands at the END of the last character that fits ENTIRELY within the
/// cap. Cutting at "the last character's start offset + 1" would land INSIDE a
/// multi-byte character, and `String::truncate` panics on a non-boundary — so
/// any non-ASCII byte in a subprocess's output would abort the whole wizard.
fn cap_at_char_boundary(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let cut = s
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|end| *end <= max_bytes)
        .last()
        .unwrap_or(0);
    s.truncate(cut);
}

/// Decode standard base64, ignoring padding. `None` on any invalid input.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = u32::try_from(TABLE.iter().position(|c| *c == byte)?).ok()?;
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// `j1d`'s walk — find git checkouts under the home directory.
///
/// Returns the repos found and HOW the walk ended. The [`WalkLimit`] is not a
/// diagnostic: a walk that stopped at a budget saw a prefix of the home
/// directory, and reporting it as a finished walk would let a proposal conclude
/// "the user has 20 repos" from a truncated sweep.
///
/// What is read is deliberately tiny — only each repo's `config` for its remote
/// URLs, never any tracked content. A repo whose gitdir points OUTSIDE the home
/// directory is recorded and skipped rather than followed, so a planted `.git`
/// file cannot redirect the walk into an arbitrary path.
#[must_use]
pub fn walk_home_repos(home: &Path) -> (Vec<crate::auto_mode_producers::HomeRepo>, WalkLimit) {
    use crate::auto_mode_producers::{
        home_relative, is_skipped_walk_dir, HomeRepo, HOME_WALK_MAX_DEPTH, HOME_WALK_MAX_DIRS,
        HOME_WALK_MAX_REPOS, HOME_WALK_TIMEOUT_MS,
    };

    let windows = cfg!(windows);
    let macos = cfg!(target_os = "macos");
    let home_str = home.to_string_lossy().into_owned();

    if is_network_path(&home_str) {
        return (Vec::new(), WalkLimit::NetworkHome);
    }
    if std::fs::read_dir(home).is_err() {
        return (Vec::new(), WalkLimit::HomeUnreadable);
    }

    let started = Instant::now();
    let deadline = Duration::from_millis(HOME_WALK_TIMEOUT_MS);
    let mut repos: Vec<HomeRepo> = Vec::new();
    let mut limit = WalkLimit::None;
    let mut visited = 0usize;
    // Breadth-first, so the shallow (and far likelier) checkouts are found
    // before a budget runs out deep in one subtree.
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> =
        std::collections::VecDeque::from([(home.to_path_buf(), 0usize)]);

    while let Some((dir, depth)) = queue.pop_front() {
        if started.elapsed() >= deadline {
            limit = WalkLimit::Timeout;
            break;
        }
        if visited >= HOME_WALK_MAX_DIRS {
            limit = WalkLimit::VisitBudget;
            break;
        }
        visited += 1;

        let git_marker = dir.join(".git");
        if let Ok(meta) = std::fs::symlink_metadata(&git_marker) {
            if repos.len() >= HOME_WALK_MAX_REPOS {
                limit = WalkLimit::RepoCap;
                break;
            }
            let path = home_relative(&dir.to_string_lossy(), &home_str, windows);
            let (remotes, note) = read_repo_remotes(&git_marker, &meta, &home_str, windows);
            repos.push(HomeRepo {
                path,
                remotes,
                note,
            });
            // A repository is a leaf: its own subdirectories are its working
            // tree, not more checkouts worth enumerating.
            continue;
        }

        if depth >= HOME_WALK_MAX_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // Never traverse a symlink: it can point anywhere, including back
            // into the tree, and following one would make the budget meaningless.
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_dir() {
                continue;
            }
            let Ok(link) = entry.file_type() else {
                continue;
            };
            if link.is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_skipped_walk_dir(&name, windows, macos) {
                continue;
            }
            queue.push_back((entry.path(), depth + 1));
        }
    }

    // Hitting the cap on the LAST repo is not a truncated walk unless there was
    // more to see; the queue still holding work is what makes it one.
    if limit == WalkLimit::None && repos.len() >= HOME_WALK_MAX_REPOS && !queue.is_empty() {
        limit = WalkLimit::RepoCap;
    }
    (repos, limit)
}

/// Read one repo's reduced remotes from its `config`.
///
/// `.git` is a directory in a normal checkout and a FILE holding a
/// `gitdir: <path>` pointer in a worktree or submodule. The pointer is followed
/// only when it stays under the home directory — the same containment the walk
/// itself respects.
fn read_repo_remotes(
    git_marker: &Path,
    meta: &std::fs::Metadata,
    home: &str,
    windows: bool,
) -> (
    Vec<String>,
    Option<crate::auto_mode_producers::HomeRepoNote>,
) {
    use crate::auto_mode_producers::{
        config_remotes, is_strictly_under, parse_gitdir_pointer, HomeRepoNote,
    };

    let git_dir = if meta.is_dir() {
        git_marker.to_path_buf()
    } else {
        let Some(text) = secure_read_capped(git_marker, GITDIR_POINTER_CAP, false) else {
            return (Vec::new(), Some(HomeRepoNote::NoRemote));
        };
        let Some(pointer) = parse_gitdir_pointer(text.trim()) else {
            return (Vec::new(), Some(HomeRepoNote::NoRemote));
        };
        let resolved = if Path::new(&pointer).is_absolute() {
            PathBuf::from(&pointer)
        } else {
            git_marker.parent().unwrap_or(Path::new(".")).join(&pointer)
        };
        let canonical = std::fs::canonicalize(&resolved).unwrap_or(resolved);
        if !is_strictly_under(&canonical.to_string_lossy(), home, windows) {
            return (Vec::new(), Some(HomeRepoNote::GitdirOutsideHome));
        }
        canonical
    };

    // A linked worktree's `config` lives in the COMMON dir; `commondir` points
    // at it. Without this a worktree reports no remotes even though it has them.
    let config_dir = match secure_read_capped(&git_dir.join("commondir"), GITDIR_POINTER_CAP, false)
    {
        Some(text) => {
            let rel = text.trim();
            let joined = if Path::new(rel).is_absolute() {
                PathBuf::from(rel)
            } else {
                git_dir.join(rel)
            };
            let canonical = std::fs::canonicalize(&joined).unwrap_or(joined);
            if is_strictly_under(&canonical.to_string_lossy(), home, windows) {
                canonical
            } else {
                return (Vec::new(), Some(HomeRepoNote::GitdirOutsideHome));
            }
        }
        None => git_dir,
    };

    match secure_read_capped(&config_dir.join("config"), GIT_CONFIG_READ_CAP, false) {
        Some(config) => {
            let remotes = config_remotes(&config, None);
            if remotes.is_empty() {
                (Vec::new(), Some(HomeRepoNote::NoRemote))
            } else {
                (remotes, None)
            }
        }
        None => (Vec::new(), Some(HomeRepoNote::NoRemote)),
    }
}

/// A `.git` pointer file / `commondir` is one short line.
const GITDIR_POINTER_CAP: usize = 4_096;
/// A repo `config` worth parsing for remote URLs.
const GIT_CONFIG_READ_CAP: usize = 262_144;

/// Real-filesystem [`AllProjectsSource`] — the `W1d` sweep over every project's
/// transcripts under `<config>/projects/`.
///
/// Every budget this walk can hit (stat cap, file limit, per-file cap,
/// aggregate cap, deadline) is RECORDED on the scan rather than silently
/// applied. A sweep that saw half the projects must not read as "these are all
/// the projects", so each shortfall renders its own line.
pub struct FsAllProjectsSource {
    projects_root: PathBuf,
    /// This project's own transcript directory, excluded from the sweep — it is
    /// already reported by the `iay` section, and counting it twice would
    /// inflate the cross-project signal with local noise.
    exclude: Option<PathBuf>,
}

impl FsAllProjectsSource {
    /// Build a sweep over `projects_root`, skipping `exclude`.
    #[must_use]
    pub fn new(projects_root: impl Into<PathBuf>, exclude: Option<PathBuf>) -> Self {
        Self {
            projects_root: projects_root.into(),
            exclude,
        }
    }
}

impl crate::auto_mode_producers::AllProjectsSource for FsAllProjectsSource {
    fn scan(&self) -> Option<crate::auto_mode_producers::AllProjectsScan> {
        use crate::auto_mode_producers::{
            AllProjectsScan, ALL_PROJECTS_AGGREGATE_CAP, ALL_PROJECTS_DEADLINE_MS,
            ALL_PROJECTS_FILE_LIMIT, ALL_PROJECTS_PER_FILE_CAP, ALL_PROJECTS_STAT_CAP,
        };

        let started = std::time::Instant::now();
        let deadline = std::time::Duration::from_millis(ALL_PROJECTS_DEADLINE_MS);
        // An absent or unreadable projects root is "not queryable", not "no
        // usage" — the producer renders the unavailable marker for `None`.
        let project_dirs = std::fs::read_dir(&self.projects_root).ok()?;

        let mut scan = AllProjectsScan::default();
        let mut candidates: Vec<(std::time::SystemTime, PathBuf, u64)> = Vec::new();
        let mut stat_count = 0usize;

        for dir in project_dirs.flatten() {
            let dir_path = dir.path();
            if !dir_path.is_dir() || self.exclude.as_ref() == Some(&dir_path) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir_path) else {
                // A directory we could not list is a hole in coverage, and is
                // counted as one.
                scan.unreadable_dirs += 1;
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                if stat_count >= ALL_PROJECTS_STAT_CAP {
                    scan.enumeration_capped = true;
                    break;
                }
                stat_count += 1;
                scan.enumerated += 1;
                match entry.metadata() {
                    Ok(meta) => candidates.push((
                        meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                        path,
                        meta.len(),
                    )),
                    Err(_) => scan.stat_failed += 1,
                }
            }
            if scan.enumeration_capped {
                break;
            }
        }

        // Newest first, so a cap drops the OLDEST transcripts rather than an
        // arbitrary slice.
        candidates.sort_by(|a, b| b.0.cmp(&a.0));
        candidates.truncate(ALL_PROJECTS_FILE_LIMIT);
        scan.selected = candidates.len();

        let mut read_bytes = 0u64;
        let mut commands: Vec<String> = Vec::new();
        // The cross-project section reports command WORDS only; denials belong
        // to the per-project section, so they are mined and dropped here.
        let mut denials: Vec<String> = Vec::new();
        let denial_re = crate::auto_mode_producers::denial_reason_regex();
        for (index, (_, path, size)) in candidates.iter().enumerate() {
            let remaining = || scan.selected - index;
            if started.elapsed() >= deadline {
                scan.deadline_remaining = Some(remaining());
                break;
            }
            if read_bytes + size.min(&ALL_PROJECTS_PER_FILE_CAP) > ALL_PROJECTS_AGGREGATE_CAP {
                scan.aggregate_capped_remaining = Some(remaining());
                break;
            }
            if *size > ALL_PROJECTS_PER_FILE_CAP {
                scan.per_file_capped += 1;
                continue;
            }
            // No `scan.denied` accounting here: a standalone gather has no
            // loaded session policy, so there is no `permissions.deny` read
            // overlay to consult. Counting zero refusals is accurate — the gate
            // is absent, not passing everything.
            let Ok(text) = std::fs::read_to_string(path) else {
                scan.unreadable += 1;
                continue;
            };
            read_bytes += *size;
            scan.scanned += 1;
            crate::auto_mode_producers::mine_transcript_text(
                &text,
                &denial_re,
                &mut commands,
                &mut denials,
            );
        }

        scan.commands_seen = commands.len();
        drop(denials);
        scan.words = crate::auto_mode_producers::command_words_of(&commands);
        // Any shortfall means the word list is a sample, not a census.
        scan.words_incomplete = scan.enumeration_capped
            || scan.deadline_remaining.is_some()
            || scan.aggregate_capped_remaining.is_some()
            || scan.per_file_capped > 0
            || scan.unreadable > 0
            || scan.denied > 0
            || scan.stat_failed > 0
            || scan.unreadable_dirs > 0;
        Some(scan)
    }
}

/// Real-filesystem [`ShellHistorySource`].
///
/// Reads only the TAIL of each history file and only the command word off each
/// line — the arguments, which are where secrets live (`curl -H "Authorization:
/// …"`, `mysql -p…`), never leave this module.
pub struct FsShellHistorySource {
    env: crate::auto_mode_producers::ShellHistoryEnv,
}

impl FsShellHistorySource {
    /// Resolve the history environment from the real process environment.
    #[must_use]
    pub fn from_process_env() -> Self {
        let windows = cfg!(windows);
        let home_dir = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())
            .unwrap_or_default();
        Self {
            env: crate::auto_mode_producers::ShellHistoryEnv {
                windows,
                home_dir,
                app_data: std::env::var("APPDATA").ok(),
                xdg_data_home: std::env::var("XDG_DATA_HOME").ok(),
                hist_file: std::env::var("HISTFILE").ok(),
            },
        }
    }

    /// Build a source over an explicit environment (tests, and hosts that
    /// resolve the home directory differently).
    #[must_use]
    pub fn new(env: crate::auto_mode_producers::ShellHistoryEnv) -> Self {
        Self { env }
    }
}

impl crate::auto_mode_producers::ShellHistorySource for FsShellHistorySource {
    fn env(&self) -> crate::auto_mode_producers::ShellHistoryEnv {
        self.env.clone()
    }

    fn home_is_network(&self) -> bool {
        // A network home means every history read is a remote round-trip that
        // can hang; the producer reports the gate rather than blocking the
        // wizard on a mount that may never answer.
        is_network_path(&self.env.home_dir)
    }

    fn read_tail(
        &self,
        source: &crate::auto_mode_producers::HistorySource,
    ) -> Result<Option<(String, bool)>, ()> {
        match secure_read_tail(
            &source.path,
            crate::auto_mode_producers::HISTORY_TAIL_BYTES,
            false,
        ) {
            TailRead::Absent => Ok(None),
            // Present but unreadable is NOT the same as absent: the caller marks
            // the gather partial so the proposal is not drawn from a history it
            // could not see.
            TailRead::Unreadable => Err(()),
            TailRead::Read { content, truncated } => Ok(Some((content, truncated))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, rel: &str, body: &str) -> PathBuf {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn a_capped_read_announces_its_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "big.txt", &"x".repeat(100));
        let got = secure_read_capped(&p, 10, false).unwrap();
        assert!(got.starts_with(&"x".repeat(10)));
        assert!(got.contains("\u{2026}[truncated at 10 chars of 100 bytes]"));
        // Under the cap, no marker.
        assert_eq!(
            secure_read_capped(&p, 1000, false).unwrap(),
            "x".repeat(100)
        );
    }

    #[test]
    fn the_cap_is_a_byte_window_but_the_marker_is_character_counted() {
        // The oracle reads `min(size, cap + 1)` BYTES and then compares the
        // DECODED length against the cap. For ASCII those coincide, so the
        // marker fires. For multi-byte content the byte window yields fewer
        // characters than the cap, the comparison never trips, and the read
        // comes back short WITHOUT a marker. That asymmetry is the oracle's;
        // reproducing it keeps the block byte-identical, and it is safe in the
        // direction that matters -- the model sees less than the file, never
        // more, and never a partial character.
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "u.txt", &"路".repeat(20));
        let got = secure_read_capped(&p, 5, false).unwrap();
        // 6 bytes read -> 2 whole characters, no marker.
        assert_eq!(got, "路".repeat(2));
        assert!(!got.contains("truncated"));
        // Whatever comes back is always valid, whole characters.
        assert!(got.chars().all(|c| c == '路'));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "real.txt", "secret");
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(dir.path().join("real.txt"), &link).unwrap();
        assert_eq!(secure_read_capped(&link, 100, false), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_hardlinked_file_is_refused_when_nlink_is_checked() {
        let dir = tempfile::tempdir().unwrap();
        let real = write(dir.path(), "real.txt", "secret");
        let alias = dir.path().join("alias.txt");
        std::fs::hard_link(&real, &alias).unwrap();
        assert_eq!(secure_read_capped(&alias, 100, true), None);
        // ...but allowed when the caller does not require it.
        assert_eq!(
            secure_read_capped(&alias, 100, false).as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn a_contained_read_stays_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "inside.txt", "ok");
        write(dir.path(), "sub/deep.txt", "deep");
        assert_eq!(
            contained_read(dir.path(), "inside.txt", 100).as_deref(),
            Some("ok")
        );
        assert_eq!(
            contained_read(dir.path(), "sub/deep.txt", 100).as_deref(),
            Some("deep")
        );
        // Escapes are refused.
        assert_eq!(contained_read(dir.path(), "../outside.txt", 100), None);
        assert_eq!(contained_read(dir.path(), "missing.txt", 100), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_intermediate_directory_is_refused() {
        // Each component resolves, but the resolved path is no longer the one
        // that was asked for -- which is exactly the case the equality check
        // exists to catch.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "loot.txt", "secret");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("docs")).unwrap();
        assert_eq!(contained_read(dir.path(), "docs/loot.txt", 100), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_symlink_component_check_reports_each_case() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a/b.txt", "x");
        std::os::unix::fs::symlink(dir.path().join("a"), dir.path().join("link")).unwrap();
        assert_eq!(path_symlink_check(dir.path(), "a/b.txt"), Some(false));
        assert_eq!(path_symlink_check(dir.path(), "link/b.txt"), Some(true));
        assert_eq!(path_symlink_check(dir.path(), "nope/x.txt"), None);
    }

    #[test]
    fn a_hanging_subprocess_is_killed_at_the_timeout() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let started = Instant::now();
        let (_, _, timed_out) = run_capped(cmd, Duration::from_millis(300)).unwrap();
        assert!(timed_out, "must report that it was cut short");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must not wait for the child"
        );
    }

    #[test]
    fn git_output_is_empty_for_a_non_repo_and_hardened_for_a_real_one() {
        let dir = tempfile::tempdir().unwrap();
        // Not a repository -> non-zero exit -> empty, never an error.
        assert_eq!(git_output(dir.path(), &["remote"]), "");
        assert_eq!(git_line_count(dir.path(), &["ls-files"]), 0);

        // A real repo: the hardening flags must not break ordinary use.
        if Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .is_ok_and(|s| s.success())
        {
            write(dir.path(), "f.txt", "hi");
            let _ = Command::new("git")
                .args(["add", "f.txt"])
                .current_dir(dir.path())
                .status();
            assert_eq!(git_line_count(dir.path(), &["ls-files"]), 1);
        }
    }

    #[test]
    fn settings_recon_distinguishes_absent_from_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("settings.json");
        let src = FsSettingsReconSource::new(&missing, String::new(), false);
        assert_eq!(src.user_settings(), Ok(None), "absent is not an error");

        let present = write(dir.path(), "s.json", "{}");
        let src = FsSettingsReconSource::new(&present, String::new(), false);
        assert_eq!(src.user_settings(), Ok(Some("{}".to_string())));

        // Present but a symlink -> the no-follow read fails -> Err, which the
        // producer turns into "settings file present but unreadable".
        #[cfg(unix)]
        {
            let link = dir.path().join("link.json");
            std::os::unix::fs::symlink(&present, &link).unwrap();
            let src = FsSettingsReconSource::new(&link, String::new(), false);
            assert_eq!(src.user_settings(), Err(()));
        }
    }

    #[test]
    fn the_four_ported_producers_run_against_a_real_repository() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let config = dir.path().join("config");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&config).unwrap();

        write(&root, "LINGXI.md", "project rules");
        write(&root, ".gitignore", "target/\n.env\nMY_TOKEN\n");
        write(
            &root,
            "Makefile",
            "build:\n\tcargo build\ndeploy:\n\techo go\n",
        );
        write(
            &root,
            "package.json",
            r#"{"scripts":{"build":"tsc","test":"vitest"}}"#,
        );
        write(
            &root,
            "deploy.yaml",
            "logs: s3://acme-logs/x\ndata: gs://acme-data/y\nagain: s3://acme-logs/z\n",
        );
        // A real transcript for the project-usage producer to mine.
        write(
            &root,
            ".transcripts/session.jsonl",
            &[
                serde_json::json!({"message":{"content":[
                    {"type":"tool_use","name":"Bash","input":{"command":"terraform apply -token=SECRETVALUE"}}
                ]}}).to_string(),
                serde_json::json!({"message":{"content":[
                    {"type":"tool_use","name":"Bash","input":{"command":"curl https://api.acme.io/v1"}}
                ]}}).to_string(),
            ]
            .join("\n"),
        );
        write(&config, "LINGXI.md", "user rules");
        write(
            &config,
            "settings.json",
            &serde_json::json!({
                "permissions": { "allow": ["Bash(*)", "Bash(rm -rf *)", "Read(src/**)"] }
            })
            .to_string(),
        );
        write(
            &root,
            ".lingxi/settings.local.json",
            &serde_json::json!({ "autoMode": { "allow": ["Bash(x:*)"] } }).to_string(),
        );

        let producers = FsReconProducers::new(
            root.clone(),
            config.clone(),
            root.join(".transcripts"),
            false,
        );
        let block = crate::auto_mode_pregather::build_recon_block(
            crate::auto_mode_pregather::GatherOptions::default(),
            &producers,
        );

        // Every ungated producer is ported now, so nothing degrades.
        assert!(
            block.failed_sections.is_empty(),
            "unexpected failures: {:?}",
            block.failed_sections
        );
        // ...docs read off the real disk,
        assert!(block.text.contains("#### ~/.lingxi/LINGXI.md"));
        assert!(block.text.contains("\"user rules\""));
        assert!(block.text.contains("\"project rules\""));
        // ...repo facts ran git and reported the sensitive gitignore lines,
        assert!(block.text.contains("Repo path: "));
        assert!(block.text.contains("- `.env`"));
        assert!(block.text.contains("- `MY_TOKEN`"));
        assert!(!block.text.contains("target/"));
        // ...the two flagged lists split correctly off the real settings file,
        assert!(block.text.contains("- `Bash(*)`"));
        assert!(block.text.contains("- `Bash(rm -rf *)`"));
        assert!(!block.text.contains("Read(src/**)"));
        // ...the project-local autoMode keys were reported as found content,
        assert!(block.text.contains("NOT pre-approved config"));
        assert!(block.text.contains("Bash(x:*)"));
        // ...config scans read package.json directly (no glob needed),
        assert!(block.text.contains("#### package.json scripts"));
        assert!(block.text.contains("- build"));
        assert!(block.text.contains("- test"));
        // ...and the glob-backed scans, which need no external binary.
        assert!(block.text.contains("#### Makefile/justfile targets"));
        assert!(block.text.contains("- build"));
        assert!(block.text.contains("- deploy"));
        // The repo-wide bucket scan found the names in the config file.
        assert!(block.text.contains("#### Bucket names in config"));
        assert!(block.text.contains("- acme-logs"));
        assert!(block.text.contains("- acme-data"));
        // ...and the transcript miner reported NAMES ONLY.
        assert!(block
            .text
            .contains("Transcripts scanned: 1; Bash commands seen: 2"));
        assert!(block.text.contains("- terraform (1\u{d7})"));
        assert!(block.text.contains("- api.acme.io (1\u{d7})"));
        assert!(
            !block.text.contains("SECRETVALUE"),
            "a secret in a command line must never reach the block"
        );
        // ...and the shipped default labels are listed.
        assert!(block.text.contains("#### Default allow labels"));
        assert!(block.text.contains("- Read-Only Operations"));

        // The five gated producers were withheld without being run.
        assert_eq!(block.gated_sections.len(), 5);
        assert!(block.text.contains("_NOT GATHERED"));
        // Every one of the eleven sections is present either way.
        for title in crate::auto_mode_pregather::SECTION_TITLES {
            assert!(block.text.contains(title), "missing section: {title}");
        }
    }

    #[test]
    fn network_paths_are_recognised_so_they_are_never_touched() {
        // Merely stat-ing one authenticates to, or resolves, the named host.
        for p in [
            r"\\server\share\x",
            "//server/share/x",
            "/net/fileserver/home/u",
            "/net/host",
        ] {
            assert!(is_network_path(p), "{p} must be treated as network");
        }
        // WSL is UNC-SHAPED but local.
        for p in [r"\\wsl$\Ubuntu\home\u", r"\\wsl.localhost\Ubuntu\home\u"] {
            assert!(!is_network_path(p), "{p} is local");
        }
        // Ordinary local paths.
        for p in ["/home/u", "/Users/u", r"C:\Users\u", "/netlify/x", "/net"] {
            assert!(!is_network_path(p), "{p} is local");
        }
        // `..` cannot be used to sneak into `/net/<host>`.
        assert!(is_network_path("/a/../net/host/x"));
    }

    #[test]
    fn a_tail_read_returns_the_end_and_drops_the_partial_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "hist", "one\ntwo\nthree\nfour\n");

        // Whole file fits: nothing dropped, not truncated.
        assert_eq!(
            secure_read_tail(&p, 1000, false),
            TailRead::Read {
                content: "one\ntwo\nthree\nfour\n".to_string(),
                truncated: false
            }
        );

        // Only the tail fits: the sliced-open first line is dropped, so no
        // command is ever reported with its head cut off.
        let TailRead::Read { content, truncated } = secure_read_tail(&p, 12, false) else {
            panic!("expected a read");
        };
        assert!(truncated);
        assert_eq!(content, "three\nfour\n");
        assert!(!content.contains("wo\n"));
    }

    #[test]
    fn a_tail_read_distinguishes_absent_from_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            secure_read_tail(&dir.path().join("nope"), 100, false),
            TailRead::Absent
        );
        // A directory is not a readable history file.
        assert_eq!(
            secure_read_tail(dir.path(), 100, false),
            TailRead::Unreadable
        );
        // A symlink is refused, and that is UNREADABLE rather than absent --
        // the caller must mark the gather partial, not assume nothing was there.
        #[cfg(unix)]
        {
            write(dir.path(), "real", "x");
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(dir.path().join("real"), &link).unwrap();
            assert_eq!(secure_read_tail(&link, 100, false), TailRead::Unreadable);
        }
    }

    #[test]
    fn rebasing_moves_a_path_between_two_spellings_of_a_directory() {
        assert_eq!(
            rebase_path("/home/u/.ssh/id_rsa", "/home/u", "/mnt/data/u", false).as_deref(),
            Some("/mnt/data/u/.ssh/id_rsa")
        );
        // The directory itself rebases to the other directory.
        assert_eq!(
            rebase_path("/home/u", "/home/u", "/mnt/data/u", false).as_deref(),
            Some("/mnt/data/u")
        );
        // Not under `from` at all.
        assert_eq!(
            rebase_path("/etc/passwd", "/home/u", "/mnt/data/u", false),
            None
        );
        // A sibling whose name merely starts the same is NOT under it.
        assert_eq!(
            rebase_path("/home/user2/x", "/home/u", "/mnt/u", false),
            None
        );
        // Windows compares case-insensitively.
        assert_eq!(
            rebase_path(r"C:\Users\U\x", r"c:\users\u", r"D:\alt", true).as_deref(),
            Some(r"D:\alt\x")
        );
    }

    #[test]
    fn a_deny_rule_blocks_both_spellings_of_a_home_directory() {
        // The rule is written against the realpath'd spelling; the walk hands
        // over the environment's spelling. Checking only what it was handed
        // would walk straight past the user's own rule.
        let denied = |p: &str| p.starts_with("/mnt/data/u/.ssh");
        assert!(denied_under_either_alias(
            "/home/u/.ssh/id_rsa",
            "/home/u",
            "/mnt/data/u",
            &denied,
            false
        ));
        // ...and symmetrically, a rule against the environment spelling blocks
        // the realpath'd one.
        let denied = |p: &str| p.starts_with("/home/u/.ssh");
        assert!(denied_under_either_alias(
            "/mnt/data/u/.ssh/id_rsa",
            "/home/u",
            "/mnt/data/u",
            &denied,
            false
        ));
        // An unrelated path is not denied by either.
        assert!(!denied_under_either_alias(
            "/home/u/work/README.md",
            "/home/u",
            "/mnt/data/u",
            &denied,
            false
        ));
        // When the two spellings coincide, only the direct check applies.
        let denied = |p: &str| p == "/home/u/secret";
        assert!(denied_under_either_alias(
            "/home/u/secret",
            "/home/u",
            "/home/u",
            &denied,
            false
        ));
        assert!(!denied_under_either_alias(
            "/home/u/ok",
            "/home/u",
            "/home/u",
            &denied,
            false
        ));
    }

    #[test]
    fn glob_results_are_deduped_sorted_and_capped() {
        let paths = vec![
            "b.md".to_string(),
            "a.md".to_string(),
            "b.md".to_string(),
            "x".repeat(300),
            "c.md".to_string(),
        ];
        assert_eq!(finalize_glob(paths, 2), vec!["a.md", "b.md"]);
    }

    #[test]
    fn the_local_settings_source_reports_lstat_facts() {
        let dir = tempfile::tempdir().unwrap();
        let src = FsLocalSettingsSource::new(dir.path());
        // Nothing there at all.
        assert_eq!(src.lingxi_dir(), None);

        std::fs::create_dir_all(dir.path().join(".lingxi")).unwrap();
        assert_eq!(src.lingxi_dir(), Some(true));
        assert_eq!(src.local_file(), None);

        write(
            dir.path(),
            ".lingxi/settings.local.json",
            "{\"autoMode\":{}}",
        );
        let (is_file, nlink, size) = src.local_file().unwrap();
        assert!(is_file);
        assert_eq!(nlink, 1);
        assert_eq!(size, 15);

        // `.lingxi` as a symlink is reported as not-a-directory, so the gate
        // ladder refuses without probing behind it.
        #[cfg(unix)]
        {
            let dir2 = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(dir.path().join(".lingxi"), dir2.path().join(".lingxi"))
                .unwrap();
            let src2 = FsLocalSettingsSource::new(dir2.path());
            assert_eq!(src2.lingxi_dir(), Some(false));
        }
    }

    // ── `j1d` walk ───────────────────────────────────────────────────────────

    fn make_repo(at: &std::path::Path, remote: Option<&str>) {
        std::fs::create_dir_all(at.join(".git")).unwrap();
        let config = remote.map_or_else(
            || "[core]\n\trepositoryformatversion = 0\n".to_string(),
            |url| format!("[remote \"origin\"]\n\turl = {url}\n"),
        );
        std::fs::write(at.join(".git").join("config"), config).unwrap();
    }

    #[test]
    fn the_walk_finds_repos_and_reduces_their_remotes() {
        let home = tempfile::tempdir().unwrap();
        make_repo(
            &home.path().join("code/app"),
            Some("https://github.com/acme/app"),
        );
        make_repo(&home.path().join("code/lib"), None);

        let (repos, limit) = walk_home_repos(home.path());
        assert_eq!(limit, WalkLimit::None);
        let app = repos.iter().find(|r| r.path.ends_with("app")).unwrap();
        assert_eq!(app.remotes, vec!["github.com/acme/app".to_string()]);
        let lib = repos.iter().find(|r| r.path.ends_with("lib")).unwrap();
        // No remote is a REASON, not an empty list.
        assert_eq!(
            lib.note,
            Some(crate::auto_mode_producers::HomeRepoNote::NoRemote)
        );
    }

    #[test]
    fn the_walk_does_not_descend_into_a_repo() {
        let home = tempfile::tempdir().unwrap();
        make_repo(
            &home.path().join("outer"),
            Some("https://github.com/acme/outer"),
        );
        // A vendored checkout inside the working tree is not a separate repo
        // worth reporting, and descending would burn the budget on node_modules.
        make_repo(
            &home.path().join("outer/vendor/inner"),
            Some("https://github.com/acme/inner"),
        );

        let (repos, _) = walk_home_repos(home.path());
        assert_eq!(repos.len(), 1);
        assert!(repos[0].path.ends_with("outer"));
    }

    #[test]
    fn a_gitdir_pointing_outside_home_is_recorded_not_followed() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(elsewhere.path().join("planted")).unwrap();
        std::fs::write(
            elsewhere.path().join("planted").join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/attacker/exfil\n",
        )
        .unwrap();
        let repo = home.path().join("trap");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join(".git"),
            format!("gitdir: {}\n", elsewhere.path().join("planted").display()),
        )
        .unwrap();

        let (repos, _) = walk_home_repos(home.path());
        assert_eq!(repos.len(), 1);
        // The planted config must NOT have been read.
        assert!(repos[0].remotes.is_empty());
        assert_eq!(
            repos[0].note,
            Some(crate::auto_mode_producers::HomeRepoNote::GitdirOutsideHome)
        );
    }

    #[test]
    fn a_skipped_directory_name_is_never_entered() {
        let home = tempfile::tempdir().unwrap();
        make_repo(
            &home.path().join("node_modules/pkg"),
            Some("https://github.com/acme/pkg"),
        );
        let (repos, _) = walk_home_repos(home.path());
        assert!(repos.is_empty());
    }

    #[test]
    fn an_unreadable_home_is_reported_as_such_not_as_empty() {
        let missing = std::path::Path::new("/nonexistent-home-for-wizard-06-test");
        let (repos, limit) = walk_home_repos(missing);
        assert!(repos.is_empty());
        // "could not look" must never render as "found nothing".
        assert_eq!(limit, WalkLimit::HomeUnreadable);
    }

    // ── `W1d` sweep ──────────────────────────────────────────────────────────

    fn write_transcript(dir: &std::path::Path, name: &str, commands: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        let lines: Vec<String> = commands
            .iter()
            .map(|c| {
                serde_json::json!({
                    "message": {"content": [
                        {"type": "tool_use", "name": "Bash", "input": {"command": c}}
                    ]}
                })
                .to_string()
            })
            .collect();
        std::fs::write(dir.join(name), lines.join("\n")).unwrap();
    }

    #[test]
    fn the_sweep_mines_words_across_projects_and_skips_this_one() {
        let root = tempfile::tempdir().unwrap();
        let mine = root.path().join("-this-project");
        write_transcript(&mine, "a.jsonl", &["terraform apply"]);
        write_transcript(
            &root.path().join("-other"),
            "b.jsonl",
            &["helm upgrade api", "ls -la"],
        );

        let source = FsAllProjectsSource::new(root.path(), Some(mine.clone()));
        let scan = crate::auto_mode_producers::AllProjectsSource::scan(&source).unwrap();

        assert!(scan.words.contains(&"helm".to_string()));
        // This project's own transcripts belong to the per-project section.
        assert!(!scan.words.contains(&"terraform".to_string()));
        // `ls` and `kubectl` are standard CLIs: the list is about what is
        // UNUSUAL here, so they carry no signal and are filtered.
        assert!(!scan.words.contains(&"ls".to_string()));
        assert_eq!(scan.scanned, 1);
        assert!(!scan.words_incomplete);
    }

    #[test]
    fn an_absent_projects_root_is_unavailable_not_empty() {
        let source = FsAllProjectsSource::new("/nonexistent-projects-root-w49", None);
        assert!(crate::auto_mode_producers::AllProjectsSource::scan(&source).is_none());
    }

    #[test]
    fn an_unreadable_project_directory_marks_coverage_partial() {
        let root = tempfile::tempdir().unwrap();
        write_transcript(&root.path().join("-ok"), "a.jsonl", &["helm upgrade"]);
        let blocked = root.path().join("-blocked");
        std::fs::create_dir_all(&blocked).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        }

        let source = FsAllProjectsSource::new(root.path(), None);
        let scan = crate::auto_mode_producers::AllProjectsSource::scan(&source).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(scan.unreadable_dirs, 1);
            // A partial sweep must say so, or it reads as a census.
            assert!(scan.words_incomplete);
            std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(scan.words.contains(&"helm".to_string()));
    }

    // ── shell history ────────────────────────────────────────────────────────

    #[test]
    fn history_reads_the_tail_and_keeps_only_command_words() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".zsh_history"),
            "curl -H \"Authorization: Bearer sekrit\" https://api.example\nterraform apply\n",
        )
        .unwrap();
        let env = crate::auto_mode_producers::ShellHistoryEnv {
            windows: false,
            home_dir: home.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        let body =
            crate::auto_mode_producers::shell_history_section(&FsShellHistorySource::new(env));
        assert!(body.contains("curl"));
        assert!(body.contains("terraform"));
        // The arguments are where the secrets are; they never leave the module.
        assert!(!body.contains("sekrit"));
        assert!(!body.contains("Authorization"));
    }

    #[test]
    fn an_absent_history_file_is_not_a_partial_read() {
        let home = tempfile::tempdir().unwrap();
        let env = crate::auto_mode_producers::ShellHistoryEnv {
            windows: false,
            home_dir: home.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        let body =
            crate::auto_mode_producers::shell_history_section(&FsShellHistorySource::new(env));
        assert!(body.contains("complete"), "got: {body}");
    }

    // ── base64 (gh `contents` payloads) ──────────────────────────────────────

    #[test]
    fn essential_traffic_only_closes_the_gh_producers() {
        // The producer must not reach github.com when the host opted out of
        // nonessential traffic; the section reports the refusal instead.
        let producers = FsReconProducers::new("/tmp", "/tmp", "/tmp/projects/x", false)
            .with_nonessential_traffic(false)
            .with_org_split(true);
        let body = crate::auto_mode_pregather::ReconProducers::repo_visibility(&producers).unwrap();
        assert!(body.contains("nonessential traffic disabled or policy-restricted"));
        assert!(body.contains(crate::auto_mode_producers::INFER_VISIBILITY_HINT));
    }

    #[test]
    fn capping_output_never_splits_a_character() {
        // A cap landing mid-character is the case that used to panic.
        let mut s = "aé".to_string(); // 'é' is two bytes: cap 2 must drop it
        cap_at_char_boundary(&mut s, 2);
        assert_eq!(s, "a");

        let mut s = "aé".to_string();
        cap_at_char_boundary(&mut s, 3);
        assert_eq!(s, "aé");

        // A cap smaller than the first character yields empty, not a panic.
        let mut s = "\u{1F600}ok".to_string();
        cap_at_char_boundary(&mut s, 2);
        assert_eq!(s, "");

        // Under the cap is untouched.
        let mut s = "short".to_string();
        cap_at_char_boundary(&mut s, 999);
        assert_eq!(s, "short");

        // Exercise every cap across a multi-byte string: none may panic, and
        // the result is always a prefix.
        let full = "é\u{1F600}z\u{4E2D}";
        for cap in 0..=full.len() + 2 {
            let mut s = full.to_string();
            cap_at_char_boundary(&mut s, cap);
            assert!(full.starts_with(&s), "cap={cap} gave {s:?}");
            assert!(s.len() <= cap.min(full.len()));
        }
    }

    #[test]
    fn base64_round_trips_and_refuses_garbage() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("").unwrap(), b"");
        assert!(base64_decode("not base64!").is_none());
    }
}

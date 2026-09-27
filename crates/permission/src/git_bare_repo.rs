//! PS-CALLER-06-2 — the planted-git-directory gate (2.1.220 `R3r`).
//!
//! Git will treat a directory carrying `HEAD` + `objects` + `refs` as a git
//! directory even without a `.git/`, and it reads **config and runs hooks from
//! it**. So an untrusted archive — a cloned repo, an extracted tarball, a
//! fetched dependency — can plant those files and get arbitrary code execution
//! the next time any git command runs there. The same holds for a `.git` file
//! or symlink that redirects somewhere unverifiable.
//!
//! Claude Code refuses to auto-approve git commands in such a directory. This
//! is the port of that probe.
//!
//! # Why the answer is three-valued, not a boolean
//!
//! A *legitimate* repository must not prompt, or the gate is worthless because
//! users click through it. Two cases matter and are easy to get wrong:
//!
//! * a real `.git/` directory is TRUSTED and stops the walk immediately;
//! * a linked **worktree**'s git dir carries `commondir`, which is exactly what
//!   distinguishes it from a planted bare repo — [`is_standalone_git_dir`]
//!   rejects it, so worktrees resolve through the redirect path and end up
//!   trusted rather than flagged.
//!
//! # Walk order
//!
//! Classify the cwd's `.git`; if that is inconclusive, walk UP. At each level a
//! trusted `.git` wins (the directory belongs to a real repo), a plantable
//! redirect loses, and bare indicators in the directory itself lose. The walk
//! is what catches a planted `HEAD`/`objects`/`refs` in a subdirectory of an
//! otherwise ordinary tree.

use std::path::{Path, PathBuf};

/// `Lkh` — a `.git` FILE larger than this is refused rather than parsed.
pub const GITDIR_FILE_MAX_BYTES: u64 = 4_096;
/// A `HEAD` larger than this is not a real one.
pub const HEAD_MAX_BYTES: u64 = 4_096;
/// Only the first this-many bytes of `HEAD` are inspected.
const HEAD_SNIFF_BYTES: usize = 255;

/// Why git commands here need approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BareRepoGate {
    /// `HEAD`/`objects`/`refs` sit outside a `.git/` directory.
    BareIndicators,
    /// A `.git` file/symlink redirects somewhere that cannot be verified.
    GitdirRedirectPlantable,
    /// A `.git` file is too large to be a real `gitdir:` pointer.
    GitdirFileOversized,
}

impl BareRepoGate {
    /// The oracle's shell-battery message.
    #[must_use]
    pub fn shell_message(self) -> &'static str {
        match self {
            Self::BareIndicators => {
                "The current directory has bare-repo indicators (HEAD/objects/refs outside a .git/ directory). Git may treat it as a git dir and run config/hooks from here, so git commands need approval."
            }
            Self::GitdirRedirectPlantable | Self::GitdirFileOversized => {
                "The .git file or symlink here redirects to a location Claude cannot verify is safe (it may have been planted by an untrusted archive). Git commands need approval."
            }
        }
    }

    /// The oracle's PowerShell-battery message (differently worded upstream).
    #[must_use]
    pub fn powershell_message(self) -> &'static str {
        match self {
            Self::BareIndicators => {
                "Git command in a directory with bare-repo indicators (HEAD/objects/refs outside a .git/ directory). Git may treat it as a git dir and run config/hooks from here."
            }
            Self::GitdirRedirectPlantable | Self::GitdirFileOversized => {
                "The .git file or symlink here redirects to a location that cannot be verified as safe (it may have been planted by an untrusted archive). Git commands need approval."
            }
        }
    }

    /// `git_bare_repo_gate` telemetry reason.
    #[must_use]
    pub fn telemetry_reason(self) -> &'static str {
        match self {
            Self::BareIndicators => "bare_indicators",
            Self::GitdirRedirectPlantable => "gitdir_target_plantable",
            Self::GitdirFileOversized => "gitdir_file_oversized",
        }
    }
}

/// How a directory's `.git` entry classifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitEntry {
    /// A genuine repository — stops the walk, no gate.
    Trusted,
    /// A redirect that cannot be verified.
    Plantable,
    /// A `.git` file too big to be a pointer.
    Oversized,
    /// Nothing conclusive here; keep walking.
    None,
}

/// Does `HEAD` at `dir` look like a real one?
///
/// `ref: refs/…` or a bare 40-hex (SHA-1) / 64-hex (SHA-256) object id.
fn head_looks_real(dir: &Path) -> bool {
    let head = dir.join("HEAD");
    let Ok(meta) = std::fs::symlink_metadata(&head) else {
        return false;
    };
    if !meta.is_file() || meta.len() > HEAD_MAX_BYTES {
        return false;
    }
    let Ok(body) = std::fs::read(&head) else {
        return false;
    };
    let text = String::from_utf8_lossy(&body[..body.len().min(HEAD_SNIFF_BYTES)]);
    head_body_looks_real(&text)
}

/// The `HEAD` content predicate, split out so it can be tested without a disk.
#[must_use]
pub fn head_body_looks_real(text: &str) -> bool {
    if let Some(rest) = text.strip_prefix("ref:") {
        let rest = rest.trim_start_matches([' ', '\t']);
        if rest.starts_with("refs/") {
            return true;
        }
    }
    let oid = text.trim_end_matches([' ', '\t', '\n', '\r']);
    (oid.len() == 40 || oid.len() == 64)
        && oid
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Is `dir` a STANDALONE git directory (as opposed to a linked worktree's)?
///
/// The `commondir` check is the load-bearing part: a linked worktree's git dir
/// carries it, and treating that as a planted bare repo would prompt on every
/// legitimate `git worktree add`.
fn is_standalone_git_dir(dir: &Path) -> bool {
    if !head_looks_real(dir) {
        return false;
    }
    for name in ["objects", "refs"] {
        let sub = dir.join(name);
        match std::fs::metadata(&sub) {
            Ok(meta) if meta.is_dir() => {}
            _ => return false,
        }
    }
    // A worktree, not a repository of its own.
    !dir.join("commondir").exists()
}

/// Are bare-repo INDICATORS present directly in `dir`?
///
/// Deliberately weaker than [`is_standalone_git_dir`]: a partial plant (just
/// `objects/`, or a `HEAD` symlink) is still enough for git to latch onto the
/// directory, so the gate fires on the weaker signal.
fn has_bare_indicators(dir: &Path) -> bool {
    if let Ok(meta) = std::fs::symlink_metadata(dir.join("HEAD")) {
        if meta.is_file() || meta.file_type().is_symlink() {
            return true;
        }
    }
    ["objects", "refs"]
        .iter()
        .any(|name| std::fs::metadata(dir.join(name)).is_ok())
}

/// Can the redirect target be canonicalised AND does it live under a `.git`
/// path segment?
///
/// A target that will not canonicalise, or that resolves somewhere with no
/// `.git` segment, is plantable: nothing stops an archive from pointing it at a
/// directory it also controls.
fn classify_redirect(target: &Path) -> GitEntry {
    let Ok(canonical) = std::fs::canonicalize(target) else {
        return GitEntry::Plantable;
    };
    let under_git_segment = canonical
        .components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case(".git"));
    if !under_git_segment {
        return GitEntry::Plantable;
    }
    if head_looks_real(&canonical) {
        GitEntry::Trusted
    } else {
        GitEntry::None
    }
}

/// Classify `dir`'s `.git` entry.
fn classify_git_entry(dir: &Path) -> GitEntry {
    let git = dir.join(".git");
    let Ok(meta) = std::fs::symlink_metadata(&git) else {
        return GitEntry::None;
    };

    if meta.file_type().is_symlink() {
        let Ok(target) = std::fs::read_link(&git) else {
            return GitEntry::Plantable;
        };
        let resolved = if target.is_absolute() {
            target
        } else {
            dir.join(target)
        };
        return classify_redirect(&resolved);
    }

    if meta.is_file() {
        if meta.len() > GITDIR_FILE_MAX_BYTES {
            return GitEntry::Oversized;
        }
        let Ok(body) = std::fs::read(&git) else {
            return GitEntry::None;
        };
        // A NUL byte means this is not the text pointer it claims to be.
        if body.contains(&0) {
            return GitEntry::Plantable;
        }
        let text = String::from_utf8_lossy(&body);
        let Some(rest) = text.strip_prefix("gitdir: ") else {
            return GitEntry::None;
        };
        let target = Path::new(rest.trim_end_matches(['\r', '\n']));
        let resolved = if target.is_absolute() {
            target.to_path_buf()
        } else {
            dir.join(target)
        };
        return classify_redirect(&resolved);
    }

    if meta.is_dir() {
        return if is_standalone_git_dir(&git) {
            GitEntry::Trusted
        } else {
            GitEntry::None
        };
    }
    GitEntry::None
}

/// `R3r` — does running git in `cwd` need approval?
///
/// `None` means the directory is safe to auto-approve: either a real repository
/// was found on the way up, or nothing suspicious exists at all.
#[must_use]
pub fn bare_repo_gate(cwd: &Path) -> Option<BareRepoGate> {
    match classify_git_entry(cwd) {
        GitEntry::Plantable => return Some(BareRepoGate::GitdirRedirectPlantable),
        GitEntry::Oversized => return Some(BareRepoGate::GitdirFileOversized),
        // A real repository right here — nothing to gate.
        GitEntry::Trusted => return None,
        GitEntry::None => {}
    }

    let mut dir: PathBuf = cwd.to_path_buf();
    loop {
        if has_bare_indicators(&dir) {
            return Some(BareRepoGate::BareIndicators);
        }
        let Some(parent) = dir.parent().map(Path::to_path_buf) else {
            break;
        };
        if parent == dir {
            break;
        }
        match classify_git_entry(&parent) {
            // An ancestor is a real repo, so this subtree is part of it.
            GitEntry::Trusted => return None,
            GitEntry::Plantable => return Some(BareRepoGate::GitdirRedirectPlantable),
            GitEntry::Oversized => return Some(BareRepoGate::GitdirFileOversized),
            GitEntry::None => {}
        }
        dir = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// A normal repository: `.git/` with HEAD, objects, refs.
    fn real_repo(root: &Path) {
        write(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join(".git/refs")).unwrap();
    }

    #[test]
    fn a_plain_directory_is_not_gated() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(bare_repo_gate(d.path()), None);
    }

    #[test]
    fn a_real_repository_is_not_gated() {
        let d = tempfile::tempdir().unwrap();
        real_repo(d.path());
        assert_eq!(bare_repo_gate(d.path()), None);
    }

    #[test]
    fn a_subdirectory_of_a_real_repository_is_not_gated() {
        // The walk must find the ancestor's `.git` and stop; prompting inside
        // every subdirectory of every repo would make the gate useless.
        let d = tempfile::tempdir().unwrap();
        real_repo(d.path());
        let sub = d.path().join("src/deep");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(bare_repo_gate(&sub), None);
    }

    #[test]
    fn planted_bare_indicators_are_gated() {
        // The attack: an archive drops HEAD/objects/refs, git latches onto the
        // directory and runs config + hooks from it.
        let d = tempfile::tempdir().unwrap();
        write(&d.path().join("HEAD"), "ref: refs/heads/main\n");
        std::fs::create_dir_all(d.path().join("objects")).unwrap();
        std::fs::create_dir_all(d.path().join("refs")).unwrap();
        assert_eq!(bare_repo_gate(d.path()), Some(BareRepoGate::BareIndicators));
    }

    #[test]
    fn a_partial_plant_is_still_gated() {
        // Weaker than a full bare repo on purpose — `objects/` alone is enough
        // for git to take an interest.
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("objects")).unwrap();
        assert_eq!(bare_repo_gate(d.path()), Some(BareRepoGate::BareIndicators));
    }

    #[test]
    fn a_dangling_git_file_redirect_is_gated() {
        let d = tempfile::tempdir().unwrap();
        write(&d.path().join(".git"), "gitdir: /nonexistent/planted\n");
        assert_eq!(
            bare_repo_gate(d.path()),
            Some(BareRepoGate::GitdirRedirectPlantable)
        );
    }

    #[test]
    fn a_git_file_redirecting_outside_any_git_segment_is_gated() {
        // Canonicalises fine, but the target is an ordinary directory the
        // archive also controls — exactly the plantable case.
        let d = tempfile::tempdir().unwrap();
        let elsewhere = d.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        write(
            &repo.join(".git"),
            &format!("gitdir: {}\n", elsewhere.display()),
        );
        assert_eq!(
            bare_repo_gate(&repo),
            Some(BareRepoGate::GitdirRedirectPlantable)
        );
    }

    #[test]
    fn an_oversized_git_file_is_refused_not_parsed() {
        let d = tempfile::tempdir().unwrap();
        write(
            &d.path().join(".git"),
            &"x".repeat(usize::try_from(GITDIR_FILE_MAX_BYTES).unwrap() + 1),
        );
        assert_eq!(
            bare_repo_gate(d.path()),
            Some(BareRepoGate::GitdirFileOversized)
        );
    }

    #[test]
    fn a_git_file_with_a_nul_byte_is_plantable() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(".git"), b"gitdir: /tmp\0/evil\n").unwrap();
        assert_eq!(
            bare_repo_gate(d.path()),
            Some(BareRepoGate::GitdirRedirectPlantable)
        );
    }

    #[test]
    fn a_legitimate_linked_worktree_is_not_gated() {
        // THE case that decides whether this gate is usable. `git worktree add`
        // leaves a `.git` FILE pointing into the parent repo's
        // `.git/worktrees/<name>`, whose `commondir` marks it as a worktree.
        // Flagging it would prompt on ordinary, safe workflows.
        let d = tempfile::tempdir().unwrap();
        let main = d.path().join("main");
        real_repo(&main);
        let wt_gitdir = main.join(".git/worktrees/feature");
        std::fs::create_dir_all(&wt_gitdir).unwrap();
        write(&wt_gitdir.join("HEAD"), "ref: refs/heads/feature\n");
        write(&wt_gitdir.join("commondir"), "../..\n");

        let wt = d.path().join("feature");
        std::fs::create_dir_all(&wt).unwrap();
        write(
            &wt.join(".git"),
            &format!("gitdir: {}\n", wt_gitdir.display()),
        );
        assert_eq!(bare_repo_gate(&wt), None);
    }

    #[test]
    fn a_git_file_without_the_gitdir_prefix_is_inconclusive_not_plantable() {
        // Some tools leave junk named `.git`; that is not evidence of a plant,
        // so it must not prompt on its own.
        let d = tempfile::tempdir().unwrap();
        write(&d.path().join(".git"), "not a pointer\n");
        assert_eq!(bare_repo_gate(d.path()), None);
    }

    #[test]
    fn head_bodies_are_recognised_the_way_git_writes_them() {
        assert!(head_body_looks_real("ref: refs/heads/main\n"));
        assert!(head_body_looks_real("ref:\trefs/heads/x"));
        assert!(head_body_looks_real(&"a".repeat(40)));
        assert!(head_body_looks_real(&format!("{}\n", "0".repeat(64))));
        // Not a HEAD.
        assert!(!head_body_looks_real("ref: heads/main"));
        assert!(!head_body_looks_real(&"a".repeat(39)));
        assert!(!head_body_looks_real("hello"));
        assert!(!head_body_looks_real(""));
        // Uppercase hex is not what git writes.
        assert!(!head_body_looks_real(&"A".repeat(40)));
    }

    #[test]
    fn the_messages_and_reasons_are_byte_exact() {
        assert_eq!(
            BareRepoGate::BareIndicators.shell_message(),
            "The current directory has bare-repo indicators (HEAD/objects/refs outside a .git/ directory). Git may treat it as a git dir and run config/hooks from here, so git commands need approval."
        );
        assert_eq!(
            BareRepoGate::GitdirRedirectPlantable.powershell_message(),
            "The .git file or symlink here redirects to a location that cannot be verified as safe (it may have been planted by an untrusted archive). Git commands need approval."
        );
        assert_eq!(
            BareRepoGate::BareIndicators.telemetry_reason(),
            "bare_indicators"
        );
        assert_eq!(
            BareRepoGate::GitdirRedirectPlantable.telemetry_reason(),
            "gitdir_target_plantable"
        );
    }

    #[test]
    fn a_git_directory_carrying_commondir_is_not_treated_as_a_repository() {
        // Exercises the `commondir` exclusion in the DIRECTORY branch (the
        // worktree test above goes through the redirect branch instead). A
        // `.git/` that is really a worktree gitdir must not read as Trusted,
        // because "trusted" stops the walk and would suppress a genuine plant
        // further up.
        let d = tempfile::tempdir().unwrap();
        write(&d.path().join(".git/HEAD"), "ref: refs/heads/main\n");
        std::fs::create_dir_all(d.path().join(".git/objects")).unwrap();
        std::fs::create_dir_all(d.path().join(".git/refs")).unwrap();
        assert!(is_standalone_git_dir(&d.path().join(".git")));

        write(&d.path().join(".git/commondir"), "../..\n");
        assert!(
            !is_standalone_git_dir(&d.path().join(".git")),
            "commondir marks a worktree gitdir, not a standalone repository"
        );
    }

    #[test]
    fn a_trusted_ancestor_does_not_mask_a_plant_below_it() {
        // Ordering guard: indicators in the cwd are checked BEFORE walking to
        // the parent, so a real repo higher up cannot vouch for a planted
        // subdirectory.
        let d = tempfile::tempdir().unwrap();
        real_repo(d.path());
        let planted = d.path().join("vendor/pkg");
        std::fs::create_dir_all(planted.join("objects")).unwrap();
        assert_eq!(
            bare_repo_gate(&planted),
            Some(BareRepoGate::BareIndicators),
            "a plant inside a real repo must still gate"
        );
    }
}

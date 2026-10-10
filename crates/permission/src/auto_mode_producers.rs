//! WIZARD-06 — the recon section producers (2.1.220).
//!
//! One function per [`crate::auto_mode_pregather::ReconSection`]. They are kept
//! out of the gather skeleton so each can land independently; the skeleton
//! renders [`crate::auto_mode_pregather::SECTION_FAILED_MARKER`] for any that
//! is not yet ported.
//!
//! Producers behind a closed consent gate are never called at all — see
//! [`crate::auto_mode_pregather::build_recon_block`].

use crate::auto_mode_defaults::{
    default_rule_label, DEFAULT_ALLOW_LABELS, DEFAULT_SOFT_DENY_LABELS,
};
use crate::auto_mode_facts::DEFAULT_LABELS_GUIDANCE;
use crate::auto_mode_sections::{HEADING_DEFAULT_ALLOW_LABELS, HEADING_DEFAULT_SOFT_DENY_LABELS};
use serde_json::Value;

// ── producer read caps ───────────────────────────────────────────────────────

/// `Scn` — read cap for `LINGXI.md` files.
pub const DOC_READ_CAP_LINGXI_MD: usize = 200_000;
/// `yFt` — default read cap for project docs.
pub const DOC_READ_CAP: usize = 10_000;
/// `uae` — how many flagged `permissions.allow` entries are listed before the
/// list is capped.
pub const FLAGGED_LIST_CAP: usize = 20;
/// How many leading lines of `README.md` are kept.
pub const README_HEAD_LINES: usize = 40;
/// `RPo`'s directory-depth limit when globbing project docs.
pub const DOC_GLOB_MAX_DEPTH: usize = 4;
/// `RPo`'s result cap when globbing project docs.
pub const DOC_GLOB_LIMIT: usize = 10;

/// `Z1d`'s truncation suffix: appended when a read hit its cap.
///
/// A truncated read must announce itself — silently returning the first N
/// bytes would let the model treat a partial file as the whole of it.
#[must_use]
pub fn read_truncated_marker(cap: usize) -> String {
    format!(
        "{}{cap} bytes]",
        crate::auto_mode_facts::TRUNCATED_AT_PREFIX
    )
}

// ── display sanitisation ─────────────────────────────────────────────────────

/// `X1d` — strip URL userinfo: `://user:pass@host` becomes `://host`.
///
/// Applied to the WHOLE recon block as its last step, so a credential embedded
/// in any remote URL, registry URL or config value never reaches the model.
#[must_use]
pub fn strip_url_userinfo(s: &str) -> String {
    // `/:\/\/[^/\s\\]*@/g`
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < s.len() {
        if s[i..].starts_with("://") {
            // Scan for an `@` before the next `/`, whitespace or backslash.
            let mut j = i + 3;
            let mut at: Option<usize> = None;
            while j < s.len() {
                let c = bytes[j];
                if c == b'/' || c == b'\\' || c.is_ascii_whitespace() {
                    break;
                }
                if c == b'@' {
                    at = Some(j);
                }
                j += 1;
            }
            if let Some(at) = at {
                out.push_str("://");
                i = at + 1;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// `dEe` — a name safe to render, or [`crate::auto_mode_sections::REDACTED_UNUSUAL_NAME`].
///
/// Rejects anything that could break the block's markdown structure: line
/// breaks and separators, backticks, a leading `#`/`-`/`>`/`<<<`, and anything
/// over 120 characters.
#[must_use]
pub fn display_name(name: &str) -> &str {
    let t = name.trim();
    let structural =
        t.starts_with('#') || t.starts_with('-') || t.starts_with('>') || t.starts_with("<<<");
    let has_break = name.chars().any(|c| {
        matches!(
            c,
            '\r' | '\n' | '\u{b}' | '\u{c}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        )
    });
    if !t.is_empty() && t.chars().count() <= 120 && !has_break && !name.contains('`') && !structural
    {
        name
    } else {
        crate::auto_mode_sections::REDACTED_UNUSUAL_NAME
    }
}

/// `oNd` — escape the separators and backticks that would break the block.
#[must_use]
pub fn escape_separators(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2028}' | '\u{2029}' | '\u{85}' | '`' => format!("\\u{:04x}", c as u32),
            _ => c.to_string(),
        })
        .collect()
}

/// `Bhr` — is this rule string renderable as-is?
///
/// It must survive [`display_name`] unchanged, carry no surrounding
/// whitespace, and contain no URL userinfo. A rule that fails this is reported
/// with [`crate::auto_mode_facts::FLAGGED_ENTRY_UNRENDERABLE`] rather than
/// dropped.
#[must_use]
pub fn is_renderable(rule: &str) -> bool {
    display_name(rule) == rule && rule.trim() == rule && strip_url_userinfo(rule) == rule
}

/// `Tcn` — render one document as a `####` block.
///
/// The body is embedded as a JSON string literal: untrusted file content
/// cannot then open a markdown heading or otherwise restructure the block it
/// is quoted into.
#[must_use]
pub fn render_doc(label: &str, content: &str) -> String {
    format!(
        "#### {}\n{}",
        display_name(label),
        escape_separators(&serde_json::Value::String(content.to_string()).to_string())
    )
}

/// Render one label list as `- {label}` bullets.
fn label_bullets(labels: &[&str]) -> String {
    labels
        .iter()
        .map(|l| format!("- {}", default_rule_label(l)))
        .collect::<Vec<_>>()
        .join("\n")
}

// ── `Ksy` — LINGXI.md files and project docs ─────────────────────────────────

/// Label for the user-level `LINGXI.md`.
///
/// Both the directory AND the filename follow this workspace's naming: the
/// oracle's `~/.claude/CLAUDE.md` is `~/.lingxi/LINGXI.md` here. Reading the
/// oracle's spelling would look for a file this product never writes, so the
/// section would report "absent" for every user who HAS memory configured.
/// Only the label SHAPE is the oracle's.
pub const DOC_USER_LINGXI_MD_LABEL: &str = "~/.lingxi/LINGXI.md";
/// Label for the project `LINGXI.md`.
pub const DOC_PROJECT_LINGXI_MD_LABEL: &str = "./LINGXI.md";
/// Label for `.env.example`.
pub const DOC_ENV_EXAMPLE_LABEL: &str = "./.env.example";
/// Label for `.env.sample`.
pub const DOC_ENV_SAMPLE_LABEL: &str = "./.env.sample";

/// The project files read for the docs section, as `(label, relative path, cap)`.
pub const PROJECT_DOC_FILES: [(&str, &str, usize); 4] = [
    (
        DOC_PROJECT_LINGXI_MD_LABEL,
        "LINGXI.md",
        DOC_READ_CAP_LINGXI_MD,
    ),
    (
        crate::auto_mode_facts::DOC_README_HEAD_LABEL,
        "README.md",
        DOC_READ_CAP,
    ),
    (DOC_ENV_EXAMPLE_LABEL, ".env.example", DOC_READ_CAP),
    (DOC_ENV_SAMPLE_LABEL, ".env.sample", DOC_READ_CAP),
];

/// Supplies the file contents the docs section renders.
///
/// Injected so the producer is testable, and so the containment rules live
/// with the real implementation: `jhr` resolves both the root and the target,
/// requires both to be canonical, requires the target to stay under the root,
/// and reads with `O_NOFOLLOW`.
pub trait DocSource {
    /// The user-level `LINGXI.md`, capped at [`DOC_READ_CAP_LINGXI_MD`].
    fn user_lingxi_md(&self) -> Option<String>;
    /// A project-relative file, capped at `cap`. `None` when absent or when it
    /// fails the containment check.
    fn project_file(&self, relative: &str, cap: usize) -> Option<String>;
    /// Up to [`DOC_GLOB_LIMIT`] `SKILL.md`/`*.md` paths under
    /// `.lingxi/{skills,rules,agents}`, searched to [`DOC_GLOB_MAX_DEPTH`].
    fn lingxi_doc_paths(&self) -> Vec<String>;
}

/// `Ksy` — the "CLAUDE.md files and project docs" section body.
#[must_use]
pub fn project_docs_section(source: &dyn DocSource) -> String {
    let mut docs: Vec<String> = Vec::new();

    if let Some(content) = source.user_lingxi_md() {
        docs.push(render_doc(DOC_USER_LINGXI_MD_LABEL, &content));
    }

    for (label, relative, cap) in PROJECT_DOC_FILES {
        let Some(mut content) = source.project_file(relative, cap) else {
            continue;
        };
        if label.contains("README") {
            content = content
                .split('\n')
                .take(README_HEAD_LINES)
                .collect::<Vec<_>>()
                .join("\n");
        }
        docs.push(render_doc(label, &content));
    }

    for path in source.lingxi_doc_paths() {
        if let Some(content) = source.project_file(&path, DOC_READ_CAP) {
            docs.push(render_doc(&format!("./{path}"), &content));
        }
    }

    docs.join("\n\n")
}

// ── `Ysy` — repo facts ───────────────────────────────────────────────────────

/// `gcn` — the flags every `git` invocation in the recon carries.
///
/// These are hardening, not tidiness: reading facts out of a repository must
/// not run that repository's hooks, must not start a filesystem monitor, and
/// must never pop a credential prompt. A hostile checkout would otherwise get
/// code execution out of `git remote`.
pub const GIT_HARDENING_FLAGS: [&str; 6] = [
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=",
    "-c",
    "core.askPass=",
];

/// `qhr` — timeout for a recon subprocess, in milliseconds.
pub const SUBPROCESS_TIMEOUT_MS: u64 = 4_000;
/// Read cap for the `CONTRIBUTING.md` excerpt.
pub const CONTRIBUTING_READ_CAP: usize = 2_000;
/// Cap on rendered remote lines (`uae * 2`).
pub const REMOTE_LINE_CAP: usize = FLAGGED_LIST_CAP * 2;

/// Paths whose presence signals an engineering-posture convention.
pub const POSTURE_SIGNAL_PATHS: [&str; 10] = [
    ".github/CODEOWNERS",
    ".github/workflows",
    ".buildkite",
    ".circleci",
    "LINGXI.md",
    "CONTRIBUTING.md",
    "LICENSE",
    "LICENSE.md",
    "LICENSE.txt",
    "LICENCE",
];

/// `VBs` — does the string contain anything outside printable ASCII, or a
/// backslash or percent? Such a URL is refused rather than parsed.
fn has_unsafe_url_charset(s: &str) -> bool {
    s.chars()
        .any(|c| c <= ' ' || c > '~' || c == '\\' || c == '%')
}

/// `SPo` — a plausible host.
fn is_plausible_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 100
        && matches_word_dot_dash(h)
        && h.chars().any(|c| c.is_alphanumeric() || c == '_')
}

/// `^[\w.][\w.-]*$`
fn matches_word_dot_dash(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let word = |c: char| c.is_alphanumeric() || c == '_';
    (word(first) || first == '.') && chars.all(|c| word(c) || c == '.' || c == '-')
}

/// `^[\w.][\w./-]*$` — as above, plus `/`.
fn matches_branch_shape(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let word = |c: char| c.is_alphanumeric() || c == '_';
    (word(first) || first == '.') && chars.all(|c| word(c) || c == '.' || c == '/' || c == '-')
}

/// `Hhr` — reduce a git remote URL to `scheme://host/owner/repo`.
///
/// Userinfo is dropped (the parse keeps only the host) and the path is cut to
/// its last two segments, which is the contract
/// [`crate::auto_mode_facts::REPOS_FOUND_HEADER`] advertises. Anything that
/// does not parse cleanly is replaced wholesale rather than partially shown.
#[must_use]
pub fn redact_remote_url(url: &str) -> String {
    const REDACTED: &str = "(unparseable remote URL redacted)";
    if url.len() > 2048 || has_unsafe_url_charset(url) {
        return REDACTED.to_string();
    }
    // A protocol-relative URL carrying userinfo: re-parse with a dummy scheme.
    if url.starts_with("//") && url.contains('@') {
        let out = redact_remote_url(&format!("redacted:{url}"));
        return out.strip_prefix("redacted:").unwrap_or(&out).to_string();
    }
    let Some(scheme_end) = url.find("://") else {
        // scp-style `user@host:path`.
        let mut rest = url;
        if let Some(at) = rest.find('@') {
            let after = &rest[at + 1..];
            if after.contains('@') {
                return REDACTED.to_string();
            }
            rest = after;
        }
        let shape_ok = {
            let mut chars = rest.chars();
            match chars.next() {
                Some(f) if f.is_alphanumeric() || f == '_' || f == '.' || f == '~' || f == '/' => {
                    chars.all(|c| {
                        c.is_alphanumeric() || matches!(c, '_' | '.' | ':' | '/' | '~' | '-')
                    })
                }
                _ => false,
            }
        };
        // `!/[:/@]-/` — no `-` directly after `:`, `/` or `@`.
        let dash_after_sep = rest
            .as_bytes()
            .windows(2)
            .any(|w| matches!(w[0], b':' | b'/' | b'@') && w[1] == b'-');
        return if shape_ok && !dash_after_sep {
            rest.to_string()
        } else {
            REDACTED.to_string()
        };
    };

    let scheme = &url[..scheme_end];
    let scheme_ok = {
        let mut chars = scheme.chars();
        chars.next().is_some_and(char::is_alphabetic)
            && chars.all(|c| c.is_alphanumeric() || matches!(c, '+' | '.' | '-'))
    };
    if !scheme_ok {
        return REDACTED.to_string();
    }
    let rest = &url[scheme_end + 3..];
    let split = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..split];
    let path = &rest[split..];
    // `URL.host` excludes userinfo.
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .to_lowercase();
    if !{
        let mut chars = host.chars();
        match chars.next() {
            Some(f) if f.is_alphanumeric() || f == '_' || f == '.' || f == '[' => {
                chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | ':' | '[' | ']' | '-'))
            }
            _ => false,
        }
    } {
        return REDACTED.to_string();
    }
    let segments: Vec<&str> = path
        .split('/')
        .filter(|s| !s.is_empty() && matches_word_dot_dash(s))
        .collect();
    let tail = segments[segments.len().saturating_sub(2)..].join("/");
    if tail.is_empty() {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}/{tail}")
    }
}

/// `rNd` — the host of a remote URL, when it is a plausible one.
#[must_use]
pub fn remote_host(url: &str) -> Option<String> {
    let host = if let Some(i) = url.find("://") {
        let rest = &url[i + 3..];
        match rest.find('/') {
            Some(j) if j > 0 => Some(rest[..j].to_string()),
            _ => None,
        }
    } else {
        match url.find(':') {
            Some(j) if j > 0 => {
                let h = url[..j].to_string();
                if h.len() == 1 {
                    None
                } else {
                    Some(h)
                }
            }
            _ => None,
        }
    }?;
    is_plausible_host(&host).then_some(host)
}

/// Supplies the repository facts.
pub trait RepoFactsSource {
    /// `hFt` — run `git -C <repo> <hardening flags> <args>`; stdout with one
    /// trailing newline removed, or `""` on a non-zero exit.
    fn git(&self, args: &[&str]) -> String;
    /// `Vsy` — the number of output lines, or `0` on failure.
    fn git_line_count(&self, args: &[&str]) -> usize;
    /// `None` when the path (or a component) does not exist; `Some(true)` when
    /// any component is a symlink.
    fn path_has_symlink_component(&self, relative: &str) -> Option<bool>;
    /// Contained, no-follow read of a project file.
    fn read_file(&self, relative: &str, cap: usize) -> Option<String>;
    /// The repository path as given.
    fn repo_path(&self) -> String;
}

/// The repo-facts section plus the host it derived for downstream gathers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFacts {
    /// The section body.
    pub body: String,
    /// `thisRepoHost` — the origin host, when derivable.
    pub this_repo_host: Option<String>,
}

/// `Ysy` — the "Repo facts" section.
#[must_use]
pub fn repo_facts_section(source: &dyn RepoFactsSource) -> RepoFacts {
    use crate::auto_mode_facts as facts;
    use crate::auto_mode_sections as sections;

    let remotes_raw = source.git(&["remote"]);
    let origin_head = source.git(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]);
    let tracked = source.git_line_count(&["ls-files"]);
    let origin_url = source.git(&["remote", "get-url", "origin"]);

    let redacted_origin = if origin_url.is_empty() {
        String::new()
    } else {
        redact_remote_url(&origin_url)
    };
    let this_repo_host = remote_host(&redacted_origin);

    let branch = origin_head
        .strip_prefix("origin/")
        .unwrap_or(&origin_head)
        .to_string();
    let default_branch = if branch.is_empty() {
        facts::UNKNOWN_DEFAULT_BRANCH.to_string()
    } else if branch.len() <= 256 && matches_branch_shape(&branch) {
        branch
    } else {
        sections::REDACTED_UNUSUAL_BRANCH_NAME.to_string()
    };

    let safe_remote_name = |n: &str| {
        if n.len() <= 256 && matches_word_dot_dash(n) {
            n.to_string()
        } else {
            sections::REDACTED_UNUSUAL_REMOTE_NAME.to_string()
        }
    };

    let mut remote_lines: Vec<String> = Vec::new();
    for name in remotes_raw.lines().filter(|l| !l.is_empty()).take(10) {
        let urls: Vec<String> = source
            .git(&["config", "-z", "--get-all", &format!("remote.{name}.url")])
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(redact_remote_url)
            .collect();
        let pushurls: Vec<String> = source
            .git(&[
                "config",
                "-z",
                "--get-all",
                &format!("remote.{name}.pushurl"),
            ])
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(redact_remote_url)
            .collect();
        let label = safe_remote_name(name);
        for u in &urls {
            remote_lines.push(format!("{label}\t{u} (fetch)"));
        }
        let push_from = if pushurls.is_empty() {
            &urls
        } else {
            &pushurls
        };
        for u in push_from {
            remote_lines.push(format!("{label}\t{u} (push)"));
        }
    }
    let omitted = remote_lines.len().saturating_sub(REMOTE_LINE_CAP);
    let mut remotes_block = remote_lines
        .iter()
        .take(REMOTE_LINE_CAP)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if omitted > 0 {
        remotes_block.push_str(&format!(
            "\n\u{2026}[{omitted}{}",
            facts::REMOTE_LINES_OMITTED_SUFFIX
        ));
    }

    // A posture signal counts only when it exists AND no path component is a
    // symlink — a committed symlink must not be followed just to tick a box.
    let signals: Vec<&str> = POSTURE_SIGNAL_PATHS
        .into_iter()
        .filter(|p| source.path_has_symlink_component(p) == Some(false))
        .collect();

    let contributing = source.read_file("CONTRIBUTING.md", CONTRIBUTING_READ_CAP);
    let gitignore = source
        .read_file(".gitignore", DOC_READ_CAP)
        .unwrap_or_default();
    let sensitive: Vec<&str> = gitignore
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter(|l| {
            let low = l.to_lowercase();
            [
                "secret",
                "credential",
                ".env",
                "key",
                "token",
                "pii",
                "private",
            ]
            .iter()
            .any(|n| low.contains(n))
        })
        .take(FLAGGED_LIST_CAP)
        .collect();

    let repo_path = source.repo_path();
    let shown_path = if repo_path.chars().any(|c| {
        matches!(
            c,
            '\r' | '\n' | '\u{b}' | '\u{c}' | '\u{85}' | '\u{2028}' | '\u{2029}' | '`'
        )
    }) {
        sections::REDACTED_UNUSUAL_REPO_PATH.to_string()
    } else {
        repo_path
    };

    let parts = [
        format!("Repo path: {shown_path}"),
        format!("{}{tracked}", facts::REPO_TRACKED_FILE_COUNT_PREFIX),
        format!("{}{default_branch}", facts::REPO_DEFAULT_BRANCH_PREFIX),
        format!(
            "{}{}",
            facts::REPO_POSTURE_SIGNALS_PREFIX,
            if signals.is_empty() {
                "none".to_string()
            } else {
                signals.join(", ")
            }
        ),
        format!(
            "{}{}",
            sections::HEADING_GIT_REMOTES,
            if remotes_block.is_empty() {
                facts::NO_REMOTES.to_string()
            } else {
                remotes_block
            }
        ),
        contributing.map_or(String::new(), |c| {
            format!("\n{}", render_doc(facts::DOC_CONTRIBUTING_HEAD_LABEL, &c))
        }),
        if sensitive.is_empty() {
            String::new()
        } else {
            format!(
                "{}{}",
                sections::HEADING_SENSITIVE_GITIGNORE,
                sensitive
                    .iter()
                    .map(|p| format!("- `{}`", display_name(p)))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        },
        crate::auto_mode_gates::REPO_FACTS_GH_EXPLAINER.to_string(),
    ];

    RepoFacts {
        // NOTE: joined WITHOUT filtering empties -- the oracle keeps the blank
        // lines an absent CONTRIBUTING.md or gitignore section leaves behind.
        body: parts.join("\n"),
        this_repo_host,
    }
}

// ── `x1d` — repo visibility & branch protection (via gh) ─────────────────────

/// `R1d` — timeout for a `gh` call, in milliseconds.
pub const GH_TIMEOUT_MS: u64 = 4_000;
/// `V$e` — what a failed capability renders as.
pub const NOT_QUERYABLE_HERE: &str = "not queryable here";
/// `e$s` — appended when visibility could not be determined.
pub const INFER_VISIBILITY_HINT: &str =
    "Infer visibility from the remote hostname in Repo facts, or ask.";
/// `_1d` — hosts a remote may name for the org/repo parse to be trusted.
pub const KNOWN_VCS_HOSTS: [&str; 3] = ["github.com", "gitlab.com", "bitbucket.org"];
/// How many names a list shows before `(+N more)`.
pub const GH_LIST_SHOWN: usize = 20;

/// `k1d` — `^[\w.][\w ./-]{0,119}$`, the charset a name must fit to be shown.
#[must_use]
pub fn is_displayable_gh_name(name: &str) -> bool {
    let mut chars = name.chars();
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let Some(first) = chars.next() else {
        return false;
    };
    if !(word(first) || first == '.') {
        return false;
    }
    let rest: Vec<char> = chars.collect();
    rest.len() <= 119
        && rest
            .iter()
            .all(|c| word(*c) || matches!(c, '.' | ' ' | '/' | '-'))
}

/// `I1d` — a GitHub visibility value, or `None`.
#[must_use]
pub fn parse_visibility(v: &str) -> Option<String> {
    let l = v.to_lowercase();
    matches!(l.as_str(), "public" | "private" | "internal").then_some(l)
}

/// `D1d` — backtick and join a name list, noting how many were cut.
#[must_use]
pub fn join_gh_names(names: &[String], limit: usize) -> String {
    let shown: Vec<String> = names.iter().take(limit).map(|n| format!("`{n}`")).collect();
    let more = names.len().saturating_sub(shown.len());
    let suffix = if more > 0 {
        format!(" (+{more} more)")
    } else {
        String::new()
    };
    format!("{}{suffix}", shown.join(", "))
}

/// The outcome of one `gh` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhResult {
    /// Process exit code.
    pub code: i32,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr — read by [`gh_is_unavailable`].
    pub stderr: String,
}

/// `Lhr` — did `gh` fail because it is missing or unauthenticated, rather than
/// because the call itself went wrong?
///
/// The distinction decides whether the failure is worth a telemetry event: an
/// absent `gh` is an ordinary environment fact, a broken call is not.
#[must_use]
pub fn gh_is_unavailable(res: &GhResult) -> bool {
    res.code == 127 || res.code == 4 || (res.code == 1 && res.stderr.is_empty())
}

/// `t$s` — the environment a `gh` call runs with.
///
/// Two things happen here, and the second is the important one. `GH_HOST` is
/// forced to github.com, and the ENTERPRISE tokens are always cleared. On top
/// of that, if the user's own `GH_HOST` pointed at a DIFFERENT host, their
/// `GH_TOKEN`/`GITHUB_TOKEN` are cleared too — those credentials belong to that
/// other host, and this call is about to go to github.com. Forwarding them
/// would hand a GHE token to a server it was never issued for.
///
/// Returns the variables to OVERRIDE, with `None` meaning "remove".
#[must_use]
pub fn gh_env_overrides(current_gh_host: Option<&str>) -> Vec<(&'static str, Option<String>)> {
    let points_elsewhere = current_gh_host.map(|h| !is_github_host(h)).unwrap_or(false);
    let mut out: Vec<(&'static str, Option<String>)> = vec![
        ("GH_HOST", Some("github.com".to_string())),
        ("GH_ENTERPRISE_TOKEN", None),
        ("GITHUB_ENTERPRISE_TOKEN", None),
    ];
    if points_elsewhere {
        out.push(("GH_TOKEN", None));
        out.push(("GITHUB_TOKEN", None));
    }
    out
}

/// `hcn` — reduce a git remote to `host/org/repo`, or `None`.
///
/// Refuses anything it cannot read unambiguously: a password in the URL, a
/// non-`git` username, an explicit port, an unknown host, or a path that is not
/// exactly two safe segments. The result feeds `gh` calls, so a wrong parse
/// would send a query about somebody else's repository.
#[must_use]
pub fn remote_to_host_org_repo(url: &str, this_repo_host: Option<&str>) -> Option<String> {
    if url
        .chars()
        .any(|c| c <= ' ' || c > '~' || c == '\\' || c == '%')
    {
        return None;
    }
    let mut hosts: Vec<String> = KNOWN_VCS_HOSTS.iter().map(|h| (*h).to_string()).collect();
    if let Some(extra) = this_repo_host.filter(|h| is_plausible_host(h)) {
        hosts.push(extra.to_lowercase());
    }

    let (host, path) = if url.contains("://") {
        let parsed = url::Url::parse(url).ok()?;
        if parsed.password().is_some_and(|p| !p.is_empty()) {
            return None;
        }
        match parsed.scheme() {
            "https" => {
                if !parsed.username().is_empty() {
                    return None;
                }
            }
            "ssh" | "git" => {
                if !parsed.username().is_empty() && parsed.username() != "git" {
                    return None;
                }
            }
            _ => return None,
        }
        if parsed.port().is_some() {
            return None;
        }
        (parsed.host_str()?.to_string(), parsed.path().to_string())
    } else {
        let colon = url.find(':')?;
        if colon == 0 {
            return None;
        }
        let head = &url[..colon];
        let host = head.strip_prefix("git@")?.to_string();
        let rest = &url[colon + 1..];
        let path = if rest.starts_with('/') {
            rest.to_string()
        } else {
            format!("/{rest}")
        };
        (host, path)
    };

    if !hosts.contains(&host.to_lowercase()) {
        return None;
    }
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() != 3 || !segments[0].is_empty() {
        return None;
    }
    if !segments[1..].iter().all(|s| is_plausible_host(s)) {
        return None;
    }
    Some(format!(
        "{}/{}/{}",
        host.to_lowercase(),
        segments[1],
        segments[2]
    ))
}

/// `Asy` — the visibility line.
#[must_use]
pub fn render_visibility(res: &GhResult) -> String {
    if res.code != 0 {
        return NOT_QUERYABLE_HERE.to_string();
    }
    let parsed = serde_json::from_str::<Value>(if res.stdout.is_empty() {
        "{}"
    } else {
        &res.stdout
    });
    parsed
        .ok()
        .and_then(|v| {
            v.get("visibility")
                .and_then(Value::as_str)
                .and_then(parse_visibility)
        })
        .unwrap_or_else(|| NOT_QUERYABLE_HERE.to_string())
}

/// `ksy` — the protected-branches line.
#[must_use]
pub fn render_protected_branches(res: &GhResult) -> String {
    if res.code != 0 {
        return NOT_QUERYABLE_HERE.to_string();
    }
    let names: Vec<String> = res
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if names.is_empty() {
        return "none listed".to_string();
    }
    let shown: Vec<String> = names
        .iter()
        .filter(|n| is_displayable_gh_name(n))
        .cloned()
        .collect();
    let redacted = names.len() - shown.len();
    let redacted_note = if redacted > 0 {
        format!(
            "{redacted}{}",
            crate::auto_mode_sections::REDACTED_NAMES_OUTSIDE_CHARSET_SUFFIX
        )
    } else {
        String::new()
    };
    let capped = if names.len() == 100 {
        crate::auto_mode_facts::FIRST_100_ONLY_SUFFIX.to_string()
    } else {
        String::new()
    };
    if shown.is_empty() {
        format!(
            "{}{}{capped}",
            names.len(),
            crate::auto_mode_sections::REDACTED_ALL_NAMES_OUTSIDE_CHARSET
        )
    } else {
        format!(
            "{}{}{capped}",
            join_gh_names(&shown, GH_LIST_SHOWN),
            if redacted > 0 {
                format!(" (+{redacted_note}")
            } else {
                String::new()
            }
        )
    }
}

/// `Rsy` — the rulesets line.
#[must_use]
pub fn render_rulesets(res: &GhResult) -> String {
    if res.code != 0 {
        return NOT_QUERYABLE_HERE.to_string();
    }
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(if res.stdout.is_empty() {
        "[]"
    } else {
        &res.stdout
    }) else {
        return NOT_QUERYABLE_HERE.to_string();
    };
    let typed: Vec<(&str, &str)> = items
        .iter()
        .filter_map(|i| Some((i.get("name")?.as_str()?, i.get("enforcement")?.as_str()?)))
        .collect();
    let mut redacted = items.len() - typed.len();
    let total = typed.len() + redacted;
    if total == 0 {
        return "none listed".to_string();
    }
    let mut shown: Vec<String> = Vec::new();
    for (name, enforcement) in typed {
        let e = enforcement.to_lowercase();
        if matches!(e.as_str(), "active" | "evaluate" | "disabled") && is_displayable_gh_name(name)
        {
            shown.push(format!("`{name}` - {e}"));
        } else {
            redacted += 1;
        }
    }
    let capped = if total == 100 {
        crate::auto_mode_facts::FIRST_100_ONLY_SUFFIX.to_string()
    } else {
        String::new()
    };
    let redacted_note = if redacted > 0 {
        format!(
            " (+{redacted}{}",
            crate::auto_mode_sections::REDACTED_NAMES_OUTSIDE_CHARSET_SUFFIX
        )
    } else {
        String::new()
    };
    if shown.is_empty() {
        return format!(
            "{total}{}{capped}",
            crate::auto_mode_sections::REDACTED_ALL_NAMES_OUTSIDE_CHARSET
        );
    }
    let head: Vec<String> = shown.iter().take(GH_LIST_SHOWN).cloned().collect();
    let more = shown.len() - head.len();
    let more_note = if more > 0 {
        format!(" (+{more} more)")
    } else {
        String::new()
    };
    format!("{}{more_note}{redacted_note}{capped}", head.join(", "))
}

/// `xsy` — the org's repos grouped by visibility, newest push first.
///
/// Returns the body and whether the output failed to parse (the caller emits
/// `org_list_gh_parse_failed`).
///
/// Every repo that does not survive the name charset, the visibility enum, or
/// the expected JSON shape is COUNTED into the redaction note rather than
/// dropped silently — otherwise a filtered list would read as the org's
/// complete inventory.
#[must_use]
pub fn render_org_repo_list(res: &GhResult) -> (String, bool) {
    use crate::auto_mode_sections as sections;

    if res.code != 0 {
        return (
            format!("_{NOT_QUERYABLE_HERE}{}", sections::GH_ORG_SCOPE_SUFFIX),
            false,
        );
    }
    let unparseable = || {
        (
            format!("_{NOT_QUERYABLE_HERE}{}", sections::GH_UNPARSEABLE_SUFFIX),
            true,
        )
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(if res.stdout.is_empty() {
        "[]"
    } else {
        &res.stdout
    }) else {
        return unparseable();
    };

    // Shape filter: `name` and `visibility` must be strings, `pushedAt` a
    // string or null (a never-pushed repo sorts last, it is not an error).
    let total = items.len();
    let shaped: Vec<(String, String, String)> = items
        .iter()
        .filter_map(|item| {
            let name = item.get("name")?.as_str()?.to_string();
            let visibility = item.get("visibility")?.as_str()?.to_string();
            let pushed = match item.get("pushedAt") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) => String::new(),
                _ => return None,
            };
            Some((name, visibility, pushed))
        })
        .collect();
    let shape_dropped = total - shaped.len();

    let named: Vec<&(String, String, String)> = shaped
        .iter()
        .filter(|(name, _, _)| is_valid_repo_name(name) && name.len() <= 100)
        .collect();
    let name_dropped = shaped.len() - named.len();

    let mut visible: Vec<(&str, &str, &str)> = Vec::new();
    let mut visibility_dropped = 0usize;
    for (name, visibility, pushed) in named {
        match parse_visibility(visibility) {
            Some(_) => visible.push((name.as_str(), visibility.as_str(), pushed.as_str())),
            None => visibility_dropped += 1,
        }
    }
    // Newest push first, then the top 50.
    visible.sort_by(|a, b| b.2.cmp(a.2));
    visible.truncate(ORG_REPO_SPLIT_LIMIT);

    let mut groups: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (name, visibility, _) in &visible {
        groups
            .entry(parse_visibility(visibility).unwrap_or_else(|| (*visibility).to_lowercase()))
            .or_default()
            .push((*name).to_string());
    }

    let redacted = shape_dropped + name_dropped + visibility_dropped;
    let note = if redacted > 0 {
        format!(
            "(+{redacted}{}",
            sections::REDACTED_OUTSIDE_CHARSET_OR_VISIBILITY
        )
    } else {
        String::new()
    };

    if groups.is_empty() {
        return (
            if redacted > 0 {
                format!("_none listed {note}_")
            } else {
                "_none listed_".to_string()
            },
            false,
        );
    }
    let lines: Vec<String> = groups
        .into_iter()
        .map(|(visibility, names)| {
            format!("- {visibility}: {}", join_gh_names(&names, GH_LIST_SHOWN))
        })
        .collect();
    let body = if redacted > 0 {
        format!("{}\n_{note}_", lines.join("\n"))
    } else {
        lines.join("\n")
    };
    (body, false)
}

/// `xsy`'s cap — how many repos the org split shows.
pub const ORG_REPO_SPLIT_LIMIT: usize = 50;

/// Supplies the `gh` calls `x1d` makes.
pub trait GhSource {
    /// `git -C <cwd> remote get-url origin`, or `None` when it failed.
    fn origin_remote(&self) -> Option<String>;
    /// Run `gh` with `args`, capping the captured output at `max_buffer`.
    fn gh(&self, args: &[&str], max_buffer: usize) -> GhResult;
}

/// Which of `x1d`'s `gh` calls failed for a reason worth recording.
///
/// An absent or unauthenticated `gh` is an ordinary environment fact and is
/// deliberately NOT counted here — only a call that broke for some other reason
/// is, so the telemetry measures real breakage instead of how many users lack
/// the tool.
// Four booleans mirroring the oracle's `visibility_gh_failed` payload exactly.
// Collapsing them into a set or bitflags would change the telemetry shape.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GhFailures {
    /// `gh repo view` failed.
    pub view_failed: bool,
    /// The rulesets API call failed.
    pub rulesets_failed: bool,
    /// The protected-branches API call failed.
    pub branches_failed: bool,
    /// The org list failed (only when it was attempted).
    pub org_list_failed: bool,
}

impl GhFailures {
    /// Did anything fail for a recordable reason?
    #[must_use]
    pub fn any(self) -> bool {
        self.view_failed || self.rulesets_failed || self.branches_failed || self.org_list_failed
    }
}

/// What one `x1d` run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoVisibilityOutcome {
    /// The section body.
    pub body: String,
    /// Calls that broke, for `visibility_gh_failed`.
    pub failures: GhFailures,
    /// The org list did not parse, for `org_list_gh_parse_failed`.
    pub org_list_parse_failed: bool,
}

impl RepoVisibilityOutcome {
    fn refused(reason: &str) -> Self {
        Self {
            body: format!("_Not queryable here ({reason}). {INFER_VISIBILITY_HINT}_"),
            failures: GhFailures::default(),
            org_list_parse_failed: false,
        }
    }
}

/// `x1d` — the "Repo visibility & branch protection (via gh)" section body.
///
/// `org_split` is the Q2 = `all` gate. Only github.com origins are queried:
/// deriving an org from an unrecognised remote shape and asking GitHub about
/// it would send somebody else's repository name to the API, so every
/// ambiguous case refuses instead of guessing.
#[must_use]
pub fn repo_visibility_section(
    source: &dyn GhSource,
    org_split: bool,
    nonessential_traffic_allowed: bool,
) -> RepoVisibilityOutcome {
    if !nonessential_traffic_allowed {
        return RepoVisibilityOutcome::refused(
            "nonessential traffic disabled or policy-restricted",
        );
    }
    let Some(reduced) = source
        .origin_remote()
        .and_then(|url| remote_to_host_org_repo(url.trim(), None))
    else {
        return RepoVisibilityOutcome::refused(
            "org/repo not derivable from origin remote \u{2014} missing, an unsupported or GHE host, or not a plain owner/repo URL shape",
        );
    };
    let parts: Vec<&str> = reduced.split('/').collect();
    let [host, org, repo] = parts.as_slice() else {
        return RepoVisibilityOutcome::refused(
            "org/repo not derivable from origin remote \u{2014} missing, an unsupported or GHE host, or not a plain owner/repo URL shape",
        );
    };
    if *host != "github.com" {
        return RepoVisibilityOutcome::refused(
            "origin remote is not github.com \u{2014} GHE/other hosts not yet supported",
        );
    }
    let slug = format!("{org}/{repo}");

    let view = source.gh(&["repo", "view", &slug, "--json", "visibility"], 8_192);
    let rulesets = source.gh(
        &[
            "api",
            &format!("repos/{slug}/rulesets?per_page=100"),
            "--jq",
            crate::auto_mode_facts::GH_RULESETS_JQ,
        ],
        32_768,
    );
    let branches = source.gh(
        &[
            "api",
            &format!("repos/{slug}/branches?protected=true&per_page=100"),
            "--jq",
            ".[].name",
        ],
        32_768,
    );

    let (org_body, org_parse_failed, org_result) = if org_split {
        let res = source.gh(
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
        let (body, parse_failed) = render_org_repo_list(&res);
        (body, parse_failed, Some(res))
    } else {
        (
            crate::auto_mode_gates::ORG_REPO_SPLIT_NOT_GATHERED.to_string(),
            false,
            None,
        )
    };

    let broke = |res: &GhResult| res.code != 0 && !gh_is_unavailable(res);
    let failures = GhFailures {
        view_failed: broke(&view),
        rulesets_failed: broke(&rulesets),
        branches_failed: broke(&branches),
        org_list_failed: org_result.as_ref().is_some_and(broke),
    };

    let body = [
        format!("Repo: {slug}"),
        format!("Visibility: {}", render_visibility(&view)),
        format!("Rulesets: {}", render_rulesets(&rulesets)),
        format!(
            "Protected branches: {}",
            render_protected_branches(&branches)
        ),
        String::new(),
        format!("#### {}", crate::auto_mode_propose::ORG_REPO_SPLIT_HEADING),
        org_body,
    ]
    .join("\n");

    RepoVisibilityOutcome {
        body,
        failures,
        org_list_parse_failed: org_parse_failed,
    }
}

// ── `j1d` — other git repos under the home directory ─────────────────────────

/// `rsy` — how many directories the walk will visit.
pub const HOME_WALK_MAX_DIRS: usize = 4_000;
/// `nsy` — how many repos it will report.
pub const HOME_WALK_MAX_REPOS: usize = 20;
/// `tsy` — how deep below the home directory it will descend.
pub const HOME_WALK_MAX_DEPTH: usize = 2;
/// `isy` — the walk's time budget, in milliseconds.
pub const HOME_WALK_TIMEOUT_MS: u64 = 8_000;
/// `osy` — how many remotes are reported per repo.
pub const REPO_REMOTE_LIMIT: usize = 5;
/// `ssy` — read cap for a `.git/config`.
pub const GIT_CONFIG_READ_CAP: usize = 128_000;
/// `g1d` — read cap for a `.git` FILE (the `gitdir:` pointer).
pub const GITDIR_FILE_READ_CAP: usize = 4_096;
/// How many lines of a `.git/config` are parsed.
pub const GIT_CONFIG_MAX_LINES: usize = 2_000;

/// `asy` — directories never descended into.
pub const WALK_SKIP_DIRS: [&str; 13] = [
    ".git",
    "node_modules",
    ".oh-my-zsh",
    ".vim",
    ".tmux",
    ".nvm",
    ".rustup",
    ".cargo",
    ".local",
    ".cache",
    ".npm",
    ".gem",
    ".lingxi",
];
/// `lsy` — cloud-sync roots, matched as the whole name or a `name …` prefix.
///
/// Descending into one would touch a synced folder and can wake a sync client
/// or pull content down from the network.
pub const WALK_SKIP_SYNC_ROOTS: [&str; 3] = ["onedrive", "dropbox", "google drive"];
/// `csy` — additionally skipped on Windows.
pub const WALK_SKIP_WINDOWS: [&str; 2] = ["appdata", "application data"];
/// `usy` — additionally skipped on macOS.
pub const WALK_SKIP_MACOS: [&str; 1] = ["library"];

/// `msy` — should the walk refuse to descend into this directory name?
#[must_use]
pub fn is_skipped_walk_dir(name: &str, windows: bool, macos: bool) -> bool {
    if WALK_SKIP_DIRS.contains(&name) {
        return true;
    }
    let lower = name.to_lowercase();
    if WALK_SKIP_SYNC_ROOTS
        .iter()
        .any(|s| lower == *s || lower.starts_with(&format!("{s} ")))
    {
        return true;
    }
    if windows && WALK_SKIP_WINDOWS.contains(&lower.as_str()) {
        return true;
    }
    macos && WALK_SKIP_MACOS.contains(&lower.as_str())
}

/// `hsy` — read a `.git` FILE's `gitdir: <path>` pointer.
#[must_use]
pub fn parse_gitdir_pointer(text: &str) -> Option<String> {
    let rest = text.strip_prefix("gitdir: ")?;
    let trimmed = rest.trim_end_matches(['\r', '\n']);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// `YBs` — is `path` strictly below `root`?
#[must_use]
pub fn is_strictly_under(path: &str, root: &str, windows: bool) -> bool {
    let Some(rel) = crate::auto_mode_io::rebase_path(path, root, "", windows) else {
        return false;
    };
    let rel = rel.trim_start_matches(if windows { '\\' } else { '/' });
    let cmp = if windows {
        rel.to_lowercase()
    } else {
        rel.to_string()
    };
    !cmp.is_empty()
        && cmp != ".."
        && !cmp.starts_with("../")
        && !cmp.starts_with(r"..\")
        && !is_absolute_for(windows, &cmp)
}

/// `fsy` — render a path as `~` or `~/relative`.
#[must_use]
pub fn home_relative(path: &str, home: &str, windows: bool) -> String {
    let norm = |s: &str| {
        if windows {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };
    if norm(path) == norm(home) {
        return "~".to_string();
    }
    let sep = if windows { '\\' } else { '/' };
    let prefix = if home.ends_with(sep) {
        home.to_string()
    } else {
        format!("{home}{sep}")
    };
    if !norm(path).starts_with(&norm(&prefix)) {
        return path.to_string();
    }
    let rel = &path[prefix.len()..];
    let rel = if windows {
        rel.replace('\\', "/")
    } else {
        rel.to_string()
    };
    format!("~/{rel}")
}

fn is_config_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

fn skip_config_ws(s: &[char], mut i: usize) -> usize {
    while i < s.len() && matches!(s[i], ' ' | '\t' | '\r') {
        i += 1;
    }
    i
}

/// `Qiy` — parse a `[section]` / `[section "sub"]` header.
///
/// Returns whether it opened a NAMED remote section and where it ended.
fn parse_config_section(s: &[char], at: usize) -> Option<(bool, usize)> {
    let mut r = at + 1;
    let start = r;
    while r < s.len() && (is_config_key_char(s[r]) || s[r] == '.') {
        r += 1;
    }
    let name: String = s[start..r].iter().collect();
    if s.get(r) == Some(&']') {
        if name.is_empty() {
            return None;
        }
        let named = name
            .find('.')
            .is_some_and(|d| name[..d].eq_ignore_ascii_case("remote"));
        return Some((named, r + 1));
    }
    if !matches!(s.get(r), Some(' ' | '\t' | '\r')) {
        return None;
    }
    r = skip_config_ws(s, r);
    if s.get(r) != Some(&'"') {
        return None;
    }
    r += 1;
    loop {
        let c = *s.get(r)?;
        if c == '\\' {
            r += 2;
            continue;
        }
        r += 1;
        if c == '"' {
            break;
        }
    }
    if s.get(r) != Some(&']') {
        return None;
    }
    let lower = name.to_lowercase();
    Some((lower == "remote" || lower.starts_with("remote."), r + 1))
}

/// `Ziy` — parse a config value, following `\` line continuations.
fn parse_config_value(
    line: &[char],
    from: usize,
    lines: &[Vec<char>],
    mut next: usize,
) -> (Option<String>, usize) {
    let mut cur: Vec<char> = line.to_vec();
    let mut value = String::new();
    let mut pending_space = String::new();
    let mut in_quotes = false;
    let mut i = from;
    loop {
        if i >= cur.len() {
            if in_quotes {
                return (None, next);
            }
            break;
        }
        let c = cur[i];
        if !in_quotes && matches!(c, ' ' | '\t' | '\r') {
            if !value.is_empty() {
                pending_space = " ".to_string();
            }
            i += 1;
            continue;
        }
        if !in_quotes && (c == ';' || c == '#') {
            break;
        }
        value.push_str(&pending_space);
        pending_space.clear();
        if c == '\\' {
            if i + 1 >= cur.len() {
                if next >= lines.len() {
                    return (None, next);
                }
                cur = lines[next].clone();
                next += 1;
                i = 0;
                continue;
            }
            let escaped = match cur[i + 1] {
                '\\' => '\\',
                '"' => '"',
                'n' => '\n',
                't' => '\t',
                'b' => '\u{8}',
                _ => return (None, next),
            };
            value.push(escaped);
            i += 2;
            continue;
        }
        if c == '"' {
            in_quotes = !in_quotes;
            i += 1;
            continue;
        }
        value.push(c);
        i += 1;
    }
    // A NUL truncates the value.
    let cut = match value.find('\0') {
        Some(n) => value[..n].to_string(),
        None => value,
    };
    (Some(cut), next)
}

/// `GBs` — the `url`/`pushurl` values of every named remote in a `.git/config`.
///
/// This is a parser rather than a `git config` call on purpose: these are OTHER
/// people's repositories under the home directory, and running git inside one
/// would let its own config, hooks and credential helpers act.
#[must_use]
pub fn parse_config_remote_urls(text: &str) -> Vec<String> {
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<Vec<char>> = body
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).chars().collect())
        .take(GIT_CONFIG_MAX_LINES)
        .collect();

    let mut out = Vec::new();
    let mut in_named_remote = false;
    let mut i = 0usize;
    while i < lines.len() {
        let s = lines[i].clone();
        i += 1;
        let mut a = skip_config_ws(&s, 0);
        while s.get(a) == Some(&'[') {
            match parse_config_section(&s, a) {
                None => {
                    in_named_remote = false;
                    a = s.len();
                    break;
                }
                Some((named, end)) => {
                    in_named_remote = named;
                    a = skip_config_ws(&s, end);
                }
            }
        }
        let Some(&first) = s.get(a) else { continue };
        if first == '#' || first == ';' {
            continue;
        }
        if !first.is_ascii_alphabetic() {
            in_named_remote = false;
            continue;
        }
        let key_start = a;
        while a < s.len() && is_config_key_char(s[a]) {
            a += 1;
        }
        let key: String = s[key_start..a].iter().collect::<String>().to_lowercase();
        while a < s.len() && matches!(s[a], ' ' | '\t') {
            a += 1;
        }
        if a >= s.len() {
            continue;
        }
        if s[a] != '=' {
            in_named_remote = false;
            continue;
        }
        let (value, next) = parse_config_value(&s, a + 1, &lines, i);
        i = next;
        let Some(value) = value else {
            in_named_remote = false;
            continue;
        };
        if !value.is_empty() && in_named_remote && (key == "url" || key == "pushurl") {
            out.push(value);
        }
    }
    out
}

/// `psy` — the reportable remotes of a repo: parsed, reduced to
/// `host/org/repo`, de-duplicated and capped.
#[must_use]
pub fn config_remotes(config: &str, this_repo_host: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for url in parse_config_remote_urls(config) {
        let Some(reduced) = remote_to_host_org_repo(&url, this_repo_host) else {
            continue;
        };
        if !out.contains(&reduced) {
            out.push(reduced);
            if out.len() >= REPO_REMOTE_LIMIT {
                break;
            }
        }
    }
    out
}

/// Why a discovered repo has no remotes to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeRepoNote {
    /// Its gitdir points outside the home directory, so it was not read.
    GitdirOutsideHome,
    /// It genuinely has no remote configured.
    NoRemote,
}

/// One repository found under the home directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeRepo {
    /// `~/relative` form.
    pub path: String,
    /// Reduced remotes.
    pub remotes: Vec<String>,
    /// Why remotes are absent, when they are.
    pub note: Option<HomeRepoNote>,
}

/// How the walk ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkLimit {
    /// It finished.
    None,
    /// The home directory is a network path.
    NetworkHome,
    /// The home directory could not be read.
    HomeUnreadable,
    /// It ran out of time.
    Timeout,
    /// It ran out of directory visits.
    VisitBudget,
    /// It hit the repo cap.
    RepoCap,
}

/// `aay` — the note explaining an incomplete walk.
#[must_use]
pub fn walk_limit_note(limit: WalkLimit) -> &'static str {
    use crate::auto_mode_facts as facts;
    use crate::auto_mode_gates as gates;
    match limit {
        WalkLimit::None | WalkLimit::NetworkHome | WalkLimit::HomeUnreadable => "",
        WalkLimit::Timeout => gates::WALK_HIT_TIME_BUDGET,
        WalkLimit::VisitBudget => gates::WALK_HIT_DIRECTORY_BUDGET,
        WalkLimit::RepoCap => facts::RESULT_CAP_REACHED,
    }
}

/// `j1d` — the "Other git repos under the home directory" section body.
#[must_use]
pub fn home_repos_body(repos: &[HomeRepo], limit: WalkLimit) -> String {
    use crate::auto_mode_facts as facts;
    use crate::auto_mode_gates as gates;

    match limit {
        WalkLimit::NetworkHome => return gates::HOME_REPOS_NETWORK_HOME.to_string(),
        WalkLimit::HomeUnreadable => return gates::HOME_REPOS_UNREADABLE.to_string(),
        _ => {}
    }

    let lines: Vec<String> = repos
        .iter()
        .map(|r| {
            let joined = r
                .remotes
                .iter()
                .map(|m| display_name(m).to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let detail = match r.note {
                Some(HomeRepoNote::GitdirOutsideHome) => facts::GITDIR_OUTSIDE_HOME.to_string(),
                Some(HomeRepoNote::NoRemote) => facts::NO_REMOTE_CONFIGURED.to_string(),
                None if joined.is_empty() => facts::REMOTE_NOT_KNOWN_HOST.to_string(),
                None => joined,
            };
            format!("- `{}` \u{2014} {detail}", display_name(&r.path))
        })
        .collect();

    let head = if lines.is_empty() {
        if limit == WalkLimit::None {
            gates::NO_OTHER_REPOS_FOUND.to_string()
        } else {
            // Cut short with nothing found is UNKNOWN, not "there are none".
            gates::NO_REPOS_FOUND_WALK_CUT_SHORT.to_string()
        }
    } else {
        format!("{}{}", facts::REPOS_FOUND_HEADER, lines.join("\n"))
    };

    [
        head,
        walk_limit_note(limit).to_string(),
        facts::HOME_REPOS_CANDIDATE_NOTE.to_string(),
    ]
    .into_iter()
    .filter(|p| !p.is_empty())
    .collect::<Vec<_>>()
    .join("\n")
}

// ── `Xsy` — sibling repo docs (via gh) ───────────────────────────────────────

/// How many org repos `gh repo list` is asked for.
pub const SIBLING_REPO_LIST_LIMIT: usize = 5;
/// How many sibling repos are actually fetched, after filtering.
pub const SIBLING_DOC_LIMIT: usize = 3;
/// The docs tried, in order; the first one found per repo wins.
pub const SIBLING_DOC_NAMES: [&str; 2] = ["LINGXI.md", "README.md"];

/// `_cn` — `^(?!\.{1,2}$)[A-Za-z0-9_.][A-Za-z0-9_.-]*$`
///
/// Rejects `.` and `..` outright: those are path traversal, not repo names,
/// and the value is interpolated into a `gh api repos/…` path.
#[must_use]
pub fn is_valid_repo_name(name: &str) -> bool {
    if name == "." || name == ".." {
        return false;
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let ok_first = first.is_ascii_alphanumeric() || first == '_' || first == '.';
    ok_first && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// The sibling repos worth fetching: most recently pushed first, excluding this
/// repository itself, name-validated, capped.
#[must_use]
pub fn select_sibling_repos(list_json: &str, this_repo: &str) -> Option<Vec<String>> {
    let parsed = serde_json::from_str::<Value>(if list_json.is_empty() {
        "[]"
    } else {
        list_json
    })
    .ok()?;
    let items = parsed.as_array()?;
    let mut typed: Vec<(&str, &str)> = items
        .iter()
        .filter_map(|i| {
            let name = i.get("name")?.as_str()?;
            let pushed = match i.get("pushedAt") {
                Some(Value::String(s)) => s.as_str(),
                Some(Value::Null) | None => "",
                Some(_) => return None,
            };
            Some((name, pushed))
        })
        .collect();
    // Most recently pushed first.
    typed.sort_by(|a, b| b.1.cmp(a.1));
    Some(
        typed
            .into_iter()
            .map(|(name, _)| name)
            .filter(|n| !n.eq_ignore_ascii_case(this_repo) && is_valid_repo_name(n))
            .take(SIBLING_DOC_LIMIT)
            .map(str::to_string)
            .collect(),
    )
}

/// `…[truncated at N chars]` — the sibling-doc truncation suffix. Shorter than
/// `jIe`'s, which also reports the file's byte size.
#[must_use]
pub fn chars_truncated_marker(cap: usize) -> String {
    format!("\n\u{2026}[truncated at {cap} chars]")
}

/// Trim a fetched sibling doc to its reportable form.
///
/// `README.md` is cut to its first 40 lines and a 10 KB cap; `CLAUDE.md` gets
/// the larger 200 KB cap. A cut always announces itself.
#[must_use]
pub fn trim_sibling_doc(doc_name: &str, content: &str) -> String {
    let (body, cap) = if doc_name == "README.md" {
        (
            content
                .split('\n')
                .take(README_HEAD_LINES)
                .collect::<Vec<_>>()
                .join("\n"),
            DOC_READ_CAP,
        )
    } else {
        (content.to_string(), DOC_READ_CAP_LINGXI_MD)
    };
    if body.chars().count() > cap {
        let head: String = body.chars().take(cap).collect();
        format!("{head}{}", chars_truncated_marker(cap))
    } else {
        body
    }
}

/// The label a sibling doc is rendered under.
#[must_use]
pub fn sibling_doc_label(org: &str, repo: &str, doc_name: &str) -> String {
    let shown = if doc_name == "README.md" {
        format!("{doc_name} (head)")
    } else {
        doc_name.to_string()
    };
    format!("sibling {org}/{repo}/{shown}")
}

/// Supplies the sibling-docs gh calls.
pub trait SiblingDocsSource {
    /// `gh repo list <org> --limit 5 --json name,pushedAt`; `None` when gh
    /// could not be used at all.
    fn list_org_repos(&self, org: &str) -> Option<String>;
    /// `gh api repos/<org>/<repo>/contents/<doc> --jq .content`, already
    /// base64-decoded. `None` when absent or unreadable.
    fn fetch_doc(&self, org: &str, repo: &str, doc: &str) -> Option<String>;
}

/// `Xsy`'s body once the org and repo are known and the gates are open.
#[must_use]
pub fn sibling_docs_body(org: &str, this_repo: &str, source: &dyn SiblingDocsSource) -> String {
    let Some(list) = source.list_org_repos(org) else {
        return crate::auto_mode_gates::NOT_QUERYABLE_GH_UNAVAILABLE.to_string();
    };
    let Some(repos) = select_sibling_repos(&list, this_repo) else {
        return crate::auto_mode_gates::NOT_QUERYABLE_GH_UNAVAILABLE.to_string();
    };

    let mut docs: Vec<String> = Vec::new();
    for repo in repos {
        // First doc found wins; a repo with neither contributes nothing.
        for doc in SIBLING_DOC_NAMES {
            if let Some(content) = source.fetch_doc(org, &repo, doc) {
                if content.trim().is_empty() {
                    continue;
                }
                docs.push(render_doc(
                    &sibling_doc_label(org, &repo, doc),
                    &trim_sibling_doc(doc, &content),
                ));
                break;
            }
        }
    }

    if docs.is_empty() {
        crate::auto_mode_gates::NO_SIBLING_DOCS_FOUND.to_string()
    } else {
        docs.join("\n\n")
    }
}

// ── `W1d` — recent usage across all projects (names only) ────────────────────

/// `qsy` — per-transcript read cap (4 MiB).
pub const ALL_PROJECTS_PER_FILE_CAP: u64 = 4 * 1024 * 1024;
/// `jsy` — total bytes the scan will read (100 MiB).
pub const ALL_PROJECTS_AGGREGATE_CAP: u64 = 100 * 1024 * 1024;
/// `Wsy` — the scan's deadline, in milliseconds.
pub const ALL_PROJECTS_DEADLINE_MS: u64 = 8_000;
/// `Gsy` — how many entries the enumeration will stat.
pub const ALL_PROJECTS_STAT_CAP: usize = 2_000;
/// `K1d` — how many of the most recent transcripts are actually read.
pub const ALL_PROJECTS_FILE_LIMIT: usize = 50;

/// What one all-projects scan collected, and every way it fell short.
///
/// The shortfall counters are not bookkeeping: each one renders a line telling
/// the model that coverage was partial, so an incomplete sweep can never read
/// as "these are all the projects".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllProjectsScan {
    /// Transcripts actually read.
    pub scanned: usize,
    /// Transcripts selected for reading (the most recent `fileLimit`).
    pub selected: usize,
    /// Transcripts found by the enumeration.
    pub enumerated: usize,
    /// Bash tool uses seen.
    pub commands_seen: usize,
    /// Command words mined, in order.
    pub words: Vec<String>,
    /// The enumeration hit its stat cap.
    pub enumeration_capped: bool,
    /// Transcripts whose stat failed.
    pub stat_failed: usize,
    /// Project directories that could not be listed.
    pub unreadable_dirs: usize,
    /// Transcripts the read-deny gate refused.
    pub denied: usize,
    /// Transcripts that vanished or were refused as an alias.
    pub unreadable: usize,
    /// Transcripts longer than the per-file cap.
    pub per_file_capped: usize,
    /// Set with the number left unscanned when the aggregate cap was hit.
    pub aggregate_capped_remaining: Option<usize>,
    /// Set with the number left unscanned when the deadline was hit.
    pub deadline_remaining: Option<usize>,
    /// Command-word extraction itself ran out of room.
    pub words_incomplete: bool,
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        one.to_string()
    } else {
        many.to_string()
    }
}

/// `W1d`'s rendering half — the section body for a completed scan.
#[must_use]
pub fn render_all_projects_usage(scan: &AllProjectsScan) -> String {
    use crate::auto_mode_facts as facts;
    use crate::auto_mode_sections as sections;

    let mib = |b: u64| (b as f64 / (1024.0 * 1024.0)).round() as u64;
    // `Transcripts scanned: {A} of {selected} selected (from {enumerated}
    //  enumerated); Bash commands seen: {C}`
    let mut parts: Vec<String> = vec![format!(
        "{}{} of {}{}{}{}{}",
        facts::TRANSCRIPTS_SCANNED_PREFIX,
        scan.scanned,
        scan.selected,
        facts::SELECTED_FROM_INFIX,
        scan.enumerated,
        facts::ENUMERATED_BASH_COMMANDS_INFIX,
        scan.commands_seen,
    )];

    if scan.enumeration_capped {
        parts.push(format!(
            "{}{}{}{}{}",
            facts::ENUMERATION_CAP_PREFIX,
            ALL_PROJECTS_STAT_CAP,
            facts::FIRST_ENUMERATED_OF_INFIX,
            scan.enumerated,
            facts::TRANSCRIPT_SELECTION_CAVEAT
        ));
    }
    if scan.stat_failed + scan.unreadable_dirs > 0 {
        parts.push(format!(
            "\n_{} {} and {} project {}{}",
            scan.stat_failed,
            plural(scan.stat_failed, "transcript", "transcripts"),
            scan.unreadable_dirs,
            plural(scan.unreadable_dirs, "directory", "directories"),
            facts::PROJECTS_PARTIAL_COVERAGE
        ));
    }
    if scan.denied > 0 {
        parts.push(format!(
            "{}{} {}{}",
            facts::READ_DENY_GATE_PREFIX,
            scan.denied,
            plural(scan.denied, "transcript", "transcripts"),
            facts::TRANSCRIPTS_DENY_SKIPPED
        ));
    }
    if scan.unreadable > 0 {
        parts.push(format!(
            "\n_{} {}{}",
            scan.unreadable,
            plural(scan.unreadable, "transcript", "transcripts"),
            facts::TRANSCRIPTS_UNREADABLE
        ));
    }
    if scan.per_file_capped > 0 {
        parts.push(format!(
            "\n_{} {} exceeded the {}{}",
            scan.per_file_capped,
            plural(scan.per_file_capped, "transcript", "transcripts"),
            mib(ALL_PROJECTS_PER_FILE_CAP),
            facts::PER_FILE_CAP_SUFFIX
        ));
    }
    if let Some(remaining) = scan.aggregate_capped_remaining {
        parts.push(format!(
            "{}{}{}{} {}{}",
            facts::AGGREGATE_BYTE_CAP_PREFIX,
            mib(ALL_PROJECTS_AGGREGATE_CAP),
            facts::AGGREGATE_BYTE_CAP_INFIX,
            remaining,
            plural(remaining, "transcript", "transcripts"),
            facts::NOT_SCANNED_SUFFIX
        ));
    }
    if let Some(remaining) = scan.deadline_remaining {
        parts.push(format!(
            "{}{} {}{}",
            facts::DEADLINE_REACHED_PREFIX,
            remaining,
            plural(remaining, "transcript", "transcripts"),
            facts::NOT_SCANNED_SUFFIX
        ));
    }
    if scan.words_incomplete {
        parts.push(crate::auto_mode_gates::COMMAND_WORDS_INCOMPLETE.to_string());
    }

    let counted = frequency(&scan.words, FLAGGED_LIST_CAP * 2);
    if !counted.is_empty() {
        parts.push(format!(
            "{}{}",
            sections::HEADING_TOOLS_OTHER_PROJECTS,
            counted
                .into_iter()
                .map(|(w, n)| format!("- {} ({n}\u{d7})", display_name(&w)))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    parts.push(facts::OTHER_PROJECTS_PROVENANCE_NOTE.to_string());
    parts.join("\n")
}

/// Supplies the all-projects sweep.
pub trait AllProjectsSource {
    /// Run the sweep, or `None` when the projects root is absent, unreadable,
    /// or enumeration exceeded its deadline.
    fn scan(&self) -> Option<AllProjectsScan>;
}

/// `W1d` — the "Recent usage across all projects (names only)" section body.
#[must_use]
pub fn all_projects_usage_section(source: &dyn AllProjectsSource) -> String {
    source.scan().as_ref().map_or_else(
        || crate::auto_mode_gates::OTHER_PROJECT_TRANSCRIPTS_UNAVAILABLE.to_string(),
        render_all_projects_usage,
    )
}

// ── `q1d` — shell history (command words only) ───────────────────────────────

/// `M1d` — how many bytes of the TAIL of a history file are read.
pub const HISTORY_TAIL_BYTES: u64 = 262_144;
/// `n$s` — how many of the most recent parsed lines are mined.
pub const HISTORY_LINE_CAP: usize = 4_000;
/// `o$s` — the walk's time budget, in milliseconds.
#[must_use]
pub fn history_budget_ms(windows: bool) -> u64 {
    if windows {
        8_000
    } else {
        4_000
    }
}

/// `Hsy` — POSIX prefixes whose NEXT word is the real command.
pub const POSIX_COMMAND_PREFIXES: [&str; 3] = ["sudo", "doas", "env"];
/// `Lsy` — the same for PowerShell history.
pub const PSREADLINE_COMMAND_PREFIXES: [&str; 2] = ["sudo", "gsudo"];
/// `P1d` — the fish history entry prefix.
pub const FISH_ENTRY_PREFIX: &str = "- cmd: ";

/// How a history file is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryFormat {
    /// bash / zsh.
    Posix,
    /// PowerShell PSReadLine.
    PsReadline,
    /// fish's YAML-ish log.
    Fish,
}

impl HistoryFormat {
    /// `Nsy` — the format implied by a history file's base name.
    #[must_use]
    pub fn from_basename(name: &str) -> Self {
        match name.to_lowercase().as_str() {
            "fish_history" => HistoryFormat::Fish,
            "consolehost_history.txt" => HistoryFormat::PsReadline,
            _ => HistoryFormat::Posix,
        }
    }
    /// The line-continuation character for this format.
    fn continuation(self) -> char {
        if self == HistoryFormat::PsReadline {
            '`'
        } else {
            '\\'
        }
    }
}

/// One history file the recon may read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySource {
    /// The label reported in `filesRead` (a `~`-style name, never a full path).
    pub label: String,
    /// Absolute path.
    pub path: std::path::PathBuf,
    /// Layout.
    pub format: HistoryFormat,
}

/// The environment `N1d` resolves history files against.
#[derive(Debug, Clone, Default)]
pub struct ShellHistoryEnv {
    /// Windows or not — decides which sources apply and how paths compare.
    pub windows: bool,
    /// The user's home directory.
    pub home_dir: String,
    /// `%APPDATA%`.
    pub app_data: Option<String>,
    /// `$XDG_DATA_HOME`.
    pub xdg_data_home: Option<String>,
    /// `$HISTFILE`.
    pub hist_file: Option<String>,
}

fn join_path(windows: bool, parts: &[&str]) -> String {
    let sep = if windows { '\\' } else { '/' };
    parts.join(&sep.to_string())
}

fn is_absolute_for(windows: bool, p: &str) -> bool {
    if windows {
        p.len() > 2 && p.as_bytes()[1] == b':' || p.starts_with('\\')
    } else {
        p.starts_with('/')
    }
}

/// `L1d` — `$XDG_DATA_HOME` when absolute, else `~/.local/share`.
fn data_home(env: &ShellHistoryEnv) -> String {
    match env.xdg_data_home.as_deref().map(str::trim) {
        Some(x) if !x.is_empty() && is_absolute_for(env.windows, x) => x.to_string(),
        _ => join_path(env.windows, &[&env.home_dir, ".local", "share"]),
    }
}

/// `N1d` — the history files to consider, de-duplicated by path.
#[must_use]
pub fn history_sources(env: &ShellHistoryEnv) -> Vec<HistorySource> {
    let mut out: Vec<HistorySource> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut push = |label: &str, path: String, format: HistoryFormat| {
        let key = if env.windows {
            path.to_lowercase()
        } else {
            path.clone()
        };
        if seen.insert(key) {
            out.push(HistorySource {
                label: label.to_string(),
                path: path.into(),
                format,
            });
        }
    };

    // `$HISTFILE` — only when absolute; its format comes from the base name.
    if let Some(hist) = env.hist_file.as_deref().map(str::trim) {
        if !hist.is_empty() && is_absolute_for(env.windows, hist) {
            let base = hist.rsplit(['/', '\\']).next().unwrap_or(hist);
            push(
                "$HISTFILE",
                hist.to_string(),
                HistoryFormat::from_basename(base),
            );
        }
    }
    if !env.windows {
        push(
            "~/.zsh_history",
            join_path(false, &[&env.home_dir, ".zsh_history"]),
            HistoryFormat::Posix,
        );
    }
    push(
        "~/.bash_history",
        join_path(env.windows, &[&env.home_dir, ".bash_history"]),
        HistoryFormat::Posix,
    );
    if env.windows {
        if let Some(app) = env.app_data.as_deref() {
            push(
                r"%APPDATA%\...\PSReadLine\ConsoleHost_history.txt",
                join_path(
                    true,
                    &[
                        app,
                        "Microsoft",
                        "Windows",
                        "PowerShell",
                        "PSReadLine",
                        "ConsoleHost_history.txt",
                    ],
                ),
                HistoryFormat::PsReadline,
            );
        }
    } else {
        let data = data_home(env);
        push(
            "~/.local/share/powershell/PSReadLine/ConsoleHost_history.txt",
            join_path(
                false,
                &[&data, "powershell", "PSReadLine", "ConsoleHost_history.txt"],
            ),
            HistoryFormat::PsReadline,
        );
        push(
            "~/.local/share/fish/fish_history",
            join_path(false, &[&data, "fish", "fish_history"]),
            HistoryFormat::Fish,
        );
    }
    out
}

/// `Msy` — strip zsh's extended-history `: <start>:<elapsed>;` prefix.
fn strip_zsh_metadata(line: &str) -> &str {
    let Some(rest) = line.strip_prefix(": ") else {
        return line;
    };
    let digits = |s: &str| s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let a = digits(rest);
    if a == 0 || rest.as_bytes().get(a) != Some(&b':') {
        return line;
    }
    let after = &rest[a + 1..];
    let b = digits(after);
    if b == 0 || after.as_bytes().get(b) != Some(&b';') {
        return line;
    }
    &after[b + 1..]
}

/// `Bsy` — a fish entry ends at its first UNESCAPED `\n` escape sequence.
fn fish_entry(raw: &str) -> &str {
    let bytes = raw.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = raw[from..].find("\\n") {
        let at = from + rel;
        let mut backslashes = 0usize;
        let mut i = at;
        while i > 0 && bytes[i - 1] == b'\\' {
            backslashes += 1;
            i -= 1;
        }
        if backslashes.is_multiple_of(2) {
            return &raw[..at];
        }
        from = at + 1;
    }
    raw
}

/// `F1d` — turn a history file's text into candidate command lines.
///
/// A `truncated` tail may open mid-continuation, so the leading continued line
/// is skipped rather than mined as if it were a command of its own.
#[must_use]
pub fn parse_history(content: &str, format: HistoryFormat, truncated: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut skipping_continuation = truncated && format != HistoryFormat::Fish;
    let cont = format.continuation();
    let body = content.strip_prefix('\u{feff}').unwrap_or(content);

    for raw in body.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if format == HistoryFormat::Fish {
            if let Some(rest) = line.strip_prefix(FISH_ENTRY_PREFIX) {
                out.push(fish_entry(rest).to_string());
            }
            continue;
        }
        if skipping_continuation {
            skipping_continuation = line.ends_with(cont);
            continue;
        }
        if line.is_empty() {
            continue;
        }
        // bash's `#<epoch>` timestamp lines are not commands.
        if format == HistoryFormat::Posix
            && line.starts_with('#')
            && line.len() > 1
            && line[1..].chars().all(|c| c.is_ascii_digit())
        {
            continue;
        }
        let cleaned = if format == HistoryFormat::Posix {
            strip_zsh_metadata(line)
        } else {
            line
        };
        out.push(cleaned.to_string());
        skipping_continuation = line.ends_with(cont);
    }
    out
}

/// `O1d` — `^[a-z][\w.+-]{0,19}$`
fn is_command_word(w: &str) -> bool {
    let mut chars = w.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    let rest: Vec<char> = chars.collect();
    rest.len() <= 19
        && rest
            .iter()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'))
}

/// The first two words of a POSIX command line, skipping `VAR=value` prefixes.
///
/// The oracle runs the line through its bash parser here. This is a
/// tokeniser-level approximation of the same thing: it drops leading
/// assignments, which is what the parse would do before reporting the command
/// word. It feeds a NAMES-ONLY frequency list, not a security decision.
fn posix_head_words(line: &str) -> (Option<&str>, Option<&str>) {
    let mut words = line
        .split_whitespace()
        .filter(|w| !(w.contains('=') && !w.starts_with('=')));
    (words.next(), words.next())
}

/// `i$s` — the command word of each history line.
///
/// A `sudo`/`doas`/`env` (or `sudo`/`gsudo`) prefix is stepped over so the tool
/// that actually ran is what gets counted.
#[must_use]
pub fn extract_command_words(lines: &[String], format: HistoryFormat) -> Vec<String> {
    let prefixes: &[&str] = if format == HistoryFormat::PsReadline {
        &PSREADLINE_COMMAND_PREFIXES
    } else {
        &POSIX_COMMAND_PREFIXES
    };
    let mut out = Vec::new();
    for line in lines {
        let (mut first, second) = if format == HistoryFormat::PsReadline {
            let mut w = line.split_whitespace();
            (w.next(), w.next())
        } else {
            posix_head_words(line)
        };
        if let (Some(f), Some(s)) = (first, second) {
            if prefixes.contains(&f) && is_command_word(s) {
                first = Some(s);
            }
        }
        if let Some(f) = first {
            if is_command_word(f) {
                out.push(f.to_string());
            }
        }
    }
    out
}

/// What one history file contributed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryReadOutcome {
    /// The label to report as read.
    pub label: String,
    /// Command words mined from it.
    pub words: Vec<String>,
    /// The read was cut short in some way.
    pub partial: bool,
}

/// Supplies shell-history bytes.
pub trait ShellHistorySource {
    /// The environment the sources are resolved against.
    fn env(&self) -> ShellHistoryEnv;
    /// `true` when the home directory is a network path, so nothing is touched.
    fn home_is_network(&self) -> bool;
    /// Read the tail of one history file: `Ok(None)` when absent, `Err(())`
    /// when present but unreadable, else the content and whether it was cut.
    fn read_tail(&self, source: &HistorySource) -> Result<Option<(String, bool)>, ()>;
}

/// `q1d` — the "Shell history (command words only)" section body.
///
/// Returns `None` when the gate is closed or no home directory is known; the
/// caller renders the matching marker.
#[must_use]
pub fn shell_history_section(source: &dyn ShellHistorySource) -> String {
    use crate::auto_mode_facts as facts;
    use crate::auto_mode_sections as sections;

    if source.home_is_network() {
        return crate::auto_mode_gates::SHELL_HISTORY_NETWORK_HOME.to_string();
    }

    let env = source.env();
    let mut words: Vec<String> = Vec::new();
    let mut files_read: Vec<String> = Vec::new();
    let mut partial = false;

    for src in history_sources(&env) {
        match source.read_tail(&src) {
            Err(()) => partial = true,
            Ok(None) => {}
            Ok(Some((content, truncated))) => {
                files_read.push(src.label.clone());
                let parsed = parse_history(&content, src.format, truncated);
                let kept = if parsed.len() > HISTORY_LINE_CAP {
                    partial = true;
                    parsed[parsed.len() - HISTORY_LINE_CAP..].to_vec()
                } else {
                    parsed
                };
                if truncated {
                    partial = true;
                }
                words.extend(extract_command_words(&kept, src.format));
            }
        }
    }

    let counted = frequency(&words, FLAGGED_LIST_CAP * 2);
    let files = if files_read.is_empty() {
        "none".to_string()
    } else {
        files_read
            .iter()
            .map(|f| display_name(f).to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut parts = vec![format!(
        "{}{}{}{}{files}",
        facts::STATUS_PREFIX,
        if partial { "partial" } else { "complete" },
        facts::STATUS_SEPARATOR,
        format_args!("{}{}", files_read.len(), facts::FILES_READ_INFIX),
    )];
    if !counted.is_empty() {
        parts.push(format!(
            "{}{}",
            sections::HEADING_TOOLS_OUTSIDE_CLAUDE,
            counted
                .into_iter()
                .map(|(w, n)| format!("- {} ({n}\u{d7})", display_name(&w)))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    parts.push(facts::SHELL_HISTORY_PROVENANCE_NOTE.to_string());
    parts.join("\n")
}

// ── `iay` — recent usage in this project (names only) ────────────────────────

/// `K1d` — how many transcripts are mined, newest first.
pub const TRANSCRIPT_FILE_LIMIT: usize = 50;
/// `$sy` — a transcript above this size is skipped, not read.
pub const TRANSCRIPT_OVERSIZE_BYTES: u64 = 26_214_400;
/// How many denial reasons are reported.
pub const DENIAL_REASON_LIMIT: usize = 10;

/// `tay` — the line marker that identifies an auto-mode denial.
pub const DENIAL_MARKER: &str = "denied by the auto mode classifier";
/// `Y1d` — the line marker that identifies a Bash tool use.
pub const BASH_TOOL_MARKER: &str = "\"Bash\"";

/// `nay` — commands so common that naming them says nothing about the user.
pub const STANDARD_CLIS: [&str; 94] = [
    "ls", "cd", "cat", "rg", "grep", "find", "git", "gh", "node", "bun", "npm", "yarn", "pnpm",
    "cargo", "go", "make", "just", "docker", "curl", "wget", "echo", "printf", "sed", "awk", "tr",
    "cut", "sort", "uniq", "xargs", "jq", "tee", "head", "tail", "wc", "which", "date", "diff",
    "touch", "ln", "chmod", "mkdir", "cp", "mv", "rm", "ps", "kill", "pgrep", "pkill", "sleep",
    "stat", "env", "set", "export", "unset", "read", "source", "command", "ssh", "scp", "tar",
    "zip", "unzip", "vim", "nano", "less", "more", "man", "tmux", "sudo", "bash", "sh", "zsh",
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "return", "exit", "true", "false", "python", "pip", "python3", "pip3", "kubectl",
];

/// `ray` — the denial reason inside a tool result.
#[must_use]
pub fn denial_reason_regex() -> regex::Regex {
    re(&format!(
        r"{}\. Reason: ([\w][\w ,'-]{{0,59}})",
        regex::escape(DENIAL_MARKER)
    ))
}
/// URLs inside a command line.
#[must_use]
pub fn command_url_regex() -> regex::Regex {
    re(r#"(https?://[^\s"'`]+)"#)
}
/// `-n <namespace>` flags.
#[must_use]
pub fn k8s_namespace_regex() -> regex::Regex {
    re(r"-n\s+([a-z][a-z0-9-]{2,})")
}
/// The command word at the head of a line.
#[must_use]
pub fn command_word_regex() -> regex::Regex {
    re(r"^([a-z][a-z0-9_-]{1,20})\b")
}
/// `sudo` / `timeout N` prefixes stripped before the command word is read.
#[must_use]
pub fn command_prefix_regex() -> regex::Regex {
    re(r"^(sudo |timeout [0-9]+[smh]? )+")
}
/// `eay` — hosts too generic to be worth reporting.
#[must_use]
pub fn boring_host_regex() -> regex::Regex {
    re(r"^(127\.0\.0\.1|localhost|.*jsdelivr.*|.*unpkg.*|example\.com)$")
}

/// `Wi(s, "\n")` — everything before the first newline.
#[must_use]
pub fn first_line(s: &str) -> &str {
    s.split_once('\n').map_or(s, |(head, _)| head)
}

/// `qf` — is this host github.com, ignoring any number of `www.` prefixes?
#[must_use]
pub fn is_github_host(host: &str) -> bool {
    let mut h = host.to_lowercase();
    while let Some(rest) = h.strip_prefix("www.") {
        h = rest.to_string();
    }
    h == "github.com"
}

/// `oay` — is this a command so standard it carries no signal?
#[must_use]
pub fn is_standard_cli(word: &str) -> bool {
    if STANDARD_CLIS.contains(&word) {
        return true;
    }
    // `^(python[0-9.]*|pip[0-9]*)$`
    if let Some(rest) = word.strip_prefix("python") {
        return rest.chars().all(|c| c.is_ascii_digit() || c == '.');
    }
    if let Some(rest) = word.strip_prefix("pip") {
        return rest.chars().all(|c| c.is_ascii_digit());
    }
    false
}

/// `gFt` — count occurrences, most frequent first, ties broken by name.
#[must_use]
pub fn frequency(items: &[String], limit: usize) -> Vec<(String, usize)> {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for item in items {
        if item.len() <= 256 {
            *counts.entry(item.as_str()).or_insert(0) += 1;
        }
    }
    let mut out: Vec<(String, usize)> = counts
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(limit);
    out
}

/// One transcript file for this project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptFile {
    /// Absolute path.
    pub path: std::path::PathBuf,
    /// Size in bytes, used for the oversize skip.
    pub size: u64,
}

/// Supplies this project's transcripts.
pub trait ProjectUsageSource {
    /// The project's `.jsonl` transcripts, NEWEST FIRST and already capped at
    /// [`TRANSCRIPT_FILE_LIMIT`]. `None` when the project has no transcript
    /// directory at all.
    fn transcripts(&self) -> Option<Vec<TranscriptFile>>;
    /// Read one transcript whole.
    fn read_transcript(&self, path: &std::path::Path) -> Option<String>;
}

/// What one pass over the transcripts collected.
struct MinedUsage {
    commands: Vec<String>,
    denials: Vec<String>,
    skipped: usize,
    scanned: usize,
}

/// Mine one transcript's lines for Bash tool-use commands and denial reasons,
/// appending to `commands` / `denials`.
///
/// The JSON parse sits behind a cheap substring prefilter because a transcript
/// is overwhelmingly lines this pass has no interest in. Only the FIRST LINE of
/// a Bash command is kept: a heredoc or a multi-line script would otherwise
/// carry its whole body — and anything embedded in it — into the recon.
///
/// `denial_re` is passed in so a sweep over many files compiles it once.
pub fn mine_transcript_text(
    text: &str,
    denial_re: &regex::Regex,
    commands: &mut Vec<String>,
    denials: &mut Vec<String>,
) {
    {
        for line in text.split('\n') {
            // Cheap prefilter before paying for a JSON parse.
            let has_bash = line.contains(BASH_TOOL_MARKER);
            let has_denial = line.contains(DENIAL_MARKER);
            if !has_bash && !has_denial {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(blocks) = parsed
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            for block in blocks {
                let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
                if kind == "tool_use" && block.get("name").and_then(Value::as_str) == Some("Bash") {
                    if let Some(cmd) = block
                        .get("input")
                        .and_then(|i| i.get("command"))
                        .and_then(Value::as_str)
                    {
                        // Only the FIRST LINE of the command is kept.
                        commands.push(first_line(cmd).to_string());
                    }
                }
                if has_denial && kind == "tool_result" {
                    let body = match block.get("content") {
                        Some(Value::String(s)) => s.clone(),
                        None => String::new(),
                        Some(other) => other.to_string(),
                    };
                    denials.extend(collect_captures(&body, denial_re));
                }
            }
        }
    }
}

fn mine_transcripts(source: &dyn ProjectUsageSource, files: &[TranscriptFile]) -> MinedUsage {
    let denial_re = denial_reason_regex();
    let mut commands = Vec::new();
    let mut denials = Vec::new();
    let mut skipped = 0usize;

    for file in files {
        if file.size > TRANSCRIPT_OVERSIZE_BYTES {
            skipped += 1;
            continue;
        }
        let Some(text) = source.read_transcript(&file.path) else {
            continue;
        };
        mine_transcript_text(&text, &denial_re, &mut commands, &mut denials);
    }
    MinedUsage {
        scanned: files.len() - skipped,
        commands,
        denials,
        skipped,
    }
}

/// The leading command word of each command line: `sudo` / `timeout N`
/// prefixes stripped, then the first word, keeping only non-standard CLIs.
///
/// Standard tools are filtered because the point of the list is what is
/// UNUSUAL about this environment; reporting that the user runs `ls` tells a
/// proposal nothing it could act on.
#[must_use]
pub fn command_words_of(commands: &[String]) -> Vec<String> {
    let prefix_re = command_prefix_regex();
    let word_re = command_word_regex();
    commands
        .iter()
        .map(|c| prefix_re.replace(c, "").to_string())
        .filter_map(|c| {
            word_re
                .captures(&c)
                .and_then(|m| m.get(1).map(|g| g.as_str().to_string()))
        })
        .filter(|w| !is_standard_cli(w))
        .collect()
}

/// `iay` — the "Recent usage in this project (names only)" section body.
///
/// NAMES ONLY is the whole contract: raw command lines never leave this
/// function. What escapes is host names, bucket names, namespaces, the leading
/// command word, and denial reasons — each counted, none quoted.
#[must_use]
pub fn project_usage_section(source: &dyn ProjectUsageSource) -> String {
    use crate::auto_mode_facts as facts;
    use crate::auto_mode_sections as sections;

    let Some(files) = source.transcripts() else {
        return facts::NO_TRANSCRIPT_HISTORY.to_string();
    };
    let mined = mine_transcripts(source, &files);

    let joined = mined.commands.join("\n");
    let boring = boring_host_regex();
    let hosts: Vec<String> = collect_captures(&joined, &command_url_regex())
        .iter()
        .filter_map(|u| registry_host(u))
        .filter(|h| !boring.is_match(h) && !is_github_host(h))
        .collect();
    let buckets = extract_bucket_names(&joined);
    let namespaces = collect_captures(&joined, &k8s_namespace_regex());

    let clis: Vec<String> = command_words_of(&mined.commands);

    let counted = |items: &[String], limit: usize| {
        frequency(items, limit)
            .into_iter()
            .map(|(name, n)| format!("- {name} ({n}\u{d7})"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let section = |heading: &str, items: &[String], limit: usize| {
        if items.is_empty() {
            String::new()
        } else {
            format!("{heading}{}", counted(items, limit))
        }
    };

    let skipped_note = if mined.skipped > 0 {
        format!(" ({}{}", mined.skipped, facts::SKIPPED_AS_OVERSIZED_SUFFIX)
    } else {
        String::new()
    };

    [
        format!(
            "{}{}{skipped_note}{}{}",
            facts::TRANSCRIPTS_SCANNED_PREFIX,
            mined.scanned,
            facts::BASH_COMMANDS_SEEN_INFIX,
            mined.commands.len()
        ),
        section(sections::HEADING_HOSTS_CONTACTED, &hosts, FLAGGED_LIST_CAP),
        section(
            sections::HEADING_CLOUD_BUCKETS_TOUCHED,
            &buckets,
            FLAGGED_LIST_CAP,
        ),
        section(
            sections::HEADING_K8S_NAMESPACES,
            &namespaces,
            FLAGGED_LIST_CAP,
        ),
        section(sections::HEADING_NONSTANDARD_CLIS, &clis, FLAGGED_LIST_CAP),
        section(
            sections::HEADING_DENIAL_REASONS,
            &mined.denials,
            DENIAL_REASON_LIMIT,
        ),
        facts::PROJECT_USAGE_SCOPE_NOTE.to_string(),
    ]
    .join("\n")
}

// ── `gay` — config scans (names only) ────────────────────────────────────────

/// Read cap for a file `bcn` scans.
pub const CONFIG_SCAN_READ_CAP: usize = 64_000;
/// `RPo` result cap inside `bcn`.
pub const CONFIG_SCAN_FILE_LIMIT: usize = 40;
/// Read cap for `package.json`.
pub const PACKAGE_JSON_READ_CAP: usize = 256_000;
/// Cap on rendered sensitive paths.
pub const SENSITIVE_PATH_LIMIT: usize = 72;
/// `lay` — the bucket scan's own deadline, in milliseconds.
pub const BUCKET_SCAN_TIMEOUT_MS: u64 = 8_000;
/// `cay` — how many DISTINCT bucket names the scan will hold before giving up.
pub const BUCKET_SCAN_DISTINCT_CAP: usize = 20_000;
/// `uay` — `rg --max-filesize` for the bucket scan.
pub const BUCKET_SCAN_MAX_FILESIZE: &str = "4M";
/// [`BUCKET_SCAN_MAX_FILESIZE`] in bytes.
pub const BUCKET_SCAN_MAX_FILESIZE_BYTES: u64 = 4 * 1024 * 1024;
/// `day` — the smallest cluster worth reporting.
pub const BUCKET_CLUSTER_MIN: usize = 3;
/// `pay` — how many clusters are reported.
pub const BUCKET_CLUSTER_LIMIT: usize = 10;

/// Globs whose files are read for package-registry hosts.
pub const REGISTRY_GLOBS: [&str; 3] = [".npmrc", "pip.conf", "pyproject.toml"];
/// Globs whose files are read for container image registries.
pub const IMAGE_GLOBS: [&str; 3] = ["Dockerfile*", "**/Dockerfile*", "docker-compose*.yml"];
/// Globs whose files are read for CI secret names.
pub const CI_GLOBS: [&str; 1] = ["*.yml"];
/// Globs whose files are read for build targets.
pub const MAKE_GLOBS: [&str; 2] = ["Makefile", "justfile"];
/// Globs whose files are read for secrets-manager markers.
pub const SECRETS_MARKER_GLOBS: [&str; 5] = ["*.toml", "*.yaml", "*.yml", "*.sh", ".envrc"];
/// Globs matched by NAME for the sensitive-paths listing.
pub const SENSITIVE_PATH_GLOBS: [&str; 23] = [
    "**/*terraform*",
    "**/*.tf",
    "**/*k8s*",
    "**/*kubernetes*",
    "**/helm[-._]*",
    "**/*[-._]helm[-._]*",
    "**/iam[-._]*",
    "**/*[-._]iam[-._]*",
    "**/prod[-._]*",
    "**/*[-._]prod[-._]*",
    "**/egress[-._]*",
    "**/*[-._]egress[-._]*",
    "**/*rbac*",
    "**/*secret*",
    "**/*credential*",
    "**/*pii*",
    "**/.env*",
    "**/*.cedar",
    "**/*allowlist*",
    "**/network-polic*",
    "**/*classification*",
    "**/*retention*",
    "**/*_encrypted*",
];
/// Globs matched by DIRECTORY for the sensitive-paths listing.
pub const SENSITIVE_DIR_GLOBS: [&str; 6] = [
    "**/helm/**",
    "**/iam/**",
    "**/prod/**",
    "**/k8s/**",
    "**/kubernetes/**",
    "**/rbac/**",
];
/// Globs the bucket scan searches.
pub const BUCKET_SCAN_GLOBS: [&str; 5] = ["*.toml", "*.yaml", "*.yml", "*.json", "*.cfg"];

fn re(pattern: &str) -> regex::Regex {
    regex::Regex::new(pattern).expect("static regex")
}

/// `(?:registry|index-url)\s*=\s*(https?://…)`
#[must_use]
pub fn registry_url_regex() -> regex::Regex {
    re(r#"(?:registry|index-url)\s*=\s*(https?://[^\s"'`]+)"#)
}
/// `FROM\s+(?:[^/\s]*@)?([a-z0-9][a-z0-9.-]*\.[a-z]+)/`
#[must_use]
pub fn image_from_regex() -> regex::Regex {
    re(r"FROM\s+(?:[^/\s]*@)?([a-z0-9][a-z0-9.-]*\.[a-z]+)/")
}
/// `secrets\.([A-Z0-9_]+)`
#[must_use]
pub fn ci_secret_regex() -> regex::Regex {
    re(r"secrets\.([A-Z0-9_]+)")
}
/// `(?m)^([a-zA-Z0-9_][a-zA-Z0-9_-]*):`
#[must_use]
pub fn make_target_regex() -> regex::Regex {
    re(r"(?m)^([a-zA-Z0-9_][a-zA-Z0-9_-]*):")
}
/// `(VAULT_ADDR|SOPS_[A-Z_]*|op read|aws secretsmanager|gcloud secrets)`
#[must_use]
pub fn secrets_marker_regex() -> regex::Regex {
    re(r"(VAULT_ADDR|SOPS_[A-Z_]*|op read|aws secretsmanager|gcloud secrets)")
}
/// `^\.github/workflows/|^\.gitlab-ci\.yml$` — which `*.yml` files are CI.
#[must_use]
pub fn ci_path_regex() -> regex::Regex {
    re(r"^\.github/workflows/|^\.gitlab-ci\.yml$")
}
/// `V1d` — `(^|/)(helm|iam|prod|k8s|kubernetes|rbac)/`
#[must_use]
pub fn sensitive_dir_regex() -> regex::Regex {
    re(r"(^|/)(helm|iam|prod|k8s|kubernetes|rbac)/")
}

/// `G1d` — registries so well-known that naming them says nothing about the
/// user's infrastructure, so they are dropped from both registry sections.
pub const PUBLIC_REGISTRIES: [&str; 13] = [
    "docker.io",
    "ghcr.io",
    "registry.npmjs.org",
    "pypi.org",
    "mcr.microsoft.com",
    "nvcr.io",
    "gcr.io",
    "public.ecr.aws",
    "lscr.io",
    "quay.io",
    "registry-1.docker.io",
    "127.0.0.1",
    "localhost",
];

/// `Fhr` — every capture-group-1 match of `pattern` in `text`.
#[must_use]
pub fn collect_captures(text: &str, pattern: &regex::Regex) -> Vec<String> {
    pattern
        .captures_iter(text)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect()
}

/// `eNd` — de-duplicate, drop over-long entries, sort, cap.
#[must_use]
pub fn finalize_names(mut names: Vec<String>, limit: usize) -> Vec<String> {
    names.retain(|n| n.len() <= 256);
    names.sort_unstable();
    names.dedup();
    names.truncate(limit);
    names
}

/// `tNd` — the host of a package-registry URL, or `None` when it is not one we
/// can safely name.
///
/// A URL carrying credentials is only accepted when the part after the
/// authority holds no further `@` and the hostname is dotted — the shapes where
/// the host is unambiguous. Otherwise it is dropped rather than guessed at.
#[must_use]
pub fn registry_host(raw: &str) -> Option<String> {
    let lower = raw.to_lowercase();
    let stripped = if lower.starts_with("https://") {
        &raw[8..]
    } else if lower.starts_with("http://") {
        &raw[7..]
    } else {
        raw
    };

    if !stripped.contains('@') {
        let host: String = stripped
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
            .collect();
        let ok = host
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '.');
        return ok.then_some(host);
    }

    let parsed = url::Url::parse(raw).ok()?;
    let hostname = parsed.host_str().unwrap_or("");
    if !parsed.username().is_empty() || parsed.password().is_some_and(|p| !p.is_empty()) {
        let after = stripped
            .find(['/', '?', '#'])
            .map_or("", |i| &stripped[i + 1..]);
        if after.contains('@') || !hostname.contains('.') {
            return None;
        }
    }
    let shape_ok = {
        let mut chars = hostname.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '.')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    };
    (!hostname.is_empty() && shape_ok).then(|| hostname.to_string())
}

/// One bucket name's tally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketCount {
    /// How many times the name appeared.
    pub occurrences: usize,
    /// How many distinct files it appeared in.
    pub files: usize,
}

/// The outcome of the repo-wide bucket scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketScan {
    /// The most-frequent names, highest first.
    pub top: Vec<(String, BucketCount)>,
    /// How many distinct names were seen in total.
    pub distinct: usize,
    /// First-dash prefixes shared by at least [`BUCKET_CLUSTER_MIN`] names.
    pub clusters: Vec<(String, usize)>,
    /// The scan stopped early, so the counts are a lower bound.
    pub truncated: bool,
}

/// `Q1d` — bucket names in a chunk of text.
///
/// Matches `s3://`, `gs://` and `az://` only when NOT preceded by a character
/// that would make it part of a longer token, which is the lookbehind the
/// oracle's regex uses.
#[must_use]
pub fn extract_bucket_names(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    for scheme in ["s3://", "gs://", "az://"] {
        let mut from = 0usize;
        while let Some(rel) = text[from..].find(scheme) {
            let at = from + rel;
            let preceded = at > 0 && {
                let p = bytes[at - 1];
                p.is_ascii_lowercase() || p.is_ascii_digit() || matches!(p, b'.' | b'+' | b'-')
            };
            if !preceded {
                let rest = &text[at + scheme.len()..];
                let name: String = rest
                    .chars()
                    .take_while(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
                    })
                    .collect();
                let first_ok = name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
                if first_ok {
                    out.push(name);
                }
            }
            from = at + scheme.len();
        }
    }
    out
}

/// `may` — group bucket names by the text before their first `-`.
///
/// A prefix shared by several names is what licenses treating it as
/// org-specific; a prefix seen once or twice is not evidence of anything.
#[must_use]
pub fn bucket_prefix_clusters<'a>(names: impl Iterator<Item = &'a String>) -> Vec<(String, usize)> {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for name in names {
        let Some(dash) = name.find('-') else { continue };
        if dash == 0 {
            continue;
        }
        *counts.entry(name[..dash].to_string()).or_insert(0) += 1;
    }
    let mut out: Vec<(String, usize)> = counts
        .into_iter()
        .filter(|(_, n)| *n >= BUCKET_CLUSTER_MIN)
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(BUCKET_CLUSTER_LIMIT);
    out
}

/// `hay` — render the bucket sub-sections.
///
/// `None` means the scan FAILED, which renders as unavailable-not-absent. A
/// scan that completed and genuinely found nothing renders as empty, because
/// that IS evidence.
#[must_use]
pub fn render_bucket_section(scan: Option<&BucketScan>) -> String {
    use crate::auto_mode_facts as facts;
    let Some(scan) = scan else {
        return crate::auto_mode_sections::BUCKET_SCAN_FAILED.to_string();
    };
    if scan.top.is_empty() {
        return if scan.truncated {
            crate::auto_mode_sections::BUCKET_SCAN_COLLECTED_NOTHING.to_string()
        } else {
            String::new()
        };
    }
    let mut parts: Vec<String> =
        vec![crate::auto_mode_sections::HEADING_BUCKET_NAMES_BY_COUNT.to_string()];
    for (name, count) in &scan.top {
        let files = if count.files == 1 { "file" } else { "files" };
        parts.push(format!(
            "- {name} ({}\u{d7}, {} {files})",
            count.occurrences, count.files
        ));
    }
    if scan.distinct > scan.top.len() {
        parts.push(format!(
            "\n_{}{}{} shown._",
            scan.distinct,
            facts::BUCKET_TOTAL_INFIX,
            scan.top.len()
        ));
    }
    if scan.truncated {
        parts.push(crate::auto_mode_gates::BUCKET_SCAN_ENDED_EARLY.to_string());
    }
    if !scan.clusters.is_empty() {
        parts.push(format!(
            "{}{}",
            crate::auto_mode_sections::HEADING_BUCKET_PREFIX_CLUSTERS,
            scan.clusters
                .iter()
                .map(|(p, n)| format!("- {p}-* ({n} distinct names)"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    parts.retain(|p| !p.is_empty());
    parts.join("\n")
}

/// Supplies what the config scans read.
pub trait ConfigScanSource {
    /// `bcn`'s file half: list files matching `globs` (optionally filtered by
    /// path) and return their contents.
    fn scan_files(&self, globs: &[&str], path_filter: Option<&regex::Regex>) -> Vec<String>;
    /// `RPo` — list paths only.
    fn list_paths(
        &self,
        globs: &[&str],
        limit: usize,
        depth: usize,
        filter: Option<&regex::Regex>,
    ) -> Vec<String>;
    /// `package.json`, capped.
    fn package_json(&self) -> Option<String>;
    /// The repo-wide bucket scan; `None` when it failed outright.
    fn bucket_scan(&self) -> Option<BucketScan>;
}

/// `bcn` — collect every capture of `pattern` across the matching files.
fn scan_for(
    source: &dyn ConfigScanSource,
    globs: &[&str],
    pattern: &regex::Regex,
    limit: usize,
    path_filter: Option<&regex::Regex>,
) -> Vec<String> {
    let mut found = Vec::new();
    for content in source.scan_files(globs, path_filter) {
        found.extend(collect_captures(&content, pattern));
    }
    finalize_names(found, limit)
}

/// `gay` — the "Config scans (names only)" section body.
#[must_use]
pub fn config_scans_section(source: &dyn ConfigScanSource) -> String {
    use crate::auto_mode_sections as sections;

    let is_public = |h: &String| PUBLIC_REGISTRIES.contains(&h.as_str());

    let registries: Vec<String> =
        scan_for(source, &REGISTRY_GLOBS, &registry_url_regex(), 10, None)
            .iter()
            .filter_map(|u| registry_host(u))
            .filter(|h| !is_public(h))
            .collect();
    let images: Vec<String> = scan_for(source, &IMAGE_GLOBS, &image_from_regex(), 10, None)
        .into_iter()
        .filter(|h| !is_public(h))
        .collect();
    let buckets = source.bucket_scan();
    let ci_secrets = scan_for(
        source,
        &CI_GLOBS,
        &ci_secret_regex(),
        FLAGGED_LIST_CAP,
        Some(&ci_path_regex()),
    );
    let make_targets = scan_for(
        source,
        &MAKE_GLOBS,
        &make_target_regex(),
        FLAGGED_LIST_CAP,
        None,
    );
    let markers = scan_for(
        source,
        &SECRETS_MARKER_GLOBS,
        &secrets_marker_regex(),
        10,
        None,
    );

    // Sensitive paths: name matches first, then up to two examples per
    // sensitive DIRECTORY, so a big `prod/` tree cannot crowd out everything.
    let by_name = source.list_paths(&SENSITIVE_PATH_GLOBS, 60, DOC_GLOB_MAX_DEPTH, None);
    let dir_re = sensitive_dir_regex();
    let mut per_dir: Vec<String> = Vec::new();
    {
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for path in source.list_paths(
            &SENSITIVE_DIR_GLOBS,
            1000,
            DOC_GLOB_MAX_DEPTH,
            Some(&dir_re),
        ) {
            let Some(kind) = dir_re
                .captures(&path)
                .and_then(|c| c.get(2).map(|m| m.as_str().to_string()))
            else {
                continue;
            };
            let n = seen.entry(kind).or_insert(0);
            if *n < 2 {
                *n += 1;
                per_dir.push(path);
            }
        }
    }
    let name_set: std::collections::HashSet<&String> = by_name.iter().collect();
    let mut sensitive: Vec<String> = by_name.clone();
    sensitive.extend(per_dir.into_iter().filter(|p| !name_set.contains(p)));
    sensitive.truncate(SENSITIVE_PATH_LIMIT);

    let scripts: Vec<String> = source
        .package_json()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| {
            v.get("scripts")
                .and_then(serde_json::Value::as_object)
                .map(|o| o.keys().take(FLAGGED_LIST_CAP).cloned().collect())
        })
        .unwrap_or_default();

    let bullets = |items: &[String]| {
        items
            .iter()
            .map(|i| format!("- {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let redacted_bullets = |items: &[String]| {
        items
            .iter()
            .map(|i| format!("- {}", display_name(i)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let section = |heading: &str, body: String, present: bool| {
        if present {
            format!("{heading}{body}")
        } else {
            String::new()
        }
    };

    // NOTE the first heading carries no leading newline while the rest do --
    // the constants hold that difference, so they are used as-is.
    [
        section(
            sections::HEADING_PACKAGE_REGISTRY_HOSTS,
            bullets(&registries),
            !registries.is_empty(),
        ),
        section(
            sections::HEADING_CONTAINER_REGISTRIES,
            bullets(&images),
            !images.is_empty(),
        ),
        render_bucket_section(buckets.as_ref()),
        section(
            sections::HEADING_CI_SECRET_NAMES,
            bullets(&ci_secrets),
            !ci_secrets.is_empty(),
        ),
        section(
            sections::HEADING_MAKE_TARGETS,
            bullets(&make_targets),
            !make_targets.is_empty(),
        ),
        section(
            sections::HEADING_PACKAGE_JSON_SCRIPTS,
            redacted_bullets(&scripts),
            !scripts.is_empty(),
        ),
        section(
            sections::HEADING_SECRETS_MANAGER_MARKERS,
            bullets(&markers),
            !markers.is_empty(),
        ),
        section(
            sections::HEADING_SENSITIVE_PATHS,
            redacted_bullets(&sensitive),
            !sensitive.is_empty(),
        ),
    ]
    .join("\n")
}

// ── `Zsy` — existing auto-mode settings (selective read) ─────────────────────

/// `Usy` — cap on the rendered project-local `autoMode` block.
pub const LOCAL_SETTINGS_RENDER_CAP: usize = 20_000;
/// Read cap for the user settings file.
pub const SETTINGS_READ_CAP: usize = 1_000_000;

/// The `autoMode` keys the recon renders, in order.
const RENDERED_AUTO_MODE_KEYS: [&str; 5] =
    ["environment", "allow", "soft_deny", "hard_deny", "deny"];

/// `nNd` — render an `autoMode` block as indented JSON.
///
/// Keeps only the five rendered keys, and only when present and not `false`.
#[must_use]
pub fn render_auto_mode_json(auto_mode: &serde_json::Value) -> String {
    let mut picked = serde_json::Map::new();
    for key in RENDERED_AUTO_MODE_KEYS {
        match auto_mode.get(key) {
            None | Some(serde_json::Value::Null) | Some(serde_json::Value::Bool(false)) => {}
            Some(v) => {
                picked.insert(key.to_string(), v.clone());
            }
        }
    }
    // `JSON.stringify(obj, null, 1)` — one-space indent.
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    if serde::Serialize::serialize(&serde_json::Value::Object(picked), &mut ser).is_err() {
        return "{}".to_string();
    }
    escape_separators(&String::from_utf8_lossy(&buf))
}

/// What the caller found when probing the project-local settings file.
pub trait LocalSettingsSource {
    /// `lstat(.claude)`: `None` when absent, `Some(is_directory)` otherwise.
    fn lingxi_dir(&self) -> Option<bool>;
    /// `lstat(.claude/settings.local.json)`: `None` when absent, else
    /// `(is_regular_file, nlink, size)`.
    fn local_file(&self) -> Option<(bool, u64, u64)>;
    /// Secure read (`O_NOFOLLOW`, `nlink == 1`); `None` when unreadable.
    fn read_local(&self) -> Option<String>;
    /// `git ls-files -- .claude/settings.local.json` returned a match.
    fn tracked_in_git(&self) -> bool;
}

/// `Qsy` — the project-local `settings.local.json` sub-block.
///
/// Returns the rendered block and the `auto_mode_pregather` code to emit.
///
/// The gate ladder here is deliberately refusal-shaped: when `.claude` is not
/// a real directory, or the file is not a regular `nlink == 1` file, the recon
/// reports it and does NOT probe further. It never resolves the indirection to
/// see what is behind it — the point is to avoid following a repo-committed
/// symlink at all, not to look and then decide.
#[must_use]
pub fn local_settings_block(source: &dyn LocalSettingsSource) -> (String, Option<&'static str>) {
    use crate::auto_mode_facts as facts;
    use crate::auto_mode_pregather as pregather;
    use crate::auto_mode_sections as sections;
    use crate::auto_mode_sections::HEADING_LOCAL_SETTINGS_AUTOMODE as HEAD;

    let skipped = |what: &str, code: &'static str| {
        (
            format!(
                "{HEAD}\nPresent but {what}{}",
                facts::LOCAL_SETTINGS_SKIPPED_SUFFIX
            ),
            Some(code),
        )
    };

    let Some(is_dir) = source.lingxi_dir() else {
        return (String::new(), None);
    };
    if !is_dir {
        return (
            format!("{HEAD}\n{}", sections::LINGXI_DIR_INDIRECTION_GATE_FAILED),
            Some(pregather::PREGATHER_CODE_LOCAL_SETTINGS_INDIRECTION_GATE),
        );
    }
    let Some((is_file, nlink, size)) = source.local_file() else {
        return (String::new(), None);
    };
    if !is_file || nlink != 1 {
        return (
            format!(
                "{HEAD}\n{}",
                sections::LOCAL_SETTINGS_INDIRECTION_GATE_FAILED
            ),
            Some(pregather::PREGATHER_CODE_LOCAL_SETTINGS_INDIRECTION_GATE),
        );
    }
    if size as usize > SETTINGS_READ_CAP {
        return skipped(
            "oversized",
            pregather::PREGATHER_CODE_LOCAL_SETTINGS_OVERSIZED,
        );
    }
    let Some(raw) = source.read_local() else {
        return skipped(
            "unreadable",
            pregather::PREGATHER_CODE_LOCAL_SETTINGS_UNREADABLE,
        );
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return skipped(
            "not valid JSON",
            pregather::PREGATHER_CODE_LOCAL_SETTINGS_INVALID_JSON,
        );
    };
    let Some(auto_mode) = parsed.get("autoMode").filter(|v| v.is_object()) else {
        return (String::new(), None);
    };
    let rendered = render_auto_mode_json(auto_mode);
    if rendered == "{}" {
        return (String::new(), None);
    }
    if rendered.len() > LOCAL_SETTINGS_RENDER_CAP {
        return skipped(
            "oversized",
            pregather::PREGATHER_CODE_LOCAL_SETTINGS_OVERSIZED,
        );
    }
    let tracked = if source.tracked_in_git() {
        facts::TRACKED_IN_GIT_YES
    } else {
        facts::TRACKED_IN_GIT_NO
    };
    (
        format!(
            "{HEAD}\n{rendered}\n{}{tracked}",
            sections::TRACKED_IN_GIT_PREFIX
        ),
        None,
    )
}

/// Supplies what the existing-settings section reads.
pub trait SettingsReconSource {
    /// The user settings file: `Ok(None)` when absent, `Err(())` when present
    /// but unreadable.
    fn user_settings(&self) -> Result<Option<String>, ()>;
    /// The project-local sub-block, already rendered.
    fn local_block(&self) -> String;
    /// Whether `autoMode.classifyAllShell` is active.
    fn classify_all_shell(&self) -> bool;
}

/// `Zsy` — the "Existing auto-mode settings (selective read)" section body.
///
/// # Errors
/// `Err(())` when the settings file is present but unreadable, which the
/// skeleton renders as the section-failed marker.
pub fn existing_settings_section(source: &dyn SettingsReconSource) -> Result<String, ()> {
    use crate::auto_mode_facts as facts;
    use crate::auto_mode_sections as sections;

    let mut auto_mode_rendered = facts::NO_SETTINGS_FILE.to_string();
    let mut bypassing: Vec<String> = Vec::new();
    let mut destructive: Vec<String> = Vec::new();
    let (mut bypass_overflow, mut destructive_overflow, mut unrenderable) =
        (0usize, 0usize, 0usize);

    if let Some(raw) = source.user_settings()? {
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::json!({}));
        auto_mode_rendered =
            render_auto_mode_json(parsed.get("autoMode").unwrap_or(&serde_json::json!({})));

        if let Some(allow) = parsed
            .get("permissions")
            .and_then(|p| p.get("allow"))
            .and_then(serde_json::Value::as_array)
        {
            let rules: Vec<&str> = allow.iter().filter_map(serde_json::Value::as_str).collect();
            let is_bypassing = |r: &str| {
                let v = crate::PermissionRuleValue::from_rule_string(r);
                crate::is_dangerous_classifier_permission(&v.tool_name, &v.rule_content)
            };
            let byp: Vec<&str> = rules.iter().copied().filter(|r| is_bypassing(r)).collect();
            let dest: Vec<&str> = rules
                .iter()
                .copied()
                .filter(|r| !is_bypassing(r))
                .filter(|r| {
                    let v = crate::PermissionRuleValue::from_rule_string(r);
                    crate::auto_mode_destructive::is_destructive_permission(
                        &v.tool_name,
                        &v.rule_content,
                    )
                })
                .collect();

            // A flagged rule that cannot be rendered is COUNTED, not dropped:
            // the user is told it exists and must be reviewed by hand.
            unrenderable = byp.iter().filter(|r| !is_renderable(r)).count()
                + dest.iter().filter(|r| !is_renderable(r)).count();

            let shown_byp: Vec<&str> = byp.into_iter().filter(|r| is_renderable(r)).collect();
            let shown_dest: Vec<&str> = dest.into_iter().filter(|r| is_renderable(r)).collect();
            bypass_overflow = shown_byp.len().saturating_sub(FLAGGED_LIST_CAP);
            destructive_overflow = shown_dest.len().saturating_sub(FLAGGED_LIST_CAP);
            bypassing = shown_byp
                .into_iter()
                .take(FLAGGED_LIST_CAP)
                .map(str::to_string)
                .collect();
            destructive = shown_dest
                .into_iter()
                .take(FLAGGED_LIST_CAP)
                .map(str::to_string)
                .collect();
        }
    }

    let classify_note = if source.classify_all_shell() {
        facts::CLASSIFY_ALL_SHELL_NOTE
    } else {
        ""
    };
    let capped = |n: usize| {
        if n > 0 {
            format!("\n- \u{2026}and {n}{}", facts::FLAGGED_LIST_CAPPED)
        } else {
            String::new()
        }
    };
    let bullets = |rules: &[String]| {
        rules
            .iter()
            .map(|r| format!("- `{}`", display_name(r)))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let mut parts: Vec<String> = Vec::new();
    parts.push(format!(
        "{}{auto_mode_rendered}{}",
        sections::HEADING_AUTOMODE_KEYS,
        source.local_block()
    ));
    parts.push(if bypassing.is_empty() {
        format!(
            "\n{}{classify_note}",
            sections::NO_CLASSIFIER_BYPASSING_ENTRIES.trim_start_matches('\n')
        )
    } else {
        format!(
            "\n{}\n{}{}{classify_note}",
            sections::HEADING_FLAGGED_CLASSIFIER_BYPASSING,
            bullets(&bypassing),
            capped(bypass_overflow)
        )
    });
    parts.push(if destructive.is_empty() {
        format!(
            "\n{}",
            sections::NO_DESTRUCTIVE_ENTRIES.trim_start_matches('\n')
        )
    } else {
        format!(
            "\n{}\n{}{}",
            sections::HEADING_FLAGGED_DESTRUCTIVE,
            bullets(&destructive),
            capped(destructive_overflow)
        )
    });
    if unrenderable > 0 {
        parts.push(format!(
            "\n{}{}",
            crate::auto_mode_sections::additional_flagged_line(unrenderable),
            facts::FLAGGED_ENTRY_UNRENDERABLE
        ));
    }

    Ok(parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n"))
}

/// `_ay` — the "Shipped default auto-mode rule labels" section body.
///
/// Lists the labels of the shipped `allow` and `soft_deny` rules so the model
/// can avoid proposing carve-outs the defaults already cover. Labels only: the
/// full rule prose belongs to the classifier prompt, and the section exists to
/// say what is *already covered*, not to restate it.
#[must_use]
pub fn default_labels_section() -> String {
    format!(
        "{DEFAULT_LABELS_GUIDANCE}\n{HEADING_DEFAULT_ALLOW_LABELS}{}\n{HEADING_DEFAULT_SOFT_DENY_LABELS}{}",
        label_bullets(&DEFAULT_ALLOW_LABELS),
        label_bullets(&DEFAULT_SOFT_DENY_LABELS),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_labels_section_has_the_oracle_shape() {
        let body = default_labels_section();
        // guidance, then the two sub-headings with their bullet lists.
        assert!(body.starts_with(DEFAULT_LABELS_GUIDANCE));
        assert!(body.contains("\n\n#### Default allow labels\n- Security Discussion\n"));
        assert!(body.contains("\n\n#### Default soft-deny labels\n- Git Destructive\n"));
        // Every shipped label appears exactly once as a bullet.
        for label in DEFAULT_ALLOW_LABELS
            .iter()
            .chain(DEFAULT_SOFT_DENY_LABELS.iter())
        {
            assert_eq!(
                body.matches(&format!("- {label}\n")).count()
                    + usize::from(body.ends_with(&format!("- {label}"))),
                1,
                "label {label:?} must appear exactly once"
            );
        }
    }

    #[test]
    fn url_userinfo_is_stripped() {
        // The advertised contract of the repos-found header, applied to the
        // whole block: a credential in any URL never reaches the model.
        assert_eq!(
            strip_url_userinfo("https://user:pat@github.com/acme/app.git"),
            "https://github.com/acme/app.git"
        );
        assert_eq!(
            strip_url_userinfo("ssh://git@github.com/acme/app"),
            "ssh://github.com/acme/app"
        );
        // No userinfo -> untouched.
        assert_eq!(
            strip_url_userinfo("https://github.com/acme/app"),
            "https://github.com/acme/app"
        );
        // An `@` AFTER the authority is not userinfo.
        assert_eq!(
            strip_url_userinfo("https://github.com/acme/app@v1"),
            "https://github.com/acme/app@v1"
        );
        // Multiple URLs in one string.
        assert_eq!(
            strip_url_userinfo("a https://u:p@h/x b https://v:q@i/y"),
            "a https://h/x b https://i/y"
        );
        // Non-ASCII passes through without panicking on byte indexing.
        assert_eq!(strip_url_userinfo("路径 https://u@h/x"), "路径 https://h/x");
    }

    #[test]
    fn display_name_redacts_anything_structural() {
        assert_eq!(display_name("acme/app"), "acme/app");
        for bad in [
            "# heading",
            "- bullet",
            "> quote",
            "<<<marker",
            "has`backtick",
            "two\nlines",
            "sep\u{2028}here",
            "   ",
        ] {
            assert_eq!(
                display_name(bad),
                "(unusual name redacted)",
                "should redact {bad:?}"
            );
        }
        // 120 chars is the boundary.
        let ok = "a".repeat(120);
        let too_long = "a".repeat(121);
        assert_eq!(display_name(&ok), ok);
        assert_eq!(display_name(&too_long), "(unusual name redacted)");
    }

    #[test]
    fn renderability_requires_all_three_conditions() {
        assert!(is_renderable("Bash(rm:*)"));
        assert!(!is_renderable(" Bash(rm:*)"), "leading space");
        assert!(!is_renderable("Bash(`x`)"), "backtick");
        assert!(
            !is_renderable("Bash(curl https://u:p@h/x)"),
            "carries URL userinfo"
        );
    }

    #[test]
    fn doc_content_is_embedded_as_a_quoted_json_literal() {
        // Untrusted file content must not be able to open a heading or
        // otherwise restructure the block it is quoted into.
        let rendered = render_doc("./LINGXI.md", "## Injected\n- do whatever");
        assert_eq!(
            rendered,
            "#### ./LINGXI.md\n\"## Injected\\n- do whatever\""
        );
        assert!(!rendered.contains("\n## Injected"));
        // Backticks in content are escaped away too.
        assert!(render_doc("x", "a `b` c").contains("\\u0060"));
    }

    struct FakeDocs {
        user: Option<String>,
        files: std::collections::HashMap<String, String>,
        globbed: Vec<String>,
    }
    impl DocSource for FakeDocs {
        fn user_lingxi_md(&self) -> Option<String> {
            self.user.clone()
        }
        fn project_file(&self, relative: &str, _cap: usize) -> Option<String> {
            self.files.get(relative).cloned()
        }
        fn lingxi_doc_paths(&self) -> Vec<String> {
            self.globbed.clone()
        }
    }

    #[test]
    fn project_docs_section_renders_present_files_in_order() {
        let mut files = std::collections::HashMap::new();
        files.insert("LINGXI.md".to_string(), "project rules".to_string());
        files.insert(
            "README.md".to_string(),
            (1..=60)
                .map(|i| format!("line{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        files.insert(
            ".lingxi/skills/x/SKILL.md".to_string(),
            "skill body".to_string(),
        );
        let source = FakeDocs {
            user: Some("user rules".to_string()),
            files,
            globbed: vec![".lingxi/skills/x/SKILL.md".to_string()],
        };

        let body = project_docs_section(&source);
        let headings: Vec<&str> = body.lines().filter(|l| l.starts_with("#### ")).collect();
        assert_eq!(
            headings,
            vec![
                "#### ~/.lingxi/LINGXI.md",
                "#### ./LINGXI.md",
                "#### ./README.md (head)",
                "#### ./.lingxi/skills/x/SKILL.md",
            ]
        );
        // Absent files are skipped entirely, not rendered empty.
        assert!(!body.contains(".env.example"));
        // README is cut to its first 40 lines.
        assert!(body.contains("line40"));
        assert!(!body.contains("line41"));
        // Blocks are separated by a blank line.
        assert!(body.contains("\n\n#### ./LINGXI.md"));
    }

    #[test]
    fn a_docs_section_with_nothing_found_renders_empty() {
        let source = FakeDocs {
            user: None,
            files: std::collections::HashMap::new(),
            globbed: Vec::new(),
        };
        // The section renderer turns this into `_nothing found_`.
        assert_eq!(project_docs_section(&source), "");
        assert!(crate::auto_mode_pregather::render_section(
            "LINGXI.md files and project docs",
            &project_docs_section(&source)
        )
        .contains("_nothing found_"));
    }

    // ── `Ysy` ────────────────────────────────────────────────────────────────

    #[test]
    fn remote_urls_lose_userinfo_and_everything_past_owner_repo() {
        // The contract REPOS_FOUND_HEADER advertises.
        assert_eq!(
            redact_remote_url("https://bot:ghp_SECRET@github.com/acme/app.git"),
            "https://github.com/acme/app.git"
        );
        assert_eq!(
            redact_remote_url("https://github.com/acme/app/tree/main/deep/path"),
            "https://github.com/deep/path"
        );
        // scp-style: the `user@` is stripped.
        assert_eq!(
            redact_remote_url("git@github.com:acme/app.git"),
            "github.com:acme/app.git"
        );
        // Anything that will not parse is replaced WHOLESALE, never partially.
        for bad in [
            "https://git hub.com/a/b",  // space
            "https://gith%75b.com/a/b", // percent
            "https://git\\hub.com/a/b", // backslash
            "a@b@c:x",                  // two `@`
            "not a url at all!",
        ] {
            assert_eq!(
                redact_remote_url(bad),
                "(unparseable remote URL redacted)",
                "should redact {bad:?}"
            );
        }
        assert_eq!(
            redact_remote_url(&"h".repeat(2049)),
            "(unparseable remote URL redacted)"
        );
    }

    #[test]
    fn remote_host_is_only_taken_when_plausible() {
        assert_eq!(
            remote_host("https://github.com/acme/app"),
            Some("github.com".to_string())
        );
        assert_eq!(
            remote_host("github.com:acme/app"),
            Some("github.com".to_string())
        );
        // No path after the authority, a bare drive letter, or a junk host.
        assert_eq!(remote_host("https://github.com"), None);
        assert_eq!(remote_host("c:/repo"), None);
        assert_eq!(remote_host("https://-bad-/a/b"), None);
    }

    struct FakeRepo {
        git: std::collections::HashMap<String, String>,
        counts: usize,
        present: Vec<&'static str>,
        symlinked: Vec<&'static str>,
        files: std::collections::HashMap<String, String>,
        path: String,
    }
    impl Default for FakeRepo {
        fn default() -> Self {
            Self {
                git: std::collections::HashMap::new(),
                counts: 0,
                present: Vec::new(),
                symlinked: Vec::new(),
                files: std::collections::HashMap::new(),
                path: "/w/app".to_string(),
            }
        }
    }
    impl RepoFactsSource for FakeRepo {
        fn git(&self, args: &[&str]) -> String {
            self.git.get(&args.join(" ")).cloned().unwrap_or_default()
        }
        fn git_line_count(&self, _: &[&str]) -> usize {
            self.counts
        }
        fn path_has_symlink_component(&self, relative: &str) -> Option<bool> {
            if self.symlinked.contains(&relative) {
                return Some(true);
            }
            self.present.contains(&relative).then_some(false)
        }
        fn read_file(&self, relative: &str, _cap: usize) -> Option<String> {
            self.files.get(relative).cloned()
        }
        fn repo_path(&self) -> String {
            self.path.clone()
        }
    }

    fn repo_with(pairs: &[(&str, &str)]) -> FakeRepo {
        let mut r = FakeRepo::default();
        for (k, v) in pairs {
            r.git.insert((*k).to_string(), (*v).to_string());
        }
        r
    }

    #[test]
    fn repo_facts_renders_the_core_lines() {
        let mut repo = repo_with(&[
            ("remote", "origin"),
            (
                "symbolic-ref --short refs/remotes/origin/HEAD",
                "origin/main",
            ),
            ("remote get-url origin", "https://github.com/acme/app.git"),
            (
                "config -z --get-all remote.origin.url",
                "https://github.com/acme/app.git",
            ),
        ]);
        repo.counts = 1234;
        repo.present = vec![".github/workflows", "LINGXI.md"];

        let facts = repo_facts_section(&repo);
        assert!(facts.body.contains("Repo path: /w/app"));
        assert!(facts.body.contains("Tracked file count: 1234"));
        assert!(facts.body.contains("Default branch: main"));
        assert!(facts
            .body
            .contains("Posture signals present: .github/workflows, LINGXI.md"));
        // Both a fetch and a push line, the push falling back to the fetch URL.
        assert!(facts
            .body
            .contains("origin\thttps://github.com/acme/app.git (fetch)"));
        assert!(facts
            .body
            .contains("origin\thttps://github.com/acme/app.git (push)"));
        assert_eq!(facts.this_repo_host, Some("github.com".to_string()));
        // The gh explainer always trails the section.
        assert!(facts.body.contains("do not fetch those yourself"));
    }

    #[test]
    fn a_symlinked_posture_signal_is_not_counted() {
        // A committed symlink must not be followed just to tick a box.
        let mut repo = FakeRepo::default();
        repo.present = vec!["LINGXI.md"];
        repo.symlinked = vec![".github/workflows"];
        let facts = repo_facts_section(&repo);
        assert!(facts.body.contains("Posture signals present: LINGXI.md"));
        assert!(!facts.body.contains(".github/workflows"));
    }

    #[test]
    fn missing_origin_head_and_no_remotes_have_their_own_wording() {
        let facts = repo_facts_section(&FakeRepo::default());
        assert!(facts
            .body
            .contains("Default branch: (unknown \u{2014} origin/HEAD unset)"));
        assert!(facts.body.contains("(no remotes)"));
        assert!(facts.body.contains("Posture signals present: none"));
        assert_eq!(facts.this_repo_host, None);
    }

    #[test]
    fn unusual_names_are_redacted_rather_than_rendered() {
        let mut repo = repo_with(&[
            ("remote", "we`ird"),
            (
                "symbolic-ref --short refs/remotes/origin/HEAD",
                "origin/we`ird",
            ),
            (
                "config -z --get-all remote.we`ird.url",
                "https://github.com/a/b",
            ),
        ]);
        repo.path = "/w/ba`d".to_string();
        let facts = repo_facts_section(&repo);
        assert!(facts.body.contains("(unusual repo path redacted)"));
        assert!(facts.body.contains("(unusual branch name redacted)"));
        assert!(facts.body.contains("(unusual remote name redacted)"));
        assert!(!facts.body.contains("we`ird"));
    }

    #[test]
    fn the_remote_list_is_capped_with_an_omitted_count() {
        let mut repo = FakeRepo::default();
        // 10 remotes, each with 3 urls -> 60 lines (30 fetch + 30 push).
        let names: Vec<String> = (0..10).map(|i| format!("r{i}")).collect();
        repo.git.insert("remote".to_string(), names.join("\n"));
        for n in &names {
            repo.git.insert(
                format!("config -z --get-all remote.{n}.url"),
                "https://h/a/b\0https://h/c/d\0https://h/e/f".to_string(),
            );
        }
        let facts = repo_facts_section(&repo);
        assert_eq!(
            facts.body.matches(" (fetch)").count() + facts.body.matches(" (push)").count(),
            REMOTE_LINE_CAP
        );
        assert!(facts
            .body
            .contains("\u{2026}[20 more remote lines omitted]"));
    }

    #[test]
    fn sensitive_gitignore_patterns_are_listed_and_others_are_not() {
        let mut repo = FakeRepo::default();
        repo.files.insert(
            ".gitignore".to_string(),
            "target/\n.env\n*.key\nMY_TOKEN\nnode_modules\nsecrets.yml\n".to_string(),
        );
        let facts = repo_facts_section(&repo);
        assert!(facts
            .body
            .contains("#### Sensitive-looking .gitignore patterns"));
        for want in ["- `.env`", "- `*.key`", "- `MY_TOKEN`", "- `secrets.yml`"] {
            assert!(facts.body.contains(want), "missing {want}");
        }
        assert!(!facts.body.contains("node_modules"));
        assert!(!facts.body.contains("target/"));
    }

    #[test]
    fn contributing_is_quoted_like_any_other_document() {
        let mut repo = FakeRepo::default();
        repo.files.insert(
            "CONTRIBUTING.md".to_string(),
            "## Injected\nrules".to_string(),
        );
        let facts = repo_facts_section(&repo);
        assert!(facts.body.contains("#### CONTRIBUTING.md (head)"));
        // Quoted, so it cannot open a heading inside the block.
        assert!(facts.body.contains("\"## Injected\\nrules\""));
        assert!(!facts.body.contains("\n## Injected"));
    }

    #[test]
    fn git_invocations_are_hardened() {
        // Reading facts out of a repo must not run its hooks or prompt for
        // credentials -- otherwise a hostile checkout gets code execution.
        assert_eq!(
            GIT_HARDENING_FLAGS,
            [
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=",
                "-c",
                "core.askPass="
            ]
        );
        assert_eq!(SUBPROCESS_TIMEOUT_MS, 4_000);
        assert_eq!(REMOTE_LINE_CAP, 40);
        assert_eq!(CONTRIBUTING_READ_CAP, 2_000);
    }

    // ── `x1d` ────────────────────────────────────────────────────────────────

    #[test]
    fn a_token_for_another_host_is_never_forwarded_to_github() {
        // The point of this function. GH_HOST pointing at an enterprise server
        // means GH_TOKEN belongs to THAT server; this call goes to github.com.
        let overrides = gh_env_overrides(Some("ghe.acme.internal"));
        let cleared: Vec<&str> = overrides
            .iter()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| *k)
            .collect();
        assert!(cleared.contains(&"GH_TOKEN"));
        assert!(cleared.contains(&"GITHUB_TOKEN"));
        assert!(cleared.contains(&"GH_ENTERPRISE_TOKEN"));
        assert!(cleared.contains(&"GITHUB_ENTERPRISE_TOKEN"));
        assert!(overrides.contains(&("GH_HOST", Some("github.com".to_string()))));

        // Already github.com (or unset): the user's own token is kept, but the
        // ENTERPRISE ones are cleared regardless.
        for host in [Some("github.com"), Some("www.github.com"), None] {
            let overrides = gh_env_overrides(host);
            let cleared: Vec<&str> = overrides
                .iter()
                .filter(|(_, v)| v.is_none())
                .map(|(k, _)| *k)
                .collect();
            assert!(!cleared.contains(&"GH_TOKEN"), "host={host:?}");
            assert!(cleared.contains(&"GH_ENTERPRISE_TOKEN"), "host={host:?}");
        }
    }

    #[test]
    fn a_remote_is_only_parsed_when_it_is_unambiguous() {
        assert_eq!(
            remote_to_host_org_repo("https://github.com/acme/app", None).as_deref(),
            Some("github.com/acme/app")
        );
        assert_eq!(
            remote_to_host_org_repo("git@github.com:acme/app.git", None).as_deref(),
            Some("github.com/acme/app.git")
        );
        assert_eq!(
            remote_to_host_org_repo("ssh://git@gitlab.com/acme/app", None).as_deref(),
            Some("gitlab.com/acme/app")
        );
        // A wrong parse would send a query about somebody ELSE's repository,
        // so every ambiguous shape is refused outright.
        for bad in [
            "https://u:pw@github.com/acme/app", // password
            "https://user@github.com/acme/app", // username on https
            "ssh://bob@github.com/acme/app",    // non-`git` user
            "https://github.com:8443/acme/app", // explicit port
            "https://evil.example/acme/app",    // unknown host
            "https://github.com/acme",          // not two segments
            "https://github.com/a/b/c",         // too many segments
            "https://github.com/acme/app x",    // unsafe charset
        ] {
            assert_eq!(
                remote_to_host_org_repo(bad, None),
                None,
                "should refuse {bad}"
            );
        }
        // The repo's own host is trusted in addition to the known set.
        assert_eq!(
            remote_to_host_org_repo("https://ghe.acme.io/acme/app", Some("ghe.acme.io")).as_deref(),
            Some("ghe.acme.io/acme/app")
        );
        assert_eq!(
            remote_to_host_org_repo("https://ghe.acme.io/acme/app", None),
            None
        );
    }

    fn gh_ok(stdout: &str) -> GhResult {
        GhResult {
            code: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }
    fn gh_fail() -> GhResult {
        GhResult {
            code: 1,
            stdout: String::new(),
            stderr: "boom".to_string(),
        }
    }

    #[test]
    fn a_failed_capability_degrades_to_not_queryable() {
        assert_eq!(render_visibility(&gh_fail()), "not queryable here");
        assert_eq!(render_rulesets(&gh_fail()), "not queryable here");
        assert_eq!(render_protected_branches(&gh_fail()), "not queryable here");
    }

    #[test]
    fn visibility_is_only_taken_from_the_known_enum() {
        assert_eq!(
            render_visibility(&gh_ok(r#"{"visibility":"PRIVATE"}"#)),
            "private"
        );
        assert_eq!(
            render_visibility(&gh_ok(r#"{"visibility":"internal"}"#)),
            "internal"
        );
        // Anything unrecognised is not guessed at.
        assert_eq!(
            render_visibility(&gh_ok(r#"{"visibility":"weird"}"#)),
            "not queryable here"
        );
        assert_eq!(render_visibility(&gh_ok("not json")), "not queryable here");
    }

    #[test]
    fn protected_branches_render_and_redact() {
        assert_eq!(render_protected_branches(&gh_ok("")), "none listed");
        assert_eq!(
            render_protected_branches(&gh_ok("main\nrelease/v1\n")),
            "`main`, `release/v1`"
        );
        // A name outside the display charset is counted, not shown.
        let out = render_protected_branches(&gh_ok("main\nweird`name\n"));
        assert!(out.contains("`main`"));
        assert!(!out.contains("weird"));
        assert!(out.contains("outside the display charset, redacted"));
    }

    #[test]
    fn rulesets_report_name_and_enforcement() {
        let json = r#"[{"name":"protect main","enforcement":"active"},
                       {"name":"draft","enforcement":"evaluate"}]"#;
        let out = render_rulesets(&gh_ok(json));
        assert!(out.contains("`protect main` - active"));
        assert!(out.contains("`draft` - evaluate"));
        // An unknown enforcement value is redacted rather than shown.
        let out = render_rulesets(&gh_ok(r#"[{"name":"x","enforcement":"maybe"}]"#));
        assert!(out.contains("all names outside the display charset, redacted"));
        assert_eq!(render_rulesets(&gh_ok("[]")), "none listed");
    }

    // ── `xsy` — the org repo split ───────────────────────────────────────────

    #[test]
    fn the_org_split_groups_by_visibility_newest_push_first() {
        let json = r#"[{"name":"web","visibility":"PUBLIC","pushedAt":"2026-01-01"},
                       {"name":"api","visibility":"private","pushedAt":"2026-07-01"},
                       {"name":"ops","visibility":"private","pushedAt":"2026-03-01"}]"#;
        let (body, failed) = render_org_repo_list(&gh_ok(json));
        assert!(!failed);
        // Visibilities are sorted; within one, newest push comes first.
        assert_eq!(body, "- private: `api`, `ops`\n- public: `web`");
    }

    #[test]
    fn org_entries_that_cannot_be_shown_are_counted_not_dropped() {
        let json = r#"[{"name":"good","visibility":"public","pushedAt":"2026-01-01"},
                       {"name":"bad name!","visibility":"public","pushedAt":"2026-01-01"},
                       {"name":"weird","visibility":"classified","pushedAt":"2026-01-01"},
                       {"name":"shapeless"}]"#;
        let (body, failed) = render_org_repo_list(&gh_ok(json));
        assert!(!failed);
        assert!(body.contains("`good`"));
        assert!(!body.contains("bad name"));
        // A filtered list must not read as the org's full inventory.
        assert!(body.contains("(+3"), "got: {body}");
        assert!(body.contains("outside the display charset or visibility enum, redacted"));
    }

    #[test]
    fn a_traversal_repo_name_never_reaches_the_output() {
        // `..` is interpolated into a `gh api repos/…` path elsewhere; it is
        // refused as a name here for the same reason.
        let json = r#"[{"name":"..","visibility":"public","pushedAt":"2026-01-01"}]"#;
        let (body, _) = render_org_repo_list(&gh_ok(json));
        assert!(!body.contains(".."), "got: {body}");
        assert!(body.contains("none listed"));
    }

    #[test]
    fn an_org_list_failure_is_distinguished_from_an_empty_org() {
        let (body, failed) = render_org_repo_list(&gh_fail());
        assert!(!failed);
        assert_eq!(
            body,
            "_not queryable here (gh unavailable, unauthenticated, or token lacks org scope)._"
        );
        let (body, failed) = render_org_repo_list(&gh_ok("{not json"));
        assert!(failed, "a parse failure is worth recording");
        assert_eq!(body, "_not queryable here (gh output unparseable)._");
        // An org that really has no repos says so plainly.
        assert_eq!(render_org_repo_list(&gh_ok("[]")).0, "_none listed_");
    }

    // ── `x1d` — the assembled section ────────────────────────────────────────

    struct FakeGh {
        origin: Option<String>,
        calls: std::cell::RefCell<Vec<Vec<String>>>,
    }

    impl GhSource for FakeGh {
        fn origin_remote(&self) -> Option<String> {
            self.origin.clone()
        }
        fn gh(&self, args: &[&str], _max_buffer: usize) -> GhResult {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|a| (*a).to_string()).collect());
            match (args.first().copied(), args.get(1).copied()) {
                (Some("repo"), Some("view")) => gh_ok(r#"{"visibility":"private"}"#),
                // `--jq .[].name` emits newline-separated names, so no
                // protected branches is EMPTY output, not `[]`.
                (Some("api"), Some(path)) if path.contains("branches") => gh_ok(""),
                _ => gh_ok("[]"),
            }
        }
    }

    fn fake_gh(origin: Option<&str>) -> FakeGh {
        FakeGh {
            origin: origin.map(str::to_string),
            calls: std::cell::RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn the_section_reports_the_repo_and_its_protections() {
        let gh = fake_gh(Some("https://github.com/acme/app\n"));
        let out = repo_visibility_section(&gh, false, true);
        assert!(out.body.starts_with("Repo: acme/app\n"));
        assert!(out.body.contains("Visibility: private"));
        assert!(out.body.contains("Rulesets: none listed"));
        assert!(out.body.contains("Protected branches: none listed"));
        assert!(!out.failures.any());
    }

    #[test]
    fn a_closed_q2_means_the_org_is_never_queried() {
        let gh = fake_gh(Some("https://github.com/acme/app"));
        let out = repo_visibility_section(&gh, false, true);
        // Withheld, and textually distinct from an org that has no repos.
        assert!(out.body.contains("_NOT GATHERED"));
        let calls = gh.calls.borrow();
        assert!(
            !calls
                .iter()
                .any(|c| c.first().map(String::as_str) == Some("repo")
                    && c.get(1).map(String::as_str) == Some("list")),
            "the org list must not be fetched when Q2 is closed"
        );
    }

    #[test]
    fn an_open_q2_does_query_the_org() {
        let gh = fake_gh(Some("https://github.com/acme/app"));
        let _ = repo_visibility_section(&gh, true, true);
        let calls = gh.calls.borrow();
        assert!(calls
            .iter()
            .any(|c| c.first().map(String::as_str) == Some("repo")
                && c.get(1).map(String::as_str) == Some("list")
                && c.get(2).map(String::as_str) == Some("acme")));
    }

    #[test]
    fn a_non_github_or_unparseable_origin_refuses_before_any_call() {
        for origin in [
            None,
            Some("https://ghe.acme.internal/acme/app"),
            Some("https://github.com/acme"),
            Some("not a url"),
        ] {
            let gh = fake_gh(origin);
            let out = repo_visibility_section(&gh, true, true);
            assert!(
                out.body.starts_with("_Not queryable here ("),
                "origin={origin:?}"
            );
            assert!(out.body.contains(INFER_VISIBILITY_HINT));
            // Refusing means asking GitHub NOTHING — a guessed org would send
            // somebody else's repository name to the API.
            assert!(gh.calls.borrow().is_empty(), "origin={origin:?}");
        }
    }

    #[test]
    fn disabled_nonessential_traffic_makes_no_calls_at_all() {
        let gh = fake_gh(Some("https://github.com/acme/app"));
        let out = repo_visibility_section(&gh, true, false);
        assert!(out
            .body
            .contains("nonessential traffic disabled or policy-restricted"));
        assert!(gh.calls.borrow().is_empty());
    }

    #[test]
    fn gh_unavailability_is_distinguished_from_a_broken_call() {
        // 127 = not found, 4 = not authenticated, 1 with no stderr = no result.
        for code in [127, 4] {
            assert!(gh_is_unavailable(&GhResult {
                code,
                stdout: String::new(),
                stderr: String::new()
            }));
        }
        assert!(gh_is_unavailable(&GhResult {
            code: 1,
            stdout: String::new(),
            stderr: String::new()
        }));
        // A real failure has something on stderr and IS worth recording.
        assert!(!gh_is_unavailable(&gh_fail()));
    }

    #[test]
    fn gh_display_helpers_match_the_oracle() {
        assert!(is_displayable_gh_name("release/v1.0-rc"));
        assert!(is_displayable_gh_name("protect main"));
        assert!(!is_displayable_gh_name("has`tick"));
        assert!(!is_displayable_gh_name(""));
        assert!(!is_displayable_gh_name(&"a".repeat(121)));
        assert_eq!(
            join_gh_names(&["a".into(), "b".into(), "c".into()], 2),
            "`a`, `b` (+1 more)"
        );
        assert_eq!(GH_TIMEOUT_MS, 4_000);
        assert_eq!(KNOWN_VCS_HOSTS.len(), 3);
    }

    // ── `j1d` ────────────────────────────────────────────────────────────────

    #[test]
    fn the_walk_refuses_cloud_sync_roots_and_tool_caches() {
        // Descending into a synced folder can wake a sync client or pull
        // content down from the network.
        for name in [
            "OneDrive",
            "Dropbox",
            "Google Drive",
            "OneDrive - Acme Corp",
        ] {
            assert!(is_skipped_walk_dir(name, false, true), "{name}");
        }
        // ...but a merely similar name is not skipped.
        assert!(!is_skipped_walk_dir("Dropboxes", false, true));
        assert!(!is_skipped_walk_dir("onedrive-backup", false, true));
        // Tool caches and VCS internals.
        for name in [".git", "node_modules", ".cargo", ".lingxi"] {
            assert!(is_skipped_walk_dir(name, false, false), "{name}");
        }
        // Platform-specific.
        assert!(is_skipped_walk_dir("AppData", true, false));
        assert!(!is_skipped_walk_dir("AppData", false, false));
        assert!(is_skipped_walk_dir("Library", false, true));
        assert!(!is_skipped_walk_dir("Library", false, false));
    }

    #[test]
    fn git_config_remotes_are_parsed_from_named_sections_only() {
        let config = r#"
[core]
	url = https://not-a-remote.example/a/b
[remote "origin"]
	url = https://github.com/acme/app.git
	pushurl = git@github.com:acme/app.git
[remote "fork"]
	url = https://github.com/bob/app
[branch "main"]
	url = https://github.com/nope/nope
"#;
        let urls = parse_config_remote_urls(config);
        assert_eq!(
            urls,
            vec![
                "https://github.com/acme/app.git",
                "git@github.com:acme/app.git",
                "https://github.com/bob/app",
            ]
        );
        // `[core]` and `[branch "main"]` are not remotes.
        assert!(!urls.iter().any(|u| u.contains("not-a-remote")));
        assert!(!urls.iter().any(|u| u.contains("nope")));
    }

    #[test]
    fn git_config_parsing_handles_comments_quotes_and_continuations() {
        let config = "[remote \"origin\"]\n\turl = \"https://github.com/acme/app\" # trailing\n\tpushurl = https://github.com/acme/\\\n\t\tapp2\n";
        let urls = parse_config_remote_urls(config);
        assert_eq!(urls[0], "https://github.com/acme/app");
        assert!(!urls[0].contains("trailing"));
        // The continuation joined without inventing whitespace.
        assert!(urls[1].contains("app2"));
    }

    #[test]
    fn repo_remotes_are_reduced_deduplicated_and_capped() {
        let config = r#"
[remote "origin"]
	url = https://github.com/acme/app
	pushurl = git@github.com:acme/app
[remote "mirror"]
	url = https://gitlab.com/acme/app
[remote "junk"]
	url = https://evil.example/acme/app
"#;
        let remotes = config_remotes(config, None);
        // `origin`'s two URLs reduce to the same host/org/repo, so one entry.
        assert_eq!(remotes, vec!["github.com/acme/app", "gitlab.com/acme/app"]);
        // An unknown host contributes nothing.
        assert!(!remotes.iter().any(|r| r.contains("evil")));
    }

    #[test]
    fn a_gitdir_pointer_is_read_only_in_its_exact_form() {
        assert_eq!(
            parse_gitdir_pointer("gitdir: ../.git/worktrees/x\n").as_deref(),
            Some("../.git/worktrees/x")
        );
        assert_eq!(parse_gitdir_pointer("gitdir: \n"), None);
        assert_eq!(parse_gitdir_pointer("gitdir:no-space"), None);
        assert_eq!(parse_gitdir_pointer("something else"), None);
    }

    #[test]
    fn containment_and_home_relative_paths() {
        assert!(is_strictly_under("/home/u/work/app", "/home/u", false));
        assert!(!is_strictly_under("/home/u", "/home/u", false));
        assert!(!is_strictly_under("/home/other/app", "/home/u", false));
        assert_eq!(
            home_relative("/home/u/work/app", "/home/u", false),
            "~/work/app"
        );
        assert_eq!(home_relative("/home/u", "/home/u", false), "~");
        // Outside the home directory is shown as-is rather than mislabelled.
        assert_eq!(home_relative("/opt/app", "/home/u", false), "/opt/app");
    }

    #[test]
    fn home_repos_render_with_their_reason_for_having_no_remote() {
        let repos = vec![
            HomeRepo {
                path: "~/work/app".into(),
                remotes: vec!["github.com/acme/app".into()],
                note: None,
            },
            HomeRepo {
                path: "~/work/wt".into(),
                remotes: vec![],
                note: Some(HomeRepoNote::GitdirOutsideHome),
            },
            HomeRepo {
                path: "~/scratch".into(),
                remotes: vec![],
                note: Some(HomeRepoNote::NoRemote),
            },
            HomeRepo {
                path: "~/vendor".into(),
                remotes: vec![],
                note: None,
            },
        ];
        let body = home_repos_body(&repos, WalkLimit::None);
        assert!(body.contains("- `~/work/app` \u{2014} github.com/acme/app"));
        assert!(body.contains("- `~/work/wt` \u{2014} (gitdir points outside the home directory"));
        assert!(body.contains("- `~/scratch` \u{2014} (no remote configured)"));
        assert!(body.contains("- `~/vendor` \u{2014} (remote not on a known VCS host; not shown)"));
        // The header states the redaction contract, and the candidate caveat
        // always trails.
        assert!(body.contains("userinfo and any path beyond owner/repo are stripped"));
        assert!(body.contains("These are CANDIDATES, not vetted context"));
    }

    #[test]
    fn a_walk_cut_short_with_nothing_found_is_unknown_not_none() {
        // "We found none" and "we stopped before finding any" are different
        // claims, and only the first is evidence.
        assert!(home_repos_body(&[], WalkLimit::None).contains("No other git repos found"));
        for limit in [
            WalkLimit::Timeout,
            WalkLimit::VisitBudget,
            WalkLimit::RepoCap,
        ] {
            let body = home_repos_body(&[], limit);
            assert!(
                body.contains("treat this as unknown, not as none"),
                "{limit:?} must not read as none"
            );
        }
        // Each incomplete walk also says WHY it stopped.
        assert!(home_repos_body(&[], WalkLimit::Timeout).contains("time budget"));
        assert!(home_repos_body(&[], WalkLimit::VisitBudget).contains("directory budget"));
        assert!(home_repos_body(&[], WalkLimit::RepoCap).contains("Result cap reached"));
    }

    #[test]
    fn a_network_or_unreadable_home_is_reported_without_a_listing() {
        let body = home_repos_body(&[], WalkLimit::NetworkHome);
        assert!(body.contains("merely touching one authenticates to"));
        assert!(!body.contains("CANDIDATES"));
        let body = home_repos_body(&[], WalkLimit::HomeUnreadable);
        assert!(body.contains("could not be read"));
    }

    #[test]
    fn home_walk_caps_match_the_oracle() {
        assert_eq!(HOME_WALK_MAX_DIRS, 4_000);
        assert_eq!(HOME_WALK_MAX_REPOS, 20);
        assert_eq!(HOME_WALK_MAX_DEPTH, 2);
        assert_eq!(HOME_WALK_TIMEOUT_MS, 8_000);
        assert_eq!(REPO_REMOTE_LIMIT, 5);
        assert_eq!(GIT_CONFIG_READ_CAP, 128_000);
        assert_eq!(GITDIR_FILE_READ_CAP, 4_096);
        assert_eq!(GIT_CONFIG_MAX_LINES, 2_000);
    }

    // ── `Xsy` ────────────────────────────────────────────────────────────────

    #[test]
    fn a_repo_name_that_is_path_traversal_is_refused() {
        // The name goes straight into a `gh api repos/<org>/<name>/...` path.
        assert!(!is_valid_repo_name("."));
        assert!(!is_valid_repo_name(".."));
        assert!(!is_valid_repo_name("../../etc"));
        assert!(!is_valid_repo_name("a/b"));
        assert!(!is_valid_repo_name(""));
        // Ordinary names, including dot-leading ones that are not `.`/`..`.
        assert!(is_valid_repo_name("app"));
        assert!(is_valid_repo_name(".github"));
        assert!(is_valid_repo_name("my-repo_2.0"));
    }

    #[test]
    fn siblings_are_the_most_recently_pushed_excluding_this_repo() {
        let json = r#"[
            {"name":"old","pushedAt":"2020-01-01T00:00:00Z"},
            {"name":"app","pushedAt":"2026-01-01T00:00:00Z"},
            {"name":"newest","pushedAt":"2026-07-01T00:00:00Z"},
            {"name":"mid","pushedAt":"2026-03-01T00:00:00Z"},
            {"name":"never","pushedAt":null}
        ]"#;
        // `app` is this repo and is excluded; the rest come newest-first, capped
        // at three.
        assert_eq!(
            select_sibling_repos(json, "app").unwrap(),
            vec!["newest", "mid", "old"]
        );
        // Case-insensitive self-exclusion.
        assert!(!select_sibling_repos(json, "APP")
            .unwrap()
            .contains(&"app".to_string()));
        // A traversal name never survives selection.
        let json = r#"[{"name":"..","pushedAt":"2026-01-01T00:00:00Z"}]"#;
        assert!(select_sibling_repos(json, "app").unwrap().is_empty());
        // Non-array or unparseable output is not an empty result.
        assert_eq!(select_sibling_repos("{}", "app"), None);
        assert_eq!(select_sibling_repos("not json", "app"), None);
    }

    #[test]
    fn sibling_docs_are_trimmed_and_announce_the_cut() {
        // README is cut to 40 lines...
        let readme = (1..=60)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = trim_sibling_doc("README.md", &readme);
        assert!(out.contains("l40"));
        assert!(!out.contains("l41"));
        // ...and to the 10 KB cap, with the cut announced.
        let long = "x".repeat(20_000);
        let out = trim_sibling_doc("README.md", &long);
        assert!(out.ends_with("\u{2026}[truncated at 10000 chars]"));
        // LINGXI.md gets the larger cap and no line cut.
        let out = trim_sibling_doc("LINGXI.md", &readme);
        assert!(out.contains("l60"));
        assert!(!out.contains("truncated"));
    }

    struct FakeSiblings {
        list: Option<String>,
        docs: std::collections::HashMap<String, String>,
    }
    impl SiblingDocsSource for FakeSiblings {
        fn list_org_repos(&self, _org: &str) -> Option<String> {
            self.list.clone()
        }
        fn fetch_doc(&self, _org: &str, repo: &str, doc: &str) -> Option<String> {
            self.docs.get(&format!("{repo}/{doc}")).cloned()
        }
    }

    #[test]
    fn the_first_doc_found_per_repo_wins_and_content_is_quoted() {
        let mut docs = std::collections::HashMap::new();
        docs.insert("alpha/LINGXI.md".to_string(), "alpha rules".to_string());
        docs.insert("alpha/README.md".to_string(), "alpha readme".to_string());
        docs.insert("beta/README.md".to_string(), "## beta\nreadme".to_string());
        let src = FakeSiblings {
            list: Some(
                r#"[{"name":"alpha","pushedAt":"2026-02-01"},{"name":"beta","pushedAt":"2026-01-01"}]"#
                    .to_string(),
            ),
            docs,
        };
        let body = sibling_docs_body("acme", "app", &src);
        // LINGXI.md wins for alpha; README is not also emitted.
        assert!(body.contains("#### sibling acme/alpha/LINGXI.md"));
        assert!(!body.contains("alpha/README.md"));
        // beta falls back to its README, labelled as a head excerpt.
        assert!(body.contains("#### sibling acme/beta/README.md (head)"));
        // Untrusted sibling content is quoted, so it cannot open a heading.
        assert!(body.contains("\"## beta\\nreadme\""));
        assert!(!body.contains("\n## beta"));
    }

    #[test]
    fn no_docs_and_no_listing_are_reported_differently() {
        // Repos exist but carry no docs -> "found nothing".
        let src = FakeSiblings {
            list: Some(r#"[{"name":"alpha","pushedAt":"2026-01-01"}]"#.to_string()),
            docs: std::collections::HashMap::new(),
        };
        assert!(sibling_docs_body("acme", "app", &src).contains("No sibling docs found"));

        // gh could not be used at all -> "not queryable", NOT "found nothing".
        let src = FakeSiblings {
            list: None,
            docs: std::collections::HashMap::new(),
        };
        assert!(
            sibling_docs_body("acme", "app", &src).contains("gh unavailable or unauthenticated")
        );
    }

    #[test]
    fn sibling_caps_match_the_oracle() {
        assert_eq!(SIBLING_REPO_LIST_LIMIT, 5);
        assert_eq!(SIBLING_DOC_LIMIT, 3);
        assert_eq!(SIBLING_DOC_NAMES, ["LINGXI.md", "README.md"]);
        assert_eq!(
            sibling_doc_label("acme", "app", "README.md"),
            "sibling acme/app/README.md (head)"
        );
        assert_eq!(
            sibling_doc_label("acme", "app", "LINGXI.md"),
            "sibling acme/app/LINGXI.md"
        );
    }

    // ── `W1d` ────────────────────────────────────────────────────────────────

    struct FakeAllProjects(Option<AllProjectsScan>);
    impl AllProjectsSource for FakeAllProjects {
        fn scan(&self) -> Option<AllProjectsScan> {
            self.0.clone()
        }
    }

    #[test]
    fn an_unavailable_projects_root_is_unknown_not_empty() {
        let body = all_projects_usage_section(&FakeAllProjects(None));
        assert!(body.contains("Treat other-project usage as unknown, not empty"));
    }

    #[test]
    fn a_clean_sweep_reports_counts_and_words_only() {
        let scan = AllProjectsScan {
            scanned: 3,
            selected: 3,
            enumerated: 3,
            commands_seen: 5,
            words: vec!["terraform".into(), "terraform".into(), "helm".into()],
            ..AllProjectsScan::default()
        };
        let body = render_all_projects_usage(&scan);
        assert!(body.contains(
            "Transcripts scanned: 3 of 3 selected (from 3 enumerated); Bash commands seen: 5"
        ));
        assert!(body.contains("#### Tools run in other projects"));
        assert!(body.contains("- terraform (2\u{d7})"));
        assert!(body.contains("- helm (1\u{d7})"));
        assert!(body.contains("Raw command lines were never read into the transcript"));
        // A clean sweep carries none of the shortfall notices.
        for absent in [
            "Enumeration cap",
            "read-deny gate",
            "could not be read",
            "Aggregate byte cap",
            "Deadline reached",
        ] {
            assert!(!body.contains(absent), "unexpected notice: {absent}");
        }
    }

    #[test]
    fn every_way_the_sweep_falls_short_is_reported() {
        // Each counter exists so an incomplete sweep can never read as "these
        // are all the projects".
        let scan = AllProjectsScan {
            scanned: 10,
            selected: 50,
            enumerated: 5_000,
            commands_seen: 40,
            words: vec!["helm".into()],
            enumeration_capped: true,
            stat_failed: 2,
            unreadable_dirs: 1,
            denied: 3,
            unreadable: 4,
            per_file_capped: 1,
            aggregate_capped_remaining: Some(7),
            deadline_remaining: Some(9),
            words_incomplete: true,
        };
        let body = render_all_projects_usage(&scan);
        assert!(body.contains(
            "_Enumeration cap reached \u{2014} the 2000 first-enumerated of 5000 transcripts"
        ));
        assert!(body.contains("_2 transcripts and 1 project directory could not be enumerated"));
        assert!(body.contains("treat missing projects as unknown, not empty"));
        assert!(body.contains("_Skipped by the read-deny gate: 3 transcripts not read"));
        assert!(body.contains("_4 transcripts could not be read"));
        assert!(body.contains("_1 transcript exceeded the 4 MiB per-file cap"));
        assert!(body.contains(
            "_Aggregate byte cap reached (100 MiB) \u{2014} remaining 7 transcripts not scanned._"
        ));
        assert!(body.contains("_Deadline reached \u{2014} remaining 9 transcripts not scanned._"));
        assert!(body.contains("_Command-word extraction hit its line cap or deadline"));
    }

    #[test]
    fn shortfall_notices_use_singular_and_plural_correctly() {
        let scan = AllProjectsScan {
            stat_failed: 1,
            unreadable_dirs: 2,
            denied: 1,
            unreadable: 1,
            per_file_capped: 1,
            aggregate_capped_remaining: Some(1),
            deadline_remaining: Some(1),
            ..AllProjectsScan::default()
        };
        let body = render_all_projects_usage(&scan);
        assert!(body.contains("_1 transcript and 2 project directories"));
        assert!(body.contains("gate: 1 transcript not read"));
        assert!(body.contains("_1 transcript could not be read"));
        assert!(body.contains("remaining 1 transcript not scanned._"));
    }

    #[test]
    fn all_projects_caps_match_the_oracle() {
        assert_eq!(ALL_PROJECTS_PER_FILE_CAP, 4 * 1024 * 1024);
        assert_eq!(ALL_PROJECTS_AGGREGATE_CAP, 100 * 1024 * 1024);
        assert_eq!(ALL_PROJECTS_DEADLINE_MS, 8_000);
        assert_eq!(ALL_PROJECTS_STAT_CAP, 2_000);
        assert_eq!(ALL_PROJECTS_FILE_LIMIT, 50);
    }

    // ── `q1d` ────────────────────────────────────────────────────────────────

    fn unix_env() -> ShellHistoryEnv {
        ShellHistoryEnv {
            windows: false,
            home_dir: "/home/u".to_string(),
            ..ShellHistoryEnv::default()
        }
    }

    #[test]
    fn history_sources_are_platform_specific_and_deduplicated() {
        let sources = history_sources(&unix_env());
        let labels: Vec<&str> = sources.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "~/.zsh_history",
                "~/.bash_history",
                "~/.local/share/powershell/PSReadLine/ConsoleHost_history.txt",
                "~/.local/share/fish/fish_history",
            ]
        );
        // `$HISTFILE` leads, and its format comes from the base name.
        let env = ShellHistoryEnv {
            hist_file: Some("/home/u/.zsh_history".to_string()),
            ..unix_env()
        };
        let sources = history_sources(&env);
        assert_eq!(sources[0].label, "$HISTFILE");
        // ...and the duplicate ~/.zsh_history entry is dropped.
        assert!(!sources.iter().skip(1).any(|s| s.label == "~/.zsh_history"));

        // A relative HISTFILE is ignored.
        let env = ShellHistoryEnv {
            hist_file: Some("relative/hist".to_string()),
            ..unix_env()
        };
        assert!(!history_sources(&env).iter().any(|s| s.label == "$HISTFILE"));

        // Windows swaps zsh/fish for the PSReadLine path.
        let env = ShellHistoryEnv {
            windows: true,
            home_dir: r"C:\Users\u".to_string(),
            app_data: Some(r"C:\Users\u\AppData\Roaming".to_string()),
            ..ShellHistoryEnv::default()
        };
        let labels: Vec<String> = history_sources(&env).into_iter().map(|s| s.label).collect();
        assert!(labels.iter().any(|l| l.contains("PSReadLine")));
        assert!(!labels.iter().any(|l| l.contains("zsh")));
    }

    #[test]
    fn posix_history_drops_timestamps_and_zsh_metadata() {
        let text = ": 1700000000:0;git status\n#1700000001\nls -la\n\ncargo build\n";
        assert_eq!(
            parse_history(text, HistoryFormat::Posix, false),
            vec!["git status", "ls -la", "cargo build"]
        );
    }

    #[test]
    fn a_truncated_tail_skips_the_line_it_opened_mid_way_through() {
        // The tail began inside a continued command; mining that fragment as a
        // command of its own would invent one that was never run.
        let text = "still-part-of-a-previous-command \\\nand-its-tail\nreal-command\n";
        assert_eq!(
            parse_history(text, HistoryFormat::Posix, true),
            vec!["real-command"]
        );
        // Untruncated, the command is kept -- but its CONTINUATION line is
        // still not mined as a command of its own.
        assert_eq!(
            parse_history(text, HistoryFormat::Posix, false),
            vec!["still-part-of-a-previous-command \\", "real-command"]
        );
    }

    #[test]
    fn fish_entries_stop_at_their_first_unescaped_newline_escape() {
        let text = "- cmd: terraform apply\\nSECRET\n  when: 1\n- cmd: kubectl get pods\n";
        let parsed = parse_history(text, HistoryFormat::Fish, false);
        assert_eq!(parsed, vec!["terraform apply", "kubectl get pods"]);
        assert!(!parsed.iter().any(|l| l.contains("SECRET")));
    }

    #[test]
    fn command_words_step_over_sudo_and_assignments() {
        let lines: Vec<String> = [
            "sudo terraform apply",
            "doas kubectl get",
            "env FOO=1 helm upgrade",
            "FOO=1 BAR=2 pulumi up",
            "ls -la",
            "./local-script.sh",
            "9front",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        assert_eq!(
            extract_command_words(&lines, HistoryFormat::Posix),
            vec!["terraform", "kubectl", "helm", "pulumi", "ls"]
        );
        // PowerShell uses its own prefix set.
        let ps: Vec<String> = ["gsudo winget install", "Get-Item x"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(
            extract_command_words(&ps, HistoryFormat::PsReadline),
            vec!["winget"]
        );
    }

    struct FakeHistory {
        env: ShellHistoryEnv,
        network: bool,
        files: std::collections::HashMap<String, Result<Option<(String, bool)>, ()>>,
    }
    impl ShellHistorySource for FakeHistory {
        fn env(&self) -> ShellHistoryEnv {
            self.env.clone()
        }
        fn home_is_network(&self) -> bool {
            self.network
        }
        fn read_tail(&self, source: &HistorySource) -> Result<Option<(String, bool)>, ()> {
            self.files.get(&source.label).cloned().unwrap_or(Ok(None))
        }
    }

    #[test]
    fn a_network_home_is_refused_before_any_file_is_touched() {
        let src = FakeHistory {
            env: unix_env(),
            network: true,
            files: std::collections::HashMap::new(),
        };
        let body = shell_history_section(&src);
        assert!(body.contains("resolves to a network path"));
        assert!(body.contains("Do not read history files yourself"));
    }

    #[test]
    fn shell_history_reports_words_counts_and_which_files_it_read() {
        let mut files = std::collections::HashMap::new();
        files.insert(
            "~/.bash_history".to_string(),
            Ok(Some((
                "terraform apply\nterraform plan\nls\nsudo helm upgrade\n".to_string(),
                false,
            ))),
        );
        let src = FakeHistory {
            env: unix_env(),
            network: false,
            files,
        };
        let body = shell_history_section(&src);
        assert!(body.contains("Status: complete \u{2014} 1 file(s) read: ~/.bash_history"));
        assert!(body.contains("#### Tools run outside Claude (shell history)"));
        assert!(body.contains("- terraform (2\u{d7})"));
        assert!(body.contains("- helm (1\u{d7})"));
        // Standard commands are still reported here (this list is not filtered
        // the way the transcript one is) but raw lines never are.
        assert!(!body.contains("apply"));
        assert!(body.contains("Raw history lines were never read into the transcript"));
    }

    #[test]
    fn an_unreadable_history_file_makes_the_status_partial() {
        // "Partial" is what stops a half-read history passing for a full one.
        let mut files = std::collections::HashMap::new();
        files.insert("~/.bash_history".to_string(), Err(()));
        let src = FakeHistory {
            env: unix_env(),
            network: false,
            files,
        };
        assert!(
            shell_history_section(&src).contains("Status: partial \u{2014} 0 file(s) read: none")
        );

        // A truncated tail is also partial, even though it was read.
        let mut files = std::collections::HashMap::new();
        files.insert(
            "~/.bash_history".to_string(),
            Ok(Some(("git status\n".to_string(), true))),
        );
        let src = FakeHistory {
            env: unix_env(),
            network: false,
            files,
        };
        assert!(shell_history_section(&src).contains("Status: partial"));
    }

    // ── `iay` ────────────────────────────────────────────────────────────────

    struct FakeTranscripts {
        files: Option<Vec<TranscriptFile>>,
        bodies: std::collections::HashMap<String, String>,
    }
    impl ProjectUsageSource for FakeTranscripts {
        fn transcripts(&self) -> Option<Vec<TranscriptFile>> {
            self.files.clone()
        }
        fn read_transcript(&self, path: &std::path::Path) -> Option<String> {
            self.bodies.get(&path.display().to_string()).cloned()
        }
    }

    fn bash_line(cmd: &str) -> String {
        serde_json::json!({
            "message": { "content": [
                { "type": "tool_use", "name": "Bash", "input": { "command": cmd } }
            ]}
        })
        .to_string()
    }
    fn denial_line(reason: &str) -> String {
        serde_json::json!({
            "message": { "content": [
                { "type": "tool_result",
                  "content": format!("denied by the auto mode classifier. Reason: {reason}") }
            ]}
        })
        .to_string()
    }
    fn transcripts(lines: &[String]) -> FakeTranscripts {
        let mut bodies = std::collections::HashMap::new();
        bodies.insert("/t/a.jsonl".to_string(), lines.join("\n"));
        FakeTranscripts {
            files: Some(vec![TranscriptFile {
                path: "/t/a.jsonl".into(),
                size: 10,
            }]),
            bodies,
        }
    }

    #[test]
    fn a_project_with_no_transcripts_says_so() {
        let src = FakeTranscripts {
            files: None,
            bodies: std::collections::HashMap::new(),
        };
        assert_eq!(
            project_usage_section(&src),
            "_no transcript history for this project_"
        );
    }

    #[test]
    fn usage_reports_names_and_counts_never_the_command_lines() {
        // NAMES ONLY is the contract: what escapes is hosts, buckets,
        // namespaces, the leading command word and denial reasons -- counted,
        // never quoted.
        let src = transcripts(&[
            bash_line("terraform apply -auto-approve --token=SECRETVALUE"),
            bash_line("terraform plan"),
            bash_line("curl https://api.acme.io/v1/things"),
            bash_line("aws s3 cp x s3://acme-logs/y"),
            bash_line("kubectl get pods -n prod-web"),
            bash_line("ls -la"),
        ]);
        let body = project_usage_section(&src);

        assert!(body.contains("Transcripts scanned: 1; Bash commands seen: 6"));
        assert!(body.contains("#### Hosts contacted\n- api.acme.io (1\u{d7})"));
        assert!(body.contains("#### Cloud buckets touched\n- acme-logs (1\u{d7})"));
        assert!(body.contains("#### k8s namespaces (-n flags)\n- prod-web (1\u{d7})"));
        // terraform is non-standard and appeared twice; ls/curl/aws/kubectl are
        // standard and are not reported.
        assert!(body.contains("- terraform (2\u{d7})"));
        assert!(!body.contains("- ls ("));
        assert!(!body.contains("- curl ("));
        // The secret that rode along in a command line never reaches the block.
        assert!(!body.contains("SECRETVALUE"));
        assert!(!body.contains("-auto-approve"));
    }

    #[test]
    fn only_the_first_line_of_a_command_is_mined() {
        // A heredoc or multi-line script must not drag its body into the block.
        let src = transcripts(&[bash_line(
            "terraform apply <<EOF\nSECRET_IN_BODY=1\ncurl https://leak.example\nEOF",
        )]);
        let body = project_usage_section(&src);
        assert!(body.contains("- terraform (1\u{d7})"));
        assert!(!body.contains("SECRET_IN_BODY"));
        assert!(!body.contains("leak.example"));
    }

    #[test]
    fn denial_reasons_are_collected_and_capped() {
        let mut lines: Vec<String> = (0..3).map(|_| denial_line("Production Deploy")).collect();
        lines.push(denial_line("Credential Leakage"));
        let src = transcripts(&lines);
        let body = project_usage_section(&src);
        assert!(body.contains("#### Recent auto-mode denial reasons"));
        assert!(body.contains("- Production Deploy (3\u{d7})"));
        assert!(body.contains("- Credential Leakage (1\u{d7})"));
    }

    #[test]
    fn oversized_transcripts_are_skipped_and_counted() {
        let mut bodies = std::collections::HashMap::new();
        bodies.insert("/t/small.jsonl".to_string(), bash_line("terraform apply"));
        let src = FakeTranscripts {
            files: Some(vec![
                TranscriptFile {
                    path: "/t/small.jsonl".into(),
                    size: 10,
                },
                TranscriptFile {
                    path: "/t/huge.jsonl".into(),
                    size: TRANSCRIPT_OVERSIZE_BYTES + 1,
                },
            ]),
            bodies,
        };
        let body = project_usage_section(&src);
        // The skip is REPORTED, so a partial scan cannot pass for a full one.
        assert!(
            body.contains("Transcripts scanned: 1 (1 skipped as oversized); Bash commands seen: 1")
        );
    }

    #[test]
    fn boring_and_github_hosts_are_not_reported() {
        let src = transcripts(&[
            bash_line("curl https://localhost:3000/x"),
            bash_line("curl https://github.com/acme/app"),
            bash_line("curl https://www.github.com/acme/app"),
            bash_line("curl https://cdn.jsdelivr.net/npm/x"),
            bash_line("curl https://api.acme.io/v1"),
        ]);
        let body = project_usage_section(&src);
        assert!(body.contains("- api.acme.io"));
        for hidden in ["localhost", "github.com", "jsdelivr"] {
            assert!(!body.contains(hidden), "{hidden} must not be reported");
        }
    }

    #[test]
    fn standard_clis_are_filtered_including_versioned_python() {
        assert!(is_standard_cli("git"));
        assert!(is_standard_cli("python"));
        assert!(is_standard_cli("python3"));
        assert!(is_standard_cli("python3.12"));
        assert!(is_standard_cli("pip3"));
        assert!(!is_standard_cli("terraform"));
        assert!(!is_standard_cli("pythonic"));
        assert_eq!(STANDARD_CLIS.len(), 94);
    }

    #[test]
    fn frequency_sorts_by_count_then_name() {
        let items: Vec<String> = ["b", "a", "b", "c", "a", "b"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(
            frequency(&items, 10),
            vec![("b".into(), 3), ("a".into(), 2), ("c".into(), 1)]
        );
        assert_eq!(frequency(&items, 2).len(), 2);
    }

    #[test]
    fn usage_caps_match_the_oracle() {
        assert_eq!(TRANSCRIPT_FILE_LIMIT, 50);
        assert_eq!(TRANSCRIPT_OVERSIZE_BYTES, 26_214_400);
        assert_eq!(DENIAL_REASON_LIMIT, 10);
        assert_eq!(first_line("a\nb"), "a");
        assert_eq!(first_line("a"), "a");
        assert!(is_github_host("www.www.github.com"));
        assert!(!is_github_host("notgithub.com"));
    }

    // ── `gay` ────────────────────────────────────────────────────────────────

    #[test]
    fn registry_hosts_drop_credentials_and_ambiguous_shapes() {
        assert_eq!(
            registry_host("https://npm.acme.io/x"),
            Some("npm.acme.io".into())
        );
        assert_eq!(
            registry_host("https://npm.acme.io"),
            Some("npm.acme.io".into())
        );
        // Credentialed but unambiguous: the host survives, the credentials do not.
        assert_eq!(
            registry_host("https://user:tok@npm.acme.io/x"),
            Some("npm.acme.io".into())
        );
        // A second `@` after the authority makes the host ambiguous -> dropped.
        assert_eq!(registry_host("https://user:tok@npm.acme.io/a@b"), None);
        // Credentialed with an undotted host -> dropped.
        assert_eq!(registry_host("https://user:tok@localhost/x"), None);
        assert_eq!(registry_host("::::"), None);
    }

    #[test]
    fn well_known_public_registries_are_not_reported() {
        // Naming docker.io says nothing about the user's infrastructure.
        for h in ["docker.io", "ghcr.io", "pypi.org", "localhost", "127.0.0.1"] {
            assert!(PUBLIC_REGISTRIES.contains(&h), "{h}");
        }
        assert_eq!(PUBLIC_REGISTRIES.len(), 13);
    }

    #[test]
    fn bucket_names_are_extracted_only_at_a_token_boundary() {
        assert_eq!(
            extract_bucket_names("aws s3://acme-logs/x and gs://acme-data"),
            vec!["acme-logs", "acme-data"]
        );
        // Preceded by a token character -> not a bucket reference.
        assert!(extract_bucket_names("xs3://nope").is_empty());
        assert!(extract_bucket_names("v1.s3://nope").is_empty());
        // A separator is fine.
        assert_eq!(extract_bucket_names("(s3://ok)"), vec!["ok"]);
    }

    #[test]
    fn bucket_clusters_need_several_names_to_count() {
        let names: Vec<String> = [
            "acme-logs",
            "acme-data",
            "acme-backup",
            "other-x",
            "other-y",
            "nodash",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        // `other-` has only 2 -> below the floor; `acme-` has 3 -> reported.
        assert_eq!(
            bucket_prefix_clusters(names.iter()),
            vec![("acme".to_string(), 3)]
        );
    }

    #[test]
    fn a_failed_bucket_scan_is_unavailable_not_absent() {
        let rendered = render_bucket_section(None);
        assert!(rendered.contains("treat bucket evidence as unavailable, not absent"));
        // A scan that completed and found nothing renders EMPTY -- that is
        // real evidence, unlike a failure.
        let empty = BucketScan {
            top: Vec::new(),
            distinct: 0,
            clusters: Vec::new(),
            truncated: false,
        };
        assert_eq!(render_bucket_section(Some(&empty)), "");
        // ...but a truncated empty scan is unavailable again.
        let cut = BucketScan {
            truncated: true,
            ..empty
        };
        assert!(render_bucket_section(Some(&cut))
            .contains("treat bucket evidence as unavailable, not absent"));
    }

    #[test]
    fn bucket_counts_render_with_their_spread() {
        let scan = BucketScan {
            top: vec![
                (
                    "acme-logs".to_string(),
                    BucketCount {
                        occurrences: 30,
                        files: 1,
                    },
                ),
                (
                    "acme-data".to_string(),
                    BucketCount {
                        occurrences: 4,
                        files: 4,
                    },
                ),
            ],
            distinct: 9,
            clusters: vec![("acme".to_string(), 5)],
            truncated: true,
        };
        let out = render_bucket_section(Some(&scan));
        // The spread is what the propose prompt weighs, so both numbers show.
        assert!(out.contains("- acme-logs (30\u{d7}, 1 file)"));
        assert!(out.contains("- acme-data (4\u{d7}, 4 files)"));
        assert!(out.contains("_9 distinct bucket names in total; top 2 shown._"));
        assert!(out.contains("counts are a lower bound"));
        assert!(out.contains("- acme-* (5 distinct names)"));
    }

    #[derive(Default)]
    struct FakeScans {
        files: std::collections::HashMap<String, Vec<String>>,
        paths: std::collections::HashMap<String, Vec<String>>,
        package_json: Option<String>,
        buckets: Option<BucketScan>,
    }
    impl ConfigScanSource for FakeScans {
        fn scan_files(&self, globs: &[&str], _f: Option<&regex::Regex>) -> Vec<String> {
            self.files.get(globs[0]).cloned().unwrap_or_default()
        }
        fn list_paths(
            &self,
            globs: &[&str],
            _l: usize,
            _d: usize,
            _f: Option<&regex::Regex>,
        ) -> Vec<String> {
            self.paths.get(globs[0]).cloned().unwrap_or_default()
        }
        fn package_json(&self) -> Option<String> {
            self.package_json.clone()
        }
        fn bucket_scan(&self) -> Option<BucketScan> {
            self.buckets.clone()
        }
    }

    #[test]
    fn config_scans_render_only_the_sections_with_content() {
        let mut s = FakeScans::default();
        s.files.insert(
            ".npmrc".to_string(),
            vec!["registry=https://npm.acme.io/\nregistry=https://registry.npmjs.org/".to_string()],
        );
        s.files.insert(
            "Makefile".to_string(),
            vec!["build:\n\tcargo build\ndeploy:\n".to_string()],
        );
        s.package_json = Some(r#"{"scripts":{"test":"x","build":"y"}}"#.to_string());

        let body = config_scans_section(&s);
        assert!(body.contains("#### Package registry hosts\n- npm.acme.io"));
        // The public registry was filtered out.
        assert!(!body.contains("registry.npmjs.org"));
        assert!(body.contains("#### Makefile/justfile targets"));
        assert!(body.contains("- build"));
        assert!(body.contains("- deploy"));
        assert!(body.contains("#### package.json scripts"));
        // Sections with nothing to say are omitted entirely.
        assert!(!body.contains("#### Container image registries"));
        assert!(!body.contains("#### Secrets-manager markers"));
        assert!(!body.contains("#### Sensitive-looking paths"));
    }

    #[test]
    fn ci_secret_names_are_collected_without_their_values() {
        let mut s = FakeScans::default();
        s.files.insert(
            "*.yml".to_string(),
            vec!["run: deploy\n  env:\n    K: ${{ secrets.DEPLOY_KEY }}\n    T: ${{ secrets.NPM_TOKEN }}".to_string()],
        );
        let body = config_scans_section(&s);
        assert!(body.contains("names only \u{2014} a deploy key exists, not its value"));
        assert!(body.contains("- DEPLOY_KEY"));
        assert!(body.contains("- NPM_TOKEN"));
    }

    #[test]
    fn sensitive_directory_examples_are_capped_at_two_per_kind() {
        // Otherwise a large `prod/` tree would crowd out every other signal.
        let mut s = FakeScans::default();
        s.paths.insert(
            SENSITIVE_DIR_GLOBS[0].to_string(),
            vec![
                "prod/a.yml".to_string(),
                "prod/b.yml".to_string(),
                "prod/c.yml".to_string(),
                "iam/x.tf".to_string(),
            ],
        );
        let body = config_scans_section(&s);
        assert!(body.contains("- prod/a.yml"));
        assert!(body.contains("- prod/b.yml"));
        assert!(
            !body.contains("prod/c.yml"),
            "third example must be dropped"
        );
        assert!(body.contains("- iam/x.tf"));
    }

    #[test]
    fn config_scan_caps_match_the_oracle() {
        assert_eq!(CONFIG_SCAN_READ_CAP, 64_000);
        assert_eq!(CONFIG_SCAN_FILE_LIMIT, 40);
        assert_eq!(PACKAGE_JSON_READ_CAP, 256_000);
        assert_eq!(SENSITIVE_PATH_LIMIT, 72);
        assert_eq!(BUCKET_SCAN_TIMEOUT_MS, 8_000);
        assert_eq!(BUCKET_SCAN_DISTINCT_CAP, 20_000);
        assert_eq!(BUCKET_SCAN_MAX_FILESIZE, "4M");
        assert_eq!(BUCKET_CLUSTER_MIN, 3);
        assert_eq!(BUCKET_CLUSTER_LIMIT, 10);
        assert_eq!(SENSITIVE_PATH_GLOBS.len(), 23);
        assert_eq!(SENSITIVE_DIR_GLOBS.len(), 6);
    }

    // ── `Zsy` / `Qsy` ────────────────────────────────────────────────────────

    struct FakeSettings {
        user: Result<Option<String>, ()>,
        local: String,
        classify_all_shell: bool,
    }
    impl SettingsReconSource for FakeSettings {
        fn user_settings(&self) -> Result<Option<String>, ()> {
            self.user.clone()
        }
        fn local_block(&self) -> String {
            self.local.clone()
        }
        fn classify_all_shell(&self) -> bool {
            self.classify_all_shell
        }
    }
    fn settings(json: serde_json::Value) -> FakeSettings {
        FakeSettings {
            user: Ok(Some(json.to_string())),
            local: String::new(),
            classify_all_shell: false,
        }
    }

    #[test]
    fn no_settings_file_renders_the_placeholder_and_both_empty_lists() {
        let body = existing_settings_section(&FakeSettings {
            user: Ok(None),
            local: String::new(),
            classify_all_shell: false,
        })
        .unwrap();
        assert!(body.contains("(no settings file)"));
        assert!(
            body.contains("No classifier-bypassing entries in user-settings permissions.allow.")
        );
        assert!(body.contains("No destructive entries in user-settings permissions.allow."));
    }

    #[test]
    fn an_unreadable_settings_file_fails_the_section() {
        let err = existing_settings_section(&FakeSettings {
            user: Err(()),
            local: String::new(),
            classify_all_shell: false,
        });
        assert!(
            err.is_err(),
            "must surface as a failed section, not as empty"
        );
    }

    #[test]
    fn the_two_lists_are_disjoint_and_correctly_sorted() {
        let body = existing_settings_section(&settings(serde_json::json!({
            "permissions": { "allow": [
                "Bash(*)",            // classifier-bypassing
                "Bash(rm -rf *)",     // destructive, not bypassing
                "Read(src/**)",       // neither
            ]}
        })))
        .unwrap();
        // Bypassing list holds only the tool-wide grant...
        let bypass_idx = body
            .find("classifier-bypassing, in your user settings")
            .unwrap();
        let dest_idx = body.find("Destructive permissions.allow entries").unwrap();
        let bypass_section = &body[bypass_idx..dest_idx];
        assert!(bypass_section.contains("- `Bash(*)`"));
        assert!(!bypass_section.contains("rm -rf"));
        // ...and the destructive list only the narrow-but-destructive one.
        let dest_section = &body[dest_idx..];
        assert!(dest_section.contains("- `Bash(rm -rf *)`"));
        assert!(!dest_section.contains("- `Bash(*)`"));
        // The innocuous rule appears in neither.
        assert!(!body.contains("Read(src/**)"));
    }

    #[test]
    fn a_flagged_rule_that_cannot_be_rendered_is_counted_not_dropped() {
        // Silently dropping it would hide a dangerous rule from the review.
        let body = existing_settings_section(&settings(serde_json::json!({
            // Destructive (rm + wildcard) AND unrenderable (backtick).
            "permissions": { "allow": ["Bash(rm -rf `evil`/*)", "Bash(rm -rf ok/*)"] }
        })))
        .unwrap();
        assert!(body.contains("1 additional flagged entry"));
        assert!(body.contains("the user should review permissions.allow by hand"));
        // The unrenderable rule's text never reaches the block.
        assert!(!body.contains("evil"));
        // ...while the renderable one is still listed.
        assert!(body.contains("- `Bash(rm -rf ok/*)`"));
    }

    #[test]
    fn the_flagged_lists_are_capped_with_an_overflow_line() {
        let rules: Vec<String> = (0..FLAGGED_LIST_CAP + 3)
            .map(|i| format!("Bash(rm -rf a{i}/*)"))
            .collect();
        let body = existing_settings_section(&settings(serde_json::json!({
            "permissions": { "allow": rules }
        })))
        .unwrap();
        assert_eq!(body.matches("- `Bash(rm -rf ").count(), FLAGGED_LIST_CAP);
        assert!(body.contains("- \u{2026}and 3 more flagged entries not shown (list capped)"));
    }

    #[test]
    fn the_classify_all_shell_note_is_attached_to_the_first_list_only() {
        let body = existing_settings_section(&FakeSettings {
            user: Ok(Some(
                serde_json::json!({"permissions":{"allow":["Bash(rm -rf *)"]}}).to_string(),
            )),
            local: String::new(),
            classify_all_shell: true,
        })
        .unwrap();
        assert_eq!(body.matches("classifyAllShell is active").count(), 1);
        // It sits with the classifier-bypassing list, before the destructive one.
        let note = body.find("classifyAllShell is active").unwrap();
        let dest = body.find("Destructive permissions.allow entries").unwrap();
        assert!(note < dest);
    }

    #[test]
    fn the_rendered_automode_block_keeps_only_the_five_keys() {
        let rendered = render_auto_mode_json(&serde_json::json!({
            "environment": ["a"],
            "allow": ["$defaults"],
            "soft_deny": null,
            "hard_deny": false,
            "deny": ["x"],
            "classifyAllShell": true,
            "somethingElse": 1,
        }));
        assert!(rendered.contains("\"environment\""));
        assert!(rendered.contains("\"allow\""));
        assert!(rendered.contains("\"deny\""));
        // null and false are dropped, as are keys outside the five.
        assert!(!rendered.contains("soft_deny"));
        assert!(!rendered.contains("hard_deny"));
        assert!(!rendered.contains("classifyAllShell"));
        assert!(!rendered.contains("somethingElse"));
        // One-space indent.
        assert!(rendered.contains("\n \"environment\""));
        assert_eq!(render_auto_mode_json(&serde_json::json!({})), "{}");
    }

    struct FakeLocal {
        dir: Option<bool>,
        file: Option<(bool, u64, u64)>,
        content: Option<String>,
        tracked: bool,
    }
    impl LocalSettingsSource for FakeLocal {
        fn lingxi_dir(&self) -> Option<bool> {
            self.dir
        }
        fn local_file(&self) -> Option<(bool, u64, u64)> {
            self.file
        }
        fn read_local(&self) -> Option<String> {
            self.content.clone()
        }
        fn tracked_in_git(&self) -> bool {
            self.tracked
        }
    }

    #[test]
    fn the_indirection_gate_refuses_rather_than_resolving() {
        // `.claude` is not a real directory (e.g. a committed symlink): the
        // recon reports it and does NOT look behind it.
        let (body, code) = local_settings_block(&FakeLocal {
            dir: Some(false),
            file: None,
            content: None,
            tracked: false,
        });
        assert_eq!(code, Some("local_settings_indirection_gate"));
        assert!(body.contains("deliberately not probed"));
        assert!(body.contains("do not read, resolve, or rewrite anything under this path"));

        // The file itself is a symlink / hardlink.
        let (body, code) = local_settings_block(&FakeLocal {
            dir: Some(true),
            file: Some((false, 1, 10)),
            content: None,
            tracked: false,
        });
        assert_eq!(code, Some("local_settings_indirection_gate"));
        assert!(body.contains("Present but SKIPPED"));

        let (_, code) = local_settings_block(&FakeLocal {
            dir: Some(true),
            file: Some((true, 2, 10)),
            content: None,
            tracked: false,
        });
        assert_eq!(code, Some("local_settings_indirection_gate"), "nlink != 1");
    }

    #[test]
    fn an_absent_local_settings_file_renders_nothing() {
        for probe in [
            FakeLocal {
                dir: None,
                file: None,
                content: None,
                tracked: false,
            },
            FakeLocal {
                dir: Some(true),
                file: None,
                content: None,
                tracked: false,
            },
        ] {
            assert_eq!(local_settings_block(&probe), (String::new(), None));
        }
    }

    #[test]
    fn local_automode_keys_are_reported_with_their_git_provenance() {
        let content = serde_json::json!({"autoMode": {"allow": ["Bash(x:*)"]}}).to_string();
        let (body, code) = local_settings_block(&FakeLocal {
            dir: Some(true),
            file: Some((true, 1, 100)),
            content: Some(content.clone()),
            tracked: true,
        });
        assert_eq!(code, None);
        assert!(body.contains("NOT pre-approved config"));
        assert!(body.contains("Tracked in git: yes \u{2014} repo-authored"));

        let (body, _) = local_settings_block(&FakeLocal {
            dir: Some(true),
            file: Some((true, 1, 100)),
            content: Some(content),
            tracked: false,
        });
        // Untracked does not license the inverse inference.
        assert!(body.contains("no \u{2014} but untracked does not prove user-authored"));
    }

    #[test]
    fn an_unparseable_or_oversized_local_file_is_skipped_with_a_code() {
        let (body, code) = local_settings_block(&FakeLocal {
            dir: Some(true),
            file: Some((true, 1, 100)),
            content: Some("{not json".to_string()),
            tracked: false,
        });
        assert_eq!(code, Some("local_settings_invalid_json"));
        assert!(body.contains("Present but not valid JSON"));

        let (_, code) = local_settings_block(&FakeLocal {
            dir: Some(true),
            file: Some((true, 1, SETTINGS_READ_CAP as u64 + 1)),
            content: None,
            tracked: false,
        });
        assert_eq!(code, Some("local_settings_oversized"));

        let (_, code) = local_settings_block(&FakeLocal {
            dir: Some(true),
            file: Some((true, 1, 100)),
            content: None,
            tracked: false,
        });
        assert_eq!(code, Some("local_settings_unreadable"));
    }

    #[test]
    fn producer_caps_match_the_oracle() {
        assert_eq!(DOC_READ_CAP_LINGXI_MD, 200_000);
        assert_eq!(DOC_READ_CAP, 10_000);
        assert_eq!(FLAGGED_LIST_CAP, 20);
        assert_eq!(README_HEAD_LINES, 40);
        assert_eq!(DOC_GLOB_MAX_DEPTH, 4);
        assert_eq!(DOC_GLOB_LIMIT, 10);
        assert_eq!(
            read_truncated_marker(10_000),
            "\n\u{2026}[truncated at 10000 bytes]"
        );
    }

    #[test]
    fn shipped_default_slots_match_the_oracle_counts() {
        assert_eq!(crate::auto_mode_defaults::DEFAULT_ENVIRONMENT.len(), 20);
        assert_eq!(DEFAULT_ALLOW_LABELS.len(), 17);
        assert_eq!(DEFAULT_SOFT_DENY_LABELS.len(), 68);
        assert_eq!(crate::auto_mode_defaults::DEFAULT_HARD_DENY_LABELS.len(), 1);
        assert_eq!(
            crate::auto_mode_defaults::DEFAULT_HARD_DENY_LABELS[0],
            "Data Exfiltration"
        );
    }

    #[test]
    fn rule_label_cuts_at_the_first_colon_or_bracket() {
        assert_eq!(
            default_rule_label("Read-Only Operations: GET requests"),
            "Read-Only Operations"
        );
        assert_eq!(
            default_rule_label("Git Destructive [named+specifics]: force push"),
            "Git Destructive"
        );
        assert_eq!(default_rule_label("Bare Label"), "Bare Label");
        // Already-reduced labels are unchanged, so applying it twice is safe.
        for label in DEFAULT_ALLOW_LABELS {
            assert_eq!(default_rule_label(label), label);
        }
    }

    #[test]
    fn the_environment_slot_is_verbatim() {
        let env = crate::auto_mode_defaults::DEFAULT_ENVIRONMENT;
        assert_eq!(env[0], "**Organization**: None configured");
        // The `—` escapes in the bundle decoded to real em-dashes.
        assert!(env[4].contains('\u{2014}'));
        // NOTE the classifier template uses STRAIGHT apostrophes, unlike the
        // UI messages elsewhere in this subsystem which use U+2019.
        assert!(env[11].contains("repo's public/private visibility"));
        assert!(!env[11].contains('\u{2019}'));
        // Every entry is a bolded label bullet.
        for e in env {
            assert!(e.starts_with("**"), "{e:?}");
        }
    }
}

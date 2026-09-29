//! Rate-limit resume checkpoint — the `RESUME.md` document, its vocabulary,
//! and the git executor that publishes it.
//!
//! SC-02. New in 2.1.238 (`performRateLimitCheckpoint` has 6 hits there and
//! **0** in 2.1.220): when a session hits — or nears — its usage limit, the
//! oracle snapshots the working tree into a detached WIP commit under a private
//! ref, writes a human-readable `RESUME.md` explaining how to pick the session
//! back up, and hands the user the ref that holds their in-progress files.
//!
//! Oracle module `eal` (cc-238.js @292197753) exports exactly:
//!
//! ```text
//! CHECKPOINT_REF_PREFIX, MAX_CHECKPOINT_FILE_COUNT, MAX_CHECKPOINT_TOTAL_BYTES,
//! RESUME_MD_REPO_PATH, clearLastCheckpointResult, getLastCheckpointResult,
//! performRateLimitCheckpoint, useRateLimitCheckpointResult
//! ```
//!
//! # What is ported here
//!
//! All of it, as of SC-02's close:
//!
//! * the byte-exact `RESUME.md` renderer [`render_resume_md`] (`K4v`
//!   @292206132) and its todo-line sanitiser [`sanitize_todo_line`] (`Zsl`
//!   @292197365);
//! * the four module constants, the trigger vocabulary and the complete
//!   14-value skip-reason taxonomy `getLastCheckpointResult` persists;
//! * the git executor [`run_checkpoint`] (`q4v` @292197916) with its whole
//!   safety envelope;
//! * the once-per-session latch and telemetry wrapper
//!   [`perform_rate_limit_checkpoint`] (`z4v` @292197753), plus
//!   [`last_checkpoint_result`] / [`clear_last_checkpoint_result`].
//!
//! The executor's envelope, step for step (every one of these is a `skipReason`
//! rather than an error, because each one means "this repository is not one we
//! may write into"):
//!
//! ```text
//! gate  Vs("allow_local_checkpoint_commit"); skip when non-interactive / remote workspace
//! stat  git root and git dir must both be canonically contained (`Qsl`), the
//!       git dir must have no `commondir`, and none of objects/refs/refs/<ns>/
//!       logs/logs/refs/logs/refs/<ns>/packed-refs/reftable — nor any entry of
//!       objects/ — may be a symlink
//! env   GIT_COMMON_DIR, GIT_WORK_TREE, GIT_ALLOW_PROTOCOL=none, GIT_NO_LAZY_FETCH=1,
//!       GIT_NO_REPLACE_OBJECTS=1, GIT_TERMINAL_PROMPT=0, and a PRIVATE
//!       GIT_INDEX_FILE (`<gitdir>/lingxi-checkpoint-index.<pid>`) so the user's
//!       own index is never touched
//! skip  sparse checkout (core.sparseCheckout=true), LFS / `filter=lfs` in
//!       .gitattributes, an in-progress sequencer (MERGE_HEAD, CHERRY_PICK_HEAD,
//!       REVERT_HEAD, BISECT_LOG, rebase-merge, rebase-apply), no HEAD
//! build read-tree HEAD -> ls-files -z --cached + ls-files -z -o --exclude-standard
//!       -> per-path lstat (paths containing \n, \r, `"`, `\` or a `.`/`..`
//!       segment are dropped; non-files dropped; uncontained realpaths dropped;
//!       mode 100755 when the owner-execute bit is set, else 100644) under the
//!       25 000-path and 2 GiB caps -> hash-object -w --no-filters --stdin-paths
//!       -> update-index --add --index-info (+ --force-remove for vanished cached
//!       paths) -> write-tree -> commit-tree -p HEAD
//! ref   update-ref --no-deref under core.logAllRefUpdates=false, then append
//!       `/<RESUME_MD_REPO_PATH>` to info/exclude and GC sibling checkpoint refs
//!       older than STALE_CHECKPOINT_REF_AGE
//! ```
//!
//! # Divergences, all of them
//!
//! * **Rebrand.** `refs/claude/checkpoint-` -> [`CHECKPOINT_REF_PREFIX`],
//!   `.claude/RESUME.md` -> [`RESUME_MD_REPO_PATH`], the commit author is
//!   `LingXi <noreply@lingxi.local>`, the commit subject is
//!   `WIP: LingXi rate-limit checkpoint (<sid8>)`, and the private index file is
//!   `lingxi-checkpoint-index.<pid>`. Every one of these names a LingXi
//!   artefact, so keeping the upstream spelling would have this feature write
//!   `refs/claude/*` into the user's repository.
//! * **`Vs("allow_local_checkpoint_commit")`** is a MANAGED-POLICY permission
//!   check upstream (`Vs` @283688009 returns `true` unless enterprise policy
//!   names the feature) — so upstream commits into the user's repository by
//!   default. The port has no feature-permission registry, so the gate is an
//!   explicit [`CheckpointGates::policy_allows`] input;
//!   [`CheckpointGates::default`] keeps upstream's polarity, while the live
//!   call site reads [`local_checkpoint_commit_allowed`]. See
//!   [`ALLOW_LOCAL_CHECKPOINT_COMMIT_ENV`] — that default is the one open
//!   PRODUCT decision here, and it is the only thing standing between this
//!   module and upstream behaviour.
//! * **No per-subprocess timeout.** [`GIT_TIMEOUT_MS`] is transcribed for the
//!   day the port grows a process runner with one; `std::process` has none.
//!   Every invocation is made local-only and non-interactive by
//!   `GIT_ALLOW_PROTOCOL=none` + `GIT_TERMINAL_PROMPT=0`, and the whole call is
//!   fire-and-forget on a detached thread, so a hung `git` costs one thread and
//!   a private index file — never the user's own index.
//! * **Telemetry.** `usage_limit_checkpoint_commit` /
//!   `tengu_rl_checkpoint_a1_shown` are not emitted; the `session` crate has no
//!   `telemetry` dependency. Both spellings live in [`CheckpointSkipReason`] /
//!   [`CheckpointTrigger`] so wiring an emitter later needs no new vocabulary.

use lingxi_core::session::{TodoItem, TodoState};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;

/// `oDi = "refs/claude/checkpoint-"` — the ref namespace a checkpoint commit is
/// parked under, one ref per session (`<prefix><first 8 chars of session id>`).
///
/// Rebranded to the `refs/lingxi/` namespace. Distinct from `local-apps`'
/// `refs/lingxi/checkpoints` (plural, a directory of refs) — the two cannot
/// collide.
pub const CHECKPOINT_REF_PREFIX: &str = "refs/lingxi/checkpoint-";

/// `nDi = ".claude/RESUME.md"` — repo-relative path of the resume document,
/// rebranded onto [`branding::DOT_DIR`]. It is written to the working tree AND
/// hashed into the checkpoint tree at this exact path, so the two must agree.
pub const RESUME_MD_REPO_PATH: &str = ".lingxi/RESUME.md";

/// `v6f = 25000` — combined cap on cached + untracked paths. Above it the
/// checkpoint is skipped as [`CheckpointSkipReason::TooLarge`].
pub const MAX_CHECKPOINT_FILE_COUNT: usize = 25_000;

/// `T6f = 2147483648` — 2 GiB cap on the total content hashed into one
/// checkpoint. Above it the checkpoint is skipped as
/// [`CheckpointSkipReason::TooLarge`].
pub const MAX_CHECKPOINT_TOTAL_BYTES: u64 = 2_147_483_648;

/// `xxe = 30000` — timeout, in milliseconds, for each git subprocess
/// (`hash-object --stdin-paths` gets `4 * xxe`).
pub const GIT_TIMEOUT_MS: u64 = 30_000;

/// `j4v = 1209600` — 14 days, in seconds. A sibling checkpoint ref whose commit
/// is older than this is deleted after a successful checkpoint.
pub const STALE_CHECKPOINT_REF_AGE_SECS: u64 = 1_209_600;

/// `b6f = 500` — todo lines are sanitised, then truncated to this many UTF-16
/// code units plus `…`.
const TODO_LINE_MAX_UTF16: usize = 500;

/// Number of leading session-id characters that name the ref (`o.slice(0,8)`).
const REF_SESSION_ID_PREFIX_LEN: usize = 8;

/// Canonical key shared by every checkpoint trigger for a protocol session.
///
/// `SessionId` displays as `sess:<uuid>`, while the persisted session id and
/// resume command use the bare UUID. Centralising the conversion prevents the
/// near-limit and hard-429 paths from occupying different latch entries.
#[must_use]
pub fn checkpoint_session_key(session_id: lingxi_core::types::SessionId) -> String {
    session_id.as_uuid().to_string()
}

/// Why the checkpoint ran.
///
/// The wire spellings (`near_limit` / `rate_limited`) go to telemetry and to the
/// persisted result; the rendered spellings (`near-limit` / `rate-limited`) go
/// into the `Trigger:` line of the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointTrigger {
    /// `"near_limit"` — the session is approaching its usage limit.
    NearLimit,
    /// `"rate_limited"` — the request was already refused.
    RateLimited,
}

impl CheckpointTrigger {
    /// The wire spelling (telemetry `trigger` property, persisted result).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NearLimit => "near_limit",
            Self::RateLimited => "rate_limited",
        }
    }

    /// The rendered spelling for the `Trigger:` line —
    /// `e.trigger==="near_limit"?"near-limit":"rate-limited"`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::NearLimit => "near-limit",
            Self::RateLimited => "rate-limited",
        }
    }
}

/// Every reason `q4v` can decline to write a checkpoint, in the order the
/// oracle tests them. All 14 are persisted through `getLastCheckpointResult`
/// and reported to `usage_limit_checkpoint_commit`, so the spellings are wire
/// values, not prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointSkipReason {
    /// `Dn()` — running non-interactively (print mode, SDK, hooks).
    NonInteractive,
    /// `Ca()` — the workspace is remote, so a local commit means nothing.
    RemoteWorkspace,
    /// `!Vs("allow_local_checkpoint_commit")` — gate off (policy / config).
    Policy,
    /// The cwd is not inside a git work tree, or the git dir cannot be resolved.
    NotGit,
    /// `VXt()!==!1` — a bare repository has no working tree to snapshot.
    BareRepo,
    /// The work-tree root does not canonicalise, or it *contains* `$HOME`
    /// (`Qsl(home, root)`) — snapshotting a repo rooted at or above the home
    /// directory is refused.
    GitRootUncontained,
    /// The git dir does not canonicalise, is not contained in the work tree,
    /// has a `commondir` (a linked worktree), or one of its well-known
    /// entries — including any entry of `objects/` — is a symlink.
    GitDirUncontained,
    /// `core.sparseCheckout=true`: the working tree is not the whole tree.
    SparseCheckout,
    /// A `lfs` git-dir directory, or `filter=lfs` in `.gitattributes` — content
    /// filters would rewrite what is hashed.
    ContentFilters,
    /// `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `BISECT_LOG`,
    /// `rebase-merge` or `rebase-apply` is present.
    SequencerInProgress,
    /// `rev-parse --verify HEAD` failed — an unborn branch has no parent to
    /// commit against.
    NoHead,
    /// Above [`MAX_CHECKPOINT_FILE_COUNT`] paths or
    /// [`MAX_CHECKPOINT_TOTAL_BYTES`] of content.
    TooLarge,
    /// Writing `RESUME.md` was refused (symlink/parent-dir guard, or an I/O
    /// error).
    ResumeWriteRefused,
    /// Any git subprocess exited non-zero, timed out, or produced output the
    /// pipeline could not use (e.g. `hash-object --stdin-paths` returning a
    /// different number of hashes than paths given).
    GitError,
}

impl CheckpointSkipReason {
    /// The wire spelling persisted into the checkpoint result and reported to
    /// `usage_limit_checkpoint_commit`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NonInteractive => "non_interactive",
            Self::RemoteWorkspace => "remote_workspace",
            Self::Policy => "policy",
            Self::NotGit => "not_git",
            Self::BareRepo => "bare_repo",
            Self::GitRootUncontained => "gitroot_uncontained",
            Self::GitDirUncontained => "gitdir_uncontained",
            Self::SparseCheckout => "sparse_checkout",
            Self::ContentFilters => "content_filters",
            Self::SequencerInProgress => "sequencer_in_progress",
            Self::NoHead => "no_head",
            Self::TooLarge => "too_large",
            Self::ResumeWriteRefused => "resume_write_refused",
            Self::GitError => "git_error",
        }
    }
}

/// The outcome `performRateLimitCheckpoint` resolves to and latches:
/// `{committed:!0,ref,resumePath,commitSha}` or
/// `{committed:!1,skipReason}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointResult {
    /// A checkpoint commit was written.
    Committed {
        /// The ref now pointing at it (`<CHECKPOINT_REF_PREFIX><sid8>`).
        ref_name: String,
        /// Repo-relative path of the written document
        /// ([`RESUME_MD_REPO_PATH`]).
        resume_path: &'static str,
        /// The commit the ref points at.
        commit_sha: String,
    },
    /// Nothing was written, for this reason.
    Skipped(CheckpointSkipReason),
}

/// The ref for `session_id` — `` `${oDi}${o.slice(0,8)}` ``.
///
/// Sliced by `char`, not by byte, so a non-ASCII id can never split a code
/// point; real session ids are UUIDs, where the two agree.
#[must_use]
pub fn checkpoint_ref(session_id: &str) -> String {
    let short: String = session_id.chars().take(REF_SESSION_ID_PREFIX_LEN).collect();
    format!("{CHECKPOINT_REF_PREFIX}{short}")
}

/// The `RESUME.md` inputs (`K4v`'s single argument).
#[derive(Debug, Clone, Copy)]
pub struct ResumeDoc<'a> {
    /// The session to resume.
    pub session_id: &'a str,
    /// The ref holding the snapshot ([`checkpoint_ref`]).
    pub ref_name: &'a str,
    /// Why the checkpoint ran.
    pub trigger: CheckpointTrigger,
    /// `TodoWrite` state at checkpoint time, in list order.
    pub todos: &'a [TodoItem],
    /// `new Date().toISOString()` for the `Written:` line — injected so the
    /// renderer stays pure. [`now_iso`] produces the oracle's format.
    pub written_iso: &'a str,
}

/// `new Date().toISOString()` — `2026-08-20T15:08:27.000Z`.
#[must_use]
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Control / bidi / zero-width characters `Zsl` collapses to a single space.
///
/// The oracle class, written out in escapes:
/// `[\x00-\x1f\x7f-\x9f\u061c\u200b-\u200f\u2028-\u202e\u2066-\u2069\ufeff]+`
/// — note the `+`: a RUN collapses to ONE space, not one space per character.
static TODO_LINE_UNSAFE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"[\x00-\x1F\x7F-\x9F\x{061C}\x{200B}-\x{200F}\x{2028}-\x{202E}\x{2066}-\x{2069}\x{FEFF}]+",
    )
    .expect("static todo-line sanitiser regex")
});

/// `Zsl(e)` (@292197365) — make one todo safe to paste into a markdown list.
///
/// ```js
/// function Zsl(e){ let t=e.replace(/…/g," ");
///   return t.length>b6f?`${wo(t,b6f)}…`:t }
/// ```
///
/// Newlines are part of the class, so a multi-line todo becomes one line and
/// can never forge a second list item; the length test is on the ALREADY
/// sanitised text, in UTF-16 code units.
#[must_use]
pub fn sanitize_todo_line(text: &str) -> String {
    let collapsed = TODO_LINE_UNSAFE.replace_all(text, " ").into_owned();
    // `wo(t,b6f)` slices to 500 UTF-16 units (dropping a trailing lone high
    // surrogate) and does NOT trim; only then is the ellipsis appended.
    // Bound in its own statement so the borrow ends before the `None` arm
    // moves `collapsed` out.
    let truncated = truncate_utf16_units(&collapsed, TODO_LINE_MAX_UTF16);
    match truncated {
        Some(head) => format!("{head}\u{2026}"),
        None => collapsed,
    }
}

/// `wo(e,t)` (@281366731) — the head of `text`, at most `max_units` UTF-16 code
/// units, or `None` when `text` already fits (`if(e.length<=t)return e`).
///
/// ```js
/// function wo(e,t){ if(t<=0)return""; if(e.length<=t)return e;
///   let r=e.slice(0,t),n=r.charCodeAt(t-1);
///   return cTu(n>=55296&&n<=56319?r.slice(0,-1):r) }
/// ```
///
/// The surrogate test is why this cannot be a `char` count: JS slices UTF-16
/// units, so a cut that lands between the halves of a non-BMP character drops
/// the orphaned high surrogate rather than emitting a lone one. (`cTu` is a
/// UTF-16 round-trip, a no-op on the well-formed result — and unrepresentable
/// in Rust, where a `String` cannot hold a lone surrogate anyway.)
fn truncate_utf16_units(text: &str, max_units: usize) -> Option<String> {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max_units {
        return None;
    }
    let mut head = &units[..max_units];
    if let Some(&last) = head.last() {
        if (0xD800..=0xDBFF).contains(&last) {
            head = &head[..head.len() - 1];
        }
    }
    Some(String::from_utf16_lossy(head))
}

/// Render `RESUME.md` — `K4v` (@292206132), line for line.
///
/// ```js
/// let t=["# Claude Code — resume checkpoint","",`Session: ${e.sessionId}`,
///        `Written: ${new Date().toISOString()}`,
///        `Trigger: ${e.trigger==="near_limit"?"near-limit":"rate-limited"}`,
///        `Ref with your in-progress files: ${e.ref}`,"","## To resume","",
///        `    claude --resume ${e.sessionId}`,"",
///        "(or open Claude Code in this directory and run /resume)","",
///        "## Plan (from TodoWrite state)",""];
/// ```
///
/// Rebrands, per repo precedent: the product noun is [`branding::PRODUCT_NAME`]
/// (as in `lingxi_core::settings::enterprise`) and the invocation is
/// `lingxi --resume <id>` — the exact spelling the port's own
/// "Session … saved. Resume with:" hint already prints
/// (`apps/cli/src/mode.rs`). Every other byte is the oracle's.
///
/// Two details worth not "tidying":
///
/// * `## What's next` and its answer are inside the non-empty-todos branch. An
///   empty list emits the "No task list was active" sentence and NOTHING else —
///   no `## What's next` heading at all.
/// * The `- [>] …    ← current step` marker is separated by FOUR spaces.
#[must_use]
pub fn render_resume_md(doc: &ResumeDoc<'_>) -> String {
    let product = branding::PRODUCT_NAME;
    let mut lines: Vec<String> = vec![
        format!("# {product} \u{2014} resume checkpoint"),
        String::new(),
        format!("Session: {}", doc.session_id),
        format!("Written: {}", doc.written_iso),
        format!("Trigger: {}", doc.trigger.label()),
        format!("Ref with your in-progress files: {}", doc.ref_name),
        String::new(),
        "## To resume".to_string(),
        String::new(),
        format!("    lingxi --resume {}", doc.session_id),
        String::new(),
        format!("(or open {product} in this directory and run /resume)"),
        String::new(),
        "## Plan (from TodoWrite state)".to_string(),
        String::new(),
    ];

    if doc.todos.is_empty() {
        lines.push(
            "No task list was active; see transcript via the resume command above.".to_string(),
        );
    } else {
        // `r` — the first PENDING content; `n` — the first IN-PROGRESS
        // activeForm. Both are `??=`, i.e. first-wins, and both hold the
        // SANITISED text (the oracle assigns from the sanitised local).
        let mut first_pending: Option<String> = None;
        let mut first_in_progress: Option<String> = None;
        for todo in doc.todos {
            match todo.status {
                TodoState::Completed => {
                    lines.push(format!("- [x] {}", sanitize_todo_line(&todo.content)));
                }
                TodoState::InProgress => {
                    let active = sanitize_todo_line(&todo.active_form);
                    lines.push(format!("- [>] {active}    \u{2190} current step"));
                    if first_in_progress.is_none() {
                        first_in_progress = Some(active);
                    }
                }
                TodoState::Pending => {
                    let content = sanitize_todo_line(&todo.content);
                    lines.push(format!("- [ ] {content}"));
                    if first_pending.is_none() {
                        first_pending = Some(content);
                    }
                }
            }
        }
        lines.push(String::new());
        lines.push("## What's next".to_string());
        lines.push(String::new());
        lines.push(match (first_pending, first_in_progress) {
            (Some(pending), _) => pending,
            (None, Some(active)) => format!("finish: {active}"),
            (None, None) => "All tasks completed.".to_string(),
        });
    }

    lines.extend([
        String::new(),
        "---".to_string(),
        String::new(),
        "Don't want these changes? Resume this session (above), then run".to_string(),
        "`/rewind` to roll back the turn's tool edits (bash-made changes".to_string(),
        format!(
            "excluded). {} holds a full snapshot until this session's",
            doc.ref_name
        ),
        "next checkpoint, or for up to ~2 weeks.".to_string(),
        String::new(),
    ]);

    lines.join("\n")
}

// ─────────────────────────────────────────────────────────────────────────────
// The executor — `q4v` (@292197916) and its latch `z4v` (@292197753).
// ─────────────────────────────────────────────────────────────────────────────

/// `da` (@282213190), verbatim:
/// `Object.freeze(["-c","core.hooksPath=/dev/null","-c","core.fsmonitor="])`.
///
/// Prefixed to EVERY git invocation the checkpoint makes. Disarming hooks is
/// what stops a repository's own `pre-commit` from running as a side effect of
/// hitting a rate limit; disabling fsmonitor stops the checkpoint from starting
/// a long-lived daemon in a repo the user only happened to be visiting.
const GIT_SAFETY_ARGS: &[&str] = &["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor="];

/// Git-dir entries that must not be symlinks — the oracle's list with
/// `refs/claude` rebranded onto [`CHECKPOINT_REF_PREFIX`]'s namespace.
const GITDIR_SYMLINK_GUARDS: &[&str] = &[
    "objects",
    "refs",
    "refs/lingxi",
    "logs",
    "logs/refs",
    "logs/refs/lingxi",
    "packed-refs",
    "reftable",
];

/// `W4v(e)` — an in-progress sequencer makes the working tree a merge artifact,
/// not the user's work.
const SEQUENCER_MARKERS: &[&str] = &[
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "rebase-merge",
    "rebase-apply",
];

/// `Qsl(e,t)` (@292199103):
///
/// ```js
/// function Qsl(e,t){ if(e===t) return !0;
///   let r=oB.relative(t,e); return r!=="" && !r.startsWith("..") && !oB.isAbsolute(r) }
/// ```
///
/// "`inner` is `outer`, or lives under it." Both arguments are expected to be
/// already canonicalised — that is the whole point of the guard, and passing a
/// non-canonical path silently weakens it.
#[must_use]
fn is_contained(inner: &std::path::Path, outer: &std::path::Path) -> bool {
    if inner == outer {
        return true;
    }
    inner.strip_prefix(outer).is_ok_and(|rest| {
        rest.components().next().is_some()
            && rest.components().next() != Some(std::path::Component::ParentDir)
    })
}

/// The gates the `session` crate cannot evaluate for itself — each one is a
/// process/UI fact the caller owns.
#[derive(Debug, Clone, Copy)]
pub struct CheckpointGates {
    /// `Dn()` — print mode / SDK / hooks. A checkpoint the user will never be
    /// told about is just an unexplained commit in their repository.
    pub non_interactive: bool,
    /// `Ca()` — the workspace is remote, so a local commit snapshots nothing.
    pub remote_workspace: bool,
    /// `Vs("allow_local_checkpoint_commit")`. Upstream this is a managed-policy
    /// permission that defaults to ALLOWED; the port has no such registry, so
    /// the caller states it. See the module docs.
    pub policy_allows: bool,
}

impl Default for CheckpointGates {
    /// Interactive, local, allowed — the upstream defaults.
    fn default() -> Self {
        Self {
            non_interactive: false,
            remote_workspace: false,
            policy_allows: true,
        }
    }
}

/// Opt-in env for [`CheckpointGates::policy_allows`] —
/// `LINGXI_ALLOW_LOCAL_CHECKPOINT_COMMIT`.
///
/// # This is the one OPEN PRODUCT DECISION in SC-02
///
/// Upstream's gate is `Vs("allow_local_checkpoint_commit")`, a managed-policy
/// permission check that returns **`true` unless enterprise policy names the
/// feature** (`Vs` @283688009) — i.e. upstream writes a WIP commit and a
/// `RESUME.md` into the user's repository by default, the first time a session
/// is rate-limited.
///
/// The port has no feature-permission registry to express that check, so
/// "default" here is a decision someone has to make rather than a fact to be
/// read out of a table:
///
/// * matching upstream means LingXi silently creates
///   `refs/lingxi/checkpoint-*` and `.lingxi/RESUME.md` in every user's
///   repository on the first rate limit;
/// * defaulting off means the mechanism is inert until a user asks for it.
///
/// [`CheckpointGates::default`] keeps UPSTREAM's polarity (allowed), so the
/// library is faithful. The live call site
/// (`orchestrator::turn_loop::maybe_checkpoint_on_rate_limit`) reads this env
/// instead, so the shipped default is off. Flipping that one expression to
/// `true` is the whole change if the answer is "match upstream".
pub const ALLOW_LOCAL_CHECKPOINT_COMMIT_ENV: &str = "LINGXI_ALLOW_LOCAL_CHECKPOINT_COMMIT";

/// Whether the caller may write a checkpoint commit into the user's repository.
/// See [`ALLOW_LOCAL_CHECKPOINT_COMMIT_ENV`].
#[must_use]
pub fn local_checkpoint_commit_allowed() -> bool {
    match std::env::var(ALLOW_LOCAL_CHECKPOINT_COMMIT_ENV) {
        Ok(raw) => {
            let v = raw.trim();
            !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
        }
        Err(_) => false,
    }
}

/// One `performRateLimitCheckpoint({todos,trigger})` call, plus the two facts
/// the oracle reads from process globals (`zt()` the session id, `er()` the
/// cwd).
#[derive(Debug, Clone, Copy)]
pub struct CheckpointRequest<'a> {
    /// `zt()` — the session the `RESUME.md` tells the user to resume.
    pub session_id: &'a str,
    /// Why the checkpoint ran.
    pub trigger: CheckpointTrigger,
    /// `TodoWrite` state at checkpoint time, in list order.
    pub todos: &'a [TodoItem],
    /// `er()` — the directory the git commands run in.
    pub cwd: &'a std::path::Path,
    /// See [`CheckpointGates`].
    pub gates: CheckpointGates,
}

/// Owned form used by fire-and-forget callers.
#[derive(Debug, Clone)]
pub struct OwnedCheckpointRequest {
    /// Owning conversation session.
    pub session_id: String,
    /// Why the checkpoint ran.
    pub trigger: CheckpointTrigger,
    /// Todo state captured at dispatch time.
    pub todos: Vec<TodoItem>,
    /// Workspace captured at dispatch time.
    pub cwd: std::path::PathBuf,
    /// Caller-supplied safety gates.
    pub gates: CheckpointGates,
}

impl OwnedCheckpointRequest {
    fn borrowed(&self) -> CheckpointRequest<'_> {
        CheckpointRequest {
            session_id: &self.session_id,
            trigger: self.trigger,
            todos: &self.todos,
            cwd: &self.cwd,
            gates: self.gates,
        }
    }
}

#[derive(Debug, Clone)]
enum CheckpointLatchEntry {
    Running(u64),
    Ready(CheckpointResult),
}

#[derive(Debug, Default)]
struct CheckpointLatch {
    by_session: HashMap<String, CheckpointLatchEntry>,
    last_ready: Option<(String, CheckpointResult)>,
    next_generation: u64,
}

/// The oracle's `lastCheckpointResult` latch is module-global because one CLI
/// process owns one session. LingXi embeds several conversations in one
/// process, so the equivalent lifetime key must be the session id. `Running`
/// is installed synchronously before a detached worker starts, closing the
/// near-limit/hard-429 race without serializing different sessions.
static CHECKPOINT_LATCH: Lazy<(std::sync::Mutex<CheckpointLatch>, std::sync::Condvar)> =
    Lazy::new(|| {
        (
            std::sync::Mutex::new(CheckpointLatch::default()),
            std::sync::Condvar::new(),
        )
    });

/// `getLastCheckpointResult()` (`_6f`) — what the last checkpoint decided, or
/// `None` if none has run in this process.
///
/// This compatibility view is the value the `useRateLimitCheckpointResult`
/// hook renders. Dispatch and dedupe use the session-keyed entries above.
#[must_use]
pub fn last_checkpoint_result() -> Option<CheckpointResult> {
    CHECKPOINT_LATCH
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .last_ready
        .as_ref()
        .map(|(_, result)| result.clone())
}

/// Finished checkpoint result for one conversation session.
#[must_use]
pub fn checkpoint_result_for_session(session_id: &str) -> Option<CheckpointResult> {
    let state = CHECKPOINT_LATCH
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match state.by_session.get(session_id) {
        Some(CheckpointLatchEntry::Ready(result)) => Some(result.clone()),
        Some(CheckpointLatchEntry::Running(_)) | None => None,
    }
}

/// Whether a checkpoint has started or finished for one session.
#[must_use]
pub fn checkpoint_started_for_session(session_id: &str) -> bool {
    CHECKPOINT_LATCH
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .by_session
        .contains_key(session_id)
}

/// `clearLastCheckpointResult()` (`tDi(null)`) — forget every embedded-session
/// latch. Tests and account-wide teardown use this; a conversation reset uses
/// [`clear_checkpoint_result_for_session`] so it cannot disturb peers.
pub fn clear_last_checkpoint_result() {
    let mut state = CHECKPOINT_LATCH
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.by_session.clear();
    state.last_ready = None;
    CHECKPOINT_LATCH.1.notify_all();
}

/// Clear only one embedded conversation's latch.
pub fn clear_checkpoint_result_for_session(session_id: &str) {
    let mut state = CHECKPOINT_LATCH
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.by_session.remove(session_id);
    if state
        .last_ready
        .as_ref()
        .is_some_and(|(last_session_id, _)| last_session_id == session_id)
    {
        state.last_ready = None;
    }
    CHECKPOINT_LATCH.1.notify_all();
}

fn next_checkpoint_generation(state: &mut CheckpointLatch) -> u64 {
    let generation = state.next_generation;
    state.next_generation = state.next_generation.wrapping_add(1);
    generation
}

fn try_claim_checkpoint(session_id: &str) -> Option<u64> {
    let mut state = CHECKPOINT_LATCH
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.by_session.contains_key(session_id) {
        return None;
    }
    let generation = next_checkpoint_generation(&mut state);
    state.by_session.insert(
        session_id.to_string(),
        CheckpointLatchEntry::Running(generation),
    );
    Some(generation)
}

fn wait_or_claim_checkpoint(session_id: &str) -> Result<u64, CheckpointResult> {
    let mut state = CHECKPOINT_LATCH
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        match state.by_session.get(session_id) {
            Some(CheckpointLatchEntry::Ready(result)) => return Err(result.clone()),
            Some(CheckpointLatchEntry::Running(_)) => {
                state = CHECKPOINT_LATCH
                    .1
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            None => {
                let generation = next_checkpoint_generation(&mut state);
                state.by_session.insert(
                    session_id.to_string(),
                    CheckpointLatchEntry::Running(generation),
                );
                return Ok(generation);
            }
        }
    }
}

fn finish_claimed_checkpoint(
    request: &CheckpointRequest<'_>,
    generation: u64,
    result: CheckpointResult,
) -> CheckpointResult {
    {
        let mut state = CHECKPOINT_LATCH
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(
            state.by_session.get(request.session_id),
            Some(CheckpointLatchEntry::Running(current)) if *current == generation
        ) {
            state.by_session.insert(
                request.session_id.to_string(),
                CheckpointLatchEntry::Ready(result.clone()),
            );
            state.last_ready = Some((request.session_id.to_string(), result.clone()));
        }
        CHECKPOINT_LATCH.1.notify_all();
    }
    match &result {
        CheckpointResult::Committed { ref_name, .. } => {
            tracing::debug!(
                trigger = request.trigger.as_str(),
                ref_name = ref_name.as_str(),
                "rate-limit checkpoint committed"
            );
        }
        CheckpointResult::Skipped(reason) => {
            tracing::debug!(
                trigger = request.trigger.as_str(),
                reason = reason.as_str(),
                "rate-limit checkpoint skipped"
            );
        }
    }
    result
}

/// `performRateLimitCheckpoint(e)` — `z4v` (@292197753).
///
/// ```js
/// async function z4v(e){ let t=_6f(); if(t!==null) return t; tDi(null);
///   … n = await q4v({todos,trigger}); tDi(n);
///   if(n.committed) ve("usage_limit_checkpoint_commit",…), N("tengu_rl_checkpoint_a1_shown",{});
///   else Se("usage_limit_checkpoint_commit", n.skipReason, …); return n }
/// ```
///
/// The leading latch read is not an optimisation — it is the reason a trigger
/// that fires on EVERY rate-limited response still produces exactly one commit.
///
/// Synchronous, and blocking: it runs several `git` subprocesses. Call it on a
/// detached thread, matching the oracle's `Promise…catch(()=>{})` at both of
/// its call sites.
pub fn perform_rate_limit_checkpoint(request: &CheckpointRequest<'_>) -> CheckpointResult {
    let generation = match wait_or_claim_checkpoint(request.session_id) {
        Ok(generation) => generation,
        Err(previous) => return previous,
    };
    let result = run_checkpoint(request);
    finish_claimed_checkpoint(request, generation, result)
}

/// Atomically claim a session's checkpoint slot and run it on a detached OS
/// thread. Returns `true` only for the first trigger in that session; later
/// near-limit or hard-429 triggers observe the synchronous `Running`/`Ready`
/// entry and do not spawn another worker.
#[must_use]
pub fn dispatch_rate_limit_checkpoint(request: OwnedCheckpointRequest) -> bool {
    let Some(generation) = try_claim_checkpoint(&request.session_id) else {
        return false;
    };
    std::thread::spawn(move || {
        let borrowed = request.borrowed();
        let result = run_checkpoint(&borrowed);
        finish_claimed_checkpoint(&borrowed, generation, result);
    });
    true
}

/// One finished `git` invocation.
struct GitOut {
    ok: bool,
    stdout: Vec<u8>,
}

impl GitOut {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
}

/// `Co(Lo(), [...da, ...args], {cwd, env, input})`.
///
/// `stdin` is written from a helper thread so a large `--stdin-paths` payload
/// cannot deadlock against a full stdout pipe — the failure mode that makes the
/// naive "write then wait" version work on small repositories and hang on real
/// ones.
fn run_git(
    cwd: &std::path::Path,
    env: &[(&str, String)],
    args: &[&str],
    stdin: Option<Vec<u8>>,
) -> Option<GitOut> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new("git");
    cmd.args(GIT_SAFETY_ARGS)
        .args(args)
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().ok()?;
    let writer = stdin.map(|payload| {
        let mut pipe = child.stdin.take().expect("stdin piped above");
        std::thread::spawn(move || {
            let _ = pipe.write_all(&payload);
            // Dropping the handle closes the pipe, which is what tells `git`
            // the `--stdin-paths` list is finished.
        })
    });
    let output = child.wait_with_output().ok()?;
    if let Some(handle) = writer {
        let _ = handle.join();
    }
    Some(GitOut {
        ok: output.status.success(),
        stdout: output.stdout,
    })
}

/// `lstat` without following the final symlink.
fn lstat(path: &std::path::Path) -> Option<std::fs::Metadata> {
    std::fs::symlink_metadata(path).ok()
}

/// Shorthand for `q4v`'s many `return {committed:!1,skipReason:…}` exits.
const fn skipped(reason: CheckpointSkipReason) -> CheckpointResult {
    CheckpointResult::Skipped(reason)
}

/// `q4v(e)` — the executor. Every early return is a
/// [`CheckpointSkipReason`], never an error: each one means "this repository is
/// not one this feature may write into".
#[allow(clippy::too_many_lines)]
fn run_checkpoint(request: &CheckpointRequest<'_>) -> CheckpointResult {
    use CheckpointSkipReason as Skip;

    if request.gates.non_interactive {
        return skipped(Skip::NonInteractive);
    }
    if request.gates.remote_workspace {
        return skipped(Skip::RemoteWorkspace);
    }
    if !request.gates.policy_allows {
        return skipped(Skip::Policy);
    }

    // No env yet — these two probes run with the ambient environment, exactly
    // like the oracle's `ru(er())` / `VXt()`.
    let bare: [(&str, String); 0] = [];
    let Some(root) = run_git(request.cwd, &bare, &["rev-parse", "--show-toplevel"], None)
        .filter(|o| o.ok)
        .map(|o| std::path::PathBuf::from(o.text()))
        .filter(|p| !p.as_os_str().is_empty())
    else {
        return skipped(Skip::NotGit);
    };
    if run_git(
        request.cwd,
        &bare,
        &["rev-parse", "--is-bare-repository"],
        None,
    )
    .is_none_or(|o| !o.ok || o.text() != "false")
    {
        return skipped(Skip::BareRepo);
    }

    // `Qsl(home, root)` — a repository rooted AT or ABOVE the home directory
    // would snapshot the user's whole home into a commit.
    let Ok(root) = root.canonicalize() else {
        return skipped(Skip::GitRootUncontained);
    };
    let Some(home) = home_dir().and_then(|h| h.canonicalize().ok()) else {
        return skipped(Skip::GitRootUncontained);
    };
    if is_contained(&home, &root) {
        return skipped(Skip::GitRootUncontained);
    }

    let Some(git_dir) = run_git(
        request.cwd,
        &bare,
        &["rev-parse", "--absolute-git-dir"],
        None,
    )
    .filter(|o| o.ok)
    .map(|o| std::path::PathBuf::from(o.text()))
    .filter(|p| !p.as_os_str().is_empty()) else {
        return skipped(Skip::NotGit);
    };
    let Ok(git_dir) = git_dir.canonicalize() else {
        return skipped(Skip::GitDirUncontained);
    };
    if !is_contained(&git_dir, &root) {
        return skipped(Skip::GitDirUncontained);
    }
    // A `commondir` means this is a LINKED worktree: its object store belongs to
    // another checkout, and the containment proof above says nothing about it.
    if lstat(&git_dir.join("commondir")).is_some() {
        return skipped(Skip::GitDirUncontained);
    }
    for entry in GITDIR_SYMLINK_GUARDS {
        if lstat(&git_dir.join(entry)).is_some_and(|m| m.file_type().is_symlink()) {
            return skipped(Skip::GitDirUncontained);
        }
    }
    if let Ok(entries) = std::fs::read_dir(git_dir.join("objects")) {
        for entry in entries.flatten() {
            if entry
                .file_type()
                .is_ok_and(|t: std::fs::FileType| t.is_symlink())
            {
                return skipped(Skip::GitDirUncontained);
            }
        }
    }

    let env: Vec<(&str, String)> = vec![
        ("GIT_COMMON_DIR", git_dir.display().to_string()),
        ("GIT_WORK_TREE", root.display().to_string()),
        ("GIT_ALLOW_PROTOCOL", "none".to_string()),
        ("GIT_NO_LAZY_FETCH", "1".to_string()),
        ("GIT_NO_REPLACE_OBJECTS", "1".to_string()),
        ("GIT_TERMINAL_PROMPT", "0".to_string()),
    ];

    if lstat(&git_dir.join("info").join("sparse-checkout")).is_some()
        && run_git(
            &root,
            &env,
            &["config", "--type=bool", "--get", "core.sparseCheckout"],
            None,
        )
        .is_some_and(|o| o.ok && o.text() == "true")
    {
        return skipped(Skip::SparseCheckout);
    }

    // Content filters rewrite what `hash-object` stores, so the commit would not
    // hold the bytes on disk.
    let attributes = root.join(".gitattributes");
    let attributes_text = lstat(&attributes)
        .filter(|m| m.is_file() && m.len() <= 65_536)
        .and_then(|_| std::fs::read_to_string(&attributes).ok())
        .unwrap_or_default();
    if lstat(&git_dir.join("lfs")).is_some() || LFS_FILTER.is_match(&attributes_text) {
        return skipped(Skip::ContentFilters);
    }

    if SEQUENCER_MARKERS
        .iter()
        .any(|marker| lstat(&git_dir.join(marker)).is_some())
    {
        return skipped(Skip::SequencerInProgress);
    }

    let Some(head) = run_git(&root, &env, &["rev-parse", "--verify", "HEAD"], None)
        .filter(|o| o.ok)
        .map(|o| o.text())
        .filter(|s| !s.is_empty())
    else {
        return skipped(Skip::NoHead);
    };

    // A PRIVATE index: every `update-index` / `write-tree` below writes here,
    // never into the user's `.git/index`.
    let index_file = git_dir.join(format!("lingxi-checkpoint-index.{}", std::process::id()));
    let mut index_env = env.clone();
    index_env.push(("GIT_INDEX_FILE", index_file.display().to_string()));
    let outcome = build_checkpoint_commit(request, &root, &head, &env, &index_env);
    let _ = std::fs::remove_file(&index_file);
    outcome
}

/// The build half of `q4v`, split out so the private index file is removed on
/// EVERY exit (the oracle's `finally{ await $q.rm(l,{force:!0}) }`).
#[allow(clippy::too_many_lines)]
fn build_checkpoint_commit(
    request: &CheckpointRequest<'_>,
    root: &std::path::Path,
    head: &str,
    env: &[(&str, String)],
    index_env: &[(&str, String)],
) -> CheckpointResult {
    use CheckpointSkipReason as Skip;

    if run_git(root, index_env, &["read-tree", head], None).is_none_or(|o| !o.ok) {
        return skipped(Skip::GitError);
    }
    let (Some(cached), Some(untracked)) = (
        run_git(root, index_env, &["ls-files", "-z", "--cached"], None).filter(|o| o.ok),
        run_git(
            root,
            index_env,
            &["ls-files", "-z", "-o", "--exclude-standard"],
            None,
        )
        .filter(|o| o.ok),
    ) else {
        return skipped(Skip::GitError);
    };
    let split = |out: &GitOut| -> Vec<String> {
        out.stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    };
    let cached = split(&cached);
    let untracked = split(&untracked);
    if cached.len() + untracked.len() > MAX_CHECKPOINT_FILE_COUNT {
        return skipped(Skip::TooLarge);
    }

    // `F(path, isCached)` — the per-path filter. `removals` are cached paths
    // that no longer exist on disk; they are force-removed from the private
    // index so the commit reflects the deletion.
    let mut removals: Vec<String> = Vec::new();
    let mut entries: Vec<(String, &'static str)> = Vec::new();
    let mut total_bytes: u64 = 0;
    for (path, is_cached) in cached
        .iter()
        .map(|p| (p, true))
        .chain(untracked.iter().map(|p| (p, false)))
    {
        // A path containing any of these cannot be expressed in the
        // `--index-info` line format, so it is dropped rather than escaped.
        if path.contains('\n') || path.contains('\r') || path.contains('"') || path.contains('\\') {
            continue;
        }
        if path.split('/').any(|seg| seg == "." || seg == "..") {
            continue;
        }
        let absolute = root.join(path);
        let Some(meta) = lstat(&absolute) else {
            if is_cached {
                removals.push(path.clone());
            }
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        // A symlink whose target escapes the work tree must not be followed
        // into the commit.
        let Ok(real) = absolute.canonicalize() else {
            continue;
        };
        if !is_contained(&real, root) {
            continue;
        }
        total_bytes += meta.len();
        entries.push((path.clone(), file_mode(&meta)));
    }
    if total_bytes > MAX_CHECKPOINT_TOTAL_BYTES {
        return skipped(Skip::TooLarge);
    }

    let ref_name = checkpoint_ref(request.session_id);
    let document = render_resume_md(&ResumeDoc {
        session_id: request.session_id,
        ref_name: &ref_name,
        trigger: request.trigger,
        todos: request.todos,
        written_iso: &now_iso(),
    });

    // The document is written to the WORKING TREE as well as hashed into the
    // commit, so a user who never resumes still finds the instructions.
    let resume_path = root.join(RESUME_MD_REPO_PATH);
    let wrote = resume_path
        .parent()
        .ok_or(())
        .and_then(|dir| std::fs::create_dir_all(dir).map_err(|_| ()))
        .and_then(|()| {
            // Refuse to write THROUGH a symlink — the oracle's `allowSymlink`
            // guard. A pre-existing regular file is overwritten, as upstream.
            if lstat(&resume_path).is_some_and(|m| m.file_type().is_symlink()) {
                return Err(());
            }
            std::fs::write(&resume_path, &document).map_err(|_| ())
        });
    if wrote.is_err() {
        return skipped(Skip::ResumeWriteRefused);
    }

    let Some(resume_blob) = run_git(
        root,
        env,
        &["hash-object", "-w", "--stdin"],
        Some(document.into_bytes()),
    )
    .filter(|o| o.ok)
    .map(|o| o.text())
    .filter(|s| !s.is_empty()) else {
        return skipped(Skip::GitError);
    };

    let mut blobs: Vec<String> = Vec::new();
    if !entries.is_empty() {
        let mut payload = String::new();
        for (path, _) in &entries {
            payload.push_str(path);
            payload.push('\n');
        }
        let Some(out) = run_git(
            root,
            env,
            &["hash-object", "-w", "--no-filters", "--stdin-paths"],
            Some(payload.into_bytes()),
        )
        .filter(|o| o.ok) else {
            return skipped(Skip::GitError);
        };
        blobs = out
            .text()
            .lines()
            .filter(|l| !l.is_empty())
            .map(ToString::to_string)
            .collect();
        // One hash per path, in order. Anything else means the pipeline lost
        // its alignment and the index lines would name the wrong content.
        if blobs.len() != entries.len() {
            return skipped(Skip::GitError);
        }
    }

    let mut index_info = String::new();
    for ((path, mode), blob) in entries.iter().zip(&blobs) {
        index_info.push_str(&format!("{mode} {blob}\t{path}\n"));
    }
    index_info.push_str(&format!("100644 {resume_blob}\t{RESUME_MD_REPO_PATH}\n"));
    if run_git(
        root,
        index_env,
        &["update-index", "--add", "--index-info"],
        Some(index_info.into_bytes()),
    )
    .is_none_or(|o| !o.ok)
    {
        return skipped(Skip::GitError);
    }
    if !removals.is_empty() {
        let payload = removals.join("\0").into_bytes();
        if run_git(
            root,
            index_env,
            &["update-index", "--force-remove", "-z", "--stdin"],
            Some(payload),
        )
        .is_none_or(|o| !o.ok)
        {
            return skipped(Skip::GitError);
        }
    }

    let Some(tree) = run_git(root, index_env, &["write-tree"], None)
        .filter(|o| o.ok)
        .map(|o| o.text())
        .filter(|s| !s.is_empty())
    else {
        return skipped(Skip::GitError);
    };

    let short: String = request
        .session_id
        .chars()
        .take(REF_SESSION_ID_PREFIX_LEN)
        .collect();
    let subject = format!(
        "WIP: {} rate-limit checkpoint ({short})",
        branding::PRODUCT_NAME
    );
    let Some(commit) = run_git(
        root,
        env,
        &[
            "-c",
            CHECKPOINT_COMMIT_USER_NAME,
            "-c",
            CHECKPOINT_COMMIT_USER_EMAIL,
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            &tree,
            "-p",
            head,
            "-m",
            &subject,
        ],
        None,
    )
    .filter(|o| o.ok)
    .map(|o| o.text())
    .filter(|s| !s.is_empty()) else {
        return skipped(Skip::GitError);
    };

    // Re-checked AFTER the commit: a linked worktree could have been created
    // while the snapshot was being built, and `update-ref` would then write
    // into a shared ref store the containment proof never covered.
    if let Some(git_dir) = run_git(root, env, &["rev-parse", "--absolute-git-dir"], None)
        .filter(|o| o.ok)
        .map(|o| std::path::PathBuf::from(o.text()))
    {
        if lstat(&git_dir.join("commondir")).is_some() {
            return skipped(Skip::GitDirUncontained);
        }
    }

    if run_git(
        root,
        env,
        &[
            "-c",
            "core.logAllRefUpdates=false",
            "update-ref",
            "--no-deref",
            &ref_name,
            &commit,
        ],
        None,
    )
    .is_none_or(|o| !o.ok)
    {
        return skipped(Skip::GitError);
    }

    exclude_resume_document(root, env);
    gc_stale_checkpoint_refs(root, env, &ref_name);

    CheckpointResult::Committed {
        ref_name,
        resume_path: RESUME_MD_REPO_PATH,
        commit_sha: commit,
    }
}

/// `-c user.name=…` — the checkpoint commit's author. Rebranded: an
/// `Anthropic` identity on a commit LingXi wrote would be a lie in the user's
/// `git log`.
const CHECKPOINT_COMMIT_USER_NAME: &str = "user.name=LingXi";

/// `-c user.email=…`, rebranded for the same reason.
const CHECKPOINT_COMMIT_USER_EMAIL: &str = "user.email=noreply@lingxi.local";

/// `\bfilter\s*=\s*lfs\b` — the `.gitattributes` probe for an LFS filter.
static LFS_FILTER: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\bfilter\s*=\s*lfs\b").expect("static lfs filter regex"));

/// `(ne.mode&64)!==0?"100755":"100644"` — the OWNER execute bit only. Git's
/// index has exactly two file modes, so anything else would be rejected by
/// `update-index`.
fn file_mode(meta: &std::fs::Metadata) -> &'static str {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o100 != 0 {
            return "100755";
        }
        "100644"
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        "100644"
    }
}

/// `$HOME`, the one process global this module reads.
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// `G4v(e,t)` (@292204600) — add `/<RESUME_MD_REPO_PATH>` to the repository's
/// `info/exclude`, so the document does not show up as an untracked file in the
/// user's `git status` forever after.
///
/// Every failure is swallowed: an un-excluded `RESUME.md` is untidy, a failed
/// checkpoint is not.
fn exclude_resume_document(root: &std::path::Path, env: &[(&str, String)]) {
    let Some(raw) = run_git(
        root,
        env,
        &["rev-parse", "--git-path", "info/exclude"],
        None,
    )
    .filter(|o| o.ok)
    .map(|o| o.text())
    .filter(|s| !s.is_empty()) else {
        return;
    };
    let path = std::path::Path::new(&raw);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let Some(parent) = path.parent() else { return };
    // Neither the file nor its directory may be a symlink, and the file must be
    // a real file of sane size — the same shape guard the oracle applies before
    // appending to a path git handed it.
    for candidate in [parent, path.as_path()] {
        if let Some(meta) = lstat(candidate) {
            if meta.file_type().is_symlink() {
                return;
            }
            if !meta.is_file() && !meta.is_dir() {
                return;
            }
            if candidate == path.as_path() && meta.is_file() && meta.len() > 65_536 {
                return;
            }
        }
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let entry = format!("/{RESUME_MD_REPO_PATH}");
    if existing
        .split('\n')
        .any(|line| line.strip_suffix('\r').unwrap_or(line) == entry)
    {
        return;
    }
    let _ = std::fs::create_dir_all(parent);
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write as _;
        let _ = file.write_all(format!("{separator}{entry}\n").as_bytes());
    }
}

/// `V4v(e,t,r)` (@292205200) — delete sibling checkpoint refs whose commit is
/// older than [`STALE_CHECKPOINT_REF_AGE_SECS`].
///
/// Skips the ref just written, anything outside the namespace, and any SYMBOLIC
/// ref (`%(symref)` non-empty) — deleting a symref would silently retarget
/// whatever it pointed at.
fn gc_stale_checkpoint_refs(root: &std::path::Path, env: &[(&str, String)], keep: &str) {
    let Some(out) = run_git(
        root,
        env,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(committerdate:unix)%00%(symref)",
            &format!("{CHECKPOINT_REF_PREFIX}*"),
        ],
        None,
    )
    .filter(|o| o.ok) else {
        return;
    };
    let now = chrono::Utc::now().timestamp();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut fields = line.split('\0');
        let Some(name) = fields.next() else { continue };
        let age_field = fields.next();
        let symref = fields.next();
        if name.is_empty()
            || name == keep
            || !name.starts_with(CHECKPOINT_REF_PREFIX)
            || symref.is_some_and(|s| !s.is_empty())
        {
            continue;
        }
        let Some(Ok(when)) = age_field.map(str::parse::<i64>) else {
            continue;
        };
        let age = now - when;
        if age < 0 || age < i64::try_from(STALE_CHECKPOINT_REF_AGE_SECS).unwrap_or(i64::MAX) {
            continue;
        }
        let _ = run_git(root, env, &["update-ref", "--no-deref", "-d", name], None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(content: &str, status: TodoState, active_form: &str) -> TodoItem {
        TodoItem {
            id: String::new(),
            content: content.to_string(),
            status,
            active_form: active_form.to_string(),
        }
    }

    fn doc(todos: &[TodoItem], trigger: CheckpointTrigger) -> ResumeDoc<'_> {
        ResumeDoc {
            session_id: "abcd1234-5678-4abc-9def-000000000000",
            ref_name: "refs/lingxi/checkpoint-abcd1234",
            trigger,
            todos,
            written_iso: "2026-08-20T15:08:27.000Z",
        }
    }

    /// `RESUME_MD_REPO_PATH` is a literal because it has to be a `const`; this
    /// pins it to the one namespace source so a rebrand cannot leave it behind.
    #[test]
    fn resume_path_tracks_the_branding_dot_dir() {
        assert_eq!(
            RESUME_MD_REPO_PATH,
            format!("{}/RESUME.md", branding::DOT_DIR)
        );
    }

    #[test]
    fn ref_is_the_prefix_plus_eight_session_id_chars() {
        assert_eq!(
            checkpoint_ref("abcd1234-5678-4abc-9def-000000000000"),
            "refs/lingxi/checkpoint-abcd1234"
        );
        // Shorter than 8: `slice(0,8)` clamps, it does not pad or panic.
        assert_eq!(checkpoint_ref("ab"), "refs/lingxi/checkpoint-ab");
        assert_eq!(checkpoint_ref(""), "refs/lingxi/checkpoint-");
    }

    #[test]
    fn protocol_session_key_is_the_bare_uuid_for_all_checkpoint_triggers() {
        let session_id = lingxi_core::types::SessionId::new();
        assert_eq!(
            checkpoint_session_key(session_id),
            session_id.as_uuid().to_string()
        );
        assert!(!checkpoint_session_key(session_id).starts_with("sess:"));
    }

    /// The empty-list document, whole. Byte-for-byte against `K4v` with the two
    /// documented rebrands — note there is NO `## What's next` section.
    #[test]
    fn empty_todo_list_document_is_byte_exact() {
        let rendered = render_resume_md(&doc(&[], CheckpointTrigger::NearLimit));
        assert_eq!(
            rendered,
            "# LingXi \u{2014} resume checkpoint\n\
             \n\
             Session: abcd1234-5678-4abc-9def-000000000000\n\
             Written: 2026-08-20T15:08:27.000Z\n\
             Trigger: near-limit\n\
             Ref with your in-progress files: refs/lingxi/checkpoint-abcd1234\n\
             \n\
             ## To resume\n\
             \n    lingxi --resume abcd1234-5678-4abc-9def-000000000000\n\
             \n\
             (or open LingXi in this directory and run /resume)\n\
             \n\
             ## Plan (from TodoWrite state)\n\
             \n\
             No task list was active; see transcript via the resume command above.\n\
             \n\
             ---\n\
             \n\
             Don't want these changes? Resume this session (above), then run\n\
             `/rewind` to roll back the turn's tool edits (bash-made changes\n\
             excluded). refs/lingxi/checkpoint-abcd1234 holds a full snapshot until this session's\n\
             next checkpoint, or for up to ~2 weeks.\n"
        );
        assert!(
            !rendered.contains("What's next"),
            "the heading lives inside the non-empty branch only"
        );
    }

    /// The plan block: one line per todo in list order, the in-progress marker,
    /// and "what's next" = the first PENDING content (a pending item outranks
    /// the in-progress one even when it comes after it).
    #[test]
    fn plan_block_renders_every_status_and_prefers_the_first_pending() {
        let todos = [
            todo("Read the audit", TodoState::Completed, "Reading the audit"),
            todo(
                "Port the renderer",
                TodoState::InProgress,
                "Porting the renderer",
            ),
            todo("Wire the trigger", TodoState::Pending, ""),
            todo("Write the report", TodoState::Pending, ""),
        ];
        let rendered = render_resume_md(&doc(&todos, CheckpointTrigger::RateLimited));
        assert!(rendered.contains("Trigger: rate-limited\n"));
        assert!(
            rendered.contains(
                "## Plan (from TodoWrite state)\n\
                 \n\
                 - [x] Read the audit\n\
                 - [>] Porting the renderer    \u{2190} current step\n\
                 - [ ] Wire the trigger\n\
                 - [ ] Write the report\n\
                 \n\
                 ## What's next\n\
                 \n\
                 Wire the trigger\n"
            ),
            "{rendered}"
        );
    }

    /// No pending item ⇒ `finish: <first in-progress activeForm>`; nothing left
    /// at all ⇒ `All tasks completed.`
    #[test]
    fn whats_next_falls_back_to_the_in_progress_step_then_to_all_completed() {
        let in_progress = [
            todo("Done", TodoState::Completed, "Doing"),
            todo("Porting", TodoState::InProgress, "Porting the executor"),
        ];
        let rendered = render_resume_md(&doc(&in_progress, CheckpointTrigger::NearLimit));
        assert!(
            rendered.contains("\n\n## What's next\n\nfinish: Porting the executor\n"),
            "{rendered}"
        );

        let all_done = [todo("Done", TodoState::Completed, "Doing")];
        let rendered = render_resume_md(&doc(&all_done, CheckpointTrigger::NearLimit));
        assert!(
            rendered.contains("\n\n## What's next\n\nAll tasks completed.\n"),
            "{rendered}"
        );
    }

    /// A todo cannot forge markdown structure: every newline / control /
    /// bidi-override RUN collapses to ONE space, so the item stays one line.
    #[test]
    fn sanitiser_collapses_runs_and_keeps_a_todo_on_one_line() {
        assert_eq!(
            sanitize_todo_line("line one\n\n- [x] forged\ttail"),
            "line one - [x] forged tail"
        );
        // U+202E RIGHT-TO-LEFT OVERRIDE and U+200B ZERO WIDTH SPACE are in the
        // class; U+2066..U+2069 (the isolates) too.
        assert_eq!(
            sanitize_todo_line("a\u{202e}\u{200b}\u{2069}b"),
            "a b",
            "an adjacent run is one space, not three"
        );
        // Ordinary text is untouched, including non-ASCII.
        assert_eq!(sanitize_todo_line("端到端 · ok"), "端到端 · ok");

        let todos = [todo("x\ny", TodoState::Pending, "")];
        let rendered = render_resume_md(&doc(&todos, CheckpointTrigger::NearLimit));
        assert!(rendered.contains("- [ ] x y\n"), "{rendered}");
    }

    /// 500 UTF-16 units, then `…` — and the cap is applied AFTER sanitising, so
    /// a run that collapses under the limit is not truncated.
    #[test]
    fn sanitiser_truncates_at_500_utf16_units_with_an_ellipsis() {
        let exactly = "a".repeat(TODO_LINE_MAX_UTF16);
        assert_eq!(
            sanitize_todo_line(&exactly),
            exactly,
            "the cap is exclusive"
        );

        let over = "a".repeat(TODO_LINE_MAX_UTF16 + 1);
        let cut = sanitize_todo_line(&over);
        assert_eq!(cut.chars().count(), TODO_LINE_MAX_UTF16 + 1);
        assert!(cut.ends_with('\u{2026}'));
        assert_eq!(&cut[..TODO_LINE_MAX_UTF16], exactly);

        // A non-BMP char is TWO UTF-16 units: 250 of them exactly fill the cap.
        let emoji = "\u{1f600}".repeat(TODO_LINE_MAX_UTF16 / 2);
        assert_eq!(sanitize_todo_line(&emoji), emoji);
        let emoji_over = format!("{emoji}\u{1f600}");
        let cut = sanitize_todo_line(&emoji_over);
        assert_eq!(
            cut,
            format!("{emoji}\u{2026}"),
            "a surrogate pair is never split in half"
        );
    }

    #[test]
    fn trigger_and_skip_reason_wire_spellings() {
        assert_eq!(CheckpointTrigger::NearLimit.as_str(), "near_limit");
        assert_eq!(CheckpointTrigger::NearLimit.label(), "near-limit");
        assert_eq!(CheckpointTrigger::RateLimited.as_str(), "rate_limited");
        assert_eq!(CheckpointTrigger::RateLimited.label(), "rate-limited");

        // All 14, in the order `q4v` tests them.
        let reasons = [
            CheckpointSkipReason::NonInteractive,
            CheckpointSkipReason::RemoteWorkspace,
            CheckpointSkipReason::Policy,
            CheckpointSkipReason::NotGit,
            CheckpointSkipReason::BareRepo,
            CheckpointSkipReason::GitRootUncontained,
            CheckpointSkipReason::GitDirUncontained,
            CheckpointSkipReason::SparseCheckout,
            CheckpointSkipReason::ContentFilters,
            CheckpointSkipReason::SequencerInProgress,
            CheckpointSkipReason::NoHead,
            CheckpointSkipReason::TooLarge,
            CheckpointSkipReason::ResumeWriteRefused,
            CheckpointSkipReason::GitError,
        ];
        let spellings: Vec<&str> = reasons.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            spellings,
            vec![
                "non_interactive",
                "remote_workspace",
                "policy",
                "not_git",
                "bare_repo",
                "gitroot_uncontained",
                "gitdir_uncontained",
                "sparse_checkout",
                "content_filters",
                "sequencer_in_progress",
                "no_head",
                "too_large",
                "resume_write_refused",
                "git_error",
            ]
        );
    }
}

#[cfg(test)]
mod executor_tests {
    use super::*;
    use std::path::Path;

    static CHECKPOINT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `Qsl(e,t)` — "`inner` IS `outer`, or lives under it". The guard that
    /// stops a repository rooted at (or above) `$HOME` from being snapshotted,
    /// and stops a symlinked working-tree file from being followed out of the
    /// tree.
    #[test]
    fn containment_matches_the_oracle_relative_test() {
        assert!(is_contained(Path::new("/a/b"), Path::new("/a/b")));
        assert!(is_contained(Path::new("/a/b/c"), Path::new("/a/b")));
        assert!(is_contained(Path::new("/a/b/c/d"), Path::new("/a/b")));
        // Escapes.
        assert!(!is_contained(Path::new("/a"), Path::new("/a/b")));
        assert!(!is_contained(Path::new("/a/bc"), Path::new("/a/b")));
        assert!(!is_contained(Path::new("/x"), Path::new("/a/b")));
        // The case the gate exists for: a repo rooted AT the home directory.
        assert!(is_contained(Path::new("/home/u"), Path::new("/home/u")));
        // …and one rooted ABOVE it.
        assert!(is_contained(Path::new("/home/u"), Path::new("/home")));
        // The normal case: a project inside home is fine.
        assert!(!is_contained(Path::new("/home/u"), Path::new("/home/u/p")));
    }

    /// `100755` iff the OWNER execute bit is set — git's index has exactly two
    /// file modes, so a group/other-only `x` must NOT promote the entry.
    #[cfg(unix)]
    #[test]
    fn file_mode_reads_only_the_owner_execute_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("f");
        std::fs::write(&path, b"x").expect("write");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert_eq!(
            file_mode(&std::fs::metadata(&path).expect("stat")),
            "100644"
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(
            file_mode(&std::fs::metadata(&path).expect("stat")),
            "100755"
        );

        // Group-execute only: still 100644.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o654)).expect("chmod");
        assert_eq!(
            file_mode(&std::fs::metadata(&path).expect("stat")),
            "100644"
        );
    }

    /// The latch is what makes `performRateLimitCheckpoint` once-per-session.
    /// (The gate ladder and the real commit are covered end to end in
    /// `session/tests/rate_limit_checkpoint_test.rs`.)
    #[test]
    fn clearing_the_latch_makes_it_none_again() {
        let _guard = CHECKPOINT_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_last_checkpoint_result();
        assert_eq!(last_checkpoint_result(), None);
    }

    #[test]
    fn detached_dispatch_dedupes_per_session_without_cross_session_leakage() {
        let _guard = CHECKPOINT_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_last_checkpoint_result();
        let root = tempfile::tempdir().expect("tempdir");
        let request = |session_id: &str, trigger| OwnedCheckpointRequest {
            session_id: session_id.to_string(),
            trigger,
            todos: Vec::new(),
            cwd: root.path().to_path_buf(),
            gates: CheckpointGates {
                policy_allows: false,
                ..CheckpointGates::default()
            },
        };
        let a = "aaaaaaaa-0000-4000-8000-000000000000";
        let b = "bbbbbbbb-0000-4000-8000-000000000000";

        assert!(dispatch_rate_limit_checkpoint(request(
            a,
            CheckpointTrigger::NearLimit
        )));
        assert!(checkpoint_started_for_session(a));
        assert!(
            !dispatch_rate_limit_checkpoint(request(a, CheckpointTrigger::RateLimited)),
            "a later hard 429 must not start duplicate work"
        );
        assert!(
            dispatch_rate_limit_checkpoint(request(b, CheckpointTrigger::NearLimit)),
            "an unrelated embedded session needs an independent latch"
        );

        for _ in 0..100 {
            if checkpoint_result_for_session(a).is_some()
                && checkpoint_result_for_session(b).is_some()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(
            checkpoint_result_for_session(a),
            Some(CheckpointResult::Skipped(CheckpointSkipReason::Policy))
        );
        assert_eq!(
            checkpoint_result_for_session(b),
            Some(CheckpointResult::Skipped(CheckpointSkipReason::Policy))
        );

        clear_checkpoint_result_for_session(a);
        assert!(!checkpoint_started_for_session(a));
        assert!(checkpoint_started_for_session(b));
        clear_last_checkpoint_result();
    }
}

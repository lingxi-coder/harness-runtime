//! File & search tools: Read, Write, Edit, NotebookEdit, Glob, Grep.
//!
//! Extracted from the `tools` monolith in M8-P5. Each tool takes a
//! `tool_api::BuiltinToolContext` at construction; `register_all` wires all
//! six into a `ToolRegistry`. Cross-tool helpers live in `tool_api::util`
//! (path validation, output truncation); file-only helpers live in `shared`.

#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::match_wildcard_for_single_variants,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::format_collect,
    clippy::similar_names,
    clippy::doc_markdown,
    clippy::manual_let_else
)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 6 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]
// Dead code kept visible, not swept: this crate had 2 item(s) rustc could
// reach from nothing when the workspace was measured (2026-09-16). The lint
// stays `warn` at the workspace level so a NEW crate still inherits it; this
// allow is scoped here so the count is per crate and repayable by deleting this
// line. This is the category where "named, computed, never wired" hides — some
// of these read like features that were built and never connected. Each wants a
// decision (delete, or wire), not a blanket deletion.
// ⚠️ The count above is ONE macOS, lib-target measurement. It is not a list of
// deletable items — see docs/HANDOFF-dead-code-adjudication-2026-09-17.md,
// which records two near-misses where it said "dead" about live code.
#![allow(dead_code)]

/// Suffix claude-code appends to a successful Edit/Write result (binary
/// const `Pyn`, with a U+2014 em-dash), telling the model it already holds the
/// current file content. claude-code gates it on the file not being
/// user-modified / stale-recovered; LingXi's non-interactive path is always
/// current, so it is appended unconditionally. The leading space is part of the
/// literal (it follows the base message directly).
pub const FILE_STATE_CURRENT_SUFFIX: &str =
    " (file state is current in your context — no need to Read it back)";

/// Whether this build of `tool-file` can decode images for `FileRead`.
///
/// A consumer that hands the model an image BY PATH (the local-app annotation
/// crop does) needs this on, or `Read` falls to the NUL scan and returns
/// `format_binary` — a silent failure where the call succeeds and the model
/// sees no picture. Exposed as a const so a downstream crate can assert its
/// own build has it, which a behavioural test cannot do through a dummy FS.
pub const IMAGE_READ_ENABLED: bool = cfg!(feature = "image-read");

mod dir_validate;
pub mod edit;
pub mod file_meta;
pub mod glob;
pub mod grep;
#[cfg(feature = "image-read")]
pub mod image_read;
pub mod notebook_edit;
pub mod notebook_read;
#[cfg(feature = "pdf-read")]
pub mod pdf_read;
#[cfg(feature = "pdf-render")]
mod pdf_render;
pub mod quotes;
pub mod read;
pub mod ripgrep_mode;
pub mod shared;
pub mod structured_patch;
#[cfg(test)]
pub(crate) mod test_env;
pub mod write;

pub use edit::FileEditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use notebook_edit::NotebookEditTool;
pub use read::FileReadTool;
pub use write::FileWriteTool;

/// Read-before-write staleness error: emitted when Edit / Write / NotebookEdit
/// detect that the target file changed on disk since the last `Read` (its
/// floor-truncated mtime advanced past the recorded read timestamp, and the
/// full-read content-equality fallback did not save it).
///
/// Shared by all three write tools.
///
/// # Byte-exactness vs the oracle (FT-07, re-verified against 2.1.238)
/// claude-code raises staleness from two distinct phases with two DIFFERENT
/// strings, and the one a model normally sees is the **validate** phase:
///   * `validateInput` — Edit `errorCode:7` (cc-238.js @226407883), Write
///     `errorCode:3` (@226416137), NotebookEdit `errorCode:10` (@226495421),
///     all three carrying THIS literal. Identical in 2.1.220.
///   * the **call**-phase re-check (`Ehv` @226404058 for Edit, @226410437 for
///     Write) `throw new Q4e(WVo)` — the richer linter/formatter sentence,
///     [`FILE_CONTENT_CHANGED_LINTER_MESSAGE`]. That branch is only reachable
///     on a validate→call race, so it is NOT the common path.
///
/// LingXi's tools have a single `call` entry point (no separate validate vs
/// call phase), so the guard emits the validate-phase literal — the bytes the
/// model actually gets upstream. (Before FT-07 the port emitted `WVo` here,
/// i.e. the oracle's RARE branch, for every stale case.)
pub const FILE_UNEXPECTEDLY_MODIFIED_ERROR: &str =
    "File has been modified since read, either by the user or by a linter. Read it again before attempting to write it.";

/// Read-before-write "never read" error: emitted when Edit / Write /
/// NotebookEdit are asked to modify an EXISTING file that has no recorded
/// `Read` (or whose last read was a partial / range view).
///
/// Byte-exact to claude-code's validate-phase message
/// (`FileEditTool.ts:280-281`, `FileWriteTool.ts:202-203`,
/// `NotebookEditTool.ts:225-226`): `"File has not been read yet. Read it first
/// before writing to it."`.
pub const FILE_NOT_READ_ERROR: &str =
    "File has not been read yet. Read it first before writing to it.";

/// Richer stale-file message for the CALL-phase re-check — byte-locked to
/// claude-code `WVo` (cc-238.js @220242969, the constant block next to
/// `qVo`/`ssa`/`asa`). It is thrown only from `Ehv` (@226404058) / the Write
/// twin (@226410437) — the validate→call race — never from `validateInput`.
///
/// LingXi has a unified `call` path, so the guard below reports the
/// validate-phase [`FILE_UNEXPECTEDLY_MODIFIED_ERROR`] instead; this constant
/// stays byte-locked here as the oracle's other branch (FT-07).
pub const FILE_CONTENT_CHANGED_LINTER_MESSAGE: &str =
    "File content has changed since it was last read. This commonly happens when a linter or formatter run via Bash rewrites the file. Call Read on this file to refresh, then retry the edit.";

/// Read-state staleness guard shared by Edit / Write / NotebookEdit (Batch F).
///
/// 1:1 port of claude-code's read-before-write guard `FOg`
/// (`FileEditTool.ts`, offset-verified in the 2.1.212 binary):
///   * no recorded `Read` at all → refuse with [`FILE_NOT_READ_ERROR`]
///     (`if(!r) throw PWn`);
///   * mtime not advanced past the recorded read (`bY(e) <= r.timestamp`) →
///     proceed;
///   * mtime advanced, but the read covered the WHOLE file
///     ([`read_covers_full_file`] = `wMe`) and its recorded content still
///     equals the current on-disk content (`RMe`) → proceed (cloud sync /
///     antivirus touched the mtime without changing bytes);
///   * otherwise → [`FILE_UNEXPECTEDLY_MODIFIED_ERROR`] (the oracle's
///     `validateInput` errorCode 7/3/10 literal).
///
/// ## 2.1.212 fix — offset/limit reads are no longer rejected
/// Before 2.1.212 (and in LingXi's prior guard) a read done WITH `offset`/
/// `limit` was treated as "not read" and rejected outright, *before* even
/// checking the mtime. 2.1.212 gives the `wMe` full-read helper a new
/// `limit===void 0 → true` branch and — crucially — the guard now raises
/// "File has not been read yet" ONLY when NO read-state entry exists. A ranged
/// read still HAS an entry, so it falls through to the mtime / content checks:
/// editing a file previously read with offset/limit now succeeds when the file
/// is unchanged on disk.
///
/// ## Edit-applies recovery (`zQi`) lives in the caller
/// claude-code's Edit path additionally recovers a stale read when the edit
/// still applies cleanly to the current content. LingXi keeps that in
/// `edit.rs` (`stale_edit_applies`, gated on the CONTENT-CHANGED error — and
/// currently held shut there for a reason worth reading), so
/// this shared guard — reused verbatim by Write / NotebookEdit, which have no
/// such recovery — stays limited to the entry / mtime / full-read checks.
///
/// Arguments:
/// - `map`: the shared `read_file_state` registry (`ctx.read_file_state`).
/// - `canon`: the canonicalized absolute path used as the registry key (the
///   same key the `Read` tool wrote under).
/// - `current_mtime_ms`: the file's *current* floor-truncated mtime (ms).
/// - `current_full_content`: the file's *current* content, decoded the SAME
///   way `Read` stores it (so the content-equality fallback compares like with
///   like). Only consulted for the full-read fallback.
///
/// Returns `Ok(())` to proceed, or a [`tool_api::tool_trait::ToolError::InvalidInput`]
/// carrying the byte-exact error string when the write must be refused.
///
/// New-file creation MUST NOT call this — callers gate it behind "the file
/// exists" (TS `ENOENT → result:true` / `meta === null` skips the guard).
///
/// # `isPartialView`
/// claude-code's `wMe` also short-circuits on `isPartialView`. LingXi now
/// TRACKS that flag (`ReadFileEntry::is_partial_view`), and
/// [`read_covers_full_file`] honours it, so the guard is byte-faithful for
/// every entry that sets it — notably the memory files seeded by
/// `ConversationOrchestrator::seed_memory_read_state`, whose recorded content
/// has YAML frontmatter and HTML comments stripped and therefore must never
/// take the content-equality fallback.
///
/// KNOWN RESIDUAL (explicitly named, not silently claimed): the `Read` tool's
/// token-truncation path (`read.rs`, the `partial_note` branch) still records
/// `is_partial_view: false`. The behaviour there is unchanged from before this
/// port and remains correct in outcome — such a read stores a *truncated*
/// slice, so `current_full_content == entry.content` already fails — but
/// wiring the flag at that site is a separate follow-up.
/// Oracle `gf` — the model ids for which read-before-write is still REQUIRED.
///
/// Upstream waives the requirement for anything NOT in this set, on the theory
/// that a newer model does not need the hand-holding. Verbatim, that reads
/// "unknown id ⇒ waive".
const READ_REQUIRED_MODELS: [&str; 10] = [
    "claude-opus-4-6",
    "claude-haiku-4-5",
    "claude-opus-4-5",
    "claude-opus-4-1",
    "claude-opus-4-0",
    "claude-sonnet-4-5",
    "claude-sonnet-4-0",
    "claude-3-7-sonnet",
    "claude-3-5-sonnet",
    "claude-3-5-haiku",
];

/// Oracle `!vot(model, remoteCall)`, with the multi-provider exit decided
/// DELIBERATELY against the verbatim reading.
///
/// Upstream only ever sees Anthropic ids, so "not in `gf`" and "newer than
/// `gf`" are the same statement there. In a multi-provider port they are not: a
/// `deepseek-*` id is not in `gf` either, and a verbatim port would hand every
/// third-party model a waiver on a guard whose whole purpose is preventing a
/// write to a file nobody read.
///
/// So the waiver requires the id to be RECOGNISABLY Anthropic and newer than
/// the set. An unrecognised id keeps the read requirement — the failure
/// direction is "occasionally demand one extra Read", which is recoverable,
/// against "silently overwrite unread content", which is not. (User decision,
/// 2026-09-14; the same question as TL-7 but with the opposite cost, so the
/// opposite answer.)
#[must_use]
pub fn model_waives_read_requirement(model: Option<&str>) -> bool {
    let Some(model) = model else {
        return false; // no model named ⇒ no waiver
    };
    let id = model.trim();
    if !id.starts_with("claude-") {
        return false; // LingXi divergence: unknown provider keeps the guard
    }
    !READ_REQUIRED_MODELS
        .iter()
        .any(|old| id == *old || id.starts_with(&format!("{old}-")))
}

/// Oracle `me = !F && !X_n(S) && !vot(V, remoteCall) && kq(...)` — whether a
/// write may proceed against a file with NO recorded read at all.
///
/// All three conjuncts must hold, and each fails SAFE on its own: a notebook
/// never qualifies, an unrecognised model never qualifies, and `kq` answers
/// `false` unless the model had a reader AND the path was readable.
#[must_use]
pub fn read_requirement_waived(model: Option<&str>, canon: &std::path::Path) -> bool {
    // `X_n(S)` — notebooks are excluded; their guard is cell-structured.
    if canon
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("ipynb"))
    {
        return false;
    }
    if !model_waives_read_requirement(model) {
        return false;
    }
    lingxi_core::host::read_auto_allow::read_auto_allowed(&canon.to_string_lossy())
}

pub fn check_read_before_write(
    map: &tool_api::read_file_state::ReadFileStateMap,
    canon: &std::path::Path,
    current_mtime_ms: i64,
    current_full_content: &str,
    waived: bool,
) -> Result<(), tool_api::tool_trait::ToolError> {
    use tool_api::tool_trait::ToolError;

    let entry = match tool_api::read_file_state::get(map, canon) {
        Some(e) => e,
        // No recorded read at all → refuse. (claude-code `FOg`: `if(!r) throw
        // PWn`.) A ranged / offset read still HAS an entry, so it is NOT
        // rejected here — it falls through to the mtime / content checks below.
        None => {
            // Oracle `if(!me) return errorCode 2`. The waiver applies ONLY to
            // the never-read case; a stale or partial read still falls through
            // to the checks below.
            if waived {
                return Ok(());
            }
            return Err(ToolError::InvalidInput(FILE_NOT_READ_ERROR.into()));
        }
    };

    // mtime not advanced past the recorded read → not stale. (claude-code:
    // `if(bY(e) <= r.timestamp) return false` — an equal floored mtime proceeds.)
    if current_mtime_ms <= entry.mtime_ms {
        return Ok(());
    }

    // mtime advanced: a full-file read (`wMe`) whose recorded content still
    // equals the current on-disk content is safe to proceed despite the bumped
    // mtime (cloud sync / antivirus can touch mtime without changing bytes).
    // (claude-code: `if(wMe(r) && RMe(r,t)) return false`.)
    if read_covers_full_file(&entry) && current_full_content == entry.content {
        return Ok(());
    }

    // The file changed on disk since the read → stale. FT-07: the oracle's
    // `validateInput` arm (Edit errorCode 7 / Write 3 / NotebookEdit 10) is the
    // branch a model normally hits, so emit ITS literal — not the call-phase
    // race sentence `WVo` ([`FILE_CONTENT_CHANGED_LINTER_MESSAGE`]).
    Err(ToolError::InvalidInput(
        FILE_UNEXPECTEDLY_MODIFIED_ERROR.into(),
    ))
}

/// claude-code `wMe(e)` (2.1.212) — whether a recorded read captured the
/// ENTIRE file, so its content may be compared against the current on-disk
/// bytes in the [`check_read_before_write`] staleness fallback.
///
/// ```text
/// wMe(e){
///   if((e.offset??1)>1||e.isPartialView) return false;      // ranged / truncated view
///   if(e.limit===void 0) return true;                        // unbounded read → whole file
///   return e.content!=="" && Cu(e.content,"\n")+1 < e.limit;  // limit not reached → EOF
/// }
/// ```
///
/// The `limit===void 0 → true` short-circuit is the 2.1.212 delta (absent in
/// 2.1.211). `isPartialView` (see the guard's Divergence note) is treated as
/// `false`. `Cu(content,"\n")+1` is claude-code's line count; a bounded read
/// whose line count is strictly below its `limit` reached EOF before the cap,
/// so it holds the whole file.
fn read_covers_full_file(entry: &tool_api::read_file_state::ReadFileEntry) -> bool {
    // (e.offset ?? 1) > 1 || e.isPartialView → a read that skipped leading
    // lines, or a recording whose content is NOT the file's on-disk bytes
    // (a seeded memory file with its frontmatter/HTML comments stripped, or a
    // token-cap-truncated read), is not full.
    if entry.offset.unwrap_or(1) > 1 || entry.is_partial_view {
        return false;
    }
    // e.limit === void 0 → unbounded read → whole file. (2.1.212 delta.)
    let Some(limit) = entry.limit else {
        return true;
    };
    // Bounded read: full iff the limit was never reached (line count strictly
    // below the cap ⇒ EOF hit first). Empty content never counts as full.
    !entry.content.is_empty() && (entry.content.matches('\n').count() as u64 + 1) < limit
}

/// Register all six file/search tools against `reg`.
///
/// TR-05: `MultiEdit` is deliberately NOT registered. 2.1.238 has no MultiEdit
/// tool object — all 12 binary hits are name strings (permission-rule advice
/// @282361899, the ultrareview allow-list `qTv` @290346662, the activity map
/// `qKT` @297388763, the trust-dialog set `XA0` @306721162, docs, V8-snapshot
/// copies), `sdk-tools-238.d.ts` has 0 hits, and the `"edits" in t` dispatch
/// shim this module once claimed has 0 hits in BOTH 2.1.238 and 2.1.220. The
/// name strings elsewhere in LingXi (permission classifier, turn_loop, skill
/// prefetch, TUI display maps) are kept — the oracle carries those too.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    register_all_with_live_cwd(reg, ctx, None);
}

/// Register all six file/search tools, injecting an optional shared live-cwd
/// cell (claude-code `getCwd()`/`Ct()`) into the tools that read the live cwd:
/// `Read` (the "File does not exist" note), `Glob`, and `Grep` (their default
/// search dir, "does not exist" notes, and result relativization). When `None`
/// (mobile / offline factory), every tool falls back to `ctx.workspace` —
/// byte-identical to [`register_all`]. The three write/edit tools and Notebook
/// take no cwd cell (they resolve absolute/trusted paths only).
pub fn register_all_with_live_cwd(
    reg: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
    live_cwd: Option<tool_api::LiveCwdCell>,
) {
    use std::sync::Arc;
    let read = match &live_cwd {
        Some(cell) => FileReadTool::new(ctx.clone()).with_live_cwd(cell.clone()),
        None => FileReadTool::new(ctx.clone()),
    };
    reg.register_builtin(Arc::new(read));
    reg.register_builtin(Arc::new(FileWriteTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(FileEditTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(NotebookEditTool::new(ctx.clone())));
    let glob = match &live_cwd {
        Some(cell) => GlobTool::new(ctx.clone()).with_live_cwd(cell.clone()),
        None => GlobTool::new(ctx.clone()),
    };
    reg.register_builtin(Arc::new(glob));
    let grep = match live_cwd {
        Some(cell) => GrepTool::new(ctx).with_live_cwd(cell),
        None => GrepTool::new(ctx),
    };
    reg.register_builtin(Arc::new(grep));
}

#[cfg(test)]
mod staleness_guard_tests {
    use super::*;
    use std::path::PathBuf;
    use tool_api::read_file_state::{new_read_file_state_map, set, ReadFileEntry};
    use tool_api::tool_trait::ToolError;

    #[test]
    fn file_not_read_error_is_byte_locked() {
        // FileEditTool.ts:280-281 / FileWriteTool.ts:202-203 /
        // NotebookEditTool.ts:225-226.
        assert_eq!(
            FILE_NOT_READ_ERROR,
            "File has not been read yet. Read it first before writing to it."
        );
    }

    #[test]
    fn modified_error_is_byte_locked_to_validate_phase_string() {
        // The model-facing validate-phase guard message — Edit errorCode 7
        // (cc-238.js @226407883), Write errorCode 3 (@226416137), NotebookEdit
        // errorCode 10 (@226495421). This is what the guard emits (FT-07).
        assert_eq!(
            FILE_UNEXPECTEDLY_MODIFIED_ERROR,
            "File has been modified since read, either by the user or by a linter. Read it again before attempting to write it."
        );
    }

    // ── `WVo`: the CALL-phase stale-file message (linter/formatter context) ──

    #[test]
    fn file_content_changed_linter_message_is_byte_locked() {
        // Oracle `WVo` (cc-238.js @220242969), thrown only from the call-phase
        // re-check `Ehv` (@226404058) / its Write twin (@226410437). Pinned here
        // as the oracle's OTHER branch; the guard emits the validateInput
        // literal (FT-07).
        assert_eq!(
            FILE_CONTENT_CHANGED_LINTER_MESSAGE,
            "File content has changed since it was last read. This commonly happens when a linter or formatter run via Bash rewrites the file. Call Read on this file to refresh, then retry the edit."
        );
        // Confirm the linter/formatter context text is present.
        assert!(FILE_CONTENT_CHANGED_LINTER_MESSAGE.contains("linter or formatter"));
        assert!(FILE_CONTENT_CHANGED_LINTER_MESSAGE.contains("Call Read on this file"));
    }

    #[test]
    fn validate_phase_message_emitted_on_stale_changed_content() {
        // FT-07: when mtime advanced AND content differs, the staleness guard
        // emits the oracle's validateInput literal
        // (FILE_UNEXPECTEDLY_MODIFIED_ERROR), NOT the call-phase race sentence
        // `WVo` (FILE_CONTENT_CHANGED_LINTER_MESSAGE).
        let map = new_read_file_state_map();
        let p = PathBuf::from("/x");
        set(
            &map,
            p.clone(),
            ReadFileEntry {
                content: "old content".into(),
                mtime_ms: 100,
                offset: None,
                limit: None,
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
        let r = check_read_before_write(&map, &p, 200, "new content", false);
        match r.unwrap_err() {
            tool_api::tool_trait::ToolError::InvalidInput(m) => {
                assert_eq!(m, FILE_UNEXPECTEDLY_MODIFIED_ERROR);
            }
            other => panic!("expected InvalidInput with the validateInput literal, got {other:?}"),
        }
    }

    #[test]
    fn partial_view_entry_is_not_a_full_read() {
        // `Aze`/`wMe` (@232452900): `if((e.offset??1)>1||e.isPartialView)
        // return false`. A partial-view entry can never take the
        // content-equality fallback, even when its recorded content happens to
        // equal the file's current content — because that content is NOT what
        // the model saw (a seeded memory file has its frontmatter stripped).
        let map = new_read_file_state_map();
        let p = PathBuf::from("/partial");
        set(
            &map,
            p.clone(),
            ReadFileEntry {
                content: "body\n".into(),
                mtime_ms: 100,
                offset: None,
                limit: None,
                from_read: false,
                seeded_from_context: true,
                is_partial_view: true,
            },
        );
        // mtime ADVANCED past the recorded read, content byte-identical.
        let r = check_read_before_write(&map, &p, 200, "body\n", false);
        match r.unwrap_err() {
            tool_api::tool_trait::ToolError::InvalidInput(m) => {
                assert_eq!(m, FILE_UNEXPECTEDLY_MODIFIED_ERROR);
            }
            other => panic!("expected InvalidInput with the validateInput literal, got {other:?}"),
        }
        // Control: the SAME entry with `is_partial_view: false` proceeds via
        // the content-equality fallback — proving the flag is what refused.
        set(
            &map,
            p.clone(),
            ReadFileEntry {
                content: "body\n".into(),
                mtime_ms: 100,
                offset: None,
                limit: None,
                from_read: false,
                seeded_from_context: true,
                is_partial_view: false,
            },
        );
        assert!(check_read_before_write(&map, &p, 200, "body\n", false).is_ok());
    }

    /// Assert the guard returned an `InvalidInput` error carrying exactly
    /// `expected` as its message (the `ToolError::Display` impl prefixes
    /// `"invalid input: "`, so we match on the `InvalidInput` payload).
    fn assert_err_msg(r: Result<(), ToolError>, expected: &str) {
        match r.unwrap_err() {
            ToolError::InvalidInput(m) => assert_eq!(m, expected),
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn guard_missing_entry_is_not_read() {
        let map = new_read_file_state_map();
        let r = check_read_before_write(&map, &PathBuf::from("/x"), 100, "content", false);
        assert_err_msg(r, FILE_NOT_READ_ERROR);
    }

    #[test]
    fn guard_offset_or_limit_read_unchanged_mtime_proceeds() {
        // 2.1.212: an offset/limit read is NOT rejected as "not read" just for
        // being ranged — with the mtime unchanged, the edit proceeds. (Before
        // 2.1.212 this raised FILE_NOT_READ_ERROR before the mtime check.)
        let map = new_read_file_state_map();
        let p = PathBuf::from("/x");
        // offset present, mtime unchanged ⇒ proceed.
        set(
            &map,
            p.clone(),
            ReadFileEntry {
                content: "c".into(),
                mtime_ms: 100,
                offset: Some(1),
                limit: None,
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
        assert!(check_read_before_write(&map, &p, 100, "c", false).is_ok());
        // limit present (offset None), mtime unchanged ⇒ proceed.
        set(
            &map,
            p.clone(),
            ReadFileEntry {
                content: "c".into(),
                mtime_ms: 100,
                offset: None,
                limit: Some(5),
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
        assert!(check_read_before_write(&map, &p, 100, "c", false).is_ok());
    }

    #[test]
    fn guard_ranged_stale_read_is_content_changed_not_not_read() {
        // A genuinely-partial (offset-skipped) read whose file changed on disk
        // yields the CONTENT-CHANGED (linter) message, NOT "not read" — so the
        // Edit tool's stale-recovery path can still apply. (claude-code `FOg`
        // has no entry ⇒ throw PWn; otherwise mtime-advanced ⇒ throw LWn.)
        let map = new_read_file_state_map();
        let p = PathBuf::from("/x");
        set(
            &map,
            p.clone(),
            ReadFileEntry {
                content: "line6\nline7".into(),
                mtime_ms: 100,
                offset: Some(6),
                limit: Some(2),
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
        assert_err_msg(
            check_read_before_write(&map, &p, 200, "whole new file", false),
            FILE_UNEXPECTEDLY_MODIFIED_ERROR,
        );
    }

    #[test]
    fn read_covers_full_file_matches_wme() {
        let mk = |offset, limit, content: &str| ReadFileEntry {
            content: content.into(),
            mtime_ms: 0,
            offset,
            limit,
            from_read: true,
            seeded_from_context: false,
            is_partial_view: false,
        };
        // Full read (no offset/limit) → whole file.
        assert!(read_covers_full_file(&mk(None, None, "a\nb")));
        // offset==1 with no limit → whole file ((offset??1)>1 is false).
        assert!(read_covers_full_file(&mk(Some(1), None, "a\nb")));
        // 2.1.212 delta: any unbounded read (limit==None) → whole file.
        assert!(read_covers_full_file(&mk(None, None, "")));
        // offset>1 → ranged, not full.
        assert!(!read_covers_full_file(&mk(Some(2), None, "a\nb")));
        // limit not reached (2 lines < limit 5) → EOF captured → full.
        assert!(read_covers_full_file(&mk(None, Some(5), "a\nb")));
        // limit reached exactly (3 lines, limit 3) → maybe more below → not full.
        assert!(!read_covers_full_file(&mk(None, Some(3), "a\nb\nc")));
        // empty content with a limit → not full.
        assert!(!read_covers_full_file(&mk(None, Some(5), "")));
    }

    #[test]
    fn regression_offset_limit_full_read_unchanged_proceeds() {
        // 2.1.212 gap: a read done WITH offset/limit that captured the whole
        // file (e.g. `Read(offset=1, limit=2000)` on a small file) must NOT be
        // rejected as "not read". With mtime unchanged, the edit proceeds.
        let map = new_read_file_state_map();
        let p = PathBuf::from("/x");
        set(
            &map,
            p.clone(),
            ReadFileEntry {
                content: "a\nb\nc".into(),
                mtime_ms: 100,
                offset: Some(1),
                limit: Some(2000),
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
        assert!(
            check_read_before_write(&map, &p, 100, "a\nb\nc", false).is_ok(),
            "offset/limit full-read + unchanged mtime must proceed"
        );
    }

    #[test]
    fn guard_unchanged_mtime_proceeds() {
        let map = new_read_file_state_map();
        let p = PathBuf::from("/x");
        set(
            &map,
            p.clone(),
            ReadFileEntry {
                content: "c".into(),
                mtime_ms: 100,
                offset: None,
                limit: None,
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
        // current == recorded ⇒ not stale ⇒ Ok.
        assert!(check_read_before_write(&map, &p, 100, "c", false).is_ok());
    }

    #[test]
    fn guard_newer_mtime_same_content_proceeds_via_fallback() {
        let map = new_read_file_state_map();
        let p = PathBuf::from("/x");
        set(
            &map,
            p.clone(),
            ReadFileEntry {
                content: "same".into(),
                mtime_ms: 100,
                offset: None,
                limit: None,
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
        // mtime advanced but content matches ⇒ fallback proceeds.
        assert!(check_read_before_write(&map, &p, 200, "same", false).is_ok());
    }

    #[test]
    fn guard_newer_mtime_different_content_is_modified() {
        let map = new_read_file_state_map();
        let p = PathBuf::from("/x");
        set(
            &map,
            p.clone(),
            ReadFileEntry {
                content: "old".into(),
                mtime_ms: 100,
                offset: None,
                limit: None,
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
        // FT-07: returns the oracle's `validateInput` literal (Edit errorCode 7 /
        // Write 3 / NotebookEdit 10) — the branch a model normally hits — not
        // the call-phase race sentence `WVo`.
        assert_err_msg(
            check_read_before_write(&map, &p, 200, "new", false),
            FILE_UNEXPECTEDLY_MODIFIED_ERROR,
        );
    }

    /// Oracle `gf`: these ids keep the read requirement, exactly as upstream.
    #[test]
    fn the_old_model_set_still_requires_a_read() {
        for old in [
            "claude-opus-4-6",
            "claude-haiku-4-5",
            "claude-opus-4-5",
            "claude-opus-4-1",
            "claude-opus-4-0",
            "claude-sonnet-4-5",
            "claude-sonnet-4-0",
            "claude-3-7-sonnet",
            "claude-3-5-sonnet",
            "claude-3-5-haiku",
        ] {
            assert!(
                !model_waives_read_requirement(Some(old)),
                "{old} is in `gf` and must still read before writing"
            );
        }
    }

    /// A NEWER Anthropic id takes the waiver, which is upstream's whole point.
    #[test]
    fn a_newer_anthropic_model_takes_the_waiver() {
        assert!(model_waives_read_requirement(Some("claude-opus-4-8")));
        assert!(model_waives_read_requirement(Some("claude-fable-5-1")));
    }

    /// THE DELIBERATE DIVERGENCE. Upstream only ever sees Anthropic ids, so
    /// "not in `gf`" and "newer than `gf`" coincide there. Here they do not: a
    /// third-party id is not in `gf` either, and a verbatim port would hand it
    /// a waiver on a guard that exists to stop a write to an unread file.
    ///
    /// The chosen failure direction is "occasionally demand one extra Read"
    /// (recoverable) over "silently overwrite unread content" (not).
    #[test]
    fn an_unrecognised_provider_keeps_the_read_requirement() {
        for foreign in [
            "deepseek-v3",
            "gpt-4o",
            "qwen-max",
            "my-local-model",
            "",
            "Claude-Opus-4-8",
        ] {
            assert!(
                !model_waives_read_requirement(Some(foreign)),
                "{foreign:?} is not a recognised Anthropic id, so it must keep \
                 the read requirement rather than inherit `newer ⇒ trusted`"
            );
        }
        assert!(
            !model_waives_read_requirement(None),
            "no model named at all ⇒ no waiver"
        );
    }

    /// The two conjuncts that short-circuit BEFORE `kq` — so this test does not
    /// depend on the process-global probe, which another test in this binary
    /// publishes. (Asserting the unpublished answer here made the result depend
    /// on test order.)
    #[test]
    fn a_notebook_or_an_unknown_model_vetoes_the_waiver_on_its_own() {
        let notebook = PathBuf::from("/w/a.ipynb");
        assert!(
            !read_requirement_waived(Some("claude-opus-4-8"), &notebook),
            "a notebook never qualifies, whatever the model or the policy says"
        );
        assert!(
            !read_requirement_waived(Some("CLAUDE-OPUS-4-8.IPYNB"), &notebook),
            "the extension check is case-insensitive"
        );
        assert!(
            !read_requirement_waived(Some("deepseek-v3"), &PathBuf::from("/w/a.rs")),
            "an unrecognised provider vetoes before the path is even consulted"
        );
        assert!(!read_requirement_waived(None, &PathBuf::from("/w/a.rs")));
    }

    /// The waiver is scoped to the NEVER-READ case (`!F`) and nothing else.
    ///
    /// Upstream computes it inside `if (!F || F.isPartialView)` and consults it
    /// only on the `!F` branch; a file that WAS read and has since changed on
    /// disk still errors, waiver or not. Widening it to the stale branch would
    /// let a waived model overwrite content it read an older version of — and
    /// that widening previously passed every other test in this file.
    #[test]
    fn the_waiver_does_not_excuse_a_stale_read() {
        let map = new_read_file_state_map();
        let p = PathBuf::from("/x");
        set(
            &map,
            p.clone(),
            ReadFileEntry {
                content: "old content".into(),
                mtime_ms: 100,
                offset: None,
                limit: None,
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
        // mtime advanced AND content differs: stale, and `waived` is true.
        let r = check_read_before_write(&map, &p, 200, "new content", true);
        match r.unwrap_err() {
            tool_api::tool_trait::ToolError::InvalidInput(m) => {
                assert_eq!(
                    m, FILE_UNEXPECTEDLY_MODIFIED_ERROR,
                    "a waived model must still be told the file changed"
                );
            }
            other => panic!("expected the staleness error, got {other:?}"),
        }
    }

    /// …and it DOES excuse the never-read case, which is the whole point.
    #[test]
    fn the_waiver_excuses_a_file_that_was_never_read() {
        let map = new_read_file_state_map();
        let p = PathBuf::from("/x");
        assert!(
            check_read_before_write(&map, &p, 100, "content", false).is_err(),
            "premise: with no waiver a never-read file is refused"
        );
        assert!(
            check_read_before_write(&map, &p, 100, "content", true).is_ok(),
            "with the waiver it proceeds"
        );
    }
}

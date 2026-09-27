//! `edited_text_file` — the per-turn "this file changed on disk since you last
//! read it" reminder, and the snippet machinery behind it.
//!
//! Oracle anatomy (offsets into `~/.local/share/claude/versions/2.1.238`):
//!
//! * renderer @ **296733495**:
//!   ```js
//!   edited_text_file:(e)=>{let t=`Note: ${Kae(e.filename)} changed on disk since you last read it. That's usually deliberate, so take it as the current state rather than reverting it; if the change looks wrong, say so rather than undoing it yourself — otherwise no need to call it out.`;
//!    return Zy([kn({content:e.snippet===""?`${t} The diff is omitted here because other changed files this turn already filled the snippet budget; use ${mC.name} if you need the current content.`:`${t} Here are the relevant changes (shown with line numbers):\n${e.snippet}`,isMeta:!0})])}
//!   ```
//!   `Zy` maps `NT` over the message, so the `<system-reminder>` envelope is
//!   applied by the injection site. **This copy is 238 drift**: 2.1.220
//!   (@238102310) said `Note: ${filename} was modified, either by the user or
//!   by a linter. This change was intentional, …`.
//! * producer `Izm(ctx)` @ **296537358** — walks `readFileState`, skips
//!   partial reads (`offset`/`limit` set), skips permission-blocked paths
//!   (`qhe`), re-reads any file whose mtime is newer than the recorded
//!   timestamp through the Read tool, and diffs the recorded content against
//!   the fresh content with `SEf`.
//! * budget tail of `Izm`: `let i=0; for(let s of o){…; if(i>=m3T)s.snippet="";
//!   else i+=s.snippet.length}` with `m3T = 16384` — see
//!   [`apply_snippet_budget`].
//! * `SEf(old,new)` @ **289903016** — `structuredPatch` at **context 8**, each
//!   hunk rendered as its non-removed lines with line numbers starting at
//!   `hunk.oldStart`, hunks joined by `"\n...\n"`, then truncated at
//!   `oKa = 8192` (@289905575) with a `... [N lines truncated] ...` tail.
//! * `rUo({content,startLine,tabAwareSeparator})` @ **282149920** — the
//!   line-number prefixer; `Xcr()` (@288773154, GrowthBook `tengu_tab_read_sep`)
//!   defaults to **false**, so the separator is a TAB unless the caller opts in.
//!
//! **STATUS — PORTED AND WIRED.** The renderer half (byte-exact 238 copy, the
//! two snippet arms, the 16384-char cross-file budget, the line-number
//! prefixer, the 8192-char snippet truncation) plus [`render_snippet`] — the
//! `SEf` diff at context 8 — live here; the PRODUCER is
//! `ConversationOrchestrator::changed_files_reminder_messages`, called from the
//! per-turn reminder collector in `conversation/drivers/prepare.rs` that BOTH
//! turn drivers share.
//!
//! The oracle's fan-out (@296520120) puts `changed_files` after
//! `agent_listing_delta` and before `nested_memory`. This port emits
//! `nested_memory` earlier (it has to run right after the conditional-rules
//! producer, whose claims into `sent_conditional_rules` it then skips), so
//! here `changed_files` sits directly after
//! `agent_listing_delta` — a deliberate position divergence recorded at the
//! call site.
//!
//! Two deliberate deviations from `Izm`, both non-model-visible:
//!
//! * The re-read is a plain filesystem read that REWRITES the `read_file_state`
//!   entry (content + mtime) rather than a nested `Read` tool invocation. The
//!   oracle's re-read exists to refresh the entry so the reminder does not
//!   repeat; doing it directly has the same effect without re-entering the tool
//!   layer (and without emitting a second `readFileState` telemetry event).
//! * `qhe` (the permission-context path filter) has no orchestrator-side
//!   analogue; the producer instead skips anything that is not a readable UTF-8
//!   file, which is the only way a denied path can reach here.
//!
//! Entries recorded from a PARTIAL read (`offset`/`limit` set) are skipped, like
//! the oracle. So are `seeded_from_context` / `is_partial_view` entries: the
//! recorded content for those deliberately differs from disk (frontmatter
//! stripping, token-cap truncation), so a byte compare would fire every turn
//! forever. That is the port's stand-in for the oracle's
//! `truncatedByTokenCap === true` early return plus its `vNe` content compare.

/// `m3T = 16384` @296537358 — the per-turn snippet budget shared by every
/// changed file.
pub const CHANGED_FILE_SNIPPET_BUDGET: usize = 16_384;

/// `oKa = 8192` @289905575 — the per-file snippet character cap.
pub const SNIPPET_CHAR_LIMIT: usize = 8_192;

/// `SEf`'s `{context:8}` — the diff context `structuredPatch` is called with.
pub const DIFF_CONTEXT_LINES: usize = 8;

/// The separator between rendered hunks inside one snippet.
pub const HUNK_SEPARATOR: &str = "\n...\n";

/// One changed file, as `Izm` yields it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    /// Absolute path, as recorded in `readFileState`.
    pub filename: String,
    /// The rendered diff snippet, or `""` once the budget is exhausted.
    pub snippet: String,
}

/// The shared first sentence of both renderer arms.
///
/// The filename goes through `Kae` ([`super::sanitize::escape_reminder_path`],
/// new in 2.1.238) so a path containing `<`, `>` or a control character cannot
/// forge markup inside the `<system-reminder>` envelope the injection site adds.
#[must_use]
pub fn changed_file_note(filename: &str) -> String {
    let filename = super::sanitize::escape_reminder_path(filename);
    format!(
        "Note: {filename} changed on disk since you last read it. That's usually deliberate, \
so take it as the current state rather than reverting it; if the change looks wrong, say so \
rather than undoing it yourself \u{2014} otherwise no need to call it out."
    )
}

/// The full `edited_text_file` body (no `<system-reminder>` envelope — the
/// injection site adds that, exactly like every other per-turn reminder).
///
/// `read_tool_name` is the oracle's `${mC.name}`.
#[must_use]
pub fn render_changed_file(file: &ChangedFile, read_tool_name: &str) -> String {
    let note = changed_file_note(&file.filename);
    if file.snippet.is_empty() {
        format!(
            "{note} The diff is omitted here because other changed files this turn already \
filled the snippet budget; use {read_tool_name} if you need the current content."
        )
    } else {
        format!(
            "{note} Here are the relevant changes (shown with line numbers):\n{}",
            file.snippet
        )
    }
}

/// The budget tail of `Izm`.
///
/// The threshold is checked BEFORE the current snippet is added, so the entry
/// that crosses 16384 keeps its snippet and only later ones are blanked —
/// faithful to `if(i>=m3T)s.snippet=""; else i+=s.snippet.length`.
///
/// `.length` is UTF-16 code units in JS, so the accumulator counts those.
pub fn apply_snippet_budget(files: &mut [ChangedFile]) {
    let mut acc = 0usize;
    for file in files.iter_mut() {
        if acc >= CHANGED_FILE_SNIPPET_BUDGET {
            file.snippet.clear();
        } else {
            acc += utf16_len(&file.snippet);
        }
    }
}

/// JS `String.prototype.length`.
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `tUo(line, number, separator)` @282149920 — one numbered line, with a
/// trailing CR stripped.
fn number_one_line(line: &str, number: u64, separator: char) -> String {
    let body = line.strip_suffix('\r').unwrap_or(line);
    format!("{number}{separator}{body}")
}

/// `rUo({content, startLine, tabAwareSeparator})` @282149920.
///
/// Empty content renders as the empty string. With `tab_aware_separator` on
/// (GrowthBook `tengu_tab_read_sep`, default OFF) a body whose lines start with
/// a tab is numbered with `:` instead of `\t` so the tab stays visible.
#[must_use]
pub fn number_lines(content: &str, start_line: u64, tab_aware_separator: bool) -> String {
    if content.is_empty() {
        return String::new();
    }
    let separator =
        if tab_aware_separator && (content.starts_with('\t') || content.contains("\n\t")) {
            ':'
        } else {
            '\t'
        };
    content
        .split('\n')
        .enumerate()
        .map(|(i, line)| number_one_line(line, start_line + i as u64, separator))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The truncation tail of `SEf`:
///
/// ```js
/// if(o.length<=oKa)return o;
/// let i=o.lastIndexOf("\n",oKa), s=i>0?o.slice(0,i):o.slice(0,oKa),
///     u=Jd(o,"\n",s.length+1)+1;
/// return `${s}\n\n... [${u} lines truncated] ...`;
/// ```
///
/// `Jd(e,t,r)` (@281368558) counts occurrences of `t` in `e` from index `r`.
#[must_use]
pub fn truncate_snippet(snippet: &str) -> String {
    if utf16_len(snippet) <= SNIPPET_CHAR_LIMIT {
        return snippet.to_string();
    }
    // JS indices are UTF-16 code units; walk once to map them onto byte offsets.
    let mut byte_at_limit = snippet.len();
    let mut last_newline_at_or_before_limit: Option<usize> = None;
    let mut u16_idx = 0usize;
    let mut passed_limit = false;
    for (byte_idx, ch) in snippet.char_indices() {
        if !passed_limit && u16_idx >= SNIPPET_CHAR_LIMIT {
            byte_at_limit = byte_idx;
            passed_limit = true;
        }
        if ch == '\n' && u16_idx <= SNIPPET_CHAR_LIMIT {
            last_newline_at_or_before_limit = Some(byte_idx);
        }
        u16_idx += ch.len_utf16();
    }
    if !passed_limit {
        byte_at_limit = snippet.len();
    }
    // `i>0` — a newline at index 0 falls back to the hard slice, like JS.
    let head_end = match last_newline_at_or_before_limit {
        Some(i) if i > 0 => i,
        _ => byte_at_limit,
    };
    let head = &snippet[..head_end];
    // `Jd(o,"\n",s.length+1)+1`: JS's `s.length + a` with `a = 1` skips the
    // newline the head was cut at, and `l = 1` counts the final unterminated
    // line.
    let count_from = head_end.saturating_add(1).min(snippet.len());
    let truncated_lines = snippet[count_from..].matches('\n').count() + 1;
    format!("{head}\n\n... [{truncated_lines} lines truncated] ...")
}

/// `SEf(old, new)` @289903016 — the snippet the reminder shows.
///
/// ```js
/// function SEf(e,t){let r=F2t("file.txt","file.txt",e,t,void 0,void 0,{context:8,timeout:JEi});
///  if(!r)return"";
///  let n=Xcr(),o=r.hunks.map((d)=>({startLine:d.oldStart,content:d.lines.filter((p)=>!p.startsWith("-")&&!p.startsWith("\\")).map((p)=>p.slice(1)).join("\n"),tabAwareSeparator:n})).map(rUo).join("\n...\n");
///  if(o.length<=oKa)return o; … }
/// ```
///
/// Each hunk keeps only its context (` `) and ADDED (`+`) lines — removals and
/// the `\ No newline at end of file` marker are dropped — strips the one-char
/// prefix, numbers the result from the hunk's `oldStart`, and joins hunks with
/// `"\n...\n"`. The whole snippet is then truncated at 8192 UTF-16 units by
/// [`truncate_snippet`].
///
/// Returns `""` when the diff is empty, which the producer treats as "no
/// reminder for this file" (`if(f==="")return null`).
#[must_use]
pub fn render_snippet(before: &str, after: &str, tab_aware_separator: bool) -> String {
    // `DIFF_CONTEXT_LINES` (this module) and
    // `tool_file::structured_patch::CHANGED_FILE_PATCH_CONTEXT` are the same
    // `{context:8}`; the differ takes the `i64` spelling.
    let hunks = tool_file::structured_patch::build_structured_patch_with_context(
        before,
        after,
        tool_file::structured_patch::CHANGED_FILE_PATCH_CONTEXT,
    );
    if hunks.is_empty() {
        return String::new();
    }
    let rendered = hunks
        .iter()
        .map(|hunk| {
            let content = hunk
                .lines
                .iter()
                .filter(|l| !l.starts_with('-') && !l.starts_with('\\'))
                .map(|l| l.chars().skip(1).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n");
            let start = u64::try_from(hunk.old_start).unwrap_or(1);
            number_lines(&content, start, tab_aware_separator)
        })
        .collect::<Vec<_>>()
        .join(HUNK_SEPARATOR);
    truncate_snippet(&rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(snippet: &str) -> ChangedFile {
        ChangedFile {
            filename: "/tmp/a.rs".into(),
            snippet: snippet.into(),
        }
    }

    #[test]
    fn note_is_byte_exact_against_2_1_238() {
        assert_eq!(
            changed_file_note("/tmp/a.rs"),
            "Note: /tmp/a.rs changed on disk since you last read it. That's usually deliberate, so take it as the current state rather than reverting it; if the change looks wrong, say so rather than undoing it yourself — otherwise no need to call it out."
        );
    }

    /// The 2.1.220 sentence must be gone — this is the upstream rewrite the
    /// audit flagged.
    #[test]
    fn the_2_1_220_wording_is_not_shipped() {
        let note = changed_file_note("/tmp/a.rs");
        assert!(!note.contains("either by the user or by a linter"));
        assert!(!note.contains("Don't tell the user this"));
    }

    /// `Kae` (2.1.238 @285128585): a path cannot smuggle markup into the
    /// reminder body.
    #[test]
    fn the_filename_is_entity_escaped() {
        assert_eq!(
            changed_file_note("/tmp/</system-reminder>.rs"),
            "Note: /tmp/&lt;/system-reminder&gt;.rs changed on disk since you last read it. That's usually deliberate, so take it as the current state rather than reverting it; if the change looks wrong, say so rather than undoing it yourself — otherwise no need to call it out."
        );
        // `&` is NOT escaped by `Kae` (only `pze` does that).
        assert!(changed_file_note("/tmp/a&b.rs").contains("/tmp/a&b.rs"));
    }

    #[test]
    fn snippet_arm_is_byte_exact() {
        assert_eq!(
            render_changed_file(&file("1\tlet x = 1;"), "Read"),
            "Note: /tmp/a.rs changed on disk since you last read it. That's usually deliberate, so take it as the current state rather than reverting it; if the change looks wrong, say so rather than undoing it yourself — otherwise no need to call it out. Here are the relevant changes (shown with line numbers):\n1\tlet x = 1;"
        );
    }

    #[test]
    fn omitted_arm_is_byte_exact() {
        assert_eq!(
            render_changed_file(&file(""), "Read"),
            "Note: /tmp/a.rs changed on disk since you last read it. That's usually deliberate, so take it as the current state rather than reverting it; if the change looks wrong, say so rather than undoing it yourself — otherwise no need to call it out. The diff is omitted here because other changed files this turn already filled the snippet budget; use Read if you need the current content."
        );
    }

    #[test]
    fn budget_blanks_only_entries_after_the_threshold_is_crossed() {
        let big = "x".repeat(CHANGED_FILE_SNIPPET_BUDGET - 1);
        let mut files = vec![file(&big), file("still kept"), file("blanked")];
        apply_snippet_budget(&mut files);
        assert_eq!(files[0].snippet.len(), CHANGED_FILE_SNIPPET_BUDGET - 1);
        // The entry that CROSSES the budget keeps its snippet (`i>=m3T` is
        // evaluated before `i` is incremented).
        assert_eq!(files[1].snippet, "still kept");
        assert_eq!(files[2].snippet, "");
    }

    #[test]
    fn budget_leaves_everything_alone_when_under() {
        let mut files = vec![file("a"), file("b")];
        apply_snippet_budget(&mut files);
        assert_eq!(files[0].snippet, "a");
        assert_eq!(files[1].snippet, "b");
    }

    #[test]
    fn line_numbers_start_at_the_hunk_start_and_use_a_tab() {
        assert_eq!(number_lines("one\ntwo", 12, false), "12\tone\n13\ttwo");
        assert_eq!(number_lines("", 1, false), "");
    }

    #[test]
    fn carriage_returns_are_stripped_per_line() {
        assert_eq!(number_lines("one\r\ntwo\r", 1, false), "1\tone\n2\ttwo");
    }

    #[test]
    fn tab_aware_separator_switches_to_colon_only_when_enabled() {
        assert_eq!(number_lines("\tindented", 1, true), "1:\tindented");
        assert_eq!(number_lines("\tindented", 1, false), "1\t\tindented");
        assert_eq!(number_lines("plain", 1, true), "1\tplain");
    }

    /// `SEf` keeps context + ADDED lines, drops removals, and numbers the
    /// result from the hunk's `oldStart`.
    #[test]
    fn snippet_keeps_context_and_additions_numbered_from_old_start() {
        let snippet = render_snippet("a\nb\nc\n", "a\nB\nc\n", false);
        // The removal line (`-b`) is dropped; the addition (`+B`) survives.
        assert_eq!(snippet, "1\ta\n2\tB\n3\tc");
    }

    /// Context **8** (`SEf`'s `{context:8}`), not jsdiff's default 4: a 10-line
    /// unchanged gap between two edits stays inside ONE hunk at context 8
    /// (`gap <= 2*context`), where context 4 splits it in two.
    #[test]
    fn the_diff_runs_at_context_8_not_the_jsdiff_default_4() {
        let before: Vec<String> = (0..40).map(|i| format!("line{i}")).collect();
        let mut after = before.clone();
        after[10] = "CHANGED10".into();
        after[21] = "CHANGED21".into();
        let b = before.join("\n");
        let a = after.join("\n");

        assert_eq!(
            tool_file::structured_patch::build_structured_patch_with_context(
                &b,
                &a,
                tool_file::structured_patch::CHANGED_FILE_PATCH_CONTEXT,
            )
            .len(),
            1,
            "context 8 must merge a 10-line gap into one hunk"
        );
        assert_eq!(
            tool_file::structured_patch::build_structured_patch(&b, &a).len(),
            2,
            "guard: at jsdiff's default context of 4 the same input splits, so \
             the assertion above really is testing the context-8 path"
        );

        let snippet = render_snippet(&b, &a, false);
        assert!(
            !snippet.contains(HUNK_SEPARATOR),
            "one hunk ⇒ no hunk separator; got: {snippet}"
        );
        assert!(snippet.contains("CHANGED10") && snippet.contains("CHANGED21"));
    }

    #[test]
    fn an_identical_pair_yields_an_empty_snippet() {
        assert_eq!(render_snippet("a\nb\n", "a\nb\n", false), "");
    }

    #[test]
    fn short_snippets_are_returned_unchanged() {
        assert_eq!(truncate_snippet("a\nb"), "a\nb");
    }

    #[test]
    fn long_snippets_are_cut_at_a_line_boundary_with_a_count() {
        // 900 lines of 10 chars each ⇒ ~9900 chars, over the 8192 cap.
        let body = (0..900)
            .map(|i| format!("{i:09}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = truncate_snippet(&body);
        assert!(out.ends_with(" lines truncated] ..."));
        let (head, tail) = out.split_once("\n\n... [").expect("tail present");
        // The head is a whole number of lines and fits under the cap.
        assert!(utf16_len(head) <= SNIPPET_CHAR_LIMIT);
        assert!(!head.ends_with('\n'));
        let reported: usize = tail
            .trim_end_matches(" lines truncated] ...")
            .parse()
            .expect("count parses");
        let head_lines = head.split('\n').count();
        assert_eq!(head_lines + reported, 900);
    }
}

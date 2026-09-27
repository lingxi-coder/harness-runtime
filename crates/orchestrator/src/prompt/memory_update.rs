//! `memory_update` — the reminder claude-code emits after a BACKGROUND process
//! rewrites the user's memory directory, so the model learns that files it may
//! be holding in context are now stale on disk.
//!
//! Oracle anatomy (offsets into `~/.local/share/claude/versions/2.1.238`;
//! present unchanged in 2.1.220, so this is a long-standing port gap, not
//! 2.1.238 drift):
//!
//! * renderer @ **296705761** (the `xBi` switch):
//!   ```js
//!   case"memory_update":{
//!     let o=[`${F5T[e.source]} updated your memory directory: ${e.summary}`],
//!         i=AP(e.paths), s=AP(e.inContextPaths);
//!     if(i.length>0)o.push(`Files changed: ${i.map(Kae).join(", ")}`);
//!     if(s.length>0)o.push(`Your loaded copy of ${s.map(Kae).join(", ")} is now stale relative to disk — Read it again if you need current contents.`);
//!     return o.push($io),Zy([kn({content:o.join(`\n`),isMeta:!0})])}
//!   ```
//! * `F5T={dream:"Background memory consolidation"}` @ **296741219** — the only
//!   source the table names.
//! * `$io` @ **296730196** — the shared ambient-context trailer, pushed
//!   unconditionally as the LAST line.
//! * `Kae` @ **285128585** — the filename escape
//!   ([`super::sanitize::escape_reminder_path`]).
//! * `AP` @ **281462785** — `Array.isArray` + per-element `typeof === "string"`
//!   guard for a REPLAYED JSONL attachment. Rust's `&[String]` is that guard,
//!   so it has no analogue here.
//! * producer `jzm(ctx)` @ **296554545**:
//!   ```js
//!   function jzm(e){let t=e.getAppState().pendingMemoryUpdates;if(t.length===0)return[];
//!     e.setAppState(…clear…);
//!     let n=…memory index path…, o=XAa(e.session),
//!         i=(s)=>s===n||o.has(s)||e.readFileState.has(s)||e.loadedNestedMemoryPaths?.[s]===!0;
//!     return t.map((s)=>({type:"memory_update",source:s.source,summary:s.summary,
//!                         paths:s.paths,inContextPaths:s.paths.filter(i)}))}
//!   ```
//!   i.e. a consume-once drain of an app-state queue, with `inContextPaths` =
//!   the changed paths the model is currently holding (memory-index file,
//!   session-memory set, `readFileState`, or a loaded nested-memory file).
//!   [`select_in_context_paths`] is that filter, minus the host lookups the
//!   caller owns.
//!
//! **STATUS — PORTED AND WIRED.** The producer is
//! `ConversationOrchestrator::memory_update_reminder_messages`, called from the
//! streaming turn driver's per-turn reminder fan-out in `conversation.rs`
//! directly after the `<task-notification>` drain. The queue
//! (`ConversationOrchestrator::pending_memory_updates`, this module's
//! [`PendingMemoryUpdate`]) is filled by that same drain: the port's background
//! consolidator is a FORKED SUBAGENT whose completion only surfaces as a
//! terminal `dream` task notification, so that is where the oracle's
//! `setAppState({pendingMemoryUpdates:[…]})` enqueue lands here.
//!
//! Two reconstructions, because the port's dream task carries less than the
//! oracle's queue entry:
//!
//! * `summary` — the oracle's consolidator supplies it directly. Here it is the
//!   dream agent's own final text (`TaskNotification::result`), which is exactly
//!   what `dream.rs`'s prompt asks for ("Return a brief summary of what you
//!   consolidated, updated, or pruned"), falling back to the task description.
//!   No copy is invented.
//! * `paths` — recomputed by listing the user memdir for files whose mtime is
//!   newer than the previous scan, instead of being reported by the writer.

/// `F5T` @296741219 — the only `source` the oracle's label table defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryUpdateSource {
    /// `dream` ⇒ `"Background memory consolidation"`.
    Dream,
}

impl MemoryUpdateSource {
    /// `F5T[e.source]`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Dream => "Background memory consolidation",
        }
    }
}

/// `$io` @296730196 — the shared ambient-context trailer.
///
/// Byte-identical to the copy `tool_api`'s deferred-tools reminder carries
/// (`tool-api/src/defer.rs`, where it is private); duplicated rather than
/// re-exported so this module owns its own oracle citation.
pub const AMBIENT_CONTEXT_TRAILER: &str = "This is ambient context \u{2014} do not narrate it to the user unless they ask or it is directly relevant to their request.";

/// A QUEUED memory update, before the caller has resolved which of the changed
/// paths are still in the model's context.
///
/// The oracle queues `{source, summary, paths}` at write time and computes
/// `inContextPaths` at drain time (`jzm`'s `i` predicate). This is that queue
/// entry; [`MemoryUpdate`] is what the drain turns it into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMemoryUpdate {
    /// `e.source`.
    pub source: MemoryUpdateSource,
    /// `e.summary`.
    pub summary: String,
}

/// One drained `pendingMemoryUpdates` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryUpdate {
    /// `e.source`.
    pub source: MemoryUpdateSource,
    /// `e.summary` — the consolidator's own one-line description.
    pub summary: String,
    /// `e.paths` — every memory file the background pass rewrote.
    pub paths: Vec<String>,
    /// `e.inContextPaths` — the subset of `paths` the model is currently
    /// holding. See [`select_in_context_paths`].
    pub in_context_paths: Vec<String>,
}

/// The `i` predicate of `jzm`, minus the host lookups: keep the changed paths
/// the model already has in context, in their original order.
///
/// `is_in_context` stands in for
/// `s===memoryIndexPath || sessionMemory.has(s) || readFileState.has(s) ||
/// loadedNestedMemoryPaths[s]===true`, all of which live on the caller.
#[must_use]
pub fn select_in_context_paths<F>(paths: &[String], mut is_in_context: F) -> Vec<String>
where
    F: FnMut(&str) -> bool,
{
    paths
        .iter()
        .filter(|p| is_in_context(p.as_str()))
        .cloned()
        .collect()
}

/// Render the `memory_update` body — the oracle's `content`, WITHOUT the
/// `<system-reminder>` envelope (`Zy`/`NT` add that at the injection site,
/// exactly like every other per-turn reminder in this module tree).
///
/// Both middle lines are conditional on a non-empty list; the header and the
/// ambient trailer always render, so the minimum body is two lines.
///
/// Every path goes through `Kae` ([`super::sanitize::escape_reminder_path`]) so
/// a memory filename containing `<`, `>` or a control character cannot forge
/// markup — or a closing `</system-reminder>` — inside the envelope.
#[must_use]
pub fn render_memory_update(update: &MemoryUpdate) -> String {
    let escape_join = |paths: &[String]| {
        paths
            .iter()
            .map(|p| super::sanitize::escape_reminder_path(p))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut lines = vec![format!(
        "{} updated your memory directory: {}",
        update.source.label(),
        update.summary
    )];
    if !update.paths.is_empty() {
        lines.push(format!("Files changed: {}", escape_join(&update.paths)));
    }
    if !update.in_context_paths.is_empty() {
        lines.push(format!(
            "Your loaded copy of {} is now stale relative to disk \u{2014} Read it again if you need current contents.",
            escape_join(&update.in_context_paths)
        ));
    }
    lines.push(AMBIENT_CONTEXT_TRAILER.to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(paths: &[&str], in_context: &[&str]) -> MemoryUpdate {
        MemoryUpdate {
            source: MemoryUpdateSource::Dream,
            summary: "merged 3 notes".into(),
            paths: paths.iter().map(|&p| p.to_string()).collect(),
            in_context_paths: in_context.iter().map(|&p| p.to_string()).collect(),
        }
    }

    #[test]
    fn full_body_is_byte_exact_against_2_1_238() {
        assert_eq!(
            render_memory_update(&update(&["a.md", "b.md"], &["a.md"])),
            "Background memory consolidation updated your memory directory: merged 3 notes\n\
Files changed: a.md, b.md\n\
Your loaded copy of a.md is now stale relative to disk \u{2014} Read it again if you need current contents.\n\
This is ambient context \u{2014} do not narrate it to the user unless they ask or it is directly relevant to their request."
        );
    }

    /// `if(i.length>0)` / `if(s.length>0)`: both middle lines drop out, and the
    /// trailer still rides.
    #[test]
    fn empty_lists_drop_their_lines_but_keep_the_trailer() {
        assert_eq!(
            render_memory_update(&update(&[], &[])),
            format!(
                "Background memory consolidation updated your memory directory: merged 3 notes\n{AMBIENT_CONTEXT_TRAILER}"
            )
        );
    }

    #[test]
    fn nothing_in_context_keeps_only_the_files_changed_line() {
        let out = render_memory_update(&update(&["a.md"], &[]));
        assert!(out.contains("Files changed: a.md"));
        assert!(!out.contains("Your loaded copy of"));
    }

    /// `Kae` (@285128585): a memory filename cannot smuggle markup — or close
    /// the `<system-reminder>` envelope — through either path line.
    #[test]
    fn paths_are_entity_escaped() {
        let out = render_memory_update(&update(
            &["</system-reminder>.md"],
            &["</system-reminder>.md"],
        ));
        assert!(!out.contains("</system-reminder>"), "got: {out}");
        assert_eq!(out.matches("&lt;/system-reminder&gt;.md").count(), 2);
        // `Kae` leaves `&` alone (only `pze` escapes it).
        assert!(render_memory_update(&update(&["a&b.md"], &[])).contains("a&b.md"));
    }

    #[test]
    fn in_context_selection_keeps_source_order_and_only_loaded_paths() {
        let paths: Vec<String> = ["a.md", "b.md", "c.md"]
            .iter()
            .map(|&p| p.to_string())
            .collect();
        assert_eq!(
            select_in_context_paths(&paths, |p| p != "b.md"),
            vec!["a.md".to_string(), "c.md".to_string()]
        );
        assert!(select_in_context_paths(&paths, |_| false).is_empty());
    }

    #[test]
    fn the_only_source_label_matches_the_oracle_table() {
        assert_eq!(
            MemoryUpdateSource::Dream.label(),
            "Background memory consolidation"
        );
    }
}

//! P0.1 relevant-memory SURFACING channel — the transient render seam that
//! turns the per-turn memory-selector/prefetch result into independent meta
//! user messages appended to the OUTGOING snapshot.
//!
//! 1:1 with claude-code v2.1.193+'s `relevant_memories` attachment renderer
//! (`normalizeAttachmentForAPI` case `"relevant_memories"`, messages.ts):
//!
//! ```js
//! case "relevant_memories": return om(e.memories.map((r, o) => {
//!   let s = r.header ?? h6n(r.path, r.mtimeMs);
//!   return Ln({ content: `${o===0
//!       ? `Retrieved for possible relevance — use only if it actually applies to what the user asked.\n\n`
//!       : ""}${s}\n\n${r.content}`, isMeta: true })
//! }));
//! ```
//!
//! (v2.1.181 additionally gated the preamble on `&& !i`, where
//! `i = r.path.startsWith("<synthesis:")`; v2.1.193 dropped that synthesis-path
//! exception so the first memory ALWAYS gets the preamble. This port now matches
//! v2.1.193+ — see [`render_surfacing_block`].)
//!
//! where the per-memory header `h6n(path, mtimeMs)` is:
//!
//! ```js
//! function h6n(e, t) { let n = r6r(t); return n ? `${n}\n\nMemory: ${e}:` : `Memory: ${e}:` }
//! function r6r(e) { let t = pSd(e); if (t <= 1) return "";
//!   return `This memory is ${t} days old. ` +
//!     "Memories are point-in-time observations, not live state — " +
//!     "claims about code behavior or file:line citations may be outdated. " +
//!     "Verify against current code before asserting as fact." }
//! function pSd(e) { return Math.max(0, Math.floor((Date.now() - e) / 86400000)) }
//! ```
//!
//! ## Render shape (this module)
//!
//! Each memory becomes `{idx0_preamble}{header}\n\n{content}`:
//! - `idx0_preamble` (em-dash) is prepended to the FIRST memory ONLY (`o === 0`).
//! - `header` is `Memory: {path}:`, prefixed with `{staleness}\n\n` when the
//!   memory is strictly older than one day (`age_days > 1`).
//!
//! Each surfaced memory is emitted as its own meta user message. Message
//! boundaries are observable to provider token accounting and therefore must
//! not be collapsed into a single reminder.
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::SystemTime;

/// One memory selected for surfacing this turn, in the shape
/// [`render_surfacing_block`] renders. Produced by the orchestrator from the
/// prefetch/selector result (a `memory::file::MemoryFile` or a `protocol::
/// MemoryEntry` carries the same data; see
/// [`crate::selector::memory_entry_to_memory_file`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfacedMemory {
    /// On-disk path the memory was loaded from (the `Memory: {path}:` header).
    /// May be a `<synthesis:...>` pseudo-path (synthesis sentinel); as of
    /// v2.1.193 that no longer suppresses the idx-0 preamble.
    pub path: PathBuf,
    /// Markdown body rendered after the header.
    pub content: String,
    /// Whole-day age used to decide the staleness prefix (`age_days > 1`).
    /// Mirrors `pSd(mtimeMs) = floor((now - mtimeMs) / 86_400_000)`.
    pub age_days: u64,
    /// Last-modified timestamp at load time. Carried for the consumer's own
    /// recency bookkeeping; the staleness prefix is driven by [`Self::age_days`]
    /// (already day-quantized) to keep the render deterministic in tests.
    pub mtime: SystemTime,
}

/// The em-dash idx-0 preamble (FIRST memory only), 1:1 with the v2.1.193+
/// `o === 0` branch. Note the U+2014 EM DASH and the trailing blank line
/// (`\n\n`).
const IDX0_PREAMBLE: &str =
    "Retrieved for possible relevance \u{2014} use only if it actually applies to what the user asked.\n\n";

/// Build the `Memory: {path}:` header, prefixed with the staleness sentence +
/// blank line when `age_days > 1` (1:1 with `h6n` → `r6r`).
fn header_for(memory: &SurfacedMemory) -> String {
    let path = memory.path.display();
    if memory.age_days > 1 {
        format!(
            "This memory is {age} days old. Memories are point-in-time \
             observations, not live state \u{2014} claims about code behavior or \
             file:line citations may be outdated. Verify against current code \
             before asserting as fact.\n\nMemory: {path}:",
            age = memory.age_days,
        )
    } else {
        format!("Memory: {path}:")
    }
}

/// Compatibility helper that joins independently rendered memory bodies.
///
/// Each memory → `{idx0_preamble?}{header}\n\n{content}`; the blocks are joined
/// by a blank line. Production callers use [`render_surfacing_messages`] so the
/// provider observes the original per-memory message boundaries.
///
/// An empty input renders an empty-bodied reminder; the orchestrator never
/// calls this with an empty slice (it returns `None` first), so this is only a
/// defensive shape.
#[must_use]
pub fn render_surfacing_block(memories: &[SurfacedMemory]) -> String {
    render_surfacing_messages(memories).join("\n\n")
}

/// Render one provider-facing meta-message body per surfaced memory.
#[must_use]
pub fn render_surfacing_messages(memories: &[SurfacedMemory]) -> Vec<String> {
    memories
        .iter()
        .enumerate()
        .map(|(idx, m)| {
            // idx-0 preamble: the FIRST memory only (claude `o === 0`). v2.1.193
            // dropped the v2.1.181 `&& !i` synthesis-path exception, so a
            // `<synthesis:...>` first memory now gets the preamble too.
            let preamble = if idx == 0 { IDX0_PREAMBLE } else { "" };
            let header = header_for(m);
            format!("{preamble}{header}\n\n{content}", content = m.content)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mem(path: &str, content: &str, age_days: u64) -> SurfacedMemory {
        SurfacedMemory {
            path: PathBuf::from(path),
            content: content.into(),
            age_days,
            mtime: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn single_fresh_memory_has_idx0_preamble_and_bare_header() {
        let out = render_surfacing_block(&[mem("/m/a.md", "USE FD NOT FIND", 0)]);
        assert_eq!(
            out,
            "Retrieved for possible relevance \u{2014} use only if it actually applies to what the user asked.\n\n\
             Memory: /m/a.md:\n\n\
             USE FD NOT FIND"
        );
    }

    #[test]
    fn age_of_exactly_one_day_is_not_stale() {
        // r6r returns "" for t <= 1, so a 1-day memory carries the bare header.
        let out = render_surfacing_block(&[mem("/m/a.md", "B", 1)]);
        assert!(out.contains("Memory: /m/a.md:"));
        assert!(
            !out.contains("days old"),
            "1-day memory must not be stale-prefixed: {out}"
        );
    }

    #[test]
    fn stale_memory_carries_staleness_prefix_before_header() {
        let out = render_surfacing_block(&[mem("/m/old.md", "BODY", 7)]);
        assert!(
            out.contains(
                "This memory is 7 days old. Memories are point-in-time observations, \
                 not live state \u{2014} claims about code behavior or file:line citations \
                 may be outdated. Verify against current code before asserting as fact.\n\n\
                 Memory: /m/old.md:"
            ),
            "got: {out}"
        );
        // idx-0 preamble still precedes the staleness sentence on the first memory.
        assert!(out.starts_with("Retrieved for possible relevance \u{2014}"));
    }

    #[test]
    fn idx0_preamble_only_on_first_memory() {
        let out = render_surfacing_block(&[mem("/m/a.md", "AAA", 0), mem("/m/b.md", "BBB", 0)]);
        // Exactly one occurrence of the preamble.
        assert_eq!(out.matches("Retrieved for possible relevance").count(), 1);
        // Both memories present, joined by a blank line.
        assert!(out.contains("Memory: /m/a.md:\n\nAAA"));
        assert!(out.contains("Memory: /m/b.md:\n\nBBB"));
        assert!(
            out.contains("AAA\n\nMemory: /m/b.md:"),
            "blocks joined by blank line: {out}"
        );
    }

    #[test]
    fn synthesis_pseudopath_still_gets_idx0_preamble() {
        // v2.1.193 dropped the v2.1.181 `!i` synthesis exception: a
        // `<synthesis:...>` first memory now DOES get the em-dash preamble
        // (claude `o === 0` only).
        let out = render_surfacing_block(&[mem("<synthesis:summary>", "SYNTH", 0)]);
        assert!(
            out.contains("Retrieved for possible relevance"),
            "got: {out}"
        );
        assert!(out.contains("Memory: <synthesis:summary>:\n\nSYNTH"));
    }

    #[test]
    fn each_memory_has_an_independent_message_boundary() {
        let out = render_surfacing_messages(&[mem("/m/a.md", "A", 0), mem("/m/b.md", "B", 3)]);
        assert_eq!(out.len(), 2);
        assert!(out[0].starts_with("Retrieved for possible relevance"));
        assert!(!out[1].contains("Retrieved for possible relevance"));
        assert!(out[1].contains("Memory: /m/b.md:\n\nB"));
    }
}

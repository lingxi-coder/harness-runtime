//! Session-memory **dual extraction** during compaction (spec §13.8).
//!
//! When the compactor runs, it can do double duty in a single LLM call:
//! produce the conversation summary AND distil a small block of durable
//! "session memory" notes the next session should re-load. claude-code emits
//! that block wrapped in `<session_memory>…</session_memory>` inside the same
//! compaction response, so [`SessionMemoryCompactor::try_extract_memory`] pulls
//! the block out of the response text without a second model round-trip — the
//! "dual extraction" of the spec heading.
//!
//! 1:1 with the §13.8 sketch:
//!
//! ```text
//! pub fn try_extract_memory(&self, compaction_response, original_messages)
//!     -> Option<SessionMemoryExtraction> {
//!   let text = compaction_response.text_content();
//!   if let Some(s) = extract_section(&text, "<session_memory>", "</session_memory>")
//!     { Some(SessionMemoryExtraction { content: s,
//!         extracted_at_message_id: original_messages.last().map(|m| m.id()) }) }
//!   else { None }
//! }
//! pub fn should_use(config) -> bool { config.enabled && config.compact_extracts_memory }
//! ```
//!
//! The extracted block flows into [`crate::post_compact`] / the
//! `PostCompactBuilder` session-memory attachment (§13.9) and, separately, the
//! standalone [`memory::session_memory::SessionMemoryExtractor`] writes it to
//! `<configHome>/agents/session-memory/<id>.md` so the *next* session re-loads
//! it through the normal memory selector/prefetch/surfacing pipeline.
//!
//! Gating is config-only: there is **no** `tengu_session_memory` flag in
//! claude-code v2.1.181 (the stale m3-02 plan invented one). The sole gate is
//! [`SessionMemoryConfig::enabled`] (+ [`SessionMemoryConfig::compact_extracts_memory`]
//! for this dual-extraction path), exactly as §13.8's `should_use` shows.

use lingxi_core::types::{ConversationMessage, MessageId};

/// Configuration for session-memory extraction during compaction (spec §13.8).
///
/// `enabled` is the single master gate (no `tengu_session_memory` flag exists
/// in v2.1.181). `compact_extracts_memory` additionally gates the §13.8
/// dual-extraction-during-compaction path.
#[derive(Debug, Clone)]
pub struct SessionMemoryConfig {
    /// Master gate for the whole session-memory subsystem.
    pub enabled: bool,
    /// When `true`, the compaction pass also extracts a `<session_memory>`
    /// block from its summary response (the §13.8 dual-extraction path).
    pub compact_extracts_memory: bool,
}

impl Default for SessionMemoryConfig {
    fn default() -> Self {
        Self {
            // Inert by default — keeps the locked fixtures byte-identical until a
            // composition root opts in, mirroring every other dormant-by-default
            // parity subsystem in this codebase.
            enabled: false,
            compact_extracts_memory: false,
        }
    }
}

/// A session-memory block distilled from a compaction response, tagged with the
/// id of the last original message it covers so the next extraction can resume
/// from there (spec §13.8 `SessionMemoryExtraction`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMemoryExtraction {
    /// The inner text of the `<session_memory>…</session_memory>` block,
    /// trimmed of surrounding whitespace.
    pub content: String,
    /// Id of the last message in `original_messages` at extraction time, or
    /// `None` when the compacted history was empty.
    pub extracted_at_message_id: Option<MessageId>,
}

/// Extracts durable session-memory notes from a compaction response in the same
/// LLM call that produced the summary (spec §13.8 "dual extraction").
#[derive(Debug, Clone, Default)]
pub struct SessionMemoryCompactor {
    config: SessionMemoryConfig,
}

impl SessionMemoryCompactor {
    /// Build a compactor bound to `config`.
    #[must_use]
    pub fn new(config: SessionMemoryConfig) -> Self {
        Self { config }
    }

    /// Borrow the active configuration.
    #[must_use]
    pub fn config(&self) -> &SessionMemoryConfig {
        &self.config
    }

    /// Pull a `<session_memory>…</session_memory>` block out of `response_text`
    /// (the compaction response), returning it tagged with the id of the last
    /// `original_messages` entry. Returns `None` when the response carries no
    /// well-formed session-memory block.
    ///
    /// 1:1 with §13.8 `try_extract_memory`: `text_content()` →
    /// `extract_section(text, "<session_memory>", "</session_memory>")` →
    /// `SessionMemoryExtraction { content, extracted_at_message_id: last.id() }`.
    /// The inner content is trimmed (the model wraps the block on its own
    /// lines); an empty/whitespace-only block yields `None`.
    #[must_use]
    pub fn try_extract_memory(
        &self,
        response_text: &str,
        original_messages: &[ConversationMessage],
    ) -> Option<SessionMemoryExtraction> {
        let section = extract_section(response_text, "<session_memory>", "</session_memory>")?;
        let content = section.trim();
        if content.is_empty() {
            return None;
        }
        Some(SessionMemoryExtraction {
            content: content.to_string(),
            extracted_at_message_id: original_messages.last().map(ConversationMessage::id),
        })
    }

    /// Whether the §13.8 dual-extraction-during-compaction path is active for
    /// `config`. Mirrors §13.8 `should_use`:
    /// `config.enabled && config.compact_extracts_memory`.
    #[must_use]
    pub fn should_use(config: &SessionMemoryConfig) -> bool {
        config.enabled && config.compact_extracts_memory
    }
}

/// Return the inner text of the first `open…close` span in `haystack` (the
/// content between the open and close tags), or `None` when no well-formed pair
/// is present. Mirrors the JS `extract_section` / `/<open>([\s\S]*?)<\/close>/`
/// first-match, non-greedy semantics used in §13.8.
///
/// Hand-rolled (no `regex` dependency on this hot path) but byte-faithful to the
/// first-match capture: scan to the first `open`, then to the next `close` after
/// it, and return the bytes between them verbatim (the caller trims).
fn extract_section<'a>(haystack: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let open_at = haystack.find(open)?;
    let inner_start = open_at + open.len();
    let close_rel = haystack[inner_start..].find(close)?;
    Some(&haystack[inner_start..inner_start + close_rel])
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::MessageId;

    fn enabled_config(compact_extracts: bool) -> SessionMemoryConfig {
        SessionMemoryConfig {
            enabled: true,
            compact_extracts_memory: compact_extracts,
        }
    }

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), text.to_string())
    }

    #[test]
    fn default_config_is_inert() {
        let c = SessionMemoryConfig::default();
        assert!(
            !c.enabled,
            "session memory must be off by default (fixtures stay byte-identical)"
        );
        assert!(!c.compact_extracts_memory);
    }

    #[test]
    fn should_use_requires_both_enabled_and_compact_extracts() {
        // Off entirely.
        assert!(!SessionMemoryCompactor::should_use(
            &SessionMemoryConfig::default()
        ));
        // enabled but dual-extraction not opted in.
        assert!(!SessionMemoryCompactor::should_use(&enabled_config(false)));
        // Both on => active.
        assert!(SessionMemoryCompactor::should_use(&enabled_config(true)));
    }

    #[test]
    fn try_extract_pulls_block_and_tags_last_message_id() {
        let compactor = SessionMemoryCompactor::new(enabled_config(true));
        let msgs = vec![user("first"), user("second"), user("LAST")];
        let last_id = msgs.last().unwrap().id();

        let resp = "Summary:\nblah blah\n\n<session_memory>\nUse fd not find.\nProject uses pnpm.\n</session_memory>\ntrailer";
        let extraction = compactor
            .try_extract_memory(resp, &msgs)
            .expect("well-formed block extracts");

        // Inner content trimmed of the surrounding newlines.
        assert_eq!(extraction.content, "Use fd not find.\nProject uses pnpm.");
        // Tagged with the id of the LAST original message.
        assert_eq!(extraction.extracted_at_message_id, Some(last_id));
    }

    #[test]
    fn try_extract_returns_none_without_block() {
        let compactor = SessionMemoryCompactor::new(enabled_config(true));
        let msgs = vec![user("only")];
        // No tags at all.
        assert!(compactor
            .try_extract_memory("just a plain summary", &msgs)
            .is_none());
        // Open tag without a close.
        assert!(compactor
            .try_extract_memory("<session_memory>unterminated", &msgs)
            .is_none());
        // Empty / whitespace-only block => None (nothing durable to write).
        assert!(compactor
            .try_extract_memory("<session_memory>   \n  </session_memory>", &msgs)
            .is_none());
    }

    #[test]
    fn try_extract_with_empty_history_tags_none() {
        let compactor = SessionMemoryCompactor::new(enabled_config(true));
        let extraction = compactor
            .try_extract_memory("<session_memory>note</session_memory>", &[])
            .expect("block still extracts with empty history");
        assert_eq!(extraction.content, "note");
        assert_eq!(extraction.extracted_at_message_id, None);
    }

    #[test]
    fn extract_section_is_first_match_non_greedy() {
        // First open .. first close after it; a later block is ignored.
        let s = "<session_memory>one</session_memory>X<session_memory>two</session_memory>";
        assert_eq!(
            extract_section(s, "<session_memory>", "</session_memory>"),
            Some("one")
        );
    }
}

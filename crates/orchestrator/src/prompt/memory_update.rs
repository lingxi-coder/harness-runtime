//! Native 2.1.319 memory-update attachments, including extraction and sync.
//! The file writer supplies exact changed paths; context membership is resolved
//! when updates are drained. See scripts/tests/memory_prompts_319_oracle.mjs.

use lingxi_core::types::utf16_json::Utf16JsonProjection;

/// Native source labels (`Nfo` / `Ffo` in 2.1.319).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryUpdateSource {
    /// `dream` ⇒ `"Background memory consolidation"`.
    Dream,
    /// Successful automatic file-memory extraction.
    Extract,
    /// Unknown source labels use Native's generic wording.
    Other,
    /// Pending local changes already supply the complete reminder body.
    SyncUnsaved,
}

impl MemoryUpdateSource {
    /// `F5T[e.source]`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Dream => "Background memory consolidation",
            Self::Extract => "Background memory extraction",
            Self::Other | Self::SyncUnsaved => "A background memory process",
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
    pub summary: Utf16JsonProjection,
}

/// One drained `pendingMemoryUpdates` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryUpdate {
    /// `e.source`.
    pub source: MemoryUpdateSource,
    /// `e.summary` — the consolidator's own one-line description.
    pub summary: Utf16JsonProjection,
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
pub fn render_memory_update(update: &MemoryUpdate) -> Utf16JsonProjection {
    if update.source == MemoryUpdateSource::SyncUnsaved {
        return update.summary.clone();
    }
    let escape_join = |paths: &[String]| {
        paths
            .iter()
            .map(|p| super::sanitize::escape_reminder_path(p))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let summary_units = update
        .summary
        .string_units("")
        .expect("memory update summary is a JSON string");
    let mut units = format!("{} updated your memory directory: ", update.source.label())
        .encode_utf16()
        .collect::<Vec<_>>();
    units.extend(summary_units);
    if !update.paths.is_empty() {
        units.extend("\nFiles changed: ".encode_utf16());
        units.extend(escape_join(&update.paths).encode_utf16());
    }
    if !update.in_context_paths.is_empty() {
        units.extend("\nYour loaded copy of ".encode_utf16());
        units.extend(escape_join(&update.in_context_paths).encode_utf16());
        units.extend(
            " is now stale relative to disk \u{2014} Read it again if you need current contents."
                .encode_utf16(),
        );
    }
    units.extend("\n".encode_utf16());
    units.extend(AMBIENT_CONTEXT_TRAILER.encode_utf16());
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("memory update body is a root string")
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
            render_memory_update(&update(&["a.md", "b.md"], &["a.md"]))
                .string_at("")
                .unwrap(),
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
            render_memory_update(&update(&[], &[])).string_at("").unwrap(),
            format!(
                "Background memory consolidation updated your memory directory: merged 3 notes\n{AMBIENT_CONTEXT_TRAILER}"
            )
        );
    }

    #[test]
    fn nothing_in_context_keeps_only_the_files_changed_line() {
        let rendered = render_memory_update(&update(&["a.md"], &[]));
        let out = rendered.string_at("").unwrap();
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
        let rendered = out.string_at("").unwrap();
        assert!(!rendered.contains("</system-reminder>"), "got: {rendered}");
        assert_eq!(rendered.matches("&lt;/system-reminder&gt;.md").count(), 2);
        // `Kae` leaves `&` alone (only `pze` escapes it).
        let rendered = render_memory_update(&update(&["a&b.md"], &[]));
        assert!(rendered.string_at("").unwrap().contains("a&b.md"));
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

#[cfg(test)]
mod native_319_tests {
    use super::*;
    #[test]
    fn all_native_update_sources_and_path_branches_match_the_oracle() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/memory_prompts_319.json"
        ))
        .unwrap();
        for case in fixture["updates"].as_array().unwrap() {
            let input = &case["input"];
            let source = match input["source"].as_str().unwrap() {
                "dream" => MemoryUpdateSource::Dream,
                "extract" => MemoryUpdateSource::Extract,
                "sync_unsaved" => MemoryUpdateSource::SyncUnsaved,
                _ => MemoryUpdateSource::Other,
            };
            let paths = |key: &str| {
                input[key]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_owned())
                    .collect()
            };
            let body = render_memory_update(&MemoryUpdate {
                source,
                summary: input["summary"].as_str().unwrap().into(),
                paths: paths("paths"),
                in_context_paths: paths("inContextPaths"),
            });
            assert_eq!(
                body.string_at(""),
                case["expected"][0]["content"].as_str(),
                "{case}"
            );
        }
    }
}

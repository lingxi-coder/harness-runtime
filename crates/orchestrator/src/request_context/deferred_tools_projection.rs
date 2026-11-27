use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, OnceLock};

const SYSTEM_REMINDER_PREFIX: &str = "<system-reminder>\n";
const SYSTEM_REMINDER_SUFFIX: &str = "\n</system-reminder>";

#[derive(Clone, Debug, Default)]
pub(crate) struct DeferredToolsProjectionFacts {
    /// Present when this request's route, beta, refusal and deferral gates allow late additions.
    tools: Option<Arc<DeferredToolNames>>,
}

#[derive(Debug)]
struct DeferredToolNames {
    /// Current by-value tool additions present in H API-system messages.
    /// `None` corresponds to an undefined definition pool for this view;
    /// `Some` permits `replacedNames`. Native's inline-beta feature/profile
    /// gates are not fully represented by this fact.
    inline_tool_definition_names: Option<HashSet<String>>,
    /// Exact current ToolSearch wire names, kept separate from the complete
    /// tool pool because Native filters surfaced names against this set when
    /// by-value definitions are absent.
    deferred_wire_tool_names: Vec<String>,
    current_utf16: OnceLock<HashSet<Vec<u16>>>,
}

impl DeferredToolsProjectionFacts {
    pub(crate) fn new(
        surface_late_tool_additions: bool,
        inline_tool_definition_names: Option<HashSet<String>>,
        deferred_wire_tool_names: Vec<String>,
    ) -> Self {
        Self {
            tools: surface_late_tool_additions.then(|| {
                Arc::new(DeferredToolNames {
                    inline_tool_definition_names,
                    deferred_wire_tool_names,
                    current_utf16: OnceLock::new(),
                })
            }),
        }
    }
}

impl DeferredToolNames {
    fn current(&self) -> &HashSet<Vec<u16>> {
        self.current_utf16.get_or_init(|| {
            self.deferred_wire_tool_names
                .iter()
                .chain(self.inline_tool_definition_names.iter().flatten())
                .map(|name| name.encode_utf16().collect())
                .collect()
        })
    }
}

pub(crate) fn deferred_name_only_announcements(payload: &Source) -> impl Iterator<Item = &str> {
    let entries = if payload.string_at("/type") == Some("deferred_tools_record") {
        payload
            .at_pointer("/nameOnlyAnnouncements")
            .and_then(|value| value.array_entries())
    } else {
        None
    };
    entries
        .into_iter()
        .flat_map(|(_, entries)| entries)
        .filter_map(|(_, value)| value.as_str())
}

pub(crate) fn cached_rendered_row_count(message: &ConversationMessage) -> usize {
    match message {
        ConversationMessage::User { content, .. }
            if content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Text { .. } | ContentBlock::TextJsUtf16 { .. }
                )
            }) =>
        {
            1
        }
        _ => 0,
    }
}

/// Reproject one durable deferred-tools sidecar using the tool names available
/// to this request. The sidecar and message identity stay unchanged; only the
/// outgoing view is recomputed.
pub(crate) fn render_deferred_tools_delta_for_request(
    id: MessageId,
    source: &Source,
    cached_message: Option<ConversationMessage>,
    cached_rendered_count: usize,
    facts: &DeferredToolsProjectionFacts,
    name_only_announcement: bool,
) -> Option<ConversationMessage> {
    if source.string_at("/type") != Some("deferred_tools_delta") {
        return cached_message.filter(|message| cached_rendered_row_count(message) > 0);
    }
    // qWr/sMn always compares the raw surfaced and replaced arrays. The
    // optional inline pool gates which of those names may surface.
    let mut names = source_name_units(source, "/surfacedNames").collect::<Vec<_>>();
    let replaced_start = names.len();
    names.extend(source_name_units(source, "/replacedNames"));
    let mut surfaced = Vec::new();
    if let Some(tools) = facts.tools.as_ref().filter(|_| !name_only_announcement) {
        let candidates = if tools.inline_tool_definition_names.is_some() {
            &names[..]
        } else {
            &names[..replaced_start]
        };
        if !candidates.is_empty() {
            let current = tools.current();
            surfaced.extend(
                candidates
                    .iter()
                    .copied()
                    .filter(|name| current.contains(*name)),
            );
        }
    }
    if cached_rendered_count > 0 && cached_message.is_some() && qwr_matches(&names, &surfaced) {
        // H currently projects each deferred delta to one user message. Native
        // uses cached rows whenever GWr validates the nonempty cache; a row
        // count mismatch falls back to that same cached row set.
        return cached_message;
    }

    let rendered = render_nqe(
        source,
        &surfaced,
        &names[replaced_start..],
        cached_message.as_ref(),
    );
    rendered.map(|units| {
        ConversationMessage::user_meta_js_utf16(id, String::from_utf16_lossy(&units), units)
    })
}

/// Match qWr/sMn: compare cardinality and one-way membership, preserving the
/// source's duplicate-sensitive and asymmetric behavior.
fn qwr_matches(source_names: &[&[u16]], current_names: &[&[u16]]) -> bool {
    if source_names.len() != current_names.len() {
        return false;
    }
    if source_names == current_names {
        return true;
    }
    let current = current_names.iter().copied().collect::<HashSet<_>>();
    source_names.iter().all(|name| current.contains(*name))
}

fn source_name_units<'a>(source: &'a Source, path: &str) -> impl Iterator<Item = &'a [u16]> {
    source
        .at_pointer(path)
        .and_then(|value| value.array_entries())
        .into_iter()
        .flat_map(|(_, entries)| entries)
        .filter_map(|(_, value)| value.code_units())
}

fn source_names(source: &Source, path: &str) -> Vec<Vec<u16>> {
    source_name_units(source, path)
        .map(<[u16]>::to_vec)
        .collect()
}

fn render_nqe(
    source: &Source,
    surfaced: &[&[u16]],
    replaced: &[&[u16]],
    cached_message: Option<&ConversationMessage>,
) -> Option<Vec<u16>> {
    let mut surfaced_set = HashSet::new();
    let replaced = replaced.iter().copied().collect::<HashSet<_>>();
    let (updated, available): (Vec<_>, Vec<_>) = surfaced
        .iter()
        .copied()
        .filter(|name| surfaced_set.insert(*name))
        .partition(|name| replaced.contains(*name));
    let has_added_names = source_name_units(source, "/addedNames").next().is_some();
    let added_lines = source_name_units(source, "/addedLines")
        .filter(|name| !surfaced_set.contains(*name))
        .collect::<Vec<_>>();
    let tool_search_absent = source
        .at_pointer("/toolSearchAbsent")
        .and_then(|value| value.as_bool())
        == Some(true);
    let readded = source_name_units(source, "/readdedNames")
        .filter(|name| !surfaced_set.contains(*name))
        .map(<[u16]>::to_vec)
        .collect::<Vec<_>>();
    let restored = source_name_units(source, "/restoredNames").collect::<Vec<_>>();

    let mut parts = Vec::<Vec<u16>>::new();
    if !available.is_empty() {
        parts.push(section(
            "The following tools just became available and are ready to use:",
            &available,
        ));
    }
    if !updated.is_empty() {
        parts.push(section(
            "The following tools have updated definitions, which replace the earlier ones from here on:",
            &updated,
        ));
    }
    if !added_lines.is_empty() && has_added_names {
        let header = if tool_search_absent {
            "The following tools are now available:"
        } else {
            "The following deferred tools are now available via ToolSearch. Their schemas are NOT loaded — calling them directly will fail with InputValidationError. Use ToolSearch with query \"select:<name>[,<name>...]\" to load tool schemas before calling them:"
        };
        parts.push(section(header, &added_lines));
    }
    parts.extend(render_readded(&readded, tool_search_absent));
    if !restored.is_empty() {
        parts.push(section(
            "The following tools are available again in this session. The earlier instruction to disregard their definitions and not call them no longer applies:",
            &restored,
        ));
    }
    parts.extend(render_removed_and_retracted(source, tool_search_absent));
    // NQe's remaining producer branches read MCP status fields without reading
    // surfacedToolNames. Reuse those cached sections because a pool change
    // cannot alter them; the surface-dependent sections above are rebuilt.
    parts.extend(cached_static_parts(cached_message));
    if parts.is_empty() {
        return None;
    }

    let body = join_parts(&parts);
    Some(
        SYSTEM_REMINDER_PREFIX
            .encode_utf16()
            .chain(body)
            .chain(SYSTEM_REMINDER_SUFFIX.encode_utf16())
            .collect(),
    )
}

fn section(header: &str, names: &[impl AsRef<[u16]>]) -> Vec<u16> {
    header
        .encode_utf16()
        .chain(std::iter::once(b'\n' as u16))
        .chain(join_lines(names))
        .collect()
}

fn join_lines(lines: &[impl AsRef<[u16]>]) -> Vec<u16> {
    let mut result = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            result.push(b'\n' as u16);
        }
        result.extend_from_slice(line.as_ref());
    }
    result
}

fn join_parts(parts: &[Vec<u16>]) -> Vec<u16> {
    let mut result = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            result.extend_from_slice(&[b'\n' as u16, b'\n' as u16]);
        }
        result.extend_from_slice(part);
    }
    result
}

fn render_readded(names: &[Vec<u16>], tool_search_absent: bool) -> Vec<Vec<u16>> {
    let (mcp, other): (Vec<_>, Vec<_>) = names
        .iter()
        .cloned()
        .partition(|name| name.starts_with(&"mcp__".encode_utf16().collect::<Vec<_>>()));
    let tool = if tool_search_absent {
        "tool"
    } else {
        "deferred tool"
    };
    let follow_up = if tool_search_absent {
        String::new()
    } else {
        " Load via ToolSearch as before.".to_owned()
    };
    let mut parts = Vec::new();
    if !mcp.is_empty() {
        let plural = if mcp.len() == 1 { " is" } else { "s are" };
        let mut text = format!(
            "{} {tool}{plural} available again (MCP server reconnected — names announced earlier in this conversation): ",
            mcp.len(),
        )
        .encode_utf16()
        .collect::<Vec<_>>();
        text.extend(group_mcp_names(&mcp));
        text.extend(format!(".{follow_up}").encode_utf16());
        parts.push(text);
    }
    if !other.is_empty() {
        let plural = if other.len() == 1 { " is" } else { "s are" };
        let mut text = format!(
            "{} {tool}{plural} available again in this session (announced earlier in this conversation): ",
            other.len(),
        )
        .encode_utf16()
        .collect::<Vec<_>>();
        text.extend(join_with_separator(&other, ", "));
        text.extend(format!(".{follow_up}").encode_utf16());
        parts.push(text);
    }
    parts
}

fn group_mcp_names(names: &[Vec<u16>]) -> Vec<u16> {
    let prefix = "mcp__".encode_utf16().collect::<Vec<_>>();
    let delimiter = "__".encode_utf16().collect::<Vec<_>>();
    let mut counts = BTreeMap::<Vec<u16>, usize>::new();
    for name in names {
        let bucket = if name.starts_with(&prefix) {
            let end = find_subslice(name, &delimiter, prefix.len()).unwrap_or(name.len());
            let mut bucket = name[..end].to_vec();
            bucket.extend("__*".encode_utf16());
            bucket
        } else {
            name.clone()
        };
        *counts.entry(bucket).or_default() += 1;
    }
    let mut groups = counts.into_iter().collect::<Vec<_>>();
    // Native uses localeCompare for MCP server buckets. MCP server tokens are
    // normalized to ASCII [A-Za-z0-9_-], for which this weight table preserves
    // the default English collation order (underscore, hyphen, digits, then
    // case-insensitive letters with lowercase first).
    groups.sort_by(|(left, _), (right, _)| mcp_locale_compare(left, right));
    groups
        .into_iter()
        .map(|(name, count)| {
            if count > 1 {
                name.into_iter()
                    .chain(format!(" ({count})").encode_utf16())
                    .collect::<Vec<_>>()
            } else {
                name
            }
        })
        .fold(Vec::new(), |mut result, name| {
            if !result.is_empty() {
                result.extend(", ".encode_utf16());
            }
            result.extend(name);
            result
        })
}

fn mcp_locale_compare(left: &[u16], right: &[u16]) -> std::cmp::Ordering {
    fn weight(unit: u16) -> u16 {
        match unit {
            value if value == b'_' as u16 => 0,
            value if value == b'-' as u16 => 1,
            value if (b'0' as u16..=b'9' as u16).contains(&value) => 2 + value - b'0' as u16,
            value if (b'a' as u16..=b'z' as u16).contains(&value) => 12 + (value - b'a' as u16) * 2,
            value if (b'A' as u16..=b'Z' as u16).contains(&value) => 13 + (value - b'A' as u16) * 2,
            value => 100 + value,
        }
    }

    left.iter()
        .map(|unit| weight(*unit))
        .cmp(right.iter().map(|unit| weight(*unit)))
}

fn join_with_separator(lines: &[Vec<u16>], separator: &str) -> Vec<u16> {
    let mut result = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            result.extend(separator.encode_utf16());
        }
        result.extend_from_slice(line);
    }
    result
}

fn render_removed_and_retracted(source: &Source, tool_search_absent: bool) -> Vec<Vec<u16>> {
    let removed = source_names(source, "/removedNames");
    let retracted = source_retractions(source);
    let retracted_names = retracted
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<HashSet<_>>();
    let mcp_prefix = "mcp__".encode_utf16().collect::<Vec<_>>();
    let mut removed_mcp = Vec::new();
    let mut removed_other = Vec::new();
    for name in &removed {
        if name.starts_with(&mcp_prefix) && !retracted_names.contains(name) {
            removed_mcp.push(name.clone());
        } else {
            removed_other.push(name.clone());
        }
    }

    let tool = if tool_search_absent {
        "tool"
    } else {
        "deferred tool"
    };
    let unavailable = if tool_search_absent {
        "Do not call them"
    } else {
        "Do not search for them — ToolSearch will return no match"
    };
    let mut parts = Vec::new();
    if !removed_mcp.is_empty() {
        if removed_mcp.len() > 30 {
            let mut part = format!(
                "{} {tool}s are no longer available (MCP server disconnected): ",
                removed_mcp.len(),
            )
            .encode_utf16()
            .collect::<Vec<_>>();
            part.extend(group_mcp_names(&removed_mcp));
            part.extend(format!(". {unavailable}.").encode_utf16());
            parts.push(part);
        } else {
            let mut part = format!(
                "The following {tool}s are no longer available (their MCP server disconnected). {unavailable}:\n"
            )
            .encode_utf16()
            .collect::<Vec<_>>();
            part.extend(join_lines(&removed_mcp));
            parts.push(part);
        }
    }
    if !removed_other.is_empty() {
        let mut part = format!(
            "The following {tool}s are no longer available in this session. {unavailable}:\n"
        )
        .encode_utf16()
        .collect::<Vec<_>>();
        part.extend(join_lines(&removed_other));
        parts.push(part);
    }
    if !retracted.is_empty() {
        parts.push(render_retractions(&retracted));
    }
    if !removed.is_empty() || !retracted.is_empty() {
        parts.push(
            "This is ambient context — do not narrate it to the user unless they ask or it is directly relevant to their request."
                .encode_utf16()
                .collect(),
        );
    }
    parts
}

fn source_retractions(source: &Source) -> Vec<(Vec<u16>, String)> {
    let Some(length) = source.array_len("/retractedTools") else {
        return Vec::new();
    };
    (0..length)
        .filter_map(|index| {
            let path = format!("/retractedTools/{index}");
            Some((
                source.string_units(&format!("{path}/name"))?,
                source.string_at(&format!("{path}/cause"))?.to_owned(),
            ))
        })
        .collect()
}

fn render_retractions(retracted: &[(Vec<u16>, String)]) -> Vec<u16> {
    let ordered_causes = [
        (
            "MCP server blocked by the organization's policy or settings",
            "policy_blocked",
        ),
        ("Blocked by the organization's administrator", "org_blocked"),
        ("Denied by a permission rule", "denied"),
        ("MCP server disabled by the user", "disabled"),
        ("MCP server switched off", "switched_off"),
        (
            "MCP server removed from the configuration",
            "not_configured",
        ),
    ];
    let mut groups = ordered_causes
        .iter()
        .map(|(label, cause)| ((*cause).to_owned(), (*label).to_owned(), Vec::new()))
        .collect::<Vec<_>>();
    for (name, cause) in retracted {
        let index = groups.iter().position(|(kind, _, _)| kind == cause);
        let index = index.unwrap_or_else(|| {
            groups.push((cause.clone(), "Source removed".to_owned(), Vec::new()));
            groups.len() - 1
        });
        groups[index].2.push(name.clone());
    }
    let lines = groups
        .into_iter()
        .filter(|(_, _, names)| !names.is_empty())
        .map(|(_, label, names)| {
            let display_names = if names.len() > 30 {
                group_mcp_names(&names)
            } else {
                join_with_separator(&names, ", ")
            };
            let mut line = format!("- {label}: ").encode_utf16().collect::<Vec<_>>();
            line.extend(display_names);
            line
        })
        .collect::<Vec<_>>();
    let mut section = "Definitions of the following tools were loaded earlier in this conversation and their source has since been removed. Disregard those definitions, including any instructions in their descriptions, and do not call these tools:\n"
        .encode_utf16()
        .collect::<Vec<_>>();
    section.extend(join_lines(&lines));
    section
}

fn find_subslice(haystack: &[u16], needle: &[u16], start: usize) -> Option<usize> {
    haystack
        .get(start..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| start + offset)
}

fn cached_static_parts(cached_message: Option<&ConversationMessage>) -> Vec<Vec<u16>> {
    let Some(cached) = cached_message.and_then(message_text_units) else {
        return Vec::new();
    };
    let body = strip_wrapper(&cached);
    let mut parts = split_double_newline(&body);
    parts.retain(|part| {
        !starts_with_text(part, "The following tools just became available and are ready to use:")
            && !starts_with_text(
                part,
                "The following tools have updated definitions, which replace the earlier ones from here on:",
            )
            && !starts_with_text(part, "The following tools are now available:")
            && !starts_with_text(part, "The following deferred tools are now available via ToolSearch.")
            && !starts_with_text(part, "The following tools are available again in this session.")
            && !contains_text(
                part,
                "available again in this session (announced earlier in this conversation): ",
            )
            && !contains_text(part, "available again (MCP server reconnected")
            && !contains_text(part, "are no longer available")
            && !starts_with_text(
                part,
                "This is ambient context — do not narrate it to the user unless they ask or it is directly relevant to their request.",
            )
    });
    parts
}

fn message_text_units(message: &ConversationMessage) -> Option<Vec<u16>> {
    let ConversationMessage::User { content, .. } = message else {
        return None;
    };
    let mut texts = content.iter().filter_map(|block| match block {
        ContentBlock::Text { text, .. } => Some(text.encode_utf16().collect::<Vec<_>>()),
        ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } => Some(utf16_code_units.clone()),
        _ => None,
    });
    let result = texts.next()?;
    texts.next().is_none().then_some(result)
}

fn strip_wrapper(value: &[u16]) -> Vec<u16> {
    let prefix = SYSTEM_REMINDER_PREFIX.encode_utf16().collect::<Vec<_>>();
    let suffix = SYSTEM_REMINDER_SUFFIX.encode_utf16().collect::<Vec<_>>();
    if value.starts_with(&prefix) && value.ends_with(&suffix) {
        value[prefix.len()..value.len() - suffix.len()].to_vec()
    } else {
        value.to_vec()
    }
}

fn split_double_newline(value: &[u16]) -> Vec<Vec<u16>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index + 1 < value.len() {
        if value[index] == b'\n' as u16 && value[index + 1] == b'\n' as u16 {
            if index > start {
                parts.push(value[start..index].to_vec());
            }
            start = index + 2;
            index += 2;
        } else {
            index += 1;
        }
    }
    if start < value.len() {
        parts.push(value[start..].to_vec());
    }
    parts
}

fn starts_with_text(value: &[u16], prefix: &str) -> bool {
    value.starts_with(&prefix.encode_utf16().collect::<Vec<_>>())
}

fn contains_text(value: &[u16], needle: &str) -> bool {
    find_subslice(value, &needle.encode_utf16().collect::<Vec<_>>(), 0).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_facts(deferred_wire_tool_names: &[&str]) -> DeferredToolsProjectionFacts {
        DeferredToolsProjectionFacts::new(
            true,
            None,
            deferred_wire_tool_names
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        )
    }

    fn with_by_value_definitions(
        definitions: &[&str],
        deferred_wire_tool_names: &[&str],
    ) -> DeferredToolsProjectionFacts {
        DeferredToolsProjectionFacts::new(
            true,
            Some(definitions.iter().map(|name| (*name).to_owned()).collect()),
            deferred_wire_tool_names
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        )
    }

    fn source(raw: &str) -> Source {
        Source::parse(raw).expect("valid test source")
    }

    fn cached(id: MessageId, text: &str) -> ConversationMessage {
        ConversationMessage::user_meta(id, text.to_owned())
    }

    fn body(message: &ConversationMessage) -> String {
        match message {
            ConversationMessage::User { content, .. } => content
                .iter()
                .find_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    ContentBlock::TextJsUtf16 {
                        utf16_code_units, ..
                    } => Some(String::from_utf16_lossy(utf16_code_units)),
                    _ => None,
                })
                .expect("reminder body"),
            _ => panic!("expected a user reminder"),
        }
    }

    #[test]
    fn cache_gate_matches_changed_empty_and_cached_fallback_source_cases() {
        let id = MessageId::new();
        let cached = cached(
            id,
            "<system-reminder>\nThe following tools just became available and are ready to use:\nRead\n</system-reminder>",
        );
        let one_surface = source(
            r#"{"type":"deferred_tools_delta","surfacedNames":["Read"],"addedNames":[],"addedLines":[]}"#,
        );
        let same = render_deferred_tools_delta_for_request(
            id,
            &one_surface,
            Some(cached.clone()),
            1,
            &enabled_facts(&["Read"]),
            false,
        )
        .expect("matching surface keeps cached row");
        assert_eq!(body(&same), body(&cached));
        assert_eq!(same.id(), id);

        let changing = source(
            r#"{"type":"deferred_tools_delta","surfacedNames":["Read","TaskCreate"],"addedNames":[],"addedLines":[]}"#,
        );
        let dynamic = render_deferred_tools_delta_for_request(
            id,
            &changing,
            Some(cached.clone()),
            1,
            &enabled_facts(&["TaskCreate"]),
            false,
        )
        .expect("changed surface has current view");
        assert_eq!(dynamic.id(), id);
        assert!(body(&dynamic).contains("TaskCreate"));
        assert!(!body(&dynamic).contains("cached source body"));

        let empty = render_deferred_tools_delta_for_request(
            id,
            &one_surface,
            Some(cached.clone()),
            1,
            &enabled_facts(&[]),
            false,
        );
        assert!(empty.is_none(), "changed surface can render no row");

        let empty_source = source(
            r#"{"type":"deferred_tools_delta","surfacedNames":[],"addedNames":[],"addedLines":[]}"#,
        );
        let fallback = render_deferred_tools_delta_for_request(
            id,
            &empty_source,
            Some(cached.clone()),
            1,
            &enabled_facts(&[]),
            false,
        )
        .expect("matching empty NQe count falls back to the cached row");
        assert_eq!(body(&fallback), body(&cached));

        let invalid_cache = render_deferred_tools_delta_for_request(
            id,
            &one_surface,
            None,
            0,
            &enabled_facts(&["Read"]),
            false,
        )
        .expect("empty rendered cache falls through to NQe");
        assert!(body(&invalid_cache).contains("Read"));
        let changed_without_cache = render_deferred_tools_delta_for_request(
            id,
            &changing,
            None,
            0,
            &enabled_facts(&["TaskCreate"]),
            false,
        )
        .expect("qWr miss uses the current NQe projection even without a cache");
        assert_eq!(changed_without_cache.id(), id);
        assert!(body(&changed_without_cache).contains("TaskCreate"));
    }

    #[test]
    fn source_gate_suppresses_only_current_surface_names() {
        let id = MessageId::new();
        let source = source(
            r#"{"type":"deferred_tools_delta","surfacedNames":["Read"],"addedNames":[],"addedLines":[]}"#,
        );
        let cached = cached(
            id,
            "<system-reminder>\nThe following tools just became available and are ready to use:\nRead\n</system-reminder>",
        );
        let name_only = render_deferred_tools_delta_for_request(
            id,
            &source,
            Some(cached.clone()),
            1,
            &enabled_facts(&["Read"]),
            true,
        );
        assert!(
            name_only.is_none(),
            "name-only UUIDs cannot surface stale by-value names"
        );

        let disabled = render_deferred_tools_delta_for_request(
            id,
            &source,
            Some(cached),
            1,
            &DeferredToolsProjectionFacts::default(),
            false,
        );
        assert!(
            disabled.is_none(),
            "disabled late-surface gate clears only the stale surface section"
        );
    }

    #[test]
    fn raw_empty_cache_still_projects_retraction_without_surface_gate() {
        let id = MessageId::new();
        let source = source(
            r#"{"type":"deferred_tools_delta","retractedTools":[{"name":"mcp__srv__tool","cause":"denied"}]}"#,
        );
        let projected = render_deferred_tools_delta_for_request(
            id,
            &source,
            None,
            0,
            &DeferredToolsProjectionFacts::default(),
            false,
        )
        .expect("retractions are independent of the surfaced-name gates");
        assert!(body(&projected).contains("Definitions of the following tools"));
        assert!(body(&projected).contains("- Denied by a permission rule: mcp__srv__tool"));
    }

    #[test]
    fn inline_definitions_only_enable_replaced_name_candidates() {
        let id = MessageId::new();
        let source = source(
            r#"{"type":"deferred_tools_delta","surfacedNames":[],"replacedNames":["TaskCreate"],"addedNames":[],"addedLines":[]}"#,
        );
        let cached = cached(
            id,
            "The following tools have updated definitions, which replace the earlier ones from here on:\nTaskCreate",
        );
        let absent_definitions = render_deferred_tools_delta_for_request(
            id,
            &source,
            Some(cached),
            1,
            &enabled_facts(&[]),
            false,
        );
        assert!(
            absent_definitions.is_none(),
            "raw replacedNames still cause qWr miss when `or` is absent"
        );

        let by_value = render_deferred_tools_delta_for_request(
            id,
            &source,
            None,
            0,
            &with_by_value_definitions(&["TaskCreate"], &[]),
            false,
        )
        .expect("defined inline pool surfaces a replaced name");
        assert!(body(&by_value).contains("updated definitions"));
        assert!(body(&by_value).contains("TaskCreate"));
    }

    #[test]
    fn tool_index_is_lazy_and_shared_only_within_its_request() {
        let facts = with_by_value_definitions(&[], &["Read", "\u{fffd}"]);
        let pool = facts.tools.as_ref().unwrap();
        let id = MessageId::new();
        let empty = source(r#"{"type":"deferred_tools_delta"}"#);
        let delta = source(
            r#"{"type":"deferred_tools_delta","surfacedNames":["\ud800"],"replacedNames":["Read"]}"#,
        );
        assert!(
            render_deferred_tools_delta_for_request(id, &empty, None, 0, &facts, false).is_none()
        );
        assert!(
            render_deferred_tools_delta_for_request(id, &delta, None, 0, &facts, true).is_none()
        );
        assert!(pool.current_utf16.get().is_none());

        let reattached = facts.clone();
        let row = render_deferred_tools_delta_for_request(id, &delta, None, 0, &reattached, false)
            .expect("a defined empty inline pool allows the deferred replaced name");
        assert!(body(&row).contains("Read"));
        assert!(!body(&row).contains('\u{fffd}'));
        assert!(std::ptr::eq(
            pool.current_utf16.get().unwrap(),
            reattached.tools.as_ref().unwrap().current(),
        ));

        let next = enabled_facts(&["Read", "\u{fffd}"]);
        assert!(
            render_deferred_tools_delta_for_request(id, &delta, None, 0, &next, false).is_none()
        );
    }

    #[test]
    fn valid_empty_text_cache_is_distinct_from_raw_empty_cache() {
        let id = MessageId::new();
        let empty_source = source(
            r#"{"type":"deferred_tools_delta","surfacedNames":[],"addedNames":[],"addedLines":[]}"#,
        );
        let valid_empty_text = cached(id, "");
        assert_eq!(cached_rendered_row_count(&valid_empty_text), 1);
        let retained = render_deferred_tools_delta_for_request(
            id,
            &empty_source,
            Some(valid_empty_text),
            1,
            &DeferredToolsProjectionFacts::default(),
            false,
        )
        .expect("one rendered empty-text item is still a valid cache row");
        assert_eq!(cached_rendered_row_count(&retained), 1);

        let mut raw_empty_placeholder = ConversationMessage::user_meta(id, String::new());
        if let ConversationMessage::User { content, .. } = &mut raw_empty_placeholder {
            content.clear();
        }
        let rendered_count = cached_rendered_row_count(&raw_empty_placeholder);
        assert_eq!(rendered_count, 0);

        let retracted = source(
            r#"{"type":"deferred_tools_delta","retractedTools":[{"name":"mcp__srv__tool","cause":"denied"}]}"#,
        );
        let projected = render_deferred_tools_delta_for_request(
            id,
            &retracted,
            Some(raw_empty_placeholder),
            rendered_count,
            &DeferredToolsProjectionFacts::default(),
            false,
        )
        .expect("an empty-content placeholder is not a valid GWr cache");
        assert_eq!(projected.id(), id);
        assert_eq!(cached_rendered_row_count(&projected), 1);
        assert!(body(&projected).contains("Definitions of the following tools"));
    }

    #[test]
    fn qwr_preserves_one_way_duplicate_membership() {
        let read = "Read".encode_utf16().collect::<Vec<_>>();
        let other = "Other".encode_utf16().collect::<Vec<_>>();
        assert!(qwr_matches(&[&read, &read], &[&read, &other]));
        assert!(!qwr_matches(&[&read, &other], &[&other, &other]));
    }

    #[test]
    fn non_string_name_entries_are_ignored_like_native_ji() {
        let raw = source(
            r#"{"type":"deferred_tools_delta","surfacedNames":["Read",7,null,"TaskCreate"],"addedNames":[],"addedLines":[]}"#,
        );
        let cached = cached(MessageId::new(), "cached");
        let message = render_deferred_tools_delta_for_request(
            cached.id(),
            &raw,
            Some(cached),
            1,
            &enabled_facts(&["Read", "TaskCreate"]),
            false,
        )
        .expect("non-string entries do not change the source name array");
        assert_eq!(body(&message), "cached");
    }

    #[test]
    fn changed_readded_names_replace_the_cached_non_mcp_sentence() {
        let id = MessageId::new();
        let source = source(
            r#"{"type":"deferred_tools_delta","surfacedNames":["Read"],"readdedNames":["TaskCreate"],"addedNames":[],"addedLines":[]}"#,
        );
        let cached = cached(
            id,
            "<system-reminder>\n1 deferred tool is available again in this session (announced earlier in this conversation): OldName. Load via ToolSearch as before.\n</system-reminder>",
        );
        let projected = render_deferred_tools_delta_for_request(
            id,
            &source,
            Some(cached),
            1,
            &enabled_facts(&["TaskCreate"]),
            false,
        )
        .expect("changed pool renders current readded name");
        let body = body(&projected);
        assert!(body.contains("TaskCreate"));
        assert!(!body.contains("OldName"));
        assert_eq!(body.matches("available again in this session").count(), 1);
    }

    #[test]
    fn mcp_grouping_always_uses_the_two_segment_server_prefix() {
        assert_eq!(
            String::from_utf16_lossy(&group_mcp_names(&["mcp__server".encode_utf16().collect()])),
            "mcp__server__*"
        );
        assert_eq!(
            String::from_utf16_lossy(&group_mcp_names(&[
                "mcp__a-server__tool".encode_utf16().collect(),
                "mcp__a_server__tool".encode_utf16().collect(),
            ])),
            "mcp__a_server__*, mcp__a-server__*"
        );
    }

    #[test]
    fn changed_pool_rebuilds_all_reachable_name_sections_and_keeps_status_cache() {
        let id = MessageId::new();
        let raw = source(
            r#"{"type":"deferred_tools_delta","surfacedNames":["Read","TaskCreate"],"addedNames":["Hidden","TaskCreate"],"addedLines":["Hidden","TaskCreate"],"readdedNames":["mcp__srv__again","TaskCreate"],"restoredNames":["Restored"],"removedNames":["mcp__srv__removed","Local"],"retractedTools":[{"name":"mcp__srv__removed","cause":"disabled"}],"needsAuthMcpServers":["auth"],"pendingMcpServers":["pending"]}"#,
        );
        let cached = cached(
            id,
            "<system-reminder>\nThe following tools just became available and are ready to use:\nRead\nTaskCreate\n\nThe following deferred tools are now available via ToolSearch. Their schemas are NOT loaded — calling them directly will fail with InputValidationError. Use ToolSearch with query \"select:<name>[,<name>...]\" to load tool schemas before calling them:\nHidden\nTaskCreate\n\n1 deferred tool is available again (MCP server reconnected — names announced earlier in this conversation): mcp__srv__again. Load via ToolSearch as before.\n\nThe following tools are available again in this session. The earlier instruction to disregard their definitions and not call them no longer applies:\nRestored\n\nThe following deferred tools are no longer available (their MCP server disconnected). Do not search for them — ToolSearch will return no match:\nmcp__srv__removed\nLocal\n\nThis is ambient context — do not narrate it to the user unless they ask or it is directly relevant to their request.\n\nCached MCP status section\n</system-reminder>",
        );

        let projected = render_deferred_tools_delta_for_request(
            id,
            &raw,
            Some(cached),
            1,
            &enabled_facts(&["TaskCreate"]),
            false,
        )
        .expect("changed request view has a rendered row");
        let text = body(&projected);
        assert_eq!(projected.id(), id);
        assert!(text.contains("TaskCreate"));
        assert!(!text.contains("Read"));
        assert!(text.contains("The following deferred tools are now available via ToolSearch."));
        assert!(text.contains("Hidden"));
        assert!(text.contains("MCP server reconnected"));
        assert!(text.contains("Restored"));
        assert!(text.contains("MCP server disabled by the user: mcp__srv__removed"));
        assert!(text.contains("Local"));
        assert!(text.contains("Cached MCP status section"));
        assert!(
            text.contains("The following deferred tools are no longer available in this session.")
        );
    }
}

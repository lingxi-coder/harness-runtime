//! Recovery rules from the official Claude Code 2.1.286 `nat` / `vmr` / `yan`
//! functions. Keep file insertion order separate from timestamps: parallel tool
//! results can finish out of order, including after a later turn was written.

use super::reader::{LoadedTranscript, MessageIndex};
use super::schema::JsonlMessage;
use lingxi_core::types::utf16_json::JsonValueView;
use std::collections::{HashMap, HashSet};

type Messages<'a> = MessageIndex<'a>;

#[cfg(test)]
thread_local! {
    static RECOVERY_OVERLAY_GROUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn same_extra_projection(left: &JsonlMessage, right: &JsonlMessage, key: &str) -> bool {
    match (
        left.extra.contains_property(key),
        right.extra.contains_property(key),
    ) {
        (false, false) => true,
        (true, true) => {
            let key_units = key.encode_utf16().collect::<Vec<_>>();
            match (
                left.extra.subprojection_property_units(&key_units),
                right.extra.subprojection_property_units(&key_units),
            ) {
                (Ok(left), Ok(right)) => left.semantically_eq(&right),
                _ => false,
            }
        }
        _ => false,
    }
}

pub(super) fn ordered_messages<'a>(
    loaded: &LoadedTranscript,
    messages: &Messages<'a>,
) -> Vec<&'a JsonlMessage> {
    let mut seen = HashSet::new();
    let mut ordered = Vec::with_capacity(messages.len());
    for entry in &loaded.messages_in_order {
        if let Some(current) = messages.get(&entry.uuid) {
            // JS Map.set replaces values without moving their insertion slot.
            if seen.insert(current.uuid.as_str()) {
                ordered.push(current);
            }
        }
    }
    // Keep deterministic order for any indexed rows without a physical slot.
    let mut remaining: Vec<_> = messages
        .values()
        .filter(|m| !seen.contains(m.uuid.as_str()))
        .collect();
    remaining.sort_by(|a, b| a.uuid.cmp(&b.uuid));
    ordered.extend(remaining);
    ordered
}

fn message_id(message: &JsonlMessage) -> Option<&str> {
    message.message.string_at("/id").filter(|id| !id.is_empty())
}

fn block_ids<'a>(message: &'a JsonlMessage, role: &str, kind: &str, field: &str) -> Vec<&'a str> {
    if message.message_type != role {
        return Vec::new();
    }
    let Some(content) = message.message.at_pointer("/content") else {
        return Vec::new();
    };
    let Some(length) = content.array_len() else {
        return Vec::new();
    };
    (0..length)
        .filter_map(|index| content.get_index(index))
        .filter(|block| block.get_property("type").and_then(JsonValueView::as_str) == Some(kind))
        .filter_map(|block| block.get_property(field).and_then(JsonValueView::as_str))
        .collect()
}

fn tool_uses(message: &JsonlMessage) -> Vec<&str> {
    block_ids(message, "assistant", "tool_use", "id")
}
fn tool_results(message: &JsonlMessage) -> Vec<&str> {
    block_ids(message, "user", "tool_result", "tool_use_id")
}
fn has_block(message: &JsonlMessage, kind: &str) -> bool {
    message
        .message
        .at_pointer("/content")
        .and_then(JsonValueView::array_len)
        .is_some_and(|length| {
            (0..length).any(|index| {
                message
                    .message
                    .at_pointer("/content")
                    .and_then(|content| content.get_index(index))
                    .and_then(|block| block.get_property("type"))
                    .and_then(JsonValueView::as_str)
                    == Some(kind)
            })
        })
}

fn batch_member(message: &JsonlMessage, id: Option<&str>) -> bool {
    match message.message_type.as_str() {
        "assistant" => id.is_some() && message_id(message) == id,
        "user" => {
            message
                .extra
                .get_property("isMeta")
                .and_then(JsonValueView::as_bool)
                == Some(true)
                || has_block(message, "tool_result")
        }
        "attachment" => true,
        "system" => message.extra.string_at("/subtype") != Some("compact_boundary"),
        _ => false,
    }
}

fn metadata(message: &JsonlMessage) -> bool {
    match message.message_type.as_str() {
        "attachment" => true,
        "system" => message.extra.string_at("/subtype") != Some("compact_boundary"),
        "user" => {
            message
                .extra
                .get_property("isMeta")
                .and_then(JsonValueView::as_bool)
                == Some(true)
                && !has_block(message, "tool_result")
        }
        _ => false,
    }
}

fn batch_ancestors<'a>(
    messages: &Messages<'a>,
    message: &JsonlMessage,
    id: &str,
) -> Vec<&'a JsonlMessage> {
    let mut result = Vec::new();
    let mut seen = HashSet::from([message.uuid.as_str()]);
    let mut cursor = message
        .parent_uuid
        .as_deref()
        .and_then(|uuid| messages.get(uuid));
    while let Some(row) = cursor {
        if !seen.insert(&row.uuid) || !batch_member(row, Some(id)) {
            break;
        }
        result.push(row);
        cursor = row
            .parent_uuid
            .as_deref()
            .and_then(|uuid| messages.get(uuid));
    }
    result
}

/// `yan`: a saved checkpoint can name one completed tool result while the
/// durable continuation descends from another result in the same model batch.
pub(super) fn follows_checkpoint(
    messages: &Messages<'_>,
    tail: &str,
    checkpoint: &str,
    direct: bool,
    through_batch: bool,
) -> bool {
    let mut source = None;
    let mut checkpoint_calls = Vec::new();
    let mut attachment_call = false;
    if through_batch {
        let mut seen = HashSet::new();
        let mut last_meta = None;
        let mut cursor = messages.get(checkpoint);
        while let Some(row) = cursor {
            if !metadata(row) || !seen.insert(row.uuid.as_str()) {
                break;
            }
            last_meta = Some(row);
            cursor = row
                .parent_uuid
                .as_deref()
                .and_then(|uuid| messages.get(uuid));
        }
        if let Some(row) =
            cursor.filter(|row| row.message_type == "user" && has_block(row, "tool_result"))
        {
            checkpoint_calls = tool_results(row);
            source = row
                .parent_uuid
                .as_deref()
                .and_then(|uuid| messages.get(uuid));
        } else if let Some(meta) = last_meta {
            let call = meta
                .attachment_projection
                .as_ref()
                .and_then(|attachment| attachment.string_at("/toolUseID"));
            if let (Some(assistant), Some(call)) =
                (cursor.filter(|row| row.message_type == "assistant"), call)
            {
                if let Some(id) = message_id(assistant) {
                    source = std::iter::once(assistant)
                        .chain(batch_ancestors(messages, assistant, id))
                        .find(|row| tool_uses(row).contains(&call));
                    if source.is_some() {
                        checkpoint_calls.push(call);
                        attachment_call = true;
                    }
                }
            }
        } else {
            source = cursor;
        }
    }
    let calls = source.map(tool_uses).unwrap_or_default();
    let batch_id = source
        .filter(|row| row.message_type == "assistant" && !calls.is_empty())
        .and_then(message_id);
    let siblings: HashSet<_> = source
        .zip(batch_id)
        .map(|(row, id)| {
            batch_ancestors(messages, row, id)
                .iter()
                .map(|row| row.uuid.as_str())
                .collect()
        })
        .unwrap_or_default();
    let mut seen = HashSet::new();
    let mut previous: Option<&JsonlMessage> = None;
    let mut crossed_sibling = false;
    let mut cursor = Some(tail);
    while let Some(uuid) = cursor {
        if direct && uuid == checkpoint {
            return true;
        }
        if !seen.insert(uuid) {
            break;
        }
        let row = messages.get(uuid);
        if let (Some(source), Some(id), Some(row)) = (source, batch_id, row) {
            if crossed_sibling && row.uuid == source.uuid {
                return true;
            }
            if row.message_type == "assistant"
                && row.uuid != source.uuid
                && message_id(row) == Some(id)
                && has_block(row, "tool_use")
            {
                if siblings.contains(row.uuid.as_str()) {
                    return true;
                }
                crossed_sibling = true;
            } else if !batch_member(row, Some(id)) {
                crossed_sibling = false;
            }
        }
        if let (Some(source), Some(row), Some(previous)) = (source, row, previous) {
            if row.uuid == source.uuid
                && checkpoint_calls.iter().any(|id| calls.contains(id))
                && tool_results(previous).iter().any(|id| {
                    calls.contains(id)
                        && if attachment_call {
                            calls.len() > 1
                        } else {
                            !checkpoint_calls.contains(id)
                        }
                })
            {
                return true;
            }
        }
        previous = row;
        cursor = row.and_then(|row| row.parent_uuid.as_deref());
    }
    false
}

fn metadata_tail<'a>(
    first: &'a JsonlMessage,
    anchor: &JsonlMessage,
    children: &HashMap<&str, Vec<&'a JsonlMessage>>,
    seen: &HashSet<String>,
) -> Vec<&'a JsonlMessage> {
    let mut tail = Vec::new();
    let mut visiting = HashSet::new();
    let mut cursor = Some(first);
    while let Some(row) = cursor {
        if seen.contains(&row.uuid)
            || !visiting.insert(row.uuid.as_str())
            || row.is_sidechain != anchor.is_sidechain
            || !metadata(row)
        {
            return Vec::new();
        }
        tail.push(row);
        let next: Vec<_> = children
            .get(row.uuid.as_str())
            .into_iter()
            .flatten()
            .filter(|child| !seen.contains(&child.uuid))
            .copied()
            .collect();
        if next.len() > 1 {
            return Vec::new();
        }
        cursor = next.first().copied();
    }
    tail
}

#[derive(Default)]
struct RecoverySpan<'a> {
    start: usize,
    end: usize,
    recovered: Vec<&'a JsonlMessage>,
    anchored: HashMap<String, Vec<&'a JsonlMessage>>,
}

/// `vmr`, with upstream's default-enabled tool-call identity recovery. Only
/// unambiguous identities are eligible; a result belonging to a different agent
/// or already answered call cannot enter the chain through the fallback index.
pub(super) fn recover_parallel_results(
    messages: &Messages<'_>,
    ordered: &[&JsonlMessage],
    chain: Vec<JsonlMessage>,
    seen: &mut HashSet<String>,
) -> Vec<JsonlMessage> {
    let mut groups: HashMap<&str, Vec<&JsonlMessage>> = HashMap::new();
    let mut results: HashMap<&str, Vec<&JsonlMessage>> = HashMap::new();
    let mut children: HashMap<&str, Vec<&JsonlMessage>> = HashMap::new();
    let mut call_sources: HashMap<&str, Option<&JsonlMessage>> = HashMap::new();
    let mut order = HashMap::new();
    for (index, &row) in ordered.iter().enumerate() {
        order.insert(row.uuid.as_str(), index * 2);
        if let Some(parent) = row.parent_uuid.as_deref() {
            children.entry(parent).or_default().push(row);
        }
        if row.message_type == "assistant" {
            if let Some(id) = message_id(row) {
                groups.entry(id).or_default().push(row);
                for call in tool_uses(row) {
                    let value = match call_sources.get(call) {
                        None => Some(row),
                        Some(Some(prior)) if message_id(prior) == Some(id) => Some(row),
                        _ => None,
                    };
                    call_sources.insert(call, value);
                }
            }
        }
    }
    for &row in ordered {
        if row.message_type != "user" || !has_block(row, "tool_result") {
            continue;
        }
        let mut owners = HashSet::new();
        if let Some(parent) = row.parent_uuid.as_deref() {
            owners.insert(parent);
        }
        let ids = tool_results(row);
        let parent_calls = row
            .parent_uuid
            .as_deref()
            .and_then(|uuid| messages.get(uuid))
            .map(tool_uses)
            .unwrap_or_default();
        if ids.is_empty() || !ids.iter().all(|id| parent_calls.contains(id)) {
            let explicit = row
                .extra
                .string_at("/sourceToolAssistantUUID")
                .and_then(|uuid| messages.get(uuid));
            for owner in explicit.into_iter().chain(
                ids.iter()
                    .filter_map(|id| call_sources.get(id).copied().flatten()),
            ) {
                if owner.is_sidechain == row.is_sidechain
                    && same_extra_projection(owner, row, "agentId")
                {
                    owners.insert(&owner.uuid);
                }
            }
        }
        for owner in owners {
            results.entry(owner).or_default().push(row);
        }
    }
    let positions: HashMap<_, _> = chain
        .iter()
        .enumerate()
        .map(|(index, row)| (row.uuid.as_str(), index))
        .collect();
    let chain_calls: HashSet<_> = chain.iter().flat_map(tool_results).collect();
    let mut overrides: HashMap<&str, usize> = HashMap::new();
    let rank = |row: &JsonlMessage, overrides: &HashMap<&str, usize>| {
        overrides
            .get(row.uuid.as_str())
            .copied()
            .unwrap_or_else(|| order.get(row.uuid.as_str()).copied().unwrap_or(usize::MAX))
    };
    let mut processed = HashSet::new();
    let mut spans = Vec::<RecoverySpan<'_>>::new();
    for (start, assistant) in chain
        .iter()
        .enumerate()
        .filter(|(_, row)| row.message_type == "assistant")
    {
        let Some(id) = message_id(assistant) else {
            continue;
        };
        if !processed.insert(id) {
            continue;
        }
        let group = groups.get(id).cloned().unwrap_or_else(|| vec![assistant]);
        let member_ids: HashSet<_> = group.iter().map(|row| row.uuid.as_str()).collect();
        let mut recovered: Vec<_> = group
            .iter()
            .filter(|row| !seen.contains(&row.uuid))
            .copied()
            .collect();
        let mut direct = Vec::new();
        let mut indirect = Vec::new();
        let mut unique = HashSet::new();
        for row in &group {
            for &result in results.get(row.uuid.as_str()).into_iter().flatten() {
                if seen.contains(&result.uuid) || !unique.insert(result.uuid.as_str()) {
                    continue;
                }
                if result
                    .parent_uuid
                    .as_deref()
                    .is_some_and(|parent| member_ids.contains(parent))
                {
                    direct.push(result);
                } else {
                    indirect.push(result);
                }
            }
        }
        if !indirect.is_empty() {
            #[cfg(test)]
            RECOVERY_OVERLAY_GROUPS.with(|groups| groups.set(groups.get() + 1));
            // The selected chain is an immutable base. Only this group's
            // additional answers belong in the overlay; sharing those extras
            // with another batch would change its identity recovery rules.
            let mut extra_answered: HashSet<_> =
                direct.iter().flat_map(|row| tool_results(row)).collect();
            indirect.sort_by_key(|row| rank(row, &overrides));
            let after_group = group
                .iter()
                .map(|row| order.get(row.uuid.as_str()).copied().unwrap_or(0))
                .max()
                .unwrap_or(0)
                + 1;
            for row in indirect {
                let ids = tool_results(row);
                if !ids.iter().any(|id| {
                    !chain_calls.contains(id)
                        && !extra_answered.contains(id)
                        && call_sources
                            .get(id)
                            .copied()
                            .flatten()
                            .is_some_and(|owner| member_ids.contains(owner.uuid.as_str()))
                }) {
                    continue;
                }
                extra_answered.extend(ids);
                overrides.insert(row.uuid.as_str(), rank(row, &overrides).max(after_group));
                direct.push(row);
            }
        }
        recovered.extend(direct);
        for row in &recovered {
            seen.insert(row.uuid.clone());
        }
        let mut end = group
            .iter()
            .filter_map(|row| positions.get(row.uuid.as_str()).copied())
            .max()
            .unwrap_or(start)
            + 1;
        while end < chain.len() && batch_member(&chain[end], Some(id)) {
            end += 1;
        }
        let mut anchored = HashMap::new();
        if group.iter().any(|row| has_block(row, "tool_use")) {
            let mut anchors = group.clone();
            let mut anchor_ids = member_ids.clone();
            for row in &group {
                for &result in results.get(row.uuid.as_str()).into_iter().flatten() {
                    if seen.contains(&result.uuid) && anchor_ids.insert(result.uuid.as_str()) {
                        anchors.push(result);
                    }
                }
            }
            for anchor in anchors {
                for &child in children.get(anchor.uuid.as_str()).into_iter().flatten() {
                    for row in metadata_tail(child, anchor, &children, seen) {
                        seen.insert(row.uuid.clone());
                        if let Some(&anchor_rank) = overrides.get(anchor.uuid.as_str()) {
                            overrides.insert(
                                row.uuid.as_str(),
                                order[row.uuid.as_str()].max(anchor_rank),
                            );
                        }
                        recovered.push(row);
                    }
                }
            }
            for anchor in &chain[start + 1..end] {
                if !metadata(anchor) {
                    continue;
                }
                let mut tail = Vec::new();
                for &child in children.get(anchor.uuid.as_str()).into_iter().flatten() {
                    for row in metadata_tail(child, anchor, &children, seen) {
                        seen.insert(row.uuid.clone());
                        tail.push(row);
                    }
                }
                if !tail.is_empty() {
                    anchored.insert(anchor.uuid.clone(), tail);
                }
            }
        }
        if recovered.is_empty() && anchored.is_empty() {
            continue;
        }
        recovered.sort_by_key(|row| (rank(row, &overrides), order[row.uuid.as_str()]));
        let span = RecoverySpan {
            start,
            end,
            recovered,
            anchored,
        };
        if let Some(prior) = spans.last_mut().filter(|prior| start < prior.end) {
            prior.end = prior.end.max(end);
            prior.recovered.extend(span.recovered);
            prior
                .recovered
                .sort_by_key(|row| (rank(row, &overrides), order[row.uuid.as_str()]));
            for (uuid, tail) in span.anchored {
                prior.anchored.entry(uuid).or_default().extend(tail);
            }
        } else {
            spans.push(span);
        }
    }
    if spans.is_empty() {
        return chain;
    }
    // Finish the borrowed recovery plan before consuming the selected chain.
    // Only newly recovered source rows need copies; the chain already owns
    // its exact projections and any parent/sidechain overlays applied by the
    // DAG walk.
    let spans: Vec<_> = spans
        .into_iter()
        .map(|span| {
            let recovered: Vec<_> = span
                .recovered
                .into_iter()
                .map(|row| (rank(row, &overrides), row.clone()))
                .collect();
            let anchored: HashMap<_, Vec<_>> = span
                .anchored
                .into_iter()
                .map(|(uuid, tail)| (uuid, tail.into_iter().cloned().collect()))
                .collect();
            (span.start, span.end, recovered, anchored)
        })
        .collect();
    let mut output = Vec::with_capacity(chain.len());
    let mut original = chain.into_iter();
    let mut cursor = 0;
    for (start, end, recovered, mut anchored) in spans {
        output.extend(original.by_ref().take(start + 1 - cursor));
        let mut recovered = recovered.into_iter().peekable();
        for row in original.by_ref().take(end - start - 1) {
            while recovered
                .peek()
                .is_some_and(|(rank, _)| *rank < order[row.uuid.as_str()])
            {
                output.push(recovered.next().unwrap().1);
            }
            let tail = anchored.remove(&row.uuid);
            output.push(row);
            if let Some(tail) = tail {
                output.extend(tail);
            }
        }
        output.extend(recovered.map(|(_, row)| row));
        cursor = end;
    }
    output.extend(original);
    output
}

/// `Smr`'s default depth-first traversal retains persisted hook/attachment rows
/// after the last user/assistant message, with stable sibling timestamp order.
pub(super) fn append_metadata_descendants(
    ordered: &[&JsonlMessage],
    tip: &str,
    chain: &mut Vec<JsonlMessage>,
    seen: &mut HashSet<String>,
) {
    let mut children: HashMap<&str, Vec<&JsonlMessage>> = HashMap::new();
    for &row in ordered {
        if matches!(row.message_type.as_str(), "user" | "assistant") {
            continue;
        }
        if let Some(parent) = row.parent_uuid.as_deref() {
            children.entry(parent).or_default().push(row);
        }
    }
    for siblings in children.values_mut() {
        siblings.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    }
    let mut pending: Vec<_> = children
        .get(tip)
        .into_iter()
        .flatten()
        .rev()
        .copied()
        .collect();
    while let Some(row) = pending.pop() {
        if !seen.insert(row.uuid.clone()) {
            continue;
        }
        chain.push(row.clone());
        pending.extend(
            children
                .get(row.uuid.as_str())
                .into_iter()
                .flatten()
                .rev()
                .copied(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::reader::route_lines;
    use super::*;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use serde_json::{json, Value};

    fn row(uuid: &str, parent: Option<&str>, kind: &str, id: &str, calls: &[&str]) -> Value {
        let content = match kind {
            "assistant" => calls
                .iter()
                .map(|call| json!({"type":"tool_use", "id":call, "name":"Read", "input":{}}))
                .collect::<Vec<_>>(),
            "user" if !calls.is_empty() => calls
                .iter()
                .map(|call| json!({"type":"tool_result", "tool_use_id":call, "content":"result"}))
                .collect(),
            _ => vec![json!({"type":"text", "text":"text"})],
        };
        json!({
            "type":kind, "uuid":uuid, "parentUuid":parent, "sessionId":"session",
            "cwd":"/workspace", "timestamp":"2026-11-12T00:00:00.000Z",
            "message":{"id":id, "role":kind, "content":content},
        })
    }

    fn load(rows: &[Value]) -> LoadedTranscript {
        route_lines(
            &rows
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    fn recover(loaded: &LoadedTranscript, chain: Vec<JsonlMessage>) -> Vec<JsonlMessage> {
        let messages = loaded.messages_by_uuid();
        let ordered = ordered_messages(loaded, &messages);
        let mut seen = chain.iter().map(|row| row.uuid.clone()).collect();
        recover_parallel_results(&messages, &ordered, chain, &mut seen)
    }

    fn select(loaded: &LoadedTranscript, ids: &[&str]) -> Vec<JsonlMessage> {
        ids.iter()
            .map(|id| loaded.message(id).unwrap().clone())
            .collect()
    }

    fn ids(chain: &[JsonlMessage]) -> Vec<&str> {
        chain.iter().map(|row| row.uuid.as_str()).collect()
    }

    fn bytes(row: &JsonlMessage) -> Vec<u8> {
        crate::jsonl::exact_json::native_message_bytes(row).unwrap()
    }

    #[test]
    fn long_serial_tool_history_keeps_no_span_fast_path_and_needs_no_overlays() {
        let mut rows = vec![row("root", None, "user", "", &[])];
        let mut parent = "root".to_owned();
        for n in 0..512 {
            let assistant = format!("assistant-{n}");
            let result = format!("result-{n}");
            let call = format!("call-{n}");
            rows.push(row(
                &assistant,
                Some(&parent),
                "assistant",
                &format!("batch-{n}"),
                &[&call],
            ));
            rows.push(row(&result, Some(&assistant), "user", "", &[&call]));
            parent = result;
        }
        let loaded = load(&rows);
        let chain = loaded.messages_in_order.clone();
        let allocation = chain.as_ptr();
        let expected = chain.iter().map(bytes).collect::<Vec<_>>();
        RECOVERY_OVERLAY_GROUPS.with(|groups| groups.set(0));
        let recovered = recover(&loaded, chain);
        assert_eq!(
            recovered.as_ptr(),
            allocation,
            "no recovery must return the original owned vector"
        );
        assert_eq!(recovered.iter().map(bytes).collect::<Vec<_>>(), expected);
        assert_eq!(
            RECOVERY_OVERLAY_GROUPS.with(|groups| groups.get()),
            0,
            "ordinary serial groups must not allocate answered-ID overlays"
        );
    }

    #[test]
    fn recovered_parallel_suffix_moves_original_rows_and_preserves_exact_overlays() {
        let mut rows = Vec::new();
        let mut parent = None;
        let mut prefix = Vec::new();
        for n in 0..256 {
            let uuid = format!("prefix-{n}");
            rows.push(row(&uuid, parent.as_deref(), "user", "", &[]));
            parent = Some(uuid.clone());
            prefix.push(uuid);
        }
        rows.extend([
            row("a1", parent.as_deref(), "assistant", "batch", &["call1"]),
            row("a2", Some("a1"), "assistant", "batch", &["call2"]),
            row("r1", Some("a1"), "user", "", &["call1"]),
            row("r2", Some("a2"), "user", "", &["call2"]),
            row("hook", Some("r2"), "attachment", "", &[]),
            row("tail", Some("hook"), "system", "", &[]),
            row("answer", Some("r1"), "assistant", "answer", &[]),
        ]);
        let mut loaded = load(&rows);
        for message in &mut loaded.messages_in_order {
            if message.uuid.starts_with("prefix") || message.uuid == "r2" {
                message.message = Utf16JsonProjection::parse(r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"call2","content":"lone\ud800-\udfff"}]}"#).unwrap();
                // Prefix rows still count as ordinary chain-owned rows; their
                // exact outer payloads must move with the inner projection.
                message.extra = Utf16JsonProjection::parse(
                    r#"{"opaque":"outer\ud800","nested":{"b":2,"a":1}}"#,
                )
                .unwrap();
                message.tool_use_result_projection =
                    Some(Utf16JsonProjection::parse(r#"{"raw":"\udfff"}"#).unwrap());
            }
        }
        loaded.reindex_messages();
        let mut chain = prefix
            .iter()
            .map(|id| loaded.message(id).unwrap().clone())
            .collect::<Vec<_>>();
        chain.extend(select(&loaded, &["a1", "r1", "answer"]));
        let a1 = chain.iter_mut().find(|row| row.uuid == "a1").unwrap();
        a1.parent_uuid = Some("native-parent-overlay".into());
        a1.is_sidechain = true;
        let original: HashMap<_, _> = chain
            .iter()
            .map(|row| (row.uuid.clone(), (row.uuid.as_ptr(), bytes(row))))
            .collect();
        let recovered = recover(&loaded, chain);
        let mut expected = prefix.iter().map(String::as_str).collect::<Vec<_>>();
        expected.extend(["a1", "a2", "r1", "r2", "hook", "tail", "answer"]);
        assert_eq!(ids(&recovered), expected);
        for message in &recovered {
            if let Some((allocation, expected)) = original.get(&message.uuid) {
                assert_eq!(
                    message.uuid.as_ptr(),
                    *allocation,
                    "original row {} was cloned",
                    message.uuid
                );
                assert_eq!(
                    bytes(message),
                    *expected,
                    "original row {} lost exact source data or overlays",
                    message.uuid
                );
            } else {
                assert_eq!(
                    bytes(message),
                    bytes(loaded.message(&message.uuid).unwrap())
                );
            }
        }
        assert_eq!(
            recovered
                .iter()
                .find(|row| row.uuid == "r2")
                .unwrap()
                .message
                .string_units("/content/0/content"),
            Some(vec![108, 111, 110, 101, 0xd800, 45, 0xdfff])
        );
    }

    #[test]
    fn indirect_answer_overlay_is_ordered_and_isolated_between_groups() {
        let loaded = load(&[
            row("root", None, "user", "", &[]),
            row("a", Some("root"), "assistant", "batch-a", &["call-a"]),
            row("b", Some("a"), "assistant", "batch-b", &["call-b"]),
            row("both", Some("root"), "user", "", &["call-a", "call-b"]),
            row("duplicate-a", Some("root"), "user", "", &["call-a"]),
            row("own-b", Some("root"), "user", "", &["call-b"]),
            row("answer", Some("b"), "assistant", "answer", &[]),
        ]);
        let recovered = recover(&loaded, select(&loaded, &["root", "a", "b", "answer"]));
        assert_eq!(ids(&recovered), ["root", "a", "both", "b", "own-b", "answer"], "a multi-ID answer is local to its accepted group; all its IDs suppress later answers only within that group");
    }

    #[test]
    fn overlapping_recovery_spans_keep_original_order_and_row_allocations() {
        let loaded = load(&[
            row("root", None, "user", "", &[]),
            row("a1", Some("root"), "assistant", "batch-a", &["a1"]),
            row("b1", Some("a1"), "assistant", "batch-b", &["b1"]),
            row("a2", Some("b1"), "assistant", "batch-a", &["a2"]),
            row("a3", Some("a2"), "assistant", "batch-a", &["a3"]),
            row("b2", Some("b1"), "assistant", "batch-b", &["b2"]),
            row("ra", Some("a3"), "user", "", &["a3"]),
            row("rb", Some("b2"), "user", "", &["b2"]),
            row("answer", Some("a2"), "assistant", "answer", &[]),
        ]);
        let chain = select(&loaded, &["root", "a1", "b1", "a2", "answer"]);
        let allocations: HashMap<_, _> = chain
            .iter()
            .map(|row| (row.uuid.clone(), row.uuid.as_ptr()))
            .collect();
        let recovered = recover(&loaded, chain);
        assert_eq!(
            ids(&recovered),
            ["root", "a1", "b1", "a2", "a3", "b2", "ra", "rb", "answer"]
        );
        for row in recovered {
            if let Some(allocation) = allocations.get(&row.uuid) {
                assert_eq!(row.uuid.as_ptr(), *allocation);
            }
        }
    }
}

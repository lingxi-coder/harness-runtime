//! Recovery rules from the official Claude Code 2.1.286 `nat` / `vmr` / `yan`
//! functions. Keep file insertion order separate from timestamps: parallel tool
//! results can finish out of order, including after a later turn was written.

use super::reader::LoadedTranscript;
use super::schema::JsonlMessage;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

type Messages = HashMap<String, JsonlMessage>;

pub(super) fn ordered_messages<'a>(
    loaded: &LoadedTranscript,
    messages: &'a Messages,
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
    // Some callers construct LoadedTranscript directly with only by_uuid.
    let mut remaining: Vec<_> = messages
        .values()
        .filter(|m| !seen.contains(m.uuid.as_str()))
        .collect();
    remaining.sort_by(|a, b| a.uuid.cmp(&b.uuid));
    ordered.extend(remaining);
    ordered
}

fn message_id(message: &JsonlMessage) -> Option<&str> {
    message
        .message
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

fn block_ids<'a>(message: &'a JsonlMessage, role: &str, kind: &str, field: &str) -> Vec<&'a str> {
    if message.message_type != role {
        return Vec::new();
    }
    message
        .message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some(kind))
        .filter_map(|block| block.get(field).and_then(Value::as_str))
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
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some(kind))
        })
}

fn batch_member(message: &JsonlMessage, id: Option<&str>) -> bool {
    match message.message_type.as_str() {
        "assistant" => id.is_some() && message_id(message) == id,
        "user" => {
            message.extra.get("isMeta").and_then(Value::as_bool) == Some(true)
                || has_block(message, "tool_result")
        }
        "attachment" => true,
        "system" => {
            message.extra.get("subtype").and_then(Value::as_str) != Some("compact_boundary")
        }
        _ => false,
    }
}

fn metadata(message: &JsonlMessage) -> bool {
    match message.message_type.as_str() {
        "attachment" => true,
        "system" => {
            message.extra.get("subtype").and_then(Value::as_str) != Some("compact_boundary")
        }
        "user" => {
            message.extra.get("isMeta").and_then(Value::as_bool) == Some(true)
                && !has_block(message, "tool_result")
        }
        _ => false,
    }
}

fn batch_ancestors<'a>(
    messages: &'a Messages,
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
    messages: &Messages,
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
                .extra
                .get("attachment")
                .and_then(|a| a.get("toolUseID"))
                .and_then(Value::as_str);
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
    messages: &Messages,
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
                .get("sourceToolAssistantUUID")
                .and_then(Value::as_str)
                .and_then(|uuid| messages.get(uuid));
            for owner in explicit.into_iter().chain(
                ids.iter()
                    .filter_map(|id| call_sources.get(id).copied().flatten()),
            ) {
                if owner.is_sidechain == row.is_sidechain
                    && owner.extra.get("agentId") == row.extra.get("agentId")
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
        let mut answered = chain_calls.clone();
        for row in &direct {
            answered.extend(tool_results(row));
        }
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
                !answered.contains(id)
                    && call_sources
                        .get(id)
                        .copied()
                        .flatten()
                        .is_some_and(|owner| member_ids.contains(owner.uuid.as_str()))
            }) {
                continue;
            }
            answered.extend(ids);
            overrides.insert(row.uuid.as_str(), rank(row, &overrides).max(after_group));
            direct.push(row);
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
    let mut output = Vec::new();
    let mut cursor = 0;
    for span in spans {
        output.extend_from_slice(&chain[cursor..=span.start]);
        let mut recovered = span.recovered.into_iter().peekable();
        for row in &chain[span.start + 1..span.end] {
            while recovered
                .peek()
                .is_some_and(|next| rank(next, &overrides) < order[row.uuid.as_str()])
            {
                output.push(recovered.next().unwrap().clone());
            }
            output.push(row.clone());
            if let Some(tail) = span.anchored.get(&row.uuid) {
                output.extend(tail.iter().map(|row| (*row).clone()));
            }
        }
        output.extend(recovered.cloned());
        cursor = span.end;
    }
    output.extend_from_slice(&chain[cursor..]);
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

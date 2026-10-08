//! MCP instruction announcements replay their baseline from durable attachment
//! rows. Claude Code 2.1.286: `zvt`/`Gxe` in src_183727495.js at character
//! offsets 1456797/1458184, renderer at 4049801. The pinned source-generated
//! fixtures also exercise removal, reconnect, cache gates and old transcripts.

use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

const KIND: &str = "mcp_instructions_delta";

/// Host-provided instructions appended after a matching eligible server's raw
/// instructions. Claude 2.1.286 `Gxe` accepts client-only blocks as well.
#[derive(Clone, Copy)]
pub(crate) struct ClientInstructionBlock<'a> {
    pub(crate) server_name: &'a str,
    pub(crate) block: &'a str,
}

/// Native `NR` / `Gvt` / `_ne`, reproduced by the hash-pinned generator.
pub(crate) const BUILTIN_CLIENT_BLOCKS: [ClientInstructionBlock<'static>; 1] =
    [ClientInstructionBlock {
        server_name: "computer-use",
        block: include_str!("computer_use_mcp_instructions_286.txt"),
    }];

/// Build the attachment payload for the next model turn. `prior` contains full
/// transcript attachment rows in transcript order; `eligible_names` contains
/// both connected and cached clients, including clients with no instructions.
/// `current` contains raw server instructions, without the server heading.
///
/// The cache gate controls change detection and raw baseline recording. A
/// connected server clearing its instructions does not retract its earlier
/// announcement; only leaving the eligible client pool emits a removal.
pub(crate) fn attachment(
    current: &[(String, String)],
    eligible_names: &[String],
    client_blocks: &[ClientInstructionBlock<'_>],
    prior: &[Value],
    cache_enabled: bool,
) -> Option<Value> {
    // Some(raw) is the current-format baseline. None marks an older persisted
    // announcement that did not record its raw server instructions.
    let mut announced: BTreeMap<&str, Option<&str>> = BTreeMap::new();
    for row in prior {
        if row.get("type").and_then(Value::as_str) != Some("attachment") {
            continue;
        }
        let Some(previous) = row.get("attachment") else {
            continue;
        };
        if previous.get("type").and_then(Value::as_str) != Some(KIND) {
            continue;
        }
        if let (Some(blocks), Some(names)) = (
            previous.get("addedBlocks").and_then(Value::as_array),
            previous.get("addedNames").and_then(Value::as_array),
        ) {
            let raw = previous
                .get("addedServerInstructions")
                .and_then(Value::as_array);
            for (index, name) in names.iter().enumerate() {
                let Some(name) = name.as_str() else {
                    continue;
                };
                let baseline = if blocks.get(index).and_then(Value::as_str).is_none() {
                    Some("")
                } else {
                    raw.and_then(|values| values.get(index))
                        .and_then(Value::as_str)
                };
                announced.insert(name, baseline);
            }
        }
        for name in strings(previous.get("removedNames")) {
            announced.remove(name);
        }
    }

    let eligible: HashSet<&str> = eligible_names.iter().map(String::as_str).collect();
    // Matches Gxe's Map: the last nonempty value for a repeated name wins.
    let mut instructions = BTreeMap::new();
    for (name, raw) in current {
        if eligible.contains(name.as_str()) && !raw.is_empty() {
            instructions.insert(name.as_str(), raw.as_str());
        }
    }
    let mut blocks: BTreeMap<_, _> = instructions
        .iter()
        .map(|(name, raw)| (*name, format!("## {name}\n{raw}")))
        .collect();
    for client in client_blocks {
        if !eligible.contains(client.server_name) {
            continue;
        }
        blocks
            .entry(client.server_name)
            .and_modify(|block| {
                block.push_str("\n\n");
                block.push_str(client.block);
            })
            .or_insert_with(|| format!("## {}\n{}", client.server_name, client.block));
    }
    let mut added: Vec<_> = blocks
        .into_iter()
        .map(|(name, block)| (name, block, instructions.get(name).copied().unwrap_or("")))
        .filter(|(name, _, raw)| match announced.get(name) {
            None => true,
            Some(Some(previous)) => cache_enabled && !raw.is_empty() && previous != raw,
            Some(None) => false,
        })
        .collect();
    added.sort_by(|(left, _, _), (right, _, _)| tool_api::wire::locale_cmp(left, right));
    let mut removed: Vec<_> = announced
        .keys()
        .filter(|name| !eligible.contains(**name))
        .copied()
        .collect();
    // JavaScript's default Array.sort compares UTF-16 code units.
    removed.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    if added.is_empty() && removed.is_empty() {
        return None;
    }

    let mut payload = json!({
        "type": KIND,
        "addedNames": added.iter().map(|(name, _, _)| *name).collect::<Vec<_>>(),
        "addedBlocks": added.iter().map(|(_, block, _)| block).collect::<Vec<_>>(),
        "removedNames": removed,
    });
    if cache_enabled {
        payload["addedServerInstructions"] =
            json!(added.iter().map(|(_, _, raw)| *raw).collect::<Vec<_>>());
    }
    Some(payload)
}

/// Render one attachment payload into the exact model-visible reminder.
/// Replay tolerates non-string array members by dropping those members, as the
/// upstream `mi` helper does. An empty attachment produces no model message.
pub(crate) fn render(attachment: &Value) -> Option<String> {
    if attachment.get("type").and_then(Value::as_str) != Some(KIND) {
        return None;
    }
    let blocks = strings(attachment.get("addedBlocks"));
    let names = strings(attachment.get("addedNames"));
    let removed = strings(attachment.get("removedNames"));
    let mut sections = Vec::new();
    if !blocks.is_empty() && !names.is_empty() {
        sections.push(format!(
            "# MCP Server Instructions\n\nThe following MCP servers have provided instructions for how to use their tools and resources:\n\n{}",
            blocks.join("\n\n")
        ));
    }
    if !removed.is_empty() {
        sections.push(format!(
            "The following MCP servers have disconnected. Their instructions above no longer apply:\n{}",
            removed.join("\n")
        ));
        sections.push(super::memory_update::AMBIENT_CONTEXT_TRAILER.to_owned());
    }
    if sections.is_empty() {
        return None;
    }
    Some(format!(
        "<system-reminder>\n{}\n</system-reminder>",
        sections.join("\n\n")
    ))
}

fn strings(value: Option<&Value>) -> Vec<&str> {
    value
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle_cases() -> Vec<Value> {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../platforms/posix/tests/fixtures/mcp_negotiation_286_oracle.json"
        ))
        .expect("valid pinned oracle fixture");
        assert_eq!(fixture["version"], "2.1.286");
        fixture["instructionCases"].as_array().unwrap().clone()
    }

    #[test]
    fn instruction_delta_matches_extracted_286_producer() {
        let mut checked = 0;
        for case in oracle_cases() {
            let clients = case["clients"].as_array().unwrap();
            let eligible: Vec<String> = clients
                .iter()
                .filter(|client| matches!(client["type"].as_str(), Some("connected" | "cached")))
                .map(|client| client["name"].as_str().unwrap().to_owned())
                .collect();
            let current: Vec<(String, String)> = clients
                .iter()
                .filter_map(|client| {
                    Some((
                        client["name"].as_str()?.to_owned(),
                        client["instructions"].as_str()?.to_owned(),
                    ))
                })
                .collect();
            let empty = Vec::new();
            let client_blocks: Vec<_> = case["clientBlocks"]
                .as_array()
                .unwrap_or(&empty)
                .iter()
                .map(|block| ClientInstructionBlock {
                    server_name: block["serverName"].as_str().unwrap(),
                    block: block["block"].as_str().unwrap(),
                })
                .collect();
            let prior = case["history"].as_array().unwrap_or(&empty);
            let actual = attachment(
                &current,
                &eligible,
                &client_blocks,
                prior,
                case["cacheEnabled"].as_bool().unwrap_or(true),
            );
            let mut expected = case["expected"]["delta"].clone();
            if !expected.is_null() {
                expected["type"] = KIND.into();
            }
            assert_eq!(actual.unwrap_or(Value::Null), expected, "{}", case["name"]);
            checked += 1;
        }
        assert_eq!(checked, 14);
    }

    #[test]
    fn builtin_computer_use_block_matches_hash_pinned_native_string() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/instructions-2.1.286/computer_use_mcp.json"
        ))
        .unwrap();
        assert_eq!(fixture["version"], "2.1.286");
        assert_eq!(
            fixture["binary_sha256"],
            "75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433"
        );
        assert_eq!(BUILTIN_CLIENT_BLOCKS[0].server_name, fixture["serverName"]);
        assert_eq!(BUILTIN_CLIENT_BLOCKS[0].block, fixture["block"]);
    }

    #[test]
    fn client_only_blocks_record_empty_raw_baseline_and_do_not_reannounce() {
        let eligible = vec!["computer-use".into()];
        let first = attachment(&[], &eligible, &BUILTIN_CLIENT_BLOCKS, &[], true).unwrap();
        assert_eq!(first["addedServerInstructions"], json!([""]));
        let prior = vec![json!({"type":"attachment", "attachment":first})];
        assert!(attachment(&[], &eligible, &BUILTIN_CLIENT_BLOCKS, &prior, true).is_none());
        assert_eq!(
            attachment(&[], &[], &BUILTIN_CLIENT_BLOCKS, &prior, true).unwrap()["removedNames"],
            json!(["computer-use"])
        );
        let server = vec![("computer-use".into(), "server instructions".into())];
        let changed = attachment(&server, &eligible, &BUILTIN_CLIENT_BLOCKS, &prior, true)
            .expect("new nonempty server instructions update the raw baseline");
        assert_eq!(
            changed["addedServerInstructions"],
            json!(["server instructions"])
        );
        assert_eq!(
            changed["addedBlocks"][0],
            format!(
                "## computer-use\nserver instructions\n\n{}",
                BUILTIN_CLIENT_BLOCKS[0].block
            )
        );
        let updated = vec![json!({"type":"attachment", "attachment":changed})];
        assert!(attachment(&[], &eligible, &BUILTIN_CLIENT_BLOCKS, &updated, true).is_none());
        assert!(attachment(&[], &[], &BUILTIN_CLIENT_BLOCKS, &[], true).is_none());
    }

    #[test]
    fn instruction_renderer_matches_extracted_286_bytes() {
        for case in oracle_cases() {
            let mut payload = case["expected"]["delta"].clone();
            let actual = if payload.is_null() {
                None
            } else {
                payload["type"] = KIND.into();
                render(&payload)
            };
            let rendered = case["expected"]["rendered"].as_array().unwrap();
            let expected = rendered
                .first()
                .and_then(|row| row["message"]["content"].as_str())
                .map(str::to_owned);
            assert_eq!(actual, expected, "{}", case["name"]);
        }
    }
}

//! MCP instruction announcements replay their baseline from durable attachment
//! rows. Claude Code 2.1.286: `zvt`/`Gxe` in src_183727495.js at character
//! offsets 1456797/1458184, renderer at 4049801. The pinned source-generated
//! fixtures also exercise removal, reconnect, cache gates and old transcripts.

use lingxi_core::types::utf16_json::Utf16JsonProjection;
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
fn string_projection(value: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(value.to_owned(), value.encode_utf16().collect())
        .expect("valid Rust string UTF-16 projection")
}

fn source_string_units(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display text is derived from exact UTF-16 code units")
}

fn concat_source_strings(parts: &[&Utf16JsonProjection]) -> Utf16JsonProjection {
    source_string_units(
        parts
            .iter()
            .flat_map(|part| part.string_units("").unwrap_or_default())
            .collect(),
    )
}

fn source_string_items(value: &Utf16JsonProjection, pointer: &str) -> Vec<Utf16JsonProjection> {
    let Some(count) = value.array_len(pointer) else {
        return Vec::new();
    };
    (0..count)
        .filter_map(|index| {
            let item = value.subprojection(&format!("{pointer}/{index}")).ok()?;
            item.string_units("").map(|_| item)
        })
        .collect()
}

fn strings_at<'a>(value: &'a Utf16JsonProjection, pointer: &str) -> Vec<&'a str> {
    let Some(array) = value.at_pointer(pointer) else {
        return Vec::new();
    };
    let Some(count) = array.array_len() else {
        return Vec::new();
    };
    (0..count)
        .filter_map(|index| array.get_index(index)?.as_str())
        .collect()
}

/// Build the attachment payload for the next model turn. `prior` contains full
/// transcript attachment rows in transcript order; `eligible_names` contains
/// both connected and cached clients, including clients with no instructions.
/// `current` contains raw server instructions, without the server heading.
///
/// The cache gate controls change detection and raw baseline recording. A
/// connected server clearing its instructions does not retract its earlier
/// announcement; only leaving the eligible client pool emits a removal.
pub(crate) fn attachment(
    current: &[(String, Utf16JsonProjection)],
    eligible_names: &[String],
    client_blocks: &[ClientInstructionBlock<'_>],
    prior: &[Utf16JsonProjection],
    cache_enabled: bool,
) -> Option<Utf16JsonProjection> {
    // Some(raw) is the current-format baseline. None marks an older persisted
    // announcement that did not record its raw server instructions.
    let mut announced: BTreeMap<&str, Option<Utf16JsonProjection>> = BTreeMap::new();
    for row in prior {
        if row.string_at("/type") != Some("attachment") {
            continue;
        }
        if row.string_at("/attachment/type") != Some(KIND) {
            continue;
        }
        let blocks = row.at_pointer("/attachment/addedBlocks");
        let names = row.at_pointer("/attachment/addedNames");
        if let (Some(blocks), Some(names)) = (blocks, names) {
            if let (Some(block_count), Some(name_count)) = (blocks.array_len(), names.array_len()) {
                for index in 0..name_count {
                    let Some(name) = names.get_index(index).and_then(|value| value.as_str()) else {
                        continue;
                    };
                    let baseline = if index >= block_count
                        || blocks
                            .get_index(index)
                            .and_then(|value| value.as_str())
                            .is_none()
                    {
                        Some(string_projection(""))
                    } else {
                        row.subprojection(&format!("/attachment/addedServerInstructions/{index}"))
                            .ok()
                            .filter(|value| value.string_units("").is_some())
                    };
                    announced.insert(name, baseline);
                }
            }
        }
        for name in strings_at(row, "/attachment/removedNames") {
            announced.remove(name);
        }
    }

    let eligible: HashSet<&str> = eligible_names.iter().map(String::as_str).collect();
    // Matches Gxe's Map: the last nonempty value for a repeated name wins.
    let mut instructions = BTreeMap::new();
    for (name, raw) in current {
        if eligible.contains(name.as_str())
            && raw.string_units("").is_some_and(|units| !units.is_empty())
        {
            instructions.insert(name.as_str(), raw);
        }
    }
    let mut blocks: BTreeMap<_, _> = instructions
        .iter()
        .map(|(name, raw)| {
            let heading = string_projection(&format!("## {name}\n"));
            (*name, concat_source_strings(&[&heading, raw]))
        })
        .collect();
    for client in client_blocks {
        if !eligible.contains(client.server_name) {
            continue;
        }
        let client_source = string_projection(client.block);
        blocks
            .entry(client.server_name)
            .and_modify(|block| {
                let separator = string_projection("\n\n");
                *block = concat_source_strings(&[&*block, &separator, &client_source]);
            })
            .or_insert_with(|| {
                let heading = string_projection(&format!("## {}\n", client.server_name));
                concat_source_strings(&[&heading, &client_source])
            });
    }
    let mut added: Vec<_> = blocks
        .into_iter()
        .map(|(name, block)| {
            (
                name,
                block,
                instructions
                    .get(name)
                    .map(|raw| (*raw).clone())
                    .unwrap_or_else(|| string_projection("")),
            )
        })
        .filter(|(name, _, raw)| match announced.get(name) {
            None => true,
            Some(Some(previous)) => {
                cache_enabled
                    && raw.string_units("").is_some_and(|units| !units.is_empty())
                    && !previous.semantically_eq(raw)
            }
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

    let mut fields = vec![
        ("type".to_owned(), string_projection(KIND)),
        (
            "addedNames".to_owned(),
            Utf16JsonProjection::array(
                added
                    .iter()
                    .map(|(name, _, _)| string_projection(name))
                    .collect(),
            )
            .ok()?,
        ),
        (
            "addedBlocks".to_owned(),
            Utf16JsonProjection::array(added.iter().map(|(_, block, _)| block.clone()).collect())
                .ok()?,
        ),
        (
            "removedNames".to_owned(),
            Utf16JsonProjection::array(
                removed.iter().map(|name| string_projection(name)).collect(),
            )
            .ok()?,
        ),
    ];
    if cache_enabled {
        fields.push((
            "addedServerInstructions".to_owned(),
            Utf16JsonProjection::array(added.iter().map(|(_, _, raw)| raw.clone()).collect())
                .ok()?,
        ));
    }
    Utf16JsonProjection::object(fields).ok()
}

/// Render the model-visible reminder while preserving exact UTF-16 in MCP
/// server instructions. The returned value is a root JSON string projection,
/// ready to become a `TextJsUtf16` conversation block.
pub(crate) fn render_source(attachment: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
    if attachment.string_at("/type") != Some(KIND) {
        return None;
    }
    let blocks = source_string_items(attachment, "/addedBlocks");
    let names = source_string_items(attachment, "/addedNames");
    let removed = source_string_items(attachment, "/removedNames");
    let mut units = Vec::new();
    if !blocks.is_empty() && !names.is_empty() {
        units.extend(
            "# MCP Server Instructions\n\nThe following MCP servers have provided instructions for how to use their tools and resources:\n\n"
                .encode_utf16(),
        );
        for (index, block) in blocks.iter().enumerate() {
            if index > 0 {
                units.extend("\n\n".encode_utf16());
            }
            units.extend(block.string_units("")?);
        }
    }
    if !removed.is_empty() {
        if !units.is_empty() {
            units.extend("\n\n".encode_utf16());
        }
        units.extend(
            "The following MCP servers have disconnected. Their instructions above no longer apply:\n"
                .encode_utf16(),
        );
        for (index, name) in removed.iter().enumerate() {
            if index > 0 {
                units.push(u16::from(b'\n'));
            }
            units.extend(name.string_units("")?);
        }
        units.extend("\n\n".encode_utf16());
        units.extend(super::memory_update::AMBIENT_CONTEXT_TRAILER.encode_utf16());
    }
    if units.is_empty() {
        return None;
    }
    let mut message_units = "<system-reminder>\n".encode_utf16().collect::<Vec<_>>();
    message_units.extend(units);
    message_units.extend("\n</system-reminder>".encode_utf16());
    Some(source_string_units(message_units))
}

/// Render one attachment payload into the exact model-visible reminder.
/// Replay tolerates non-string array members by dropping those members, as the
/// upstream `mi` helper does. An empty attachment produces no model message.
#[cfg(test)]
pub(crate) fn render(attachment: &Utf16JsonProjection) -> Option<String> {
    render_source(attachment).and_then(|source| source.string_at("").map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn projection(value: &Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(value).unwrap()
    }

    fn value(projection: &Utf16JsonProjection) -> Value {
        serde_json::from_str(&projection.to_json_string().unwrap()).unwrap()
    }

    fn projections(values: &[Value]) -> Vec<Utf16JsonProjection> {
        values.iter().map(projection).collect()
    }

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
            let current: Vec<(String, Utf16JsonProjection)> = clients
                .iter()
                .filter_map(|client| {
                    Some((
                        client["name"].as_str()?.to_owned(),
                        string_projection(client["instructions"].as_str()?),
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
            let prior = projections(case["history"].as_array().unwrap_or(&empty));
            let actual = attachment(
                &current,
                &eligible,
                &client_blocks,
                &prior,
                case["cacheEnabled"].as_bool().unwrap_or(true),
            )
            .as_ref()
            .map(value);
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
        assert_eq!(value(&first)["addedServerInstructions"], json!([""]));
        let prior = vec![projection(
            &json!({"type":"attachment", "attachment":value(&first)}),
        )];
        assert!(attachment(&[], &eligible, &BUILTIN_CLIENT_BLOCKS, &prior, true).is_none());
        assert_eq!(
            value(&attachment(&[], &[], &BUILTIN_CLIENT_BLOCKS, &prior, true).unwrap())
                ["removedNames"],
            json!(["computer-use"])
        );
        let server = vec![(
            "computer-use".into(),
            string_projection("server instructions"),
        )];
        let changed = attachment(&server, &eligible, &BUILTIN_CLIENT_BLOCKS, &prior, true)
            .expect("new nonempty server instructions update the raw baseline");
        let changed_value = value(&changed);
        assert_eq!(
            changed_value["addedServerInstructions"],
            json!(["server instructions"])
        );
        assert_eq!(
            changed_value["addedBlocks"][0],
            format!(
                "## computer-use\nserver instructions\n\n{}",
                BUILTIN_CLIENT_BLOCKS[0].block
            )
        );
        let updated = vec![projection(
            &json!({"type":"attachment", "attachment":changed_value}),
        )];
        assert!(attachment(&[], &eligible, &BUILTIN_CLIENT_BLOCKS, &updated, true).is_none());
        assert!(attachment(&[], &[], &BUILTIN_CLIENT_BLOCKS, &[], true).is_none());
    }

    #[test]
    fn server_instruction_source_keeps_unpaired_utf16_through_attachment_and_render() {
        let raw = source_string_units(vec![0xD800]);
        let eligible = vec!["records".into()];
        let attachment = attachment(&[("records".into(), raw)], &eligible, &[], &[], true).unwrap();

        assert_eq!(
            attachment.string_units("/addedServerInstructions/0"),
            Some(vec![0xD800])
        );
        let rendered = render_source(&attachment).unwrap();
        let mut expected = "<system-reminder>\n# MCP Server Instructions\n\nThe following MCP servers have provided instructions for how to use their tools and resources:\n\n## records\n"
            .encode_utf16()
            .collect::<Vec<_>>();
        expected.push(0xD800);
        expected.extend("\n</system-reminder>".encode_utf16());
        assert_eq!(rendered.string_units(""), Some(expected));
    }

    #[test]
    fn instruction_renderer_matches_extracted_286_bytes() {
        for case in oracle_cases() {
            let mut payload = case["expected"]["delta"].clone();
            let actual = if payload.is_null() {
                None
            } else {
                payload["type"] = KIND.into();
                render(&projection(&payload))
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

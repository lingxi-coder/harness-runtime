//! MCP task result receipt (`ee`/`se`, Claude Code 2.1.263).
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use mcp::{decode_base64, persist_binary_content, PersistBinaryResult};
use std::path::Path;

#[cfg(test)]
use serde_json::Value;

const PERSIST_BUDGET: usize = 100 * 1024 * 1024;
const RESULT_BUDGET: usize = 100_000;
const READ_LINE_LIMIT: usize = 80_000;

pub(crate) struct TaskResultReceipt {
    pub text: Utf16JsonProjection,
    pub saved_hint: Option<Utf16JsonProjection>,
}

fn root_text(units: &[u16]) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(units), units.to_vec())
        .expect("UTF-16 units form a JSON string")
}

fn generated_text(text: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::from(text)
}

fn append_units(target: &mut Vec<u16>, value: &Utf16JsonProjection) {
    target.extend(value.string_units("").unwrap_or_default());
}

fn concat_text(parts: &[&Utf16JsonProjection]) -> Utf16JsonProjection {
    let mut units = Vec::new();
    for part in parts {
        append_units(&mut units, part);
    }
    root_text(&units)
}

fn source_string_at(value: &Utf16JsonProjection, pointer: &str) -> Option<Utf16JsonProjection> {
    let result = value.subprojection(pointer).ok()?;
    result.string_units("").map(|_| result)
}

fn source_string_property(value: &Utf16JsonProjection, key: &str) -> Option<Utf16JsonProjection> {
    source_string_at(value, &format!("/{key}"))
}

fn escaped_units(text: &[u16]) -> usize {
    text.iter()
        .map(|unit| match unit {
            0x0026 => 5,
            0x003c | 0x003e => 4,
            _ => 1,
        })
        .sum()
}

fn clipped(text: &Utf16JsonProjection, budget: usize) -> Utf16JsonProjection {
    let source_units = text.string_units("").unwrap_or_default();
    if escaped_units(&source_units) <= budget {
        return text.clone();
    }
    let suffix: Vec<u16> = "… [truncated]".encode_utf16().collect();
    let mut take = source_units.len() * budget / escaped_units(&source_units);
    loop {
        let end = take.min(source_units.len());
        if escaped_units(&source_units[..end]) + suffix.len() <= budget || take == 0 {
            let mut output = source_units[..end].to_vec();
            output.extend_from_slice(&suffix);
            return root_text(&output);
        }
        take = take * 9 / 10;
    }
}

pub(crate) fn prepare(
    content: &Utf16JsonProjection,
    output_dir: &Path,
    seed: &str,
) -> TaskResultReceipt {
    prepare_with_budget(content, output_dir, seed, PERSIST_BUDGET)
}

fn prepare_with_budget(
    content: &Utf16JsonProjection,
    output_dir: &Path,
    seed: &str,
    mut remaining: usize,
) -> TaskResultReceipt {
    let raw = if content.string_units("").is_some() {
        content.clone()
    } else {
        match content.at_pointer("").and_then(|root| root.array_len()) {
            None => root_text(
                &content
                    .to_json_string()
                    .unwrap_or_default()
                    .encode_utf16()
                    .collect::<Vec<_>>(),
            ),
            Some(block_count) => {
                let mut block_texts = Vec::with_capacity(block_count);
                for index in 0..block_count {
                    let block = content
                        .subprojection(&format!("/{index}"))
                        .expect("array item pointer is valid");
                    let kind_source = source_string_property(&block, "type")
                        .unwrap_or_else(|| generated_text("undefined"));
                    let kind_units = kind_source.string_units("").unwrap_or_default();
                    let kind = String::from_utf16(&kind_units).ok();

                    if kind.as_deref() == Some("text") {
                        if let Some(text) = source_string_property(&block, "text") {
                            block_texts.push(text);
                            continue;
                        }
                    }

                    let resource = block.subprojection("/resource").ok();
                    let source = block.subprojection("/source").ok();
                    let data_and_mime = source_string_property(&block, "data")
                        .map(|data| (data, source_string_property(&block, "mimeType")))
                        .or_else(|| {
                            source.as_ref().and_then(|value| {
                                source_string_property(value, "data")
                                    .map(|data| (data, source_string_property(value, "media_type")))
                            })
                        })
                        .or_else(|| {
                            resource.as_ref().and_then(|value| {
                                source_string_property(value, "blob")
                                    .map(|data| (data, source_string_property(value, "mimeType")))
                            })
                        });

                    if let Some((data, mime_source)) = data_and_mime {
                        let data_units = data.string_units("").unwrap_or_default();
                        let estimate = data_units.len().saturating_mul(3) / 4;
                        if estimate > PERSIST_BUDGET {
                            block_texts.push(concat_text(&[
                                &generated_text("["),
                                &kind_source,
                                &generated_text(" content too large to save]"),
                            ]));
                            continue;
                        }
                        if estimate > remaining {
                            block_texts.push(concat_text(&[
                                &generated_text("["),
                                &kind_source,
                                &generated_text(
                                    " content skipped: per-result persist budget exhausted]",
                                ),
                            ]));
                            continue;
                        }
                        let data_display = String::from_utf16_lossy(&data_units);
                        let Ok(bytes) = decode_base64(&data_display) else {
                            block_texts.push(concat_text(&[
                                &generated_text("["),
                                &kind_source,
                                &generated_text("]"),
                            ]));
                            continue;
                        };
                        let mime_units = mime_source
                            .as_ref()
                            .and_then(|value| value.string_units(""));
                        let mime_display = mime_units
                            .as_deref()
                            .and_then(|units| String::from_utf16(units).ok());
                        let mime = mime_display.as_deref();
                        match persist_binary_content(
                            &bytes,
                            mime,
                            &format!("mcp-task-result-{seed}-{index}"),
                            output_dir,
                        ) {
                            PersistBinaryResult::Ok { filepath, size, .. } => {
                                remaining = remaining.saturating_sub(size as usize);
                                let source_description = concat_text(&[
                                    &generated_text("["),
                                    &kind_source,
                                    &generated_text(" result] "),
                                ]);
                                block_texts.push(mcp::binary_blob_saved_message(
                                    &filepath,
                                    mime,
                                    size,
                                    &source_description,
                                ));
                            }
                            PersistBinaryResult::Err { error } => {
                                let mime_label =
                                    mime_source.unwrap_or_else(|| generated_text("unknown type"));
                                block_texts.push(concat_text(&[
                                    &generated_text("["),
                                    &kind_source,
                                    &generated_text(" content ("),
                                    &mime_label,
                                    &generated_text(&format!(
                                        ") could not be saved to disk: {error}]"
                                    )),
                                ]));
                            }
                        }
                        continue;
                    }

                    if let Some(resource) = resource {
                        if let Some(text) = source_string_property(&resource, "text") {
                            let uri = source_string_property(&resource, "uri");
                            block_texts.push(match uri {
                                Some(uri) => concat_text(&[
                                    &generated_text("[Resource at "),
                                    &uri,
                                    &generated_text("] "),
                                    &text,
                                ]),
                                None => text,
                            });
                            continue;
                        }
                    }
                    if kind.as_deref() == Some("resource_link") {
                        if let (Some(name), Some(uri)) = (
                            source_string_property(&block, "name"),
                            source_string_property(&block, "uri"),
                        ) {
                            let description =
                                source_string_property(&block, "description").filter(|value| {
                                    value
                                        .string_units("")
                                        .is_some_and(|units| !units.is_empty())
                                });
                            block_texts.push(match description {
                                Some(description) => concat_text(&[
                                    &generated_text("[Resource link: "),
                                    &name,
                                    &generated_text("] "),
                                    &uri,
                                    &generated_text(" ("),
                                    &description,
                                    &generated_text(")"),
                                ]),
                                None => concat_text(&[
                                    &generated_text("[Resource link: "),
                                    &name,
                                    &generated_text("] "),
                                    &uri,
                                ]),
                            });
                            continue;
                        }
                    }
                    block_texts.push(concat_text(&[
                        &generated_text("["),
                        &kind_source,
                        &generated_text("]"),
                    ]));
                }
                let mut raw_units = Vec::new();
                for (index, block_text) in block_texts.iter().enumerate() {
                    if index > 0 {
                        raw_units.push(u16::from(b'\n'));
                    }
                    append_units(&mut raw_units, block_text);
                }
                root_text(&raw_units)
            }
        }
    };
    let text = clipped(&raw, RESULT_BUDGET);
    if text == raw {
        return TaskResultReceipt {
            text,
            saved_hint: None,
        };
    }
    let raw_units = raw.string_units("").unwrap_or_default();
    if raw_units.len() > remaining {
        return TaskResultReceipt {
            text,
            saved_hint: Some(generated_text(
                "[The portion truncated above was not saved: the per-result persist budget is exhausted.]",
            )),
        };
    }
    // Dy enforces its own byte cap after ee's UTF-16 persist-budget check.
    let filesystem_text = String::from_utf16_lossy(&raw_units);
    if filesystem_text.len() > PERSIST_BUDGET {
        return TaskResultReceipt {
            text,
            saved_hint: None,
        };
    }
    let saved_hint = match persist_binary_content(
        filesystem_text.as_bytes(),
        Some("text/plain"),
        &format!("mcp-task-result-{seed}-text"),
        output_dir,
    ) {
        PersistBinaryResult::Ok { filepath, .. } => {
            let instruction = if raw_units.contains(&u16::from(b'\n'))
                && raw_units
                    .split(|unit| *unit == u16::from(b'\n'))
                    .all(|line| line.len() <= READ_LINE_LIMIT)
            {
                "use Read to retrieve the portion truncated above"
            } else {
                let python = if cfg!(windows) { "python" } else { "python3" };
                // The instruction explicitly targets Bash on both platforms.
                // Encode the Python literal first, then quote the complete
                // script for the shell (including quotes/newlines in paths).
                let path = serde_json::to_string(&filepath).expect("a string serializes");
                let script = format!("print(open({path}).read()[A:B])").replace('\'', "'\\''");
                return TaskResultReceipt {
                    text,
                    saved_hint: Some(generated_text(&format!(
                        "[The complete {}-character output was saved to {filepath}; its lines are too long for Read's offset/limit — slice by character range via Bash instead, e.g. {python} -c '{script}' in ~80,000-char spans.]",
                        raw_units.len()
                    ))),
                };
            };
            Some(generated_text(&format!(
                "[The complete {}-character output was saved to {filepath}; {instruction}.]",
                raw_units.len()
            )))
        }
        PersistBinaryResult::Err { .. } => None,
    };
    TaskResultReceipt { text, saved_hint }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_test_value(value: &Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(value).expect("bounded test JSON is valid")
    }

    fn display(value: &Utf16JsonProjection) -> String {
        String::from_utf16_lossy(&value.string_units("").expect("receipt text is a string"))
    }

    #[test]
    fn receipt_persists_full_text_and_selects_read_or_character_slice() {
        let dir = tempfile::tempdir().unwrap();
        let raw = "line &\n".repeat(20_000);
        let receipt = prepare(
            &source_test_value(&Value::String(raw.clone())),
            dir.path(),
            "lines",
        );
        assert!(display(receipt.saved_hint.as_ref().unwrap()).contains("use Read"));
        assert!(display(&receipt.text).ends_with("… [truncated]"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("mcp-task-result-lines-text.txt")).unwrap(),
            raw
        );
        let receipt = prepare(
            &source_test_value(&Value::String("x".repeat(100_001))),
            dir.path(),
            "long",
        );
        assert!(display(receipt.saved_hint.as_ref().unwrap())
            .contains("slice by character range via Bash"));
    }
    #[test]
    fn receipt_shell_hint_quotes_unusual_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quotes\"'\\newline\n");
        let receipt = prepare(
            &source_test_value(&Value::String("x".repeat(100_001))),
            &path,
            "quoted",
        );
        let hint = display(receipt.saved_hint.as_ref().unwrap());
        let file = path
            .join("mcp-task-result-quoted-text.txt")
            .to_string_lossy()
            .into_owned();
        let python_literal = serde_json::to_string(&file).unwrap();
        let expected = format!("print(open({python_literal}).read()[A:B])").replace('\'', "'\\''");
        assert!(hint.contains(&format!("-c '{expected}'")), "{hint}");
        assert!(expected.contains("\\\""));
        assert!(expected.contains("\\n"));
    }

    #[test]
    fn receipt_exhausted_budget_and_write_failure_do_not_claim_saved_files() {
        let dir = tempfile::tempdir().unwrap();
        let content = serde_json::json!([{"type":"audio","data":"YWJj","mimeType":"audio/wav"},{"type":"text","text":"x".repeat(100_001)}]);
        let receipt =
            prepare_with_budget(&source_test_value(&content), dir.path(), "budget", 100_003);
        assert!(
            display(receipt.saved_hint.as_ref().unwrap()).contains("persist budget is exhausted")
        );
        let impossible = dir.path().join("not-a-directory");
        std::fs::write(&impossible, "").unwrap();
        assert!(prepare(
            &source_test_value(&Value::String("&".repeat(30_000))),
            &impossible,
            "failed"
        )
        .saved_hint
        .is_none());
        assert!(prepare(
            &source_test_value(&Value::String("short".into())),
            dir.path(),
            "short"
        )
        .saved_hint
        .is_none());
    }

    #[test]
    fn receipt_preserves_exact_source_text_and_resource_uri() {
        let text = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&[b'a' as u16, 0xD800, b'b' as u16]),
            vec![b'a' as u16, 0xD800, b'b' as u16],
        )
        .unwrap();
        let source = Utf16JsonProjection::object(vec![
            ("type".into(), generated_text("resource")),
            (
                "resource".into(),
                Utf16JsonProjection::object(vec![
                    (
                        "uri".into(),
                        Utf16JsonProjection::root_string(
                            String::from_utf16_lossy(&[0xD800]),
                            vec![0xD800],
                        )
                        .unwrap(),
                    ),
                    ("text".into(), text.clone()),
                ])
                .unwrap(),
            ),
        ])
        .unwrap();
        let content = Utf16JsonProjection::array(vec![source]).unwrap();
        let dir = tempfile::tempdir().unwrap();

        let receipt = prepare(&content, dir.path(), "source");

        let mut expected = "[Resource at ".encode_utf16().collect::<Vec<_>>();
        expected.push(0xD800);
        expected.extend("] ".encode_utf16());
        expected.extend([b'a' as u16, 0xD800, b'b' as u16]);
        assert_eq!(receipt.text.string_units(""), Some(expected));
    }
}

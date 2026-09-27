//! MCP task result receipt (`ee`/`se`, Claude Code 2.1.263).
use mcp::{decode_base64, persist_binary_content, PersistBinaryResult};
use serde_json::Value;
use std::path::Path;

const PERSIST_BUDGET: usize = 100 * 1024 * 1024;
const RESULT_BUDGET: usize = 100_000;
const READ_LINE_LIMIT: usize = 80_000;

pub(crate) struct TaskResultReceipt {
    pub text: String,
    pub saved_hint: Option<String>,
}

fn units(text: &str) -> usize {
    text.encode_utf16().count()
}
fn escaped_units(text: &str) -> usize {
    text.chars()
        .map(|c| match c {
            '&' => 5,
            '<' | '>' => 4,
            _ => c.len_utf16(),
        })
        .sum()
}
fn clipped(text: &str, budget: usize) -> String {
    if escaped_units(text) <= budget {
        return text.to_string();
    }
    let mut take = units(text) * budget / escaped_units(text);
    loop {
        let mut used = 0;
        let prefix: String = text
            .chars()
            .take_while(|c| {
                used += c.len_utf16();
                used <= take
            })
            .collect();
        if escaped_units(&prefix) + 13 <= budget || take == 0 {
            return format!("{prefix}… [truncated]");
        }
        take = take * 9 / 10;
    }
}

pub(crate) fn prepare(content: &Value, output_dir: &Path, seed: &str) -> TaskResultReceipt {
    prepare_with_budget(content, output_dir, seed, PERSIST_BUDGET)
}

fn prepare_with_budget(
    content: &Value,
    output_dir: &Path,
    seed: &str,
    mut remaining: usize,
) -> TaskResultReceipt {
    let raw = if let Some(text) = content.as_str() {
        text.to_string()
    } else {
        content
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .enumerate()
                    .map(|(index, block)| {
                        let kind = block
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("undefined");
                        if kind == "text" {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                return text.to_string();
                            }
                        }
                        let blob = block
                            .get("data")
                            .and_then(Value::as_str)
                            .map(|data| (data, block.get("mimeType")))
                            .or_else(|| {
                                block.get("source").and_then(|source| {
                                    source
                                        .get("data")
                                        .and_then(Value::as_str)
                                        .map(|data| (data, source.get("media_type")))
                                })
                            })
                            .or_else(|| {
                                block.get("resource").and_then(|resource| {
                                    resource
                                        .get("blob")
                                        .and_then(Value::as_str)
                                        .map(|data| (data, resource.get("mimeType")))
                                })
                            });
                        if let Some((data, mime)) = blob {
                            let estimate = data.len().saturating_mul(3) / 4;
                            if estimate > PERSIST_BUDGET {
                                return format!("[{kind} content too large to save]");
                            }
                            if estimate > remaining {
                                return format!(
                                    "[{kind} content skipped: per-result persist budget exhausted]"
                                );
                            }
                            let Ok(bytes) = decode_base64(data) else {
                                return format!("[{kind}]");
                            };
                            let mime = mime.and_then(Value::as_str);
                            return match persist_binary_content(
                                &bytes,
                                mime,
                                &format!("mcp-task-result-{seed}-{index}"),
                                output_dir,
                            ) {
                                PersistBinaryResult::Ok { filepath, size, .. } => {
                                    remaining = remaining.saturating_sub(size as usize);
                                    mcp::binary_blob_saved_message(
                                        &filepath,
                                        mime,
                                        size,
                                        &format!("[{kind} result] "),
                                    )
                                }
                                PersistBinaryResult::Err { error } => format!(
                                    "[{kind} content ({}) could not be saved to disk: {error}]",
                                    mime.unwrap_or("unknown type")
                                ),
                            };
                        }
                        if let Some(resource) = block.get("resource") {
                            if let Some(text) = resource.get("text").and_then(Value::as_str) {
                                return resource.get("uri").and_then(Value::as_str).map_or_else(
                                    || text.to_string(),
                                    |uri| format!("[Resource at {uri}] {text}"),
                                );
                            }
                        }
                        if kind == "resource_link" {
                            if let (Some(name), Some(uri)) = (
                                block.get("name").and_then(Value::as_str),
                                block.get("uri").and_then(Value::as_str),
                            ) {
                                let description = block
                                    .get("description")
                                    .and_then(Value::as_str)
                                    .map_or(String::new(), |d| format!(" ({d})"));
                                return format!("[Resource link: {name}] {uri}{description}");
                            }
                        }
                        format!("[{kind}]")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_else(|| content.to_string())
    };
    let text = clipped(&raw, RESULT_BUDGET);
    if text == raw {
        return TaskResultReceipt {
            text,
            saved_hint: None,
        };
    }
    if units(&raw) > remaining {
        return TaskResultReceipt { text, saved_hint: Some("[The portion truncated above was not saved: the per-result persist budget is exhausted.]".into()) };
    }
    // Dy enforces its own byte cap after ee's UTF-16 persist-budget check.
    if raw.len() > PERSIST_BUDGET {
        return TaskResultReceipt {
            text,
            saved_hint: None,
        };
    }
    let saved_hint = match persist_binary_content(
        raw.as_bytes(),
        Some("text/plain"),
        &format!("mcp-task-result-{seed}-text"),
        output_dir,
    ) {
        PersistBinaryResult::Ok { filepath, .. } => {
            let instruction = if raw.contains('\n')
                && raw.split('\n').all(|line| units(line) <= READ_LINE_LIMIT)
            {
                "use Read to retrieve the portion truncated above".to_string()
            } else {
                let python = if cfg!(windows) { "python" } else { "python3" };
                // The instruction explicitly targets Bash on both platforms.
                // Encode the Python literal first, then quote the complete
                // script for the shell (including quotes/newlines in paths).
                let path = serde_json::to_string(&filepath).expect("a string serializes");
                let script = format!("print(open({path}).read()[A:B])").replace('\'', "'\\''");
                format!("its lines are too long for Read's offset/limit — slice by character range via Bash instead, e.g. {python} -c '{script}' in ~80,000-char spans")
            };
            Some(format!(
                "[The complete {}-character output was saved to {filepath}; {instruction}.]",
                units(&raw)
            ))
        }
        PersistBinaryResult::Err { .. } => None,
    };
    TaskResultReceipt { text, saved_hint }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_persists_full_text_and_selects_read_or_character_slice() {
        let dir = tempfile::tempdir().unwrap();
        let raw = "line &\n".repeat(20_000);
        let receipt = prepare(&Value::String(raw.clone()), dir.path(), "lines");
        assert!(receipt.saved_hint.unwrap().contains("use Read"));
        assert!(receipt.text.ends_with("… [truncated]"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("mcp-task-result-lines-text.txt")).unwrap(),
            raw
        );
        let receipt = prepare(&Value::String("x".repeat(100_001)), dir.path(), "long");
        assert!(receipt
            .saved_hint
            .unwrap()
            .contains("slice by character range via Bash"));
    }
    #[test]
    fn receipt_shell_hint_quotes_unusual_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quotes\"'\\newline\n");
        let receipt = prepare(&Value::String("x".repeat(100_001)), &path, "quoted");
        let hint = receipt.saved_hint.unwrap();
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
        let receipt = prepare_with_budget(&content, dir.path(), "budget", 100_003);
        assert!(receipt
            .saved_hint
            .unwrap()
            .contains("persist budget is exhausted"));
        let impossible = dir.path().join("not-a-directory");
        std::fs::write(&impossible, "").unwrap();
        assert!(
            prepare(&Value::String("&".repeat(30_000)), &impossible, "failed")
                .saved_hint
                .is_none()
        );
        assert!(prepare(&Value::String("short".into()), dir.path(), "short")
            .saved_hint
            .is_none());
    }
}

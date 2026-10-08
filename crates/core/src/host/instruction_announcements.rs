//! Pure .286 instructions announcement payloads and model rendering.
//! Oracle: src_183727495.js H_o / FFn / O_o / Z4e / W4e / X4e,
//! src_179372957.js ARe / W5n; attachment projection uses Yi / ol.

use super::instructions::{
    InstructionContext, InstructionFile, InstructionFileType, InstructionRendering,
};
use serde::Deserialize;
use serde_json::{json, Value};

#[cfg(test)]
#[path = "instruction_announcements_tests.rs"]
mod tests;

pub const SESSION_CONTEXT_KEYS: [&str; 4] =
    ["userEmail", "attachedProject", "gitStatus", "perforceMode"];
const PREAMBLE: &str = "Codebase and user instructions are shown below. Be sure to adhere to these instructions. IMPORTANT: These instructions OVERRIDE any default behavior and you MUST follow them exactly as written.";
const AMBIENT: &str = "This is ambient context — do not narrate it to the user unless they ask or it is directly relevant to their request.";

#[must_use]
pub fn js_trim(value: &str) -> &str {
    value.trim_matches(super::subagent_output_guard::is_js_space)
}

/// xd / a$ validate the latest raw snapshot; an invalid latest family never
/// falls back to an older routing hint. Invalid optional hints become absent.
#[must_use]
pub fn latest_snapshot_context_rendering(attachments: &[Value]) -> Option<InstructionRendering> {
    let snapshot = attachments
        .iter()
        .rev()
        .find(|value| value["type"] == "prompt_snapshot")?;
    let valid_prompt = snapshot
        .get("systemPrompt")
        .and_then(Value::as_array)
        .is_some_and(|members| members.iter().all(Value::is_string));
    let valid_tools = snapshot.get("tools").is_none_or(|tools| {
        tools.as_array().is_some_and(|tools| {
            tools.iter().all(|tool| {
                tool.get("name").is_some_and(Value::is_string)
                    && tool.get("description").is_some_and(Value::is_string)
                    && tool.get("schema").is_none_or(Value::is_object)
                    && tool.get("server").is_none_or(Value::is_string)
            })
        })
    });
    if !valid_prompt || !valid_tools || !snapshot.get("cliPrefix").is_none_or(Value::is_string) {
        return None;
    }
    match snapshot.get("contextRendering").and_then(Value::as_str) {
        Some("inline") => Some(InstructionRendering::Inline),
        Some("announced") => Some(InstructionRendering::Announced),
        _ => None,
    }
}

#[derive(Deserialize)]
struct InstructionsAttachment {
    files: Vec<InstructionFile>,
    #[serde(default)]
    removed: Vec<String>,
    #[serde(default)]
    changed: bool,
    reason: Option<Value>,
}

fn parse_instructions(value: &Value) -> Option<InstructionsAttachment> {
    (value.get("type")?.as_str()? == "instructions")
        .then(|| serde_json::from_value(value.clone()).ok())?
}

/// Native schemas validate announcement baselines before comparing them.
/// A valid empty announcement may have no model projection.
#[must_use]
pub fn is_valid_context_attachment(value: &Value) -> bool {
    match value.get("type").and_then(Value::as_str) {
        Some("instructions") => parse_instructions(value).is_some(),
        Some("session_context") => {
            value
                .get("context")
                .and_then(Value::as_object)
                .is_some_and(|context| {
                    context.iter().all(|(key, value)| {
                        SESSION_CONTEXT_KEYS.contains(&key.as_str()) && value.is_string()
                    })
                })
        }
        Some("context_sections") => {
            value
                .get("sections")
                .and_then(Value::as_array)
                .is_some_and(|sections| {
                    sections.iter().all(|section| {
                        section.get("name").is_some_and(Value::is_string)
                            && section.get("text").is_some_and(Value::is_string)
                    })
                })
        }
        Some("date") => {
            value.get("date").is_some_and(Value::is_string)
                && value.get("changed").is_none_or(Value::is_boolean)
        }
        _ => false,
    }
}

/// Reconstruct the latest initial announcement plus its subsequent deltas.
/// Updating a path retains insertion order, matching the native Map.
#[must_use]
pub fn recover_instruction_files(attachments: &[Value]) -> Option<Vec<InstructionFile>> {
    let start = attachments.iter().rposition(|value| {
        parse_instructions(value).is_some_and(|attachment| !attachment.changed)
    })?;
    let mut files: Vec<InstructionFile> = Vec::new();
    for value in &attachments[start..] {
        let Some(attachment) = parse_instructions(value) else {
            continue;
        };
        for file in attachment.files {
            if let Some(previous) = files.iter_mut().find(|previous| previous.path == file.path) {
                *previous = file;
            } else {
                files.push(file);
            }
        }
        files.retain(|file| !attachment.removed.contains(&file.path));
    }
    Some(files)
}

/// Only new, changed, or removed files are announced after the initial copy.
#[must_use]
pub fn instructions_delta(
    current: &[InstructionFile],
    prior: Option<&[InstructionFile]>,
    reason: Option<&str>,
) -> Option<Value> {
    let Some(prior) = prior else {
        return (!current.is_empty()).then(|| json!({"type":"instructions","files":current}));
    };
    let changed: Vec<_> = current
        .iter()
        .filter(|file| {
            prior
                .iter()
                .find(|previous| previous.path == file.path)
                .is_none_or(|previous| {
                    previous.content != file.content || previous.kind != file.kind
                })
        })
        .collect();
    let removed: Vec<_> = prior
        .iter()
        .filter(|file| !current.iter().any(|current| current.path == file.path))
        .map(|file| &file.path)
        .collect();
    if changed.is_empty() && removed.is_empty() {
        return None;
    }
    let mut value = json!({"type":"instructions","files":changed,"changed":true});
    if !removed.is_empty() {
        value["removed"] = json!(removed);
    }
    if let Some(reason) = reason {
        value["reason"] = json!(reason);
    }
    Some(value)
}

fn file_blocks(files: &[InstructionFile]) -> String {
    files
        .iter()
        .map(|file| {
            let suffix = match file.kind {
                InstructionFileType::Managed => " (organization-managed policy instructions)",
                InstructionFileType::User => {
                    " (user's private global instructions for all projects)"
                }
                InstructionFileType::Project => {
                    " (project instructions, checked into the codebase)"
                }
                InstructionFileType::Local => {
                    " (user's private project instructions, not checked in)"
                }
                InstructionFileType::AutoMem => {
                    " (user's auto-memory, persists across conversations)"
                }
            };
            let sanitized = (file.kind == InstructionFileType::AutoMem)
                .then(|| super::instruction_memory_sanitize::sanitize_memory_body(&file.content));
            let content = sanitized.as_deref().unwrap_or(&file.content);
            format!("Contents of {}{suffix}:\n\n{}", file.path, js_trim(content))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn refresh_reason(reason: &str) -> Option<&'static str> {
    Some(match reason {
        "session_start" => "when this session started",
        "compaction" => "after the conversation was compacted",
        "policy_refresh" => "after the organization's managed settings changed",
        "directory_added" => "after a working directory was added",
        "settings_sync" => "after settings were synced onto this machine",
        "account_change" => "after the account changed",
        "hooks_invalidate" => "after a plugin had the context re-read",
        "policy_verdict" => "after the organization's policy arrived",
        "memory_paused" => "after memory was paused for this session",
        "memory_resumed" => "after memory was resumed for this session",
        "auto_memory_off" => "after auto-memory was turned off for this session",
        "auto_memory_back_on" => "after auto-memory was turned back on for this session",
        _ => return None,
    })
}

fn render_instructions(value: &Value) -> Option<String> {
    let attachment = parse_instructions(value)?;
    let blocks = file_blocks(&attachment.files);
    if !attachment.changed {
        return (!blocks.is_empty()).then(|| format!("{PREAMBLE}\n\n{blocks}"));
    }
    let reason = attachment
        .reason
        .as_ref()
        .and_then(Value::as_str)
        .and_then(refresh_reason);
    let mut parts = Vec::new();
    if !blocks.is_empty() {
        let start = reason.map_or_else(
            || "These instruction files changed".to_string(),
            |reason| format!("Instruction files were re-read {reason}; these differ from their earlier copies"),
        );
        parts.push(format!("{start} — each replaces its earlier copy, and the instructions preamble above still applies:\n\n{blocks}"));
    } else if !attachment.removed.is_empty() {
        if let Some(reason) = reason {
            parts.push(format!("Instruction files were re-read {reason}."));
        }
    }
    parts.extend(
        attachment
            .removed
            .iter()
            .map(|path| format!("Instructions no longer present: {path}")),
    );
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn render_session_context(value: &Value) -> Option<String> {
    let context = value.get("context")?.as_object()?;
    if context
        .keys()
        .any(|key| !SESSION_CONTEXT_KEYS.contains(&key.as_str()))
    {
        return None;
    }
    let mut sections = Vec::new();
    for key in SESSION_CONTEXT_KEYS {
        if let Some(raw) = context.get(key) {
            let text = raw.as_str()?;
            if !text.is_empty() {
                sections.push(format!("# {key}\n{text}"));
            }
        }
    }
    let changed = value.get("changed").and_then(Value::as_bool) == Some(true);
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .and_then(refresh_reason);
    if sections.is_empty() {
        return changed.then(|| format!("The session context was re-read{}; the values announced earlier (account, project, git status) no longer apply.", reason.map_or_else(String::new, |reason| format!(" {reason}"))));
    }
    let prefix = if changed {
        reason.map_or_else(|| "The session context has changed; these values replace the earlier ones:".into(), |reason| format!("The session context was re-read {reason}; these values replace the earlier ones:"))
    } else {
        "As you answer the user's questions, you can use the following context:".into()
    };
    Some(format!("{prefix}\n{}\n\n{} attached this context automatically; it isn't part of the user's message. It describes the user's own account and workspace, so they don't need it reported back.", sections.join("\n"), branding::PRODUCT_NAME))
}

/// Project one native attachment into its outgoing model message. The typed
/// attachment is the durable row; this rendered reminder is its projection.
#[must_use]
pub fn render_instruction_attachment(value: &Value) -> Option<String> {
    if !is_valid_context_attachment(value) {
        return None;
    }
    let body = match value.get("type")?.as_str()? {
        "instructions" => render_instructions(value)?,
        "session_context" => render_session_context(value)?,
        "context_sections" => {
            let sections = value.get("sections")?.as_array()?;
            if sections.is_empty() {
                return None;
            }
            let sections: Option<Vec<_>> = sections
                .iter()
                .map(|section| {
                    Some(format!(
                        "# {}\n{}",
                        section.get("name")?.as_str()?,
                        section.get("text")?.as_str()?
                    ))
                })
                .collect();
            format!("{}\n\n{AMBIENT}", sections?.join("\n"))
        }
        "date" => {
            let date = value.get("date")?.as_str()?;
            if value
                .get("changed")
                .is_some_and(|changed| !changed.is_boolean())
            {
                return None;
            }
            if value.get("changed").and_then(Value::as_bool) == Some(true) {
                format!("The date has changed. Today's date is now {date}. No need to announce the new date — the user's own clock shows it.")
            } else {
                format!("Today's date is {date}.")
            }
        }
        _ => return None,
    };
    Some(format!("<system-reminder>\n{body}\n</system-reminder>"))
}

/// Normal Or / Br routing for the shared userContext fields. `date` is the
/// caller's current local calendar day, rather than a cached currentDate value.
#[must_use]
pub fn context_attachments(context: &InstructionContext, date: &str, reason: &str) -> Vec<Value> {
    let history = &context.announcement_history;
    let mut attachments = Vec::new();
    if context.user_context.contains_key("instructions") {
        if let Some(files) = &context.eager_instructions {
            let files: Vec<_> = files
                .iter()
                .filter(|file| {
                    !context.managed_instructions_only || file.kind == InstructionFileType::Managed
                })
                .cloned()
                .collect();
            let previous = recover_instruction_files(history);
            if let Some(attachment) = instructions_delta(&files, previous.as_deref(), Some(reason))
            {
                attachments.push(attachment);
            }
        }
    }
    let announced: serde_json::Map<String, Value> = SESSION_CONTEXT_KEYS
        .iter()
        .filter_map(|key| {
            context
                .user_context
                .get(*key)
                .filter(|value| !value.is_empty())
                .map(|value| ((*key).into(), json!(value)))
        })
        .collect();
    let previous = history
        .iter()
        .rev()
        .find(|value| value["type"] == "session_context")
        .filter(|value| is_valid_context_attachment(value))
        .and_then(|value| value.get("context"))
        .and_then(Value::as_object);
    let carries_context = !context.user_context.is_empty();
    let differs = previous.is_none_or(|previous| {
        SESSION_CONTEXT_KEYS
            .iter()
            .any(|key| previous.get(*key) != announced.get(*key))
    });
    if differs && (!announced.is_empty() || carries_context) {
        let mut attachment = json!({"type":"session_context","context":announced});
        if previous.is_some() {
            attachment["changed"] = json!(true);
            attachment["reason"] = json!(reason);
        }
        attachments.push(attachment);
    }
    let mut keys = context.user_context_order.clone();
    for key in ["Environment", "instructions", "userEmail", "currentDate"] {
        if !keys.iter().any(|existing| existing == key) {
            keys.push(key.into());
        }
    }
    for key in context.user_context.keys() {
        if !keys.contains(key) {
            keys.push(key.clone());
        }
    }
    let sections: Vec<_> = keys
        .iter()
        .filter(|key| {
            !SESSION_CONTEXT_KEYS.contains(&key.as_str())
                && key.as_str() != "currentDate"
                && !(key.as_str() == "instructions" && context.eager_instructions.is_some())
        })
        .filter_map(|key| {
            context
                .user_context
                .get(key)
                .filter(|text| !text.is_empty())
                .map(|text| json!({"name":key,"text":text}))
        })
        .collect();
    if !sections.is_empty()
        && !history
            .iter()
            .rev()
            .find(|value| value["type"] == "context_sections")
            .is_some_and(|value| is_valid_context_attachment(value))
    {
        attachments.push(json!({"type":"context_sections","sections":sections}));
    }
    let previous_date = history
        .iter()
        .rev()
        .find(|value| value["type"] == "date")
        .filter(|value| is_valid_context_attachment(value))
        .and_then(|value| value["date"].as_str());
    if previous_date != Some(date) {
        let mut attachment = json!({"type":"date","date":date});
        if previous_date.is_some() {
            attachment["changed"] = json!(true);
        }
        attachments.push(attachment);
    }
    attachments
}

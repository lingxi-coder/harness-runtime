//! Pure .286 instructions announcement payloads and model rendering.
//! Oracle: src_183727495.js H_o / FFn / O_o / Z4e / W4e / X4e,
//! src_179372957.js ARe / W5n; attachment projection uses Yi / ol.

use super::instructions::{
    InstructionContext, InstructionFile, InstructionFileType, InstructionRendering,
};
use crate::types::utf16_json::Utf16JsonProjection;
use serde::de::DeserializeOwned;
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

fn projection_value<T: DeserializeOwned>(
    projection: &Utf16JsonProjection,
    pointer: &str,
) -> Option<T> {
    let value = projection.subprojection(pointer).ok()?;
    serde_json::from_str(&value.to_json_string().ok()?).ok()
}

fn projection_from_typed_value(value: &Value) -> Option<Utf16JsonProjection> {
    Utf16JsonProjection::from_serde_value(value).ok()
}

/// xd / a$ validate the latest raw snapshot; an invalid latest family never
/// falls back to an older routing hint. Invalid optional hints become absent.
#[must_use]
pub fn latest_snapshot_context_rendering(
    attachments: &[Utf16JsonProjection],
) -> Option<InstructionRendering> {
    let snapshot = attachments
        .iter()
        .rev()
        .find(|value| value.string_at("/type") == Some("prompt_snapshot"))?;
    let valid_prompt = snapshot
        .at_pointer("/systemPrompt")
        .and_then(|value| value.array_len().map(|count| (value, count)))
        .is_some_and(|(members, count)| {
            (0..count).all(|index| {
                members
                    .get_index(index)
                    .is_some_and(|item| item.as_str().is_some())
            })
        });
    let valid_tools = match snapshot.at_pointer("/tools") {
        None => true,
        Some(tools) => tools.array_len().is_some_and(|count| {
            (0..count).all(|index| {
                let Some(tool) = tools.get_index(index) else {
                    return false;
                };
                tool.get_property("name")
                    .is_some_and(|value| value.as_str().is_some())
                    && tool
                        .get_property("description")
                        .is_some_and(|value| value.as_str().is_some())
                    && tool
                        .get_property("schema")
                        .is_none_or(|value| value.object_entries().is_some())
                    && tool
                        .get_property("server")
                        .is_none_or(|value| value.as_str().is_some())
            })
        }),
    };
    if !valid_prompt
        || !valid_tools
        || snapshot
            .at_pointer("/cliPrefix")
            .is_some_and(|value| value.as_str().is_none())
    {
        return None;
    }
    match snapshot.string_at("/contextRendering") {
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
    reason: Option<String>,
}

fn parse_instructions(value: &Utf16JsonProjection) -> Option<InstructionsAttachment> {
    (value.string_at("/type")? == "instructions").then(|| projection_value(value, ""))?
}

/// Native schemas validate announcement baselines before comparing them.
/// A valid empty announcement may have no model projection.
#[must_use]
pub fn is_valid_context_attachment(value: &Utf16JsonProjection) -> bool {
    match value.string_at("/type") {
        Some("instructions") => parse_instructions(value).is_some(),
        Some("session_context") => value
            .at_pointer("/context")
            .and_then(|context| context.object_entries())
            .is_some_and(|context| {
                context.iter().all(|entry| {
                    SESSION_CONTEXT_KEYS.contains(&entry.display_key)
                        && entry.value.as_str().is_some()
                })
            }),
        Some("context_sections") => value
            .at_pointer("/sections")
            .and_then(|sections| sections.array_len().map(|count| (sections, count)))
            .is_some_and(|(sections, count)| {
                (0..count).all(|index| {
                    sections.get_index(index).is_some_and(|section| {
                        section
                            .get_property("name")
                            .is_some_and(|value| value.as_str().is_some())
                            && section
                                .get_property("text")
                                .is_some_and(|value| value.as_str().is_some())
                    })
                })
            }),
        Some("date") => {
            value.string_at("/date").is_some()
                && value
                    .at_pointer("/changed")
                    .is_none_or(|changed| changed.as_bool().is_some())
        }
        _ => false,
    }
}

/// Reconstruct the latest initial announcement plus its subsequent deltas.
/// Updating a path retains insertion order, matching the native Map.
#[must_use]
pub fn recover_instruction_files(
    attachments: &[Utf16JsonProjection],
) -> Option<Vec<InstructionFile>> {
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
) -> Option<Utf16JsonProjection> {
    let Some(prior) = prior else {
        return (!current.is_empty())
            .then(|| projection_from_typed_value(&json!({"type":"instructions","files":current})))
            .flatten();
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
    projection_from_typed_value(&value)
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

fn render_instructions(value: &Utf16JsonProjection) -> Option<String> {
    // Native rendering treats an unrecognized/non-string refresh reason as a
    // generic change. Baseline validation remains strict in parse_instructions.
    let mut render_value = value.clone();
    if value.at_pointer("/reason").is_some()
        && value
            .string_units("/reason")
            .and_then(|units| String::from_utf16(&units).ok())
            .is_none()
    {
        render_value.remove_field("reason").ok()?;
    }
    let attachment = parse_instructions(&render_value)?;
    let blocks = file_blocks(&attachment.files);
    if !attachment.changed {
        return (!blocks.is_empty()).then(|| format!("{PREAMBLE}\n\n{blocks}"));
    }
    let reason = attachment.reason.as_deref().and_then(refresh_reason);
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

fn render_session_context(value: &Utf16JsonProjection) -> Option<String> {
    let context = value.at_pointer("/context")?;
    let entries = context.object_entries()?;
    if entries
        .iter()
        .any(|entry| !SESSION_CONTEXT_KEYS.contains(&entry.display_key))
    {
        return None;
    }
    let mut sections = Vec::new();
    for key in SESSION_CONTEXT_KEYS {
        if let Some(raw) = context.get_property(key) {
            let text = raw.as_str()?;
            if !text.is_empty() {
                sections.push(format!("# {key}\n{text}"));
            }
        }
    }
    let changed = value
        .at_pointer("/changed")
        .and_then(|changed| changed.as_bool())
        == Some(true);
    let reason = value.string_at("/reason").and_then(refresh_reason);
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
pub fn render_instruction_attachment(value: &Utf16JsonProjection) -> Option<String> {
    // The instructions renderer accepts an unknown reason as a generic change;
    // it validates the remaining fields itself. Other families retain their
    // strict baseline guard, which is also unchanged for history selection.
    if value.string_at("/type") != Some("instructions") && !is_valid_context_attachment(value) {
        return None;
    }
    let body = match value.string_at("/type")? {
        "instructions" => render_instructions(value)?,
        "session_context" => render_session_context(value)?,
        "context_sections" => {
            let sections = value.at_pointer("/sections")?;
            let count = sections.array_len()?;
            if count == 0 {
                return None;
            }
            let sections: Option<Vec<_>> = (0..count)
                .map(|index| {
                    let section = sections.get_index(index)?;
                    Some(format!(
                        "# {}\n{}",
                        section.get_property("name")?.as_str()?,
                        section.get_property("text")?.as_str()?
                    ))
                })
                .collect();
            format!("{}\n\n{AMBIENT}", sections?.join("\n"))
        }
        "date" => {
            let date = value.string_at("/date")?;
            if value
                .at_pointer("/changed")
                .is_some_and(|changed| changed.as_bool().is_none())
            {
                return None;
            }
            if value
                .at_pointer("/changed")
                .and_then(|changed| changed.as_bool())
                == Some(true)
            {
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
pub fn context_attachments(
    context: &InstructionContext,
    date: &str,
    reason: &str,
) -> Vec<Utf16JsonProjection> {
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

    let announced_fields: Vec<_> = SESSION_CONTEXT_KEYS
        .iter()
        .filter_map(|key| {
            context
                .user_context
                .get(*key)
                .filter(|value| !value.is_empty())
                .map(|value| ((*key).to_owned(), json!(value)))
        })
        .collect();
    let announced_value = serde_json::Value::Object(announced_fields.iter().cloned().collect());
    let announced = projection_from_typed_value(&announced_value);
    let previous = history
        .iter()
        .rev()
        .find(|value| value.string_at("/type") == Some("session_context"))
        .filter(|value| is_valid_context_attachment(value))
        .and_then(|value| value.subprojection("/context").ok());
    let carries_context = !context.user_context.is_empty();
    let differs = match (&previous, &announced) {
        (Some(previous), Some(announced)) => !previous.semantically_eq(announced),
        _ => previous.is_some() != announced.is_none() || announced.is_some(),
    };
    if differs && (!announced_fields.is_empty() || carries_context) {
        let mut attachment = json!({"type":"session_context","context":announced_value});
        if previous.is_some() {
            attachment["changed"] = json!(true);
            attachment["reason"] = json!(reason);
        }
        if let Some(attachment) = projection_from_typed_value(&attachment) {
            attachments.push(attachment);
        }
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
    let has_prior_sections = history
        .iter()
        .rev()
        .find(|value| value.string_at("/type") == Some("context_sections"))
        .is_some_and(is_valid_context_attachment);
    if !sections.is_empty() && !has_prior_sections {
        if let Some(attachment) = projection_from_typed_value(&json!({
            "type":"context_sections",
            "sections":sections,
        })) {
            attachments.push(attachment);
        }
    }

    let previous_date = history
        .iter()
        .rev()
        .find(|value| value.string_at("/type") == Some("date"))
        .filter(|value| is_valid_context_attachment(value))
        .and_then(|value| value.string_at("/date"));
    if previous_date != Some(date) {
        let mut attachment = json!({"type":"date","date":date});
        if previous_date.is_some() {
            attachment["changed"] = json!(true);
        }
        if let Some(attachment) = projection_from_typed_value(&attachment) {
            attachments.push(attachment);
        }
    }
    attachments
}

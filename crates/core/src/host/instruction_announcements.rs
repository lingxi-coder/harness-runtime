//! Pure .286 instructions announcement payloads and model rendering.
//! Oracle: src_183727495.js H_o / FFn / O_o / Z4e / W4e / X4e,
//! src_179372957.js ARe / W5n; attachment projection uses Yi / ol.

use super::instructions::{
    InstructionContext, InstructionFile, InstructionFileType, InstructionRendering,
};
use crate::types::utf16_json::Utf16JsonProjection;

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

fn root_string_units(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display string matches exact UTF-16 units")
}

fn bool_projection(value: bool) -> Utf16JsonProjection {
    Utf16JsonProjection::plain(serde_json::Value::Bool(value))
}

fn unit_trim_bounds(value: &[u16]) -> std::ops::Range<usize> {
    let is_js_space_unit = |unit: u16| {
        char::from_u32(u32::from(unit)).is_some_and(super::subagent_output_guard::is_js_space)
    };
    let start = value
        .iter()
        .position(|unit| !is_js_space_unit(*unit))
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|unit| !is_js_space_unit(*unit))
        .map_or(start, |index| index + 1);
    start..end
}

fn trimmed_units(value: &[u16]) -> &[u16] {
    let bounds = unit_trim_bounds(value);
    &value[bounds]
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

struct InstructionsAttachment {
    files: Vec<InstructionFile>,
    removed: Vec<Utf16JsonProjection>,
    changed: bool,
    reason: Option<String>,
}

fn parse_instructions(value: &Utf16JsonProjection) -> Option<InstructionsAttachment> {
    if value.string_at("/type")? != "instructions" {
        return None;
    }
    let files = value.at_pointer("/files")?;
    let file_count = files.array_len()?;
    let mut parsed_files = Vec::with_capacity(file_count);
    for index in 0..file_count {
        let pointer = format!("/files/{index}");
        let file = value.at_pointer(&pointer)?;
        let path_units = file.get_property("path")?.code_units()?.to_vec();
        let path = root_string_units(path_units);
        let kind = match file.get_property("type").and_then(|kind| kind.as_str()) {
            Some("Managed") => InstructionFileType::Managed,
            Some("Project") => InstructionFileType::Project,
            Some("Local") => InstructionFileType::Local,
            Some("AutoMem" | "AutoMemPinned") => InstructionFileType::AutoMem,
            _ => InstructionFileType::User,
        };
        let content_units = file.get_property("content")?.code_units()?.to_vec();
        parsed_files.push(InstructionFile::new(
            path,
            kind,
            root_string_units(content_units),
        ));
    }
    let removed = match value.get_property("removed") {
        None => Vec::new(),
        Some(removed) => {
            let count = removed.array_len()?;
            let mut paths = Vec::with_capacity(count);
            for index in 0..count {
                paths.push(root_string_units(
                    removed.get_index(index)?.code_units()?.to_vec(),
                ));
            }
            paths
        }
    };
    let changed = match value.get_property("changed") {
        None => false,
        Some(changed) => changed.as_bool()?,
    };
    let reason = match value.get_property("reason") {
        None => None,
        Some(reason) if reason.is_null() => None,
        Some(reason) => Some(reason.as_str()?.to_owned()),
    };
    Some(InstructionsAttachment {
        files: parsed_files,
        removed,
        changed,
        reason,
    })
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
                        && entry.value.code_units().is_some()
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
                            .is_some_and(|value| value.code_units().is_some())
                            && section
                                .get_property("text")
                                .is_some_and(|value| value.code_units().is_some())
                    })
                })
            }),
        Some("date") => {
            value.string_units("/date").is_some()
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
            if let Some(previous) = files
                .iter_mut()
                .find(|previous| previous.path_units() == file.path_units())
            {
                *previous = file;
            } else {
                files.push(file);
            }
        }
        files.retain(|file| {
            !attachment.removed.iter().any(|removed| {
                removed.string_units("").as_deref() == Some(file.path_units().as_slice())
            })
        });
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
    let (files, removed, changed) = if let Some(prior) = prior {
        let changed_files = current
            .iter()
            .filter(|file| {
                prior
                    .iter()
                    .find(|previous| previous.path_units() == file.path_units())
                    .is_none_or(|previous| {
                        previous.content_units() != file.content_units()
                            || previous.kind != file.kind
                    })
            })
            .collect::<Vec<_>>();
        let removed_paths = prior
            .iter()
            .filter(|file| {
                !current
                    .iter()
                    .any(|current| current.path_units() == file.path_units())
            })
            .map(InstructionFile::path_units)
            .collect::<Vec<_>>();
        if changed_files.is_empty() && removed_paths.is_empty() {
            return None;
        }
        (changed_files, removed_paths, true)
    } else {
        if current.is_empty() {
            return None;
        }
        (current.iter().collect(), Vec::new(), false)
    };

    let file_values = files
        .into_iter()
        .map(instruction_file_projection)
        .collect::<Vec<_>>();
    let mut fields = vec![
        ("type".into(), Utf16JsonProjection::from("instructions")),
        (
            "files".into(),
            Utf16JsonProjection::array(file_values).ok()?,
        ),
    ];
    if changed {
        fields.push(("changed".into(), bool_projection(true)));
    }
    if !removed.is_empty() {
        fields.push((
            "removed".into(),
            Utf16JsonProjection::array(removed.into_iter().map(root_string_units).collect())
                .ok()?,
        ));
    }
    if changed {
        if let Some(reason) = reason {
            fields.push(("reason".into(), Utf16JsonProjection::from(reason)));
        }
    }
    Utf16JsonProjection::object(fields).ok()
}

fn instruction_file_projection(file: &InstructionFile) -> Utf16JsonProjection {
    let kind = match file.kind {
        InstructionFileType::Managed => "Managed",
        InstructionFileType::User => "User",
        InstructionFileType::Project => "Project",
        InstructionFileType::Local => "Local",
        InstructionFileType::AutoMem => "AutoMem",
    };
    Utf16JsonProjection::object(vec![
        ("path".into(), file.path.clone()),
        ("type".into(), Utf16JsonProjection::from(kind)),
        ("content".into(), file.content.clone()),
    ])
    .expect("instruction descriptor fields form an object")
}

fn file_blocks(files: &[InstructionFile]) -> Vec<u16> {
    let blocks = files
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
            let mut content_units = file.content_units();
            if file.kind == InstructionFileType::AutoMem {
                content_units =
                    super::instruction_memory_sanitize::sanitize_memory_body_utf16(&content_units);
            }
            let mut block = "Contents of ".encode_utf16().collect::<Vec<_>>();
            block.extend(file.path_units());
            block.extend(suffix.encode_utf16());
            block.extend(":\n\n".encode_utf16());
            block.extend(trimmed_units(&content_units));
            block
        })
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    for (index, block) in blocks.into_iter().enumerate() {
        if index > 0 {
            output.extend("\n\n".encode_utf16());
        }
        output.extend(block);
    }
    output
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

fn render_instructions(value: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
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
        if blocks.is_empty() {
            return None;
        }
        let mut units = format!("{PREAMBLE}\n\n").encode_utf16().collect::<Vec<_>>();
        units.extend(blocks);
        return Some(root_string_units(units));
    }
    let reason = attachment.reason.as_deref().and_then(refresh_reason);
    let mut parts = Vec::<Vec<u16>>::new();
    if !blocks.is_empty() {
        let start = reason.map_or_else(
            || "These instruction files changed".to_string(),
            |reason| {
                format!(
                    "Instruction files were re-read {reason}; these differ from their earlier copies"
                )
            },
        );
        let mut part = format!("{start} — each replaces its earlier copy, and the instructions preamble above still applies:\n\n")
            .encode_utf16()
            .collect::<Vec<_>>();
        part.extend(blocks);
        parts.push(part);
    } else if !attachment.removed.is_empty() {
        if let Some(reason) = reason {
            parts.push(
                format!("Instruction files were re-read {reason}.")
                    .encode_utf16()
                    .collect(),
            );
        }
    }
    parts.extend(attachment.removed.iter().map(|path| {
        let mut part = "Instructions no longer present: "
            .encode_utf16()
            .collect::<Vec<_>>();
        part.extend(path.string_units("").expect("removed path is a string"));
        part
    }));
    if parts.is_empty() {
        return None;
    }
    let mut units = Vec::new();
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            units.extend("\n\n".encode_utf16());
        }
        units.extend(part);
    }
    Some(root_string_units(units))
}

fn render_session_context(value: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
    let context = value.at_pointer("/context")?;
    let entries = context.object_entries()?;
    if entries
        .iter()
        .any(|entry| !SESSION_CONTEXT_KEYS.contains(&entry.display_key))
    {
        return None;
    }
    let mut sections = Vec::<Vec<u16>>::new();
    for key in SESSION_CONTEXT_KEYS {
        if let Some(raw) = context.get_property(key) {
            let text = raw.code_units()?;
            if !text.is_empty() {
                let mut section = format!("# {key}\n").encode_utf16().collect::<Vec<_>>();
                section.extend_from_slice(text);
                sections.push(section);
            }
        }
    }
    let changed = value
        .at_pointer("/changed")
        .and_then(|changed| changed.as_bool())
        == Some(true);
    let reason = value.string_at("/reason").and_then(refresh_reason);
    if sections.is_empty() {
        return changed.then(|| {
            root_string_units(
                format!("The session context was re-read{}; the values announced earlier (account, project, git status) no longer apply.", reason.map_or_else(String::new, |reason| format!(" {reason}")))
                    .encode_utf16()
                    .collect(),
            )
        });
    }
    let prefix = if changed {
        reason.map_or_else(|| "The session context has changed; these values replace the earlier ones:".into(), |reason| format!("The session context was re-read {reason}; these values replace the earlier ones:"))
    } else {
        "As you answer the user's questions, you can use the following context:".into()
    };
    let mut units = prefix.encode_utf16().collect::<Vec<_>>();
    units.push(b'\n' as u16);
    for (index, section) in sections.into_iter().enumerate() {
        if index > 0 {
            units.push(b'\n' as u16);
        }
        units.extend(section);
    }
    units.extend(
        format!("\n\n{} attached this context automatically; it isn't part of the user's message. It describes the user's own account and workspace, so they don't need it reported back.", branding::PRODUCT_NAME)
            .encode_utf16(),
    );
    Some(root_string_units(units))
}

/// Project one native attachment into its outgoing model message. The typed
/// attachment is the durable row; this rendered reminder is its projection.
#[must_use]
pub fn render_instruction_attachment(value: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
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
                    let name = section.get_property("name")?.code_units()?;
                    let text = section.get_property("text")?.code_units()?;
                    let mut rendered = "# ".encode_utf16().collect::<Vec<_>>();
                    rendered.extend_from_slice(name);
                    rendered.push(b'\n' as u16);
                    rendered.extend_from_slice(text);
                    Some(rendered)
                })
                .collect();
            let mut rendered = Vec::new();
            for (index, section) in sections?.into_iter().enumerate() {
                if index > 0 {
                    rendered.push(b'\n' as u16);
                }
                rendered.extend(section);
            }
            rendered.extend(format!("\n\n{AMBIENT}").encode_utf16());
            root_string_units(rendered)
        }
        "date" => {
            let date = value.string_units("/date")?;
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
                {
                    let mut rendered = "The date has changed. Today's date is now "
                        .encode_utf16()
                        .collect::<Vec<_>>();
                    rendered.extend(date);
                    rendered.extend(
                        ". No need to announce the new date — the user's own clock shows it."
                            .encode_utf16(),
                    );
                    root_string_units(rendered)
                }
            } else {
                let mut rendered = "Today's date is ".encode_utf16().collect::<Vec<_>>();
                rendered.extend(date);
                rendered.push(b'.' as u16);
                root_string_units(rendered)
            }
        }
        _ => return None,
    };
    let mut units = "<system-reminder>\n".encode_utf16().collect::<Vec<_>>();
    units.extend(body.string_units("").expect("rendered body is a string"));
    units.extend("\n</system-reminder>".encode_utf16());
    Some(root_string_units(units))
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
    if context
        .user_context_value(&"instructions".encode_utf16().collect::<Vec<_>>())
        .is_some()
    {
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
                .user_context_value(&key.encode_utf16().collect::<Vec<_>>())
                .and_then(|value| {
                    value
                        .string_units("")
                        .filter(|units| !units.is_empty())
                        .map(|units| ((*key).to_owned(), root_string_units(units)))
                })
        })
        .collect();
    let announced = Utf16JsonProjection::object_units(
        announced_fields
            .iter()
            .map(|(key, value)| (key.encode_utf16().collect::<Vec<_>>(), value.clone()))
            .collect(),
    )
    .ok();
    let previous = history
        .iter()
        .rev()
        .find(|value| value.string_at("/type") == Some("session_context"))
        .filter(|value| is_valid_context_attachment(value))
        .and_then(|value| value.subprojection("/context").ok());
    let carries_context = !context.user_context_is_empty();
    let differs = match (&previous, &announced) {
        (Some(previous), Some(announced)) => !previous.semantically_eq(announced),
        _ => previous.is_some() != announced.is_none() || announced.is_some(),
    };
    if differs && (!announced_fields.is_empty() || carries_context) {
        let Some(announced) = announced else {
            return attachments;
        };
        let mut fields = vec![
            ("type".into(), Utf16JsonProjection::from("session_context")),
            ("context".into(), announced),
        ];
        if previous.is_some() {
            fields.push(("changed".into(), bool_projection(true)));
            fields.push(("reason".into(), Utf16JsonProjection::from(reason)));
        }
        if let Ok(attachment) = Utf16JsonProjection::object(fields) {
            attachments.push(attachment);
        }
    }

    let mut keys = context
        .user_context_entries()
        .into_iter()
        .map(|(key, _)| key)
        .collect::<Vec<_>>();
    for key in ["Environment", "instructions", "userEmail", "currentDate"] {
        let units = key.encode_utf16().collect::<Vec<_>>();
        if !keys.iter().any(|existing| existing == &units) {
            keys.push(units);
        }
    }
    let sections: Vec<Utf16JsonProjection> = keys
        .iter()
        .filter(|key| {
            let key_text = String::from_utf16(key).ok();
            key_text.as_deref().is_none_or(|key| {
                !SESSION_CONTEXT_KEYS.contains(&key)
                    && key != "currentDate"
                    && !(key == "instructions" && context.eager_instructions.is_some())
            })
        })
        .filter_map(|key| {
            let text = context.user_context_value(key)?;
            let text_units = text.string_units("")?;
            if text_units.is_empty() {
                return None;
            }
            Utf16JsonProjection::object(vec![
                ("name".into(), root_string_units(key.clone())),
                ("text".into(), root_string_units(text_units)),
            ])
            .ok()
        })
        .collect();
    let has_prior_sections = history
        .iter()
        .rev()
        .find(|value| value.string_at("/type") == Some("context_sections"))
        .is_some_and(is_valid_context_attachment);
    if !sections.is_empty() && !has_prior_sections {
        if let Ok(attachment) = Utf16JsonProjection::object(vec![
            ("type".into(), Utf16JsonProjection::from("context_sections")),
            (
                "sections".into(),
                Utf16JsonProjection::array(sections).expect("sections array"),
            ),
        ]) {
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
        let mut fields = vec![
            ("type".into(), Utf16JsonProjection::from("date")),
            ("date".into(), Utf16JsonProjection::from(date)),
        ];
        if previous_date.is_some() {
            fields.push(("changed".into(), bool_projection(true)));
        }
        if let Ok(attachment) = Utf16JsonProjection::object(fields) {
            attachments.push(attachment);
        }
    }
    attachments
}

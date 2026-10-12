//! Native's main-loop delta for Bash sandbox instructions.

use super::{ConversationOrchestrator, NativeSandboxHostFacts};
use crate::request_context::{rendered_system_message, PromptIngressAttachment};
use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
use lingxi_core::types::{ConversationMessage, MessageId};
use std::collections::{HashMap, HashSet};

const BASH_TOOL_NAME: &str = "Bash";
const ATTACHMENT_TYPE: &str = "sandbox_instructions";
const SANDBOX_DISABLED_TEXT: &str = "The Bash command sandbox has been disabled. Commands now run without sandbox restrictions; the earlier sandbox instructions no longer apply.";
const SANDBOX_BOUNDARY_NOTE: &str = "The sandbox marks out what this session was given: the directories listed below, the network destinations the task involves, and the credentials the user supplied for it. Treat that as the boundary even where a limit below is not enforced. Commands can reach more than that — credentials and keys elsewhere on this machine, the user's other projects and configuration, sockets that control this machine or other workloads, cloud metadata endpoints — but being reachable does not make them provided; those are the user's, not the task's, unless the user's request calls for them. If the task cannot be finished with what you were given, do what you can and tell the user plainly what is missing instead of finding another way to it; that report is a complete answer.";

fn string_source(value: &str) -> Source {
    Source::from(value)
}

fn object(fields: Vec<(&str, Source)>) -> Source {
    Source::object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
    .expect("sandbox attachment payload has unique keys")
}

fn array(values: Vec<Source>) -> Source {
    Source::array(values).expect("sandbox config array is valid")
}

fn source_at(source: &Source, pointer: &str) -> Option<Source> {
    source.subprojection(pointer).ok()
}

fn array_items(source: &Source, pointer: &str) -> Vec<Source> {
    let Some(value) = source.at_pointer(pointer) else {
        return Vec::new();
    };
    let Some(len) = value.array_len() else {
        return Vec::new();
    };
    (0..len)
        .filter_map(|index| source_at(source, &format!("{pointer}/{index}")))
        .filter(|item| {
            item.at_pointer("")
                .is_some_and(|value| value.as_str().is_some())
        })
        .collect()
}

fn dedupe(values: Vec<Source>) -> Vec<Source> {
    let mut seen = HashSet::<Vec<u16>>::new();
    values
        .into_iter()
        .filter(|value| {
            value
                .string_units("")
                .is_some_and(|units| seen.insert(units))
        })
        .collect()
}

fn truncate(values: Vec<Source>) -> Vec<Source> {
    const LIMIT: usize = 50;
    if values.len() <= LIMIT {
        return values;
    }
    let remaining = values.len() - LIMIT;
    let mut truncated = values.into_iter().take(LIMIT).collect::<Vec<_>>();
    truncated.push(string_source(&format!(
        "... and {remaining} more (truncated for prompt size)"
    )));
    truncated
}

fn js_truthy(value: &Source) -> bool {
    let Some(value) = value.at_pointer("") else {
        return false;
    };
    if value.is_null() {
        return false;
    }
    if let Some(value) = value.as_bool() {
        return value;
    }
    if let Some(value) = value.as_str() {
        return !value.is_empty();
    }
    if let Some(value) = value.as_number() {
        return value != 0.0 && !value.is_nan();
    }
    // JavaScript arrays and objects are truthy, even when empty.
    true
}

fn js_json_string(value: &Source) -> String {
    value
        .to_json_string()
        .expect("sandbox config source is valid JSON")
}

/// Native's imported `Yd` helper: stringify first, then escape `<`, `>`, and
/// control characters for safe insertion into the prompt.
fn safe_json_text(value: &Source) -> String {
    let escaped = js_json_string(value)
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let mut output = String::with_capacity(escaped.len());
    for ch in escaped.chars() {
        let code = ch as u32;
        if (0x00..=0x1f).contains(&code)
            || (0x7f..=0x9f).contains(&code)
            || matches!(code, 0x2028 | 0x2029)
        {
            output.push_str(&format!("&#{code};"));
        } else {
            output.push(ch);
        }
    }
    output
}

fn map_write_temps(values: Vec<Source>, facts: &NativeSandboxHostFacts) -> Vec<Source> {
    let exports_tmpdir =
        !facts.filesystem_policy_relaxed || facts.platform.as_deref() == Some("windows");
    let mapped = values
        .into_iter()
        .filter_map(|value| {
            let is_temp = value
                .string_at("")
                .is_some_and(|path| facts.temp_paths.iter().any(|temp_path| temp_path == path));
            if is_temp && !exports_tmpdir {
                return None;
            }
            Some(if is_temp {
                string_source("$TMPDIR")
            } else {
                value
            })
        })
        .collect();
    // Native maps temp paths to `$TMPDIR` before its `Set` deduplication.
    dedupe(mapped)
}

fn paths_config(facts: &NativeSandboxHostFacts) -> Vec<String> {
    let read_config = &facts.filesystem_read;
    let write_config = &facts.filesystem_write;
    let allow_write = truncate(map_write_temps(
        array_items(write_config, "/allowOnly"),
        facts,
    ));
    let deny_read = truncate(dedupe(array_items(read_config, "/denyOnly")));
    let allow_read = truncate(dedupe(array_items(read_config, "/allowWithinDeny")));
    let deny_write = truncate(dedupe(array_items(write_config, "/denyWithinAllow")));

    let filesystem = if facts.filesystem_policy_relaxed {
        let mut details = vec![
            "Filesystem: in this configuration the sandbox does not confine commands to particular directories for reading or writing.".to_owned(),
        ];
        if !allow_write.is_empty() {
            details.push(format!(
                "The directories this session is set up to work in: {}.",
                safe_json_text(&array(allow_write))
            ));
        }
        if !deny_read.is_empty() {
            details.push(format!(
                "Paths configured as not to be read, which stands even though it is not enforced here: {}{}.",
                safe_json_text(&array(deny_read)),
                if allow_read.is_empty() {
                    String::new()
                } else {
                    format!(", except {}", safe_json_text(&array(allow_read)))
                }
            ));
        }
        details.push("Leave LingXi Code's own settings, hooks, skills and plugin files, and the repository's git hooks and git config, unchanged unless the user asks; credential files and keys elsewhere on this machine are the user's, not the task's.".to_owned());
        details.join(" ")
    } else {
        let mut read_fields = vec![("denyOnly", array(deny_read))];
        if read_config.at_pointer("/allowWithinDeny").is_some() {
            read_fields.push(("allowWithinDeny", array(allow_read)));
        }
        let read = object(read_fields);
        let write = object(vec![
            ("allowOnly", array(allow_write)),
            ("denyWithinAllow", array(deny_write)),
        ]);
        format!(
            "Filesystem: {}",
            safe_json_text(&object(vec![("read", read), ("write", write)]))
        )
    };

    let mut sections = vec![filesystem];
    let denied_hosts = facts
        .network_restriction_config
        .as_ref()
        .and_then(|network| source_at(network, "/deniedHosts"))
        .filter(js_truthy)
        .map(|network| truncate(dedupe(array_items(&network, ""))));
    let unix_sockets = facts
        .allow_unix_sockets
        .as_ref()
        .filter(|sockets| js_truthy(sockets))
        .map(|sockets| truncate(dedupe(array_items(sockets, ""))));
    let mut network_fields = Vec::new();
    if let Some(denied_hosts) = denied_hosts {
        network_fields.push(("deniedHosts", array(denied_hosts)));
    }
    if let Some(unix_sockets) = unix_sockets {
        network_fields.push(("allowUnixSockets", array(unix_sockets)));
    }
    if !network_fields.is_empty() {
        sections.push(format!(
            "Network: {}",
            safe_json_text(&object(network_fields))
        ));
    }

    if let Some(ignored) = facts
        .ignore_violations
        .as_ref()
        .filter(|value| js_truthy(value))
    {
        sections.push(format!("Ignored violations: {}", safe_json_text(ignored)));
    }
    sections
}

#[derive(Clone)]
enum PromptItem {
    Text(&'static str),
    Owned(String),
    Nested(Vec<String>),
}

fn native_bullets(items: Vec<PromptItem>) -> Vec<String> {
    let mut lines = Vec::new();
    for item in items {
        match item {
            PromptItem::Text(value) => lines.push(format!(" - {value}")),
            PromptItem::Owned(value) => lines.push(format!(" - {value}")),
            PromptItem::Nested(values) => {
                lines.extend(values.into_iter().map(|value| format!("  - {value}")));
            }
        }
    }
    lines
}

fn render_active_sandbox_instructions(facts: &NativeSandboxHostFacts) -> String {
    let platform_suffix = match facts.platform.as_deref() {
        Some("macos") => " (macOS Seatbelt)",
        Some("linux" | "wsl") => " (Linux bubblewrap)",
        _ => "",
    };
    let boundary_branch = facts.boundary_note_enabled;
    // `cmn` uses its filtered `X` object for the Network row, but uses the
    // original network config `r` for the egress and port-binding instructions.
    // `allowedHosts` alone therefore suppresses the row while still enabling
    // those instructions.
    let has_network_config = facts
        .network_restriction_config
        .as_ref()
        .and_then(Source::object_entries)
        .is_some_and(|entries| !entries.is_empty());
    let restrictions = paths_config(facts);
    let allow_unsandboxed = facts.allow_unsandboxed_commands;
    let explain_restriction = format!(
        "Briefly explain what sandbox restriction likely caused the failure. Be sure to mention that the user can use the `/sandbox` command to {}.",
        if facts.sandbox_command_changes_settings {
            "change the sandbox settings"
        } else {
            "manage restrictions"
        }
    );

    let mut instructions = if allow_unsandboxed {
        vec![
            PromptItem::Text("You should always default to running commands within the sandbox. Do NOT attempt to set `dangerouslyDisableSandbox: true` unless:"),
            PromptItem::Nested(vec![
                "The user *explicitly* asks you to bypass sandbox".to_owned(),
                "A specific command just failed and you see evidence of sandbox restrictions causing the failure. Note that commands can fail for many reasons unrelated to the sandbox (missing files, wrong arguments, network issues, etc.).".to_owned(),
            ]),
            PromptItem::Text("Evidence of sandbox-caused failures includes:"),
            PromptItem::Nested(vec![
                "\"Operation not permitted\" errors for file/network operations".to_owned(),
                "Access denied to specific paths outside allowed directories".to_owned(),
                "Network connection failures to non-whitelisted hosts".to_owned(),
                "Unix socket connection errors".to_owned(),
            ]),
            PromptItem::Text("When you see evidence of sandbox-caused failure:"),
            PromptItem::Nested(if boundary_branch {
                vec![
                    if facts.command_network_lists_offered {
                        "If the `<sandbox_violations>` block names a denied host, re-run the command with that host in its `allowed_domains` (auto mode); otherwise retry with `dangerouslyDisableSandbox: true` directly rather than asking in prose first — the retry itself goes through the permission gate (a user prompt, or the auto-mode classifier when auto mode is active)"
                    } else {
                        "Retry with `dangerouslyDisableSandbox: true` directly rather than asking in prose first — the retry itself goes through the permission gate (a user prompt, or the auto-mode classifier when auto mode is active)"
                    }
                    .to_owned(),
                    explain_restriction.clone(),
                ]
            } else {
                vec![
                    if facts.command_network_lists_offered {
                        "If the `<sandbox_violations>` block names a denied host, re-run the command with that host in its `allowed_domains` (auto mode); otherwise retry with `dangerouslyDisableSandbox: true` (don't ask, just do it)"
                    } else {
                        "Immediately retry with `dangerouslyDisableSandbox: true` (don't ask, just do it)"
                    }
                    .to_owned(),
                    explain_restriction.clone(),
                    "This goes through the permission gate (a user prompt, or the auto-mode classifier when auto mode is active)".to_owned(),
                ]
            }),
        ]
    } else {
        vec![
            PromptItem::Text(
                "The `dangerouslyDisableSandbox` parameter is disabled in this session's configuration; setting it does not take a command out of the sandbox.",
            ),
            PromptItem::Text(
                "If a command the task needs fails on a sandbox restriction, tell the user which restriction it hit; changing the sandbox settings is their decision, not yours.",
            ),
        ]
    };

    if allow_unsandboxed {
        if boundary_branch {
            instructions.push(PromptItem::Text("A sandbox denial on a credential, a file or a host that the task does not involve is the boundary above at work: tell the user rather than retrying with `dangerouslyDisableSandbox: true`."));
        }
        instructions.push(PromptItem::Text("Treat each command you execute with `dangerouslyDisableSandbox: true` individually. Even if you have recently run a command with this setting, you should default to running future commands within the sandbox."));
        instructions.push(PromptItem::Text("Do not suggest adding sensitive paths like ~/.bashrc, ~/.zshrc, ~/.ssh/*, or credential files to the sandbox allowlist."));
    }

    if has_network_config {
        let auto_suffix = if facts.command_network_lists_offered {
            ". In auto mode, list the hosts a command needs beyond that in its `allowed_domains` — a denied host named in the block goes there on the re-run"
        } else {
            ""
        };
        instructions.push(PromptItem::Owned(format!(
            "Network egress goes through a filtering proxy. Attempt requests and read the error rather than predicting whether a host is reachable; denied connections are reported in a `<sandbox_violations>` block explaining the reason{auto_suffix}."
        )));
    }
    if has_network_config
        && facts.platform.as_deref() == Some("macos")
        && !facts.allow_local_binding
    {
        if allow_unsandboxed {
            instructions.push(PromptItem::Text("If a command fails to bind or listen on a local port with \"Operation not permitted\" (EPERM), local port binding is off in this sandbox. Treat it as the sandbox-caused failure described above, and tell the user that `sandbox.network.allowLocalBinding: true` in their settings (it applies without a restart) allows it without leaving the sandbox."));
        } else {
            let suffix = if !facts.interactive_session {
                ""
            } else if !facts.session_kind_present {
                ", exclude the command with `/sandbox exclude <pattern>`, or run it themselves with the `!` prefix"
            } else {
                ", or exclude the command with `/sandbox exclude <pattern>`"
            };
            instructions.push(PromptItem::Owned(format!(
                "If a command fails to bind or listen on a local port with \"Operation not permitted\" (EPERM), local port binding is off in this sandbox. Tell the user they can allow it with `sandbox.network.allowLocalBinding: true` in their settings (it applies without a restart){suffix}; changing sandbox settings is their decision, not yours."
            )));
        }
    }
    if facts.platform.as_deref() == Some("macos") && !facts.allow_apple_events {
        instructions.push(PromptItem::Text("Opening an app, file or URL with `open`, or scripting another app with `osascript`, is blocked inside this sandbox (macOS Launch Services and Apple Events are off) and fails with errors such as -10822 (kLSServerCommunicationErr), -54, -600 or \"LSOpenURLsWithRole() failed\". That failure is the sandbox, not a problem with what you built; do not retry it with `dangerouslyDisableSandbox: true`. Tell the user the sandbox blocked it and give them the exact command to run themselves."));
    }
    instructions.push(PromptItem::Text(if facts.filesystem_policy_relaxed {
        "For temporary files, create a scratch directory with `mktemp -d` and reference it by absolute path. Do NOT assume `$TMPDIR` is set — the sandbox does not export it in this configuration."
    } else {
        "For temporary files, always use the `$TMPDIR` environment variable. TMPDIR is automatically set to the correct sandbox-writable directory in sandbox mode. Do NOT use `/tmp` directly - use `$TMPDIR` instead."
    }));
    if facts.interactive_session {
        instructions.push(PromptItem::Text("If a clipboard utility such as `pbcopy`, `xclip`, or `wl-copy` fails inside the sandbox and the user wants the text on their clipboard, put the text in a fenced code block in your response and tell them to run `/copy` (it copies from outside the sandbox; when the picker appears they can select just that block), rather than writing a file for them to copy manually."));
    }

    let mut sections = vec![format!("## LingXi Code command sandbox")];
    if boundary_branch {
        sections.extend([
            format!("By default, LingXi Code commands run inside an OS-level sandbox{platform_suffix} applied to each command separately, not to the session as a whole; how it is configured in this session is described below."),
            String::new(),
            SANDBOX_BOUNDARY_NOTE.to_owned(),
            String::new(),
            "How the sandbox is configured in this session:".to_owned(),
        ]);
    } else if facts.filesystem_policy_relaxed {
        sections.extend([
            "By default, LingXi Code tool commands run in a sandbox. This sandbox controls which network hosts commands may access without an explicit override.".to_owned(),
            String::new(),
            "The sandbox is configured as follows:".to_owned(),
        ]);
    } else {
        sections.extend([
            "By default, LingXi Code tool commands run in a sandbox. This sandbox controls which directories and network hosts commands may access or modify without an explicit override.".to_owned(),
            String::new(),
            "The sandbox has the following restrictions:".to_owned(),
        ]);
    }
    sections.push(restrictions.join("\n"));
    sections.push(String::new());
    sections.extend(native_bullets(instructions));
    sections.join("\n")
}

fn previous_content_matches(previous: Option<&Source>, current_units: &[u16]) -> bool {
    let Some(previous) = previous else {
        return current_units.is_empty();
    };
    match previous.at_pointer("/content") {
        None => current_units.is_empty(),
        Some(value) if value.is_null() => current_units.is_empty(),
        Some(_) => previous
            .string_units("/content")
            .is_some_and(|previous_units| previous_units == current_units),
    }
}

fn latest_sandbox_attachment<'a>(
    history: &[ConversationMessage],
    attachments: &'a HashMap<MessageId, Source>,
) -> Option<&'a Source> {
    history.iter().rev().find_map(|message| {
        attachments
            .get(&message.id())
            .filter(|payload| payload.string_at("/type") == Some(ATTACHMENT_TYPE))
    })
}

fn ingress_attachment(content: String) -> Option<PromptIngressAttachment> {
    let content_source = string_source(&content);
    let payload = object(vec![
        ("type", string_source(ATTACHMENT_TYPE)),
        ("content", content_source),
    ]);
    let rendered_text = if content.is_empty() {
        SANDBOX_DISABLED_TEXT.to_owned()
    } else {
        content.clone()
    };
    let rendered_units = rendered_text.encode_utf16().collect::<Vec<_>>();
    let message = ConversationMessage::user_meta_js_utf16(
        MessageId::new(),
        rendered_text.clone(),
        rendered_units,
    );
    let rendered = rendered_system_message(&message)?;
    Some(PromptIngressAttachment {
        message,
        payload,
        rendered,
    })
}

fn delta_attachment(
    bash_available: bool,
    current_content: Option<String>,
    previous: Option<&Source>,
) -> Vec<PromptIngressAttachment> {
    if !bash_available {
        return Vec::new();
    }
    let Some(current_content) = current_content else {
        return Vec::new();
    };
    let current_units = current_content.encode_utf16().collect::<Vec<_>>();
    if previous_content_matches(previous, &current_units) {
        return Vec::new();
    }
    ingress_attachment(current_content).into_iter().collect()
}

impl ConversationOrchestrator {
    /// Return Native's current Bash sandbox instruction delta for the initial
    /// user-prompt attachment batch.
    pub(crate) async fn native_sandbox_prompt_ingress_attachments(
        &self,
        prior_history: &[ConversationMessage],
    ) -> Vec<PromptIngressAttachment> {
        if !self.native_prompt_attachments_enabled() {
            return Vec::new();
        }
        if !self
            .filtered_available_tools()
            .await
            .iter()
            .any(|tool| tool.name() == BASH_TOOL_NAME)
        {
            return Vec::new();
        }
        let Some(provider) = self.native_sandbox_host_facts_provider.as_ref() else {
            return Vec::new();
        };
        let facts = provider();
        let current = if facts.sandbox_available {
            Some(render_active_sandbox_instructions(&facts))
        } else {
            Some(String::new())
        };
        let previous = {
            let attachments = self
                .transcript
                .model_reminder_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            latest_sandbox_attachment(prior_history, &attachments).cloned()
        };
        delta_attachment(true, current, previous.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(content: &str) -> Source {
        object(vec![
            ("type".into(), string_source(ATTACHMENT_TYPE)),
            ("content".into(), string_source(content)),
        ])
    }

    #[test]
    fn bash_gate_and_empty_reset_follow_native_delta_rules() {
        let old = payload("previous sandbox policy");
        assert!(delta_attachment(false, Some(String::new()), Some(&old)).is_empty());
        let reset = delta_attachment(true, Some(String::new()), Some(&old));
        assert_eq!(reset.len(), 1, "a disabled sandbox resets old instructions");
        assert_eq!(reset[0].payload.string_at("/content"), Some(""));
        assert_eq!(reset[0].message.text_content(), SANDBOX_DISABLED_TEXT);
        assert!(delta_attachment(true, Some(String::new()), None).is_empty());
        assert!(delta_attachment(true, Some(String::new()), Some(&payload(""))).is_empty());
    }

    #[test]
    fn current_content_is_compared_to_latest_raw_utf16_history() {
        let older = payload("older");
        let current = payload("policy 🧪");
        assert_eq!(
            current.string_units("/content").unwrap().len(),
            "policy 🧪".encode_utf16().count()
        );
        let older_id = MessageId::new();
        let current_id = MessageId::new();
        let history = vec![
            ConversationMessage::user_meta(older_id, "older".into()),
            ConversationMessage::user_meta(current_id, "current".into()),
        ];
        let attachments = HashMap::from([(older_id, older), (current_id, current.clone())]);
        assert!(latest_sandbox_attachment(&history, &attachments)
            .is_some_and(|latest| latest.string_units("/content")
                == payload("policy 🧪").string_units("/content")));
        assert!(delta_attachment(true, Some("policy 🧪".into()), Some(&current)).is_empty());
        let changed = delta_attachment(true, Some("changed".into()), Some(&current));
        assert_eq!(changed.len(), 1);
        assert_eq!(
            changed[0].payload.string_units("/content"),
            Some("changed".encode_utf16().collect::<Vec<_>>())
        );
    }

    #[test]
    fn history_comparison_does_not_round_trip_through_lossy_display_text() {
        let previous = Source::parse(r#"{"type":"sandbox_instructions","content":"\ud800"}"#)
            .expect("the source projection retains JavaScript lone surrogates");
        assert_eq!(previous.string_units("/content"), Some(vec![0xd800]));
        assert!(!previous_content_matches(Some(&previous), &[0xfffd]));
    }

    #[test]
    fn active_instructions_render_only_host_facts_and_actual_configuration() {
        let facts = NativeSandboxHostFacts {
            sandbox_available: true,
            filesystem_read: Source::parse(
                r#"{"denyOnly":["/private/credentials"],"allowWithinDeny":[]}"#,
            )
            .unwrap(),
            filesystem_write: Source::parse(
                r#"{"allowOnly":["/workspace/project"],"denyWithinAllow":[]}"#,
            )
            .unwrap(),
            network_restriction_config: Some(
                Source::parse(r#"{"allowedHosts":[],"deniedHosts":["example.invalid"]}"#).unwrap(),
            ),
            allow_unix_sockets: None,
            ignore_violations: None,
            sandbox_settings: Some(Source::parse(r#"{"enabled":true}"#).unwrap()),
            allow_unsandboxed_commands: false,
            allow_apple_events: false,
            allow_local_binding: false,
            platform: Some("macos".into()),
            auto_mode: false,
            filesystem_policy_relaxed: false,
            temp_paths: vec!["/tmp/claude-501/".into(), "/tmp/claude-501/".into()],
            boundary_note_enabled: true,
            command_network_lists_offered: false,
            sandbox_command_changes_settings: false,
            interactive_session: true,
            session_kind_present: false,
            settings_layers: Vec::new(),
        };
        let rendered = render_active_sandbox_instructions(&facts);
        assert!(rendered.contains("macOS Seatbelt"));
        assert!(rendered.contains("/workspace/project"));
        assert!(rendered.contains("example.invalid"));
        assert!(rendered.contains("dangerouslyDisableSandbox` parameter is disabled"));
        assert!(rendered.contains("Apple Events"));
    }

    #[test]
    fn active_renderer_matches_pinned_native_source_goldens() {
        let goldens: HashMap<String, String> = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/native_sandbox_instructions_295_goldens.json"
        )))
        .expect("source-oracle goldens are valid JSON");

        let cases = [
            (
                "strict_boundary_bypass",
                NativeSandboxHostFacts {
                    sandbox_available: true,
                    filesystem_read: Source::parse(
                        r#"{"denyOnly":["/private/credentials","/private/credentials"],"allowWithinDeny":["/workspace/shared"]}"#,
                    )
                    .unwrap(),
                    filesystem_write: Source::parse(
                        r#"{"allowOnly":["/tmp/claude-501/","/workspace/project","/tmp/claude-501/"],"denyWithinAllow":["/workspace/project/.env"]}"#,
                    )
                    .unwrap(),
                    network_restriction_config: Some(
                        Source::parse(
                            r#"{"allowedHosts":["api.example"],"deniedHosts":["blocked.example"]}"#,
                        )
                        .unwrap(),
                    ),
                    allow_unix_sockets: Some(Source::parse(r#"["/var/run/docker.sock"]"#).unwrap()),
                    ignore_violations: Some(
                        Source::parse(r#"{"Bash":["/workspace/project/generated"]}"#).unwrap(),
                    ),
                    sandbox_settings: None,
                    allow_unsandboxed_commands: true,
                    allow_apple_events: false,
                    allow_local_binding: false,
                    platform: Some("macos".into()),
                    auto_mode: false,
                    filesystem_policy_relaxed: false,
                    temp_paths: vec!["/tmp/claude-501/".into(), "/tmp/claude-501/".into()],
                    boundary_note_enabled: true,
                    command_network_lists_offered: false,
                    sandbox_command_changes_settings: true,
                    interactive_session: false,
                    session_kind_present: false,
                    settings_layers: Vec::new(),
                },
            ),
            (
                "relaxed_filesystem_interactive",
                NativeSandboxHostFacts {
                    sandbox_available: true,
                    filesystem_read: Source::parse(
                        r#"{"denyOnly":["/private/secret"],"allowWithinDeny":["/private/secret/public"]}"#,
                    )
                    .unwrap(),
                    filesystem_write: Source::parse(
                        r#"{"allowOnly":["/tmp/claude-501/","/workspace/project"],"denyWithinAllow":[]}"#,
                    )
                    .unwrap(),
                    network_restriction_config: Some(
                        Source::parse(r#"{"allowedHosts":["api.example"]}"#).unwrap(),
                    ),
                    allow_unix_sockets: None,
                    ignore_violations: None,
                    sandbox_settings: None,
                    allow_unsandboxed_commands: false,
                    allow_apple_events: true,
                    allow_local_binding: true,
                    platform: Some("linux".into()),
                    auto_mode: false,
                    filesystem_policy_relaxed: true,
                    temp_paths: vec!["/tmp/claude-501/".into(), "/tmp/claude-501/".into()],
                    boundary_note_enabled: false,
                    command_network_lists_offered: false,
                    sandbox_command_changes_settings: false,
                    interactive_session: true,
                    session_kind_present: false,
                    settings_layers: Vec::new(),
                },
            ),
            (
                "strict_auto_network_lists",
                NativeSandboxHostFacts {
                    sandbox_available: true,
                    filesystem_read: Source::parse(r#"{"denyOnly":[],"allowWithinDeny":[]}"#)
                        .unwrap(),
                    filesystem_write: Source::parse(
                        r#"{"allowOnly":["/workspace/project"],"denyWithinAllow":[]}"#,
                    )
                    .unwrap(),
                    network_restriction_config: Some(
                        Source::parse(
                            r#"{"allowedHosts":["api.example"],"deniedHosts":["denied.example"]}"#,
                        )
                        .unwrap(),
                    ),
                    allow_unix_sockets: Some(Source::parse("[]").unwrap()),
                    ignore_violations: Some(Source::parse("{}").unwrap()),
                    sandbox_settings: None,
                    allow_unsandboxed_commands: true,
                    allow_apple_events: true,
                    allow_local_binding: true,
                    platform: Some("macos".into()),
                    auto_mode: true,
                    filesystem_policy_relaxed: false,
                    temp_paths: vec!["/tmp/claude-501/".into(), "/tmp/claude-501/".into()],
                    boundary_note_enabled: false,
                    command_network_lists_offered: true,
                    sandbox_command_changes_settings: true,
                    interactive_session: false,
                    session_kind_present: true,
                    settings_layers: Vec::new(),
                },
            ),
            (
                "allowed_hosts_only_and_lone_surrogate",
                NativeSandboxHostFacts {
                    sandbox_available: true,
                    filesystem_read: Source::parse(
                        r#"{"denyOnly":["/private/\ud800","/private/\ud800"],"allowWithinDeny":[]}"#,
                    )
                    .unwrap(),
                    filesystem_write: Source::parse(
                        r#"{"allowOnly":["/workspace/project"],"denyWithinAllow":[]}"#,
                    )
                    .unwrap(),
                    network_restriction_config: Some(
                        Source::parse(r#"{"allowedHosts":["api.example"]}"#).unwrap(),
                    ),
                    allow_unix_sockets: None,
                    ignore_violations: None,
                    sandbox_settings: None,
                    allow_unsandboxed_commands: false,
                    allow_apple_events: false,
                    allow_local_binding: false,
                    platform: Some("macos".into()),
                    auto_mode: false,
                    filesystem_policy_relaxed: false,
                    temp_paths: vec!["/tmp/claude-501/".into(), "/tmp/claude-501/".into()],
                    boundary_note_enabled: false,
                    command_network_lists_offered: false,
                    sandbox_command_changes_settings: true,
                    interactive_session: false,
                    session_kind_present: false,
                    settings_layers: Vec::new(),
                },
            ),
            (
                "ordered_json_js_numbers_and_utf16",
                NativeSandboxHostFacts {
                    sandbox_available: true,
                    filesystem_read: Source::parse(
                        r#"{"denyOnly":[],"allowWithinDeny":[]}"#,
                    )
                    .unwrap(),
                    filesystem_write: Source::parse(
                        r#"{"allowOnly":["/workspace/project"],"denyWithinAllow":[]}"#,
                    )
                    .unwrap(),
                    network_restriction_config: None,
                    allow_unix_sockets: None,
                    ignore_violations: Some(
                        Source::parse(
                            r#"{"Bash":{"z":1.0,"3":"\ud800","a":1e21,"b":1e-7}}"#,
                        )
                        .unwrap(),
                    ),
                    sandbox_settings: None,
                    allow_unsandboxed_commands: false,
                    allow_apple_events: true,
                    allow_local_binding: true,
                    platform: Some("linux".into()),
                    auto_mode: false,
                    filesystem_policy_relaxed: false,
                    temp_paths: vec!["/tmp/claude-501/".into(), "/tmp/claude-501/".into()],
                    boundary_note_enabled: false,
                    command_network_lists_offered: false,
                    sandbox_command_changes_settings: true,
                    interactive_session: false,
                    session_kind_present: false,
                    settings_layers: Vec::new(),
                },
            ),
        ];

        assert_eq!(goldens.len(), cases.len());
        for (name, facts) in cases {
            assert_eq!(
                render_active_sandbox_instructions(&facts),
                goldens
                    .get(name)
                    .map(String::as_str)
                    .unwrap_or_else(|| panic!("missing source golden: {name}")),
                "Native source output changed for {name}",
            );
        }
    }

    #[test]
    fn allowed_hosts_alone_emit_egress_guidance_but_no_network_config_row() {
        let facts = NativeSandboxHostFacts {
            sandbox_available: true,
            filesystem_read: Source::parse(r#"{"denyOnly":[],"allowWithinDeny":[]}"#).unwrap(),
            filesystem_write: Source::parse(r#"{"allowOnly":[],"denyWithinAllow":[]}"#).unwrap(),
            network_restriction_config: Some(
                Source::parse(r#"{"allowedHosts":["api.example"]}"#).unwrap(),
            ),
            allow_unix_sockets: None,
            ignore_violations: None,
            sandbox_settings: None,
            allow_unsandboxed_commands: false,
            allow_apple_events: true,
            allow_local_binding: false,
            platform: Some("macos".into()),
            auto_mode: false,
            filesystem_policy_relaxed: false,
            temp_paths: Vec::new(),
            boundary_note_enabled: false,
            command_network_lists_offered: false,
            sandbox_command_changes_settings: true,
            interactive_session: false,
            session_kind_present: false,
            settings_layers: Vec::new(),
        };

        let rendered = render_active_sandbox_instructions(&facts);
        assert!(!rendered.contains("Network:"));
        assert!(rendered.contains("Network egress goes through a filtering proxy"));
        assert!(rendered.contains(
            "If a command fails to bind or listen on a local port with \"Operation not permitted\" (EPERM), local port binding is off in this sandbox. Tell the user they can allow it with `sandbox.network.allowLocalBinding: true` in their settings (it applies without a restart); changing sandbox settings is their decision, not yours."
        ));
    }

    #[test]
    fn cold_resume_restores_the_same_typed_sandbox_content() {
        let row = serde_json::from_str::<session::JsonlMessage>(
            r#"{"type":"attachment","uuid":"88888888-8888-4888-8888-888888888888","parentUuid":null,"isSidechain":false,"timestamp":"2026-10-11T12:00:00Z","cwd":"/workspace","version":"test","sessionId":"22222222-2222-4222-8222-222222222222","message":null,"attachment":{"type":"sandbox_instructions","content":"policy 🧪"}}"#,
        )
        .expect("fixture JSONL attachment parses");
        let row_id = MessageId::from_uuid(
            uuid::Uuid::parse_str("88888888-8888-4888-8888-888888888888")
                .expect("fixture UUID parses"),
        );
        let cold = crate::resume::model_reminder_attachments_from_messages(&[row]);
        assert_eq!(cold[&row_id].string_at("/type"), Some(ATTACHMENT_TYPE));
        assert!(delta_attachment(true, Some("policy 🧪".into()), Some(&cold[&row_id])).is_empty());
        assert_eq!(
            delta_attachment(true, Some("next policy".into()), Some(&cold[&row_id])).len(),
            1
        );
    }
}

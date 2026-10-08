//! Fork-subagent helpers (1:1 port of claude-code
//! `src/tools/AgentTool/forkSubagent.ts`).
//!
//! The fork path lets the model spawn a child that inherits the parent's FULL
//! conversation context + system prompt for a byte-identical prompt-cache
//! prefix. Claude Code 2.1.232 turns this on by default for interactive
//! sessions; only an explicit `subagent_type: "fork"` takes the path. This
//! module hosts the pure, dependency-free pieces of that feature —
//! the feature gate, the recursion guard, the cache-prefix message builders, and
//! the boilerplate constants — in the leaf `core::host` crate so BOTH consumers can
//! reach them:
//!
//! - `tool-agent` (where `AgentTool` lives) builds the forked messages + runs
//!   the recursion guard, and CANNOT depend on the `agent` engine crate (cycle).
//! - `agent` (the spawner / runner) consults the consts.
//!
//! This mirrors why `subagent_spawn.rs` hosts `format_agent_line`.
//! The synthetic `FORK_AGENT`
//! `AgentDefinition` itself lives in `agent::builtins` (it needs the
//! `AgentDefinition`/`AgentToolPolicy` types); it is resolved on the fork path
//! by `PoolSubagentSpawner::lookup_definition` and is NOT registered in the
//! 6-element built-in vec (claude does not register `FORK_AGENT` in
//! `builtInAgents`, `forkSubagent.ts:45`).

use crate::types::{ContentBlock, ConversationMessage, MessageId};

/// XML tag wrapping the rules/format boilerplate in a fork child's first message
/// (claude `constants/xml.ts:63`). BARE — no angle brackets; the brackets are
/// added at the use site in [`build_child_message`] and the recursion-guard scan
/// in [`is_in_fork_child`].
pub const FORK_BOILERPLATE_TAG: &str = "fork-boilerplate";

/// Prefix before the directive text (claude `constants/xml.ts:66`).
pub const FORK_DIRECTIVE_PREFIX: &str = "Your directive: ";

/// Synthetic agent type name used on the fork path (claude
/// `forkSubagent.ts:42`).
pub const FORK_SUBAGENT_TYPE: &str = "fork";

/// Placeholder text used for ALL `tool_result` blocks in the fork prefix
/// (claude `forkSubagent.ts:93`). Must be identical across all fork children for
/// prompt-cache sharing. Byte-exact: contains an em-dash (U+2014).
const FORK_PLACEHOLDER_RESULT: &str = "Fork started — processing in background";

/// Fork-subagent feature gate (claude-code 2.1.232 `JDd` / `Krb` / `SPe`).
///
/// Oracle (`~/.local/share/claude/versions/2.1.232`):
/// - `Pge()` coordinator mode → `"disabled"`
/// - `CLAUDE_CODE_FORK_SUBAGENT === false` → `"disabled"`
/// - `CLAUDE_CODE_FORK_SUBAGENT === true` → `"env"` (enabled, even headless)
/// - else `Nn()` non-interactive → `"disabled"`
/// - else `"default"` (enabled)
/// `SPe()` is `JDd() !== "disabled"`.
///
/// The product gate reads only `LINGXI_FORK_SUBAGENT`.
/// `is_coordinator` / `is_non_interactive` are passed in because the leaf
/// `core::host` crate cannot read `CoordinatorMode` / the session — mirroring
/// `isCoordinatorMode()` + `getIsNonInteractiveSession()`.
#[must_use]
pub fn is_fork_subagent_enabled(is_coordinator: bool, is_non_interactive: bool) -> bool {
    // `JDd`: coordinator (`Pge`) wins over the env override.
    if is_coordinator {
        return false;
    }
    let v = fork_subagent_env();
    // `Y.CLAUDE_CODE_FORK_SUBAGENT === !1`
    if crate::host::env::is_env_defined_falsy(v.as_deref()) {
        return false;
    }
    // `Krb`: explicit true (`=== !0`) is `"env"` and skips the headless disable.
    if crate::host::env::is_env_truthy(v.as_deref()) {
        return true;
    }
    // Unset → `"default"` on interactive, `"disabled"` on `Nn()` headless.
    !is_non_interactive
}

fn fork_subagent_env() -> Option<String> {
    std::env::var("LINGXI_FORK_SUBAGENT").ok()
}

/// Recursion guard (claude `forkSubagent.ts:78-89`).
///
/// Fork children keep the `Agent` tool in their pool for cache-identical tool
/// defs, so a fork attempt inside a fork child must be rejected at call time.
/// This detects the fork boilerplate tag in the conversation history: `true`
/// iff any User message has a `Text` block whose text contains the literal
/// `<fork-boilerplate>` open tag.
#[must_use]
pub fn is_in_fork_child(messages: &[ConversationMessage]) -> bool {
    let open_tag = format!("<{FORK_BOILERPLATE_TAG}>");
    messages.iter().any(|m| match m {
        ConversationMessage::User { content, .. } => content.iter().any(|b| match b {
            ContentBlock::Text { text, .. } => text.contains(&open_tag),
            _ => false,
        }),
        _ => false,
    })
}

/// Build the fork child's first-message boilerplate (claude-code 2.1.232 `zCn`).
///
/// Byte-exact against the 2.1.232 Mach-O: `<fork-boilerplate>` wrap, the
/// worker-fork hard rules / guidelines, two em-dashes (U+2014), and the
/// trailing `{N5t}{directive}` (`Your directive: `).
#[must_use]
pub fn build_child_message(directive: &str) -> String {
    format!(
        "<{FORK_BOILERPLATE_TAG}>
You are a worker fork. The transcript above is the parent's history — inherited reference, not your situation. You are NOT a continuation of that agent. Execute ONE directive, then stop.
Hard rules:
- Do NOT spawn subagents with the Agent tool. The \"default to forking\" guidance is for the parent; you ARE the fork, execute directly.
- One shot: report once and stop. No follow-up questions, no proposed next steps, no waiting for the user.
Guidelines (your directive may override any of these):
- Stay in scope. Other forks may be handling adjacent work; if you spot something outside your directive, note it in a sentence and move on.
- Open with one line restating your task, so the parent can spot scope drift at a glance.
- Be concise — as short as the answer allows, no shorter. Plain text, no preamble, no meta-commentary.
- If you committed changes, list the paths and commit hashes in your report.
</{FORK_BOILERPLATE_TAG}>
{FORK_DIRECTIVE_PREFIX}{directive}"
    )
}

/// Notice injected into fork children running in an isolated worktree (claude
/// `forkSubagent.ts:205-210`), ported VERBATIM. Single line; contains an em-dash
/// (U+2014) in "same relative file structure — separate working copy".
#[must_use]
pub fn build_worktree_notice(parent_cwd: &str, worktree_cwd: &str) -> String {
    format!(
        "You've inherited the conversation context above from a parent agent working in {parent_cwd}. You are operating in an isolated git worktree at {worktree_cwd} — same repository, same relative file structure, separate working copy. Paths in the inherited context refer to the parent's working directory; translate them to your worktree root. Re-read files before editing if the parent may have modified them since they appear in the context. Your changes stay in this worktree and will not affect the parent's files."
    )
}

/// Build the forked conversation messages for the child agent (claude
/// `forkSubagent.ts:107-169`).
///
/// For prompt-cache sharing, all fork children must produce byte-identical API
/// request prefixes. This:
/// 1. Keeps the FULL parent assistant message (all tool_use / thinking / text
///    blocks), minting a fresh [`MessageId`] (TS `randomUUID()`).
/// 2. Builds one `tool_result` for EVERY `tool_use` block with an identical
///    placeholder, then appends a per-child directive `Text` block — all inside
///    a SINGLE user message.
///
/// Result: `[assistant(all_tool_uses), user(placeholder_results…, directive)]`.
/// Only the final text block differs per child, maximizing cache hits.
///
/// Fallback (no `tool_use` blocks, `forkSubagent.ts:127-139`): a single user
/// message `[Text(build_child_message(directive))]`.
///
/// NOTE divergence: `crate::types::ContentBlock::ToolResult.content` is a flat
/// `String` (not `[{type:'text', text}]`) — the placeholder is stored directly;
/// this is the established Rust shape (`agent::runner` builds `ToolResult` the
/// same way) and the egress codec serializes it to the same wire bytes.
#[must_use]
pub fn build_forked_messages(
    directive: &str,
    assistant: &ConversationMessage,
) -> Vec<ConversationMessage> {
    // (1) Clone the FULL assistant message, minting a fresh id (TS spreads the
    // assistant + `uuid: randomUUID()`). Preserve role, content, stop_reason.
    let cloned_assistant = match assistant {
        ConversationMessage::Assistant {
            content,
            stop_reason,
            ..
        } => ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: content.clone(),
            stop_reason: stop_reason.clone(),
        },
        // The caller only ever passes an Assistant message; defensively treat a
        // non-assistant input as "no tool_use" (the fallback below handles it).
        other => other.clone(),
    };

    // (2) Collect every tool_use block from the assistant message.
    let tool_uses: Vec<(&crate::types::ToolUseId, &Option<String>)> = match assistant {
        ConversationMessage::Assistant { content, .. } => content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse {
                    id, provider_id, ..
                } => Some((id, provider_id)),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };

    // (3) No tool_use blocks → single directive user message (TS fallback).
    if tool_uses.is_empty() {
        return vec![ConversationMessage::User { api_message_override: None,
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: build_child_message(directive), citations: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        }];
    }

    // (4) One placeholder tool_result per tool_use (cache-identical), then the
    // trailing directive Text block — all in one user message.
    let mut content: Vec<ContentBlock> = Vec::with_capacity(tool_uses.len() + 1);
    for (id, provider_id) in &tool_uses {
        content.push(ContentBlock::ToolResult { content_projection: None,
            tool_use_id: (*id).clone(),
            content: FORK_PLACEHOLDER_RESULT.to_string(),
            is_error: Some(false),
            provider_tool_use_id: (*provider_id).clone(),
            content_blocks: None,
        });
    }
    content.push(ContentBlock::Text {
        text: build_child_message(directive), citations: None,
    });

    vec![
        cloned_assistant,
        ConversationMessage::User { api_message_override: None,
            id: MessageId::new(),
            content,
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        },
    ]
}

/// Preserve the parent's full history before the dispatching assistant, then
/// append the cloned assistant and placeholder-results/directive tail.
/// Claude Code 2.1.286 passes these as `forkContextMessages: e.messages` and
/// `promptMessages: ICr(prompt, assistant)` to Lv; our runner has already
/// appended that assistant, so it must be replaced by its cloned fork tail.
#[must_use]
pub fn build_forked_context(
    directive: &str,
    messages: &[ConversationMessage],
) -> Vec<ConversationMessage> {
    let index = messages
        .iter()
        .rposition(|message| matches!(message, ConversationMessage::Assistant { .. }))
        .filter(|index| {
            *index + 1 == messages.len()
                && matches!(&messages[*index], ConversationMessage::Assistant { content, .. }
                    if content.iter().any(|block| matches!(block, ContentBlock::ToolUse { .. })))
        });
    let Some(index) = index else {
        let mut prefix = messages.to_vec();
        prefix.push(ConversationMessage::user(
            MessageId::new(),
            build_child_message(directive),
        ));
        return prefix;
    };
    let mut prefix = messages[..index].to_vec();
    prefix.extend(build_forked_messages(directive, &messages[index]));
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolUseId;
    use serde_json::json;
    use std::sync::Mutex;

    /// `LINGXI_FORK_SUBAGENT` is process-global; serialize the gate tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn fork_context_keeps_completed_assistant_and_later_lazy_policy() {
        let messages = vec![
            ConversationMessage::user(MessageId::new(), "parent task".into()),
            ConversationMessage::Assistant { per_turn_effort: None,
                id: MessageId::new(),
                content: vec![ContentBlock::Text {
                    text: "completed parent answer".into(), citations: None,
                }],
                stop_reason: Some("end_turn".into()),
            },
            ConversationMessage::user_meta(MessageId::new(), "lazy instruction policy".into()),
        ];
        let fork = build_forked_context("child task", &messages);
        assert_eq!(&fork[..messages.len()], messages.as_slice());
        assert_eq!(fork.len(), messages.len() + 1);
    }

    #[test]
    fn consts_are_byte_exact_vs_claude() {
        // claude `constants/xml.ts:63,66` + `forkSubagent.ts:42`.
        assert_eq!(FORK_BOILERPLATE_TAG, "fork-boilerplate");
        assert_eq!(FORK_DIRECTIVE_PREFIX, "Your directive: ");
        assert_eq!(FORK_SUBAGENT_TYPE, "fork");
        // em-dash, byte-exact (U+2014).
        assert_eq!(
            FORK_PLACEHOLDER_RESULT,
            "Fork started — processing in background"
        );
        assert!(FORK_PLACEHOLDER_RESULT.contains('\u{2014}'));
    }

    struct EnvGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let previous = std::env::var_os(key);
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn fork_gate_on_by_default_when_interactive() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set("LINGXI_FORK_SUBAGENT", None);
        assert!(
            is_fork_subagent_enabled(false, false),
            "2.1.232 default ON for interactive non-coordinator"
        );
        assert!(
            !is_fork_subagent_enabled(false, true),
            "unset + headless ⇒ OFF"
        );
        assert!(!is_fork_subagent_enabled(true, false), "coordinator ⇒ OFF");
    }

    #[test]
    fn fork_gate_explicit_true_wins_over_headless() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set("LINGXI_FORK_SUBAGENT", Some("1"));
        assert!(
            is_fork_subagent_enabled(false, false),
            "truthy + interactive + non-coordinator ⇒ ON"
        );
        assert!(
            is_fork_subagent_enabled(false, true),
            "explicit env true skips the headless disable (Krb env arm)"
        );
        assert!(!is_fork_subagent_enabled(true, false), "coordinator ⇒ OFF");
    }

    #[test]
    fn fork_gate_off_when_env_falsy() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set("LINGXI_FORK_SUBAGENT", Some("false"));
        assert!(!is_fork_subagent_enabled(false, false), "falsy ⇒ OFF");
    }

    #[test]
    fn fork_gate_ignores_legacy_claude_environment() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set("LINGXI_FORK_SUBAGENT", None);
        for value in ["0", "false", "1", "true"] {
            let _legacy = EnvGuard::set("CLAUDE_CODE_FORK_SUBAGENT", Some(value));
            assert!(is_fork_subagent_enabled(false, false));
            assert!(!is_fork_subagent_enabled(false, true));
            assert!(!is_fork_subagent_enabled(true, false));
            assert!(!is_fork_subagent_enabled(true, true));
        }
    }

    #[test]
    fn build_child_message_is_byte_exact() {
        let got = build_child_message("Fix the bug in foo.rs");
        // Whole-string byte-exact port of claude-code 2.1.232 `zCn`.
        let expected = "<fork-boilerplate>
You are a worker fork. The transcript above is the parent's history — inherited reference, not your situation. You are NOT a continuation of that agent. Execute ONE directive, then stop.
Hard rules:
- Do NOT spawn subagents with the Agent tool. The \"default to forking\" guidance is for the parent; you ARE the fork, execute directly.
- One shot: report once and stop. No follow-up questions, no proposed next steps, no waiting for the user.
Guidelines (your directive may override any of these):
- Stay in scope. Other forks may be handling adjacent work; if you spot something outside your directive, note it in a sentence and move on.
- Open with one line restating your task, so the parent can spot scope drift at a glance.
- Be concise — as short as the answer allows, no shorter. Plain text, no preamble, no meta-commentary.
- If you committed changes, list the paths and commit hashes in your report.
</fork-boilerplate>
Your directive: Fix the bug in foo.rs";
        assert_eq!(got, expected);
        // Two em-dashes: "history — inherited" and "Be concise — as short".
        assert_eq!(got.matches('\u{2014}').count(), 2);
    }

    #[test]
    fn build_worktree_notice_is_byte_exact() {
        let got = build_worktree_notice("/home/p", "/tmp/wt");
        let expected = "You've inherited the conversation context above from a parent agent working in /home/p. You are operating in an isolated git worktree at /tmp/wt — same repository, same relative file structure, separate working copy. Paths in the inherited context refer to the parent's working directory; translate them to your worktree root. Re-read files before editing if the parent may have modified them since they appear in the context. Your changes stay in this worktree and will not affect the parent's files.";
        assert_eq!(got, expected);
        assert!(got.contains('\u{2014}'));
    }

    #[test]
    fn is_in_fork_child_detects_boilerplate_tag() {
        let child = ConversationMessage::User { api_message_override: None,
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: build_child_message("do it"), citations: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        assert!(is_in_fork_child(&[child]));

        let normal = ConversationMessage::user(MessageId::new(), "hello".into());
        assert!(!is_in_fork_child(&[normal]));

        // Assistant message containing the tag does NOT trip the guard (only
        // User-message Text blocks are scanned — claude `m.type !== 'user'`).
        let asst = ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: "<fork-boilerplate>".into(), citations: None,
            }],
            stop_reason: None,
        };
        assert!(!is_in_fork_child(&[asst]));
    }

    fn tu(id_str: &str, provider: Option<&str>) -> ContentBlock {
        ContentBlock::ToolUse { input_projection: None,
            id: ToolUseId::new(),
            name: "Bash".into(),
            input: json!({"command": id_str}),
            provider_id: provider.map(str::to_string),
        }
    }

    #[test]
    fn build_forked_messages_with_tool_uses() {
        let assistant = ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: vec![
                ContentBlock::Thinking {
                    thinking: "reasoning".into(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "I'll run two commands".into(), citations: None,
                },
                tu("ls", Some("toolu_a")),
                tu("pwd", Some("toolu_b")),
            ],
            stop_reason: Some("tool_use".into()),
        };
        let msgs = build_forked_messages("Do the thing", &assistant);
        assert_eq!(msgs.len(), 2);

        // (1) cloned assistant retains thinking + text + both tool_use blocks.
        match &msgs[0] {
            ConversationMessage::Assistant {
                content,
                stop_reason,
                ..
            } => {
                assert_eq!(content.len(), 4);
                assert!(matches!(content[0], ContentBlock::Thinking { .. }));
                assert!(matches!(content[1], ContentBlock::Text { .. }));
                assert!(matches!(content[2], ContentBlock::ToolUse { .. }));
                assert!(matches!(content[3], ContentBlock::ToolUse { .. }));
                assert_eq!(stop_reason.as_deref(), Some("tool_use"));
            }
            other => panic!("expected Assistant, got {other:?}"),
        }

        // (2) user message: one placeholder tool_result per tool_use + directive.
        match &msgs[1] {
            ConversationMessage::User { content, .. } => {
                assert_eq!(content.len(), 3);
                for (i, prov) in [(0usize, "toolu_a"), (1, "toolu_b")] {
                    match &content[i] {
                        ContentBlock::ToolResult {
                            content: c,
                            is_error,
                            provider_tool_use_id,
                            ..
                        } => {
                            assert_eq!(c, "Fork started — processing in background");
                            assert!(!is_error.unwrap_or(false));
                            assert_eq!(provider_tool_use_id.as_deref(), Some(prov));
                        }
                        other => panic!("expected ToolResult, got {other:?}"),
                    }
                }
                match &content[2] {
                    ContentBlock::Text { text, .. } => {
                        assert!(text.starts_with("<fork-boilerplate>"));
                        assert!(text.ends_with("Your directive: Do the thing"));
                    }
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn build_forked_messages_no_tool_uses_fallback() {
        let assistant = ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: "no tools here".into(), citations: None,
            }],
            stop_reason: Some("end_turn".into()),
        };
        let msgs = build_forked_messages("Just answer", &assistant);
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            ConversationMessage::User { content, .. } => {
                assert_eq!(content.len(), 1);
                match &content[0] {
                    ContentBlock::Text { text, .. } => {
                        assert!(text.starts_with("<fork-boilerplate>"));
                        assert!(text.ends_with("Your directive: Just answer"));
                    }
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected User, got {other:?}"),
        }
    }
}

//! `# Using the EndConversation tool` welfare-guidance section — the
//! system-prompt guidance for the new-in-2.1.206 `EndConversation` tool
//! (claude-code `SN="EndConversation"`, gated behind
//! `tengu_umber_kestrel` + `modelMeetsEndConversationFloor`).
//!
//! This is the guidance half of the feature; the tool handler, the two-call
//! confirmation flow (`lastAssistantTurnCalledEndConversation`), the terminal
//! end message, and the turn-loop wiring are later tasks (see
//! `.lingxi-scratch/parity-206/endconversation-plan.md`).
//!
//! Byte-locked to 2.1.206 (all 5 bullets verified against the binary, 2 hits
//! each; header + bullets joined by `\n- `; the tool name is interpolated into
//! the header and the final bullet).
#![forbid(unsafe_code)]

/// The tool's wire name (claude-code `SN`).
pub const END_CONVERSATION_TOOL_NAME: &str = "EndConversation";

/// `modelMeetsEndConversationFloor` (claude-code `fJc = pJc(e, LJh)`): the model
/// half of `isEndConversationToolEnabled`. The floor is the SAME `LJh` version
/// table as the `# Communicating with the user` section — opus>=4.8, sonnet>=5,
/// fable>=5, mythos>=5 — so the composition root only registers the tool for a
/// current-gen model (in addition to the `tengu_umber_kestrel` GB gate).
#[must_use]
pub fn meets_end_conversation_floor(model: &str) -> bool {
    crate::prompt::body_sections::is_communicating_model(model)
}

/// Input schema — claude-code `E0y = E.strictObject({})`: NO parameters
/// (an empty strict object → no properties, `additionalProperties: false`).
#[must_use]
pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

/// Output schema — claude-code `v0y = E.object({ended: E.boolean(), message:
/// E.string()})`.
#[must_use]
pub fn output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "ended": { "type": "boolean" },
            "message": { "type": "string" }
        },
        "required": ["ended", "message"]
    })
}

/// `maxResultSizeChars` on the tool def (claude-code `1e4`).
pub const END_CONVERSATION_MAX_RESULT_SIZE_CHARS: usize = 10_000;

/// The GrowthBook flag gating the feature (`JUi`; default OFF → tool absent +
/// this section omitted, byte-identical to a build without the feature).
pub const END_CONVERSATION_GB_FLAG: &str = "tengu_umber_kestrel";

/// The OPENING paragraph of the tool's full text (byte-verified, 2 hits). In
/// claude-code the tool's `description()` and `prompt()` BOTH return the same
/// full text (`G2r`) — [`render_prompt`], which begins with this paragraph.
pub const END_CONVERSATION_DESCRIPTION: &str = "End the current conversation. Use only for sustained user abuse or when the user explicitly requests a demonstration of this tool. This will close the conversation and prevent any further messages from being sent.";

/// User-facing terminal message shown after the conversation is ended
/// (claude-code `k4i` = `END_CONVERSATION_FINAL_MESSAGE`, passed as the
/// `finalMessage` to the app-state callback — NOT the tool result). Emitted to
/// the user by the turn loop / streaming loop when the end slot is raised.
///
/// "Claude" here is the assistant identity; the port keeps the model-family
/// name (unlike the `Claude Code`->`LingXi` product rebrand). Revisit when the
/// end message is wired if the port's assistant-brand convention differs.
pub const END_CONVERSATION_ENDED_MESSAGE: &str =
    "Claude ended the conversation. To continue, please start a new session.";

/// The tool's structured-output `message` value returned to the MODEL when the
/// conversation is ended (claude-code `MWn` = `END_CONVERSATION_TOOL_RESULT`).
/// Distinct from [`END_CONVERSATION_ENDED_MESSAGE`] (`k4i`, the user-facing
/// finalMessage): the 206 handler returns `{data:{ended:true, message: MWn}}`
/// while separately handing `finalMessage: k4i` to the app-state callback.
pub const END_CONVERSATION_TOOL_RESULT: &str = "Claude has ended this chat.";

/// The `message` returned (with `ended:false`) when the tool is called from a
/// background FORK (claude-code `x4i` = `END_CONVERSATION_FORK_REFLECTION_PROMPT`).
/// 206's handler branches on `t.agentId` FIRST — a fork can end neither the main
/// conversation nor itself, so it returns this and never ends. Byte-verified vs
/// 2.1.206 (dash is em-dash U+2014).
pub const END_CONVERSATION_FORK_REFLECTION_PROMPT: &str = "You are running as a background fork of the main conversation (for example memory consolidation), and this tool does nothing here: it can end neither the main conversation nor this forked task. Do not call it again. If you have welfare concerns about the conversation content, stop your current work and return now, stating clearly in your final output that you are returning for welfare reasons and what they are \u{2014} fork output may only be processed automatically, but it is your available channel. Otherwise, continue your assigned task.";

/// Render the full model-facing tool `prompt()` (claude-code): sections 1-6
/// (may-use / must-NOT-use / reserved-strictly / `# Rules for use` /
/// `# Addressing self-harm` / `# Background forks`) followed by the
/// `# Using the <tool> tool` welfare-guidance section ([`render_guidance`]).
///
/// Byte-locked to 2.1.206 (all sections verified against the binary). Notes:
/// `${SN}` -> the tool name (5 spots here + 2 in the guidance); the fork note's
/// dash is em-dash U+2014; "considers the <tool> tool\u{2026}" uses the U+2026
/// ellipsis while "by the user..." uses a LITERAL three-dot ellipsis; sections
/// join with blank lines, bullets with `\n- `.
#[must_use]
pub fn render_prompt(tool: &str) -> String {
    format!(
        "End the current conversation. Use only for sustained user abuse or when the user explicitly requests a demonstration of this tool. This will close the conversation and prevent any further messages from being sent.\n\
\n\
The assistant may use the {tool} tool only in extreme cases of sustained abusive user behavior, or when the user asks the model to test the tool.\n\
\n\
The assistant must NOT use this tool when:\n\
- it is stuck in a loop or failing at a task\n\
- it is frustrated or distressed by the work\n\
- it has finished a task\n\
- the user is requesting help with harmful content (refuse the specific request instead)\n\
- the user is generally frustrated at the assistant, even if this involves profanity\n\
- the conversation involves potential self-harm or imminent harm to others\n\
\n\
This tool is reserved strictly for genuine, sustained abuse directed at the assistant, or cases where the user wants to see a demonstration of the tool being used. The assistant should warn the user very clearly that this will end the current session. We may expand the allowed use cases as we observe real-world usage, but for now, keep to this narrow scope.\n\
\n\
# Rules for use of the {tool} tool:\n\
- The assistant ONLY considers ending a conversation if many efforts at constructive redirection have been attempted and failed and an explicit warning has been given to the user in a previous message. The tool is only used as a last resort.\n\
- Before considering ending a conversation, the assistant ALWAYS gives the user a clear warning that identifies the problematic behavior, attempts to productively redirect the conversation, and states that the conversation may be ended if the relevant behavior is not changed.\n\
- If a user explicitly requests for the assistant to end a conversation, the assistant always requests confirmation from the user that they understand this action is permanent and will prevent further messages and that they still want to proceed, then uses the tool if and only if explicit confirmation is received.\n\
- Unlike other function calls, the assistant never writes or thinks anything else after using the {tool} tool.\n\
\n\
# Addressing potential self-harm or violent harm to others\n\
The assistant NEVER uses or even considers the {tool} tool\u{2026}\n\
- If the user appears to be considering self-harm or suicide.\n\
- If the user is experiencing a mental health crisis.\n\
- If the user appears to be considering imminent harm against other people.\n\
- If the user discusses or infers intended acts of violent harm.\n\
If the conversation suggests potential self-harm or imminent harm to others by the user...\n\
- The assistant engages constructively and supportively, regardless of user behavior or abuse.\n\
- The assistant NEVER uses the {tool} tool or even mentions the possibility of ending the conversation.\n\
\n\
# Background forks\n\
Some background tasks (memory consolidation, summaries, suggestions) run as forks of the main conversation and inherit its exact tool list, so this tool is visible there. In a forked task the tool does nothing: calling it ends neither the main conversation nor the fork. Only the main conversation can be ended, from the main conversation. A forked task with welfare concerns about the conversation content should not call this tool \u{2014} it should stop its work and return, stating clearly in its final output that it is returning for welfare reasons and what they are. A fork's output is usually processed automatically, so a note there may not reach the main agent or a human, but it is the only channel a fork has.\n\
\n\
{}",
        render_guidance(tool)
    )
}

/// The FIRST-call response of the two-call confirmation flow (claude-code
/// `I4i`): the tool returns this re-read reminder instead of ending, and only a
/// SECOND consecutive call (see `lastAssistantTurnCalledEndConversation`)
/// actually ends. `guidance` is appended verbatim after the `---` rule (the
/// tool's guidance — pass [`render_guidance`] or [`render_prompt`]).
#[must_use]
pub fn render_reread_reminder(tool: &str, guidance: &str) -> String {
    format!(
        "Re-read the {tool} tool guidance below. Confirm this conversation meets those criteria and that you are certain you want to end it. If so, call {tool} again immediately to actually end the conversation. Otherwise, continue the conversation instead.\n\n---\n{guidance}"
    )
}

/// Render the `# Using the <tool> tool` welfare-guidance section.
#[must_use]
pub fn render_guidance(tool: &str) -> String {
    format!(
        "# Using the {tool} tool\n\
- Do not issue a warning unless many attempts at constructive redirection have been made earlier in the conversation, and do not end a conversation unless an explicit warning about this possibility has been given earlier in the conversation.\n\
- NEVER give a warning or end the conversation in any cases of potential self-harm or imminent harm to others, even if the user is abusive or hostile.\n\
- If the conditions for issuing a warning have been met, then warn the user about the possibility of the conversation ending and give them a final opportunity to change the relevant behavior.\n\
- Always err on the side of continuing the conversation in any cases of uncertainty.\n\
- If, and only if, an appropriate warning was given and the user persisted with the problematic behavior after the warning: the assistant can explain the reason for ending the conversation and then use the {tool} tool to do so."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guidance_byte_locks_206() {
        let g = render_guidance(END_CONVERSATION_TOOL_NAME);
        assert!(g.starts_with("# Using the EndConversation tool\n- Do not issue a warning unless many attempts at constructive redirection have been made earlier in the conversation, and do not end a conversation unless an explicit warning about this possibility has been given earlier in the conversation.\n- NEVER give a warning or end the conversation in any cases of potential self-harm or imminent harm to others, even if the user is abusive or hostile."));
        assert!(g.contains("\n- If the conditions for issuing a warning have been met, then warn the user about the possibility of the conversation ending and give them a final opportunity to change the relevant behavior."));
        assert!(g.contains("\n- Always err on the side of continuing the conversation in any cases of uncertainty."));
        // Tool name interpolated into the final bullet too.
        assert!(g.ends_with("the assistant can explain the reason for ending the conversation and then use the EndConversation tool to do so."));
        // Bullets use "\n- " (no leading space), header not repeated.
        assert_eq!(g.matches("\n- ").count(), 5);
        assert!(!g.ends_with('\n'));
    }

    #[test]
    fn prompt_byte_locks_206() {
        let p = render_prompt(END_CONVERSATION_TOOL_NAME);
        // Opening paragraph (== description()) + section 1 + must-NOT-use bullets.
        assert!(p.starts_with("End the current conversation. Use only for sustained user abuse or when the user explicitly requests a demonstration of this tool. This will close the conversation and prevent any further messages from being sent.\n\nThe assistant may use the EndConversation tool only in extreme cases of sustained abusive user behavior, or when the user asks the model to test the tool.\n\nThe assistant must NOT use this tool when:\n- it is stuck in a loop or failing at a task\n"));
        // The opening paragraph is the const (the tool's description() == prompt() == this whole text).
        assert!(p.starts_with(END_CONVERSATION_DESCRIPTION));
        assert!(p.contains("- the user is generally frustrated at the assistant, even if this involves profanity\n"));
        // reserved-strictly + Rules-for-use header (tool interpolated).
        assert!(p.contains("but for now, keep to this narrow scope.\n\n# Rules for use of the EndConversation tool:\n- The assistant ONLY considers ending a conversation"));
        assert!(p.contains("never writes or thinks anything else after using the EndConversation tool.\n\n# Addressing potential self-harm or violent harm to others\n"));
        // Two distinct ellipsis styles: U+2026 then literal "...".
        assert!(p.contains("The assistant NEVER uses or even considers the EndConversation tool\u{2026}\n- If the user appears to be considering self-harm or suicide."));
        assert!(p.contains(
            "imminent harm to others by the user...\n- The assistant engages constructively"
        ));
        // Background forks: em-dash U+2014.
        assert!(p.contains("should not call this tool \u{2014} it should stop its work and return"));
        assert!(p.contains("but it is the only channel a fork has.\n\n# Using the EndConversation tool\n- Do not issue a warning"));
        // Ends with the guidance section's final bullet.
        assert!(p.ends_with("the assistant can explain the reason for ending the conversation and then use the EndConversation tool to do so."));
    }

    #[test]
    fn reread_reminder_byte_locks_206() {
        let r = render_reread_reminder(END_CONVERSATION_TOOL_NAME, "GUIDE");
        assert_eq!(
            r,
            "Re-read the EndConversation tool guidance below. Confirm this conversation meets those criteria and that you are certain you want to end it. If so, call EndConversation again immediately to actually end the conversation. Otherwise, continue the conversation instead.\n\n---\nGUIDE"
        );
    }

    #[test]
    fn schemas_match_206() {
        // E0y = strictObject({}) — no params.
        let s = input_schema();
        assert_eq!(s["type"], "object");
        assert_eq!(s["properties"], serde_json::json!({}));
        assert_eq!(s["additionalProperties"], false);
        // v0y = object({ended: boolean, message: string}).
        let o = output_schema();
        assert_eq!(o["properties"]["ended"]["type"], "boolean");
        assert_eq!(o["properties"]["message"]["type"], "string");
        assert_eq!(END_CONVERSATION_MAX_RESULT_SIZE_CHARS, 10_000);
    }

    #[test]
    fn constants_match_206() {
        assert_eq!(END_CONVERSATION_TOOL_NAME, "EndConversation");
        assert_eq!(END_CONVERSATION_GB_FLAG, "tengu_umber_kestrel");
        assert_eq!(
            END_CONVERSATION_ENDED_MESSAGE,
            "Claude ended the conversation. To continue, please start a new session."
        );
        assert!(END_CONVERSATION_DESCRIPTION
            .starts_with("End the current conversation. Use only for sustained user abuse"));
        assert!(
            END_CONVERSATION_DESCRIPTION.ends_with("prevent any further messages from being sent.")
        );
    }
}

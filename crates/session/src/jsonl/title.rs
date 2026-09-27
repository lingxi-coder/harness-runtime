//! First-user-message title extraction — 1:1 port of claude-code's
//! `extractFirstPrompt` + `getFirstMeaningfulUserMessageTextContent`
//! (`claude-code/src/utils/sessionStorage.ts:1725-1812`), with the
//! enriched-picker empty fallback `'(session)'`
//! (`sessionStorage.ts:5052-5054`).
//!
//! `extract_title` feeds the `/resume` picker rows (`loader.rs::list_recent_sessions`),
//! which is the structural equivalent of claude-code's `enrichLog` path: the
//! stored `firstPrompt` is capped at 200 chars (`sessionStorage.ts:1732`) and an
//! empty/absent prompt is shown as `'(session)'` (`sessionStorage.ts:5052-5054`).
//!
//! Sub-rules — aligned to the v2.1.193 binary's title fn (`Jxt`/`Dln`/`wps`,
//! @199254565), which REWROTE the command path vs the older src:
//!  - PER-BLOCK normalize FIRST (`i=s.replaceAll('\n',' ').trim(); if(!i)continue`)
//!    — every check below runs on the normalized `i`, and the winner is capped to
//!    200 chars with `…`;
//!  - skip `isMeta` user messages and `isCompactSummary` (`JsonlMessage::extra`);
//!  - a `tool_result` block ABORTS the whole message (Jxt array branch `return`);
//!  - command-name: capture the BARE name into a `commandFallback` (first wins)
//!    and `continue` — v2.1.193 NO LONGER reads `<command-args>`, formats
//!    `<name> <args>`, or consults `builtInCommandNames()`. The fallback is
//!    returned ONLY when no message yields real text;
//!  - bash-input → `! <cmd>` prefix (uncapped);
//!  - `SKIP_FIRST_PROMPT_PATTERN` skip of leading lowercase XML tags /
//!    `[Request interrupted…]` markers.

use crate::jsonl::schema::JsonlMessage;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

/// Maximum number of `char`s stored for the title before truncation + ellipsis.
///
/// claude-code stores a "reasonably long" version (200 chars) and re-truncates
/// at display time by terminal width (`sessionStorage.ts:1730-1734`).
pub const TITLE_MAX_CHARS: usize = 200;

/// The single-codepoint Unicode ellipsis (U+2026) appended when truncating.
pub const TITLE_ELLIPSIS: char = '…';

/// Fallback title when no meaningful user prompt can be extracted —
/// claude-code's `enrichLog` shows `'(session)'` (`sessionStorage.ts:5052-5054`).
pub const EMPTY_TITLE_FALLBACK: &str = "(session)";

/// Fallback `/branch` default fork name when no meaningful prompt is found —
/// the binary's `I2l`/`deriveFirstPrompt` empty branch returns
/// `"Branched conversation"` (@217273303), distinct from the title path's
/// `"(session)"`.
pub const FORK_NAME_FALLBACK: &str = "Branched conversation";

/// Max `char`s stored for a `/branch` default fork name — the binary's `I2l`
/// `.slice(0,100)` (vs the title path's 200).
pub const FORK_NAME_MAX_CHARS: usize = 100;

/// `SKIP_FIRST_PROMPT_PATTERN` (`sessionStorage.ts:125-126`): leading lowercase
/// XML-like tag (IDE context, hook output, task notifications, …) or a synthetic
/// `[Request interrupted by user…]` marker.
static SKIP_FIRST_PROMPT_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:\s*<[a-z][\w-]*[\s>]|\[Request interrupted by user[^\]]*\])")
        .expect("SKIP_FIRST_PROMPT_PATTERN is a valid regex")
});

/// Title-path command-name regex — the binary's inlined `Lyu`
/// (`/<command-name>(.*?)<\/command-name>/`, v2.1.193 @199254820): case-SENSITIVE,
/// NO attribute allowance, `.` (single-line) content. NOT the generic case-
/// insensitive `extractTag` builder — the title path uses this dedicated literal.
static COMMAND_NAME_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<command-name>(.*?)</command-name>").expect("valid"));
/// Title-path bash-input regex — the binary's inlined
/// `/<bash-input>([\s\S]*?)<\/bash-input>/` (v2.1.193 @199254560): case-SENSITIVE,
/// no attrs, `[\s\S]` content.
static BASH_INPUT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<bash-input>([\s\S]*?)</bash-input>").expect("valid"));

/// `/\s+/g` — the fork-name whitespace-run collapse in the binary's `I2l`
/// (`.replace(/\s+/g," ")`).
static WHITESPACE_RUN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").expect("valid"));

/// Returns the session title.
///
/// 1. Selects the first *meaningful* user-message text via
///    [`first_meaningful_user_text`] (port of
///    `getFirstMeaningfulUserMessageTextContent`).
/// 2. Post-processes it (`extractFirstPrompt`): newline→space, trim, 200-char cap.
/// 3. Falls back to [`EMPTY_TITLE_FALLBACK`] (`'(session)'`) when nothing
///    meaningful is found OR the processed text is empty.
#[must_use]
pub fn extract_title(messages: &[JsonlMessage]) -> String {
    // first_meaningful_user_text already normalizes per-block (newline→space,
    // trim, 200-cap) like the binary's Jxt, so no further post_process here.
    match first_meaningful_user_text(messages) {
        Some(text) if !text.is_empty() => text,
        _ => EMPTY_TITLE_FALLBACK.to_string(),
    }
}

/// Whether the first-prompt scan encountered an autonomous `<tick>` prompt.
/// Claude suppresses the raw XML from the picker and uses a meaningful label
/// when no higher-precedence title or real user prompt exists.
pub(crate) fn has_autonomous_tick_prompt(messages: &[JsonlMessage]) -> bool {
    messages.iter().any(|message| {
        if message.message_type != "user"
            || extra_flag(message, "isMeta")
            || extra_flag(message, "isCompactSummary")
        {
            return false;
        }
        let Some(content) = message.message.get("content") else {
            return false;
        };
        collect_texts(content).is_some_and(|texts| {
            texts
                .iter()
                .any(|text| text.replace('\n', " ").trim().starts_with("<tick>"))
        })
    })
}

/// Derive the default `/branch` fork name from a conversation's messages — a
/// 1:1 port of the binary's `I2l`/`deriveFirstPrompt` (@217273303): iterate
/// messages, take the first meaningful USER prompt (skipping `isMeta` and —
/// the 2.1.198 fix — `isCompactSummary`, so a session whose history begins
/// with a compaction summary is NAMED from the first REAL prompt, not the
/// summary), then `.replace(/\s+/g," ").trim().slice(0,100).trimEnd() ||
/// "Branched conversation"`.
///
/// Shares [`first_meaningful_user_text`] (the `n9e` extractor: identical
/// user/meta/compact-summary skip + `<command-name>` fallback + `<bash-input>`
/// → `! cmd` + skip-XML rules) with [`extract_title`]; only the trailing
/// whitespace-collapse, 100-char cap, and `"Branched conversation"` fallback
/// differ from the title path's 200-cap + `"(session)"`.
#[must_use]
pub fn derive_fork_name(messages: &[JsonlMessage]) -> String {
    // `n ??= t.commandFallback` is already folded into `first_meaningful_user_text`.
    let raw = first_meaningful_user_text(messages).unwrap_or_default();
    // I2l trailing normalize: `.replace(/\s+/g," ").trim().slice(0,100).trimEnd()`.
    let collapsed = WHITESPACE_RUN.replace_all(&raw, " ");
    let capped: String = collapsed.trim().chars().take(FORK_NAME_MAX_CHARS).collect();
    let capped = capped.trim_end();
    if capped.is_empty() {
        FORK_NAME_FALLBACK.to_string()
    } else {
        capped.to_string()
    }
}

/// Port of `getFirstMeaningfulUserMessageTextContent`
/// (`sessionStorage.ts:1746-1812`). Returns the raw selected text (pre
/// newline-collapse / cap) or `None`.
fn first_meaningful_user_text(messages: &[JsonlMessage]) -> Option<String> {
    // `commandFallback` (binary `t.commandFallback`): the bare command name of the
    // FIRST `<command-name>` block seen, returned ONLY if no later message yields
    // real text. v2.1.193 dropped the old `builtInCommandNames()` skip + the
    // `<command-args>` formatting — every command now contributes just its bare
    // name as a last-resort fallback.
    let mut command_fallback: Option<String> = None;
    for m in messages {
        // `if (msg.type !== 'user' || msg.isMeta) continue` (1750).
        if m.message_type != "user" || extra_flag(m, "isMeta") {
            continue;
        }
        // `if ('isCompactSummary' in msg && msg.isCompactSummary) continue` (1752).
        if extra_flag(m, "isCompactSummary") {
            continue;
        }

        // `const content = msg.message?.content; if (!content) continue` (1754).
        let Some(content) = m.message.get("content") else {
            continue;
        };

        // Collect text blocks. `None` = the message carried a `tool_result` block
        // ⇒ the binary's Jxt `return`s (whole message yields nothing) — skip it.
        let Some(texts) = collect_texts(content) else {
            continue;
        };
        for raw in texts {
            // Per-block normalize FIRST (binary `i=s.replaceAll('\n',' ').trim()`),
            // then run every check against the normalized `i`.
            let normalized = raw.replace('\n', " ");
            let i = normalized.trim();
            // `if (!i) continue` (empty after trim).
            if i.is_empty() {
                continue;
            }

            // Command-name (binary `a=Lyu.exec(i); if(a){if(!t.commandFallback)
            // t.commandFallback=a[1]; continue}`): capture the bare name as the
            // fallback (first wins), then CONTINUE — never returns directly, never
            // reads args.
            if let Some(command_name) = extract_tag(i, &COMMAND_NAME_RE) {
                if command_fallback.is_none() {
                    command_fallback = Some(command_name);
                }
                continue;
            }

            // Bash input → `! <cmd>` (binary `l=...exec(i); if(l)return`!
            // ${l[1].trim()}``), checked before the XML skip. NOT capped.
            if let Some(bash) = extract_tag(i, &BASH_INPUT_RE) {
                return Some(format!("! {}", bash.trim()));
            }

            // Skip leading-XML / interrupt markers (binary `if(Oyu.test(i))continue`).
            if SKIP_FIRST_PROMPT_PATTERN.is_match(i) {
                continue;
            }

            // Winner: cap to 200 chars (binary `if(i.length>200)i=i.slice(0,200)
            // .trim()+'…'; return i`).
            return Some(truncate_with_ellipsis(i));
        }
    }
    // No message yielded real text → the bare command name, if any (binary
    // `return t.commandFallback`).
    command_fallback
}

/// `extractFirstPrompt` post-processing (`sessionStorage.ts:1728-1734`):
/// `result.replace(/\n/g,' ').trim()`, then cap at 200 chars with `'…'`.
fn post_process(text: &str) -> String {
    let collapsed = text.replace('\n', " ");
    truncate_with_ellipsis(collapsed.trim())
}

/// Normalize a SIDE-MAP title (`custom-title` / `ai-title` / `summary`) with the
/// exact same rule [`extract_title`] applies to a first-user-message prompt:
/// newline → space, `trim`, then the [`TITLE_MAX_CHARS`]-char cap +
/// [`TITLE_ELLIPSIS`]. The resume picker resolves
/// custom-title > ai-title > summary > first-prompt
/// ([`crate::jsonl::loader::collect_dir`]) and runs the winning side-map title
/// through this so every `SessionMetadata::title`, regardless of source, shares
/// one truncation/ellipsis contract (the display surfaces then re-truncate by
/// terminal width). Mirrors claude-code's `getLogDisplayTitle`
/// (`utils/log.ts:30`), which renders `customTitle || summary || firstPrompt`
/// through one `.trim()`-and-cap pipeline; the stored side-map titles are NOT
/// run through the `<command-name>`/`bash-input`/skip-XML transforms (those are
/// first-prompt-only, `getFirstMeaningfulUserMessageTextContent`).
#[must_use]
pub(crate) fn truncate_title(text: &str) -> String {
    post_process(text)
}

/// Collect text from message content. String → one element; array → the `text`
/// of every `type: "text"` block, ABORTING the whole message (`None`) on the
/// first `type: "tool_result"` block — the binary's Jxt array branch
/// `for(let s of r){...if(s.type==="tool_result")return;
/// if(s.type==="text"&&typeof s.text==="string")o.push(s.text)}` (a tool_result
/// carrier yields nothing). Non-string/non-array content → `Some(empty)`.
fn collect_texts(content: &Value) -> Option<Vec<String>> {
    if let Some(s) = content.as_str() {
        Some(vec![s.to_string()])
    } else if let Some(arr) = content.as_array() {
        let mut out = Vec::new();
        for block in arr {
            let ty = block.get("type").and_then(Value::as_str);
            // First tool_result block aborts the entire message.
            if ty == Some("tool_result") {
                return None;
            }
            if ty == Some("text") {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    out.push(t.to_string());
                }
            }
        }
        Some(out)
    } else {
        Some(Vec::new())
    }
}

/// `extractTag` (`messages.ts:633-687`): returns the inner content of the first
/// `<tag …>…</tag>` match, or `None` when absent or empty (TS returns content
/// only when truthy).
fn extract_tag(text: &str, re: &Regex) -> Option<String> {
    let inner = re.captures(text)?.get(1)?.as_str();
    if inner.is_empty() {
        None
    } else {
        Some(inner.to_string())
    }
}

/// Read a boolean outer field (e.g. `isMeta`, `isCompactSummary`) from
/// `JsonlMessage::extra`; missing / non-bool ⇒ `false`.
fn extra_flag(m: &JsonlMessage, key: &str) -> bool {
    m.extra.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Truncate `s` to [`TITLE_MAX_CHARS`] `char`s, trimming the slice and appending
/// [`TITLE_ELLIPSIS`] when anything was cut — `slice(0,200).trim() + '…'`
/// (`sessionStorage.ts:1733`).
#[must_use]
fn truncate_with_ellipsis(s: &str) -> String {
    if s.chars().count() <= TITLE_MAX_CHARS {
        return s.to_string();
    }
    // Byte index of the (TITLE_MAX_CHARS)th char → slice on a valid UTF-8 boundary.
    let cutoff_byte = s
        .char_indices()
        .nth(TITLE_MAX_CHARS)
        .map_or_else(|| s.len(), |(idx, _)| idx);
    let mut out = s[..cutoff_byte].trim().to_string();
    out.push(TITLE_ELLIPSIS);
    out
}

#[cfg(test)]
#[allow(clippy::needless_pass_by_value)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a `user` JSONL message with the given inner `content`.
    fn user(content: Value) -> JsonlMessage {
        serde_json::from_value(json!({
            "type": "user",
            "uuid": "11111111-1111-1111-1111-111111111111",
            "parentUuid": null,
            "sessionId": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "timestamp": "2026-05-25T12:00:00.000Z",
            "cwd": "/tmp",
            "version": "0.6.0",
            "isSidechain": false,
            "userType": "external",
            "message": {"role": "user", "content": content}
        }))
        .expect("valid user message")
    }

    /// Build a `user` message with extra outer fields merged in (e.g. isMeta).
    fn user_with(content: Value, extra: Value) -> JsonlMessage {
        let mut base = json!({
            "type": "user",
            "uuid": "11111111-1111-1111-1111-111111111111",
            "parentUuid": null,
            "sessionId": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "timestamp": "2026-05-25T12:00:00.000Z",
            "cwd": "/tmp",
            "version": "0.6.0",
            "isSidechain": false,
            "userType": "external",
            "message": {"role": "user", "content": content}
        });
        let obj = base.as_object_mut().unwrap();
        for (k, v) in extra.as_object().unwrap() {
            obj.insert(k.clone(), v.clone());
        }
        serde_json::from_value(base).expect("valid user message")
    }

    #[test]
    fn short_string_is_returned_verbatim() {
        assert_eq!(extract_title(&[user(json!("hello world"))]), "hello world");
    }

    #[test]
    fn newline_is_replaced_with_space() {
        assert_eq!(
            extract_title(&[user(json!("line1\nline2\nline3"))]),
            "line1 line2 line3"
        );
    }

    #[test]
    fn whitespace_is_trimmed() {
        assert_eq!(extract_title(&[user(json!("   hi   "))]), "hi");
    }

    #[test]
    fn cap_is_200_not_50() {
        // 120 chars must survive verbatim (old 50-char cap would have truncated).
        let s = "a".repeat(120);
        assert_eq!(extract_title(&[user(json!(s.clone()))]), s);
    }

    #[test]
    fn exactly_200_chars_no_ellipsis() {
        let s = "a".repeat(200);
        let title = extract_title(&[user(json!(s.clone()))]);
        assert_eq!(title, s);
        assert!(!title.ends_with('…'));
    }

    #[test]
    fn over_200_chars_is_truncated_with_ellipsis() {
        let s = "a".repeat(250);
        let title = extract_title(&[user(json!(s))]);
        assert_eq!(title.chars().count(), 201); // 200 'a' + '…'
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().take(200).collect::<String>(), "a".repeat(200));
    }

    #[test]
    fn cap_counts_chars_not_bytes() {
        let s = "中".repeat(260); // 3 bytes each
        let title = extract_title(&[user(json!(s))]);
        assert_eq!(title.chars().count(), 201); // 200 '中' + '…'
        assert!(title.ends_with('…'));
    }

    #[test]
    fn empty_messages_fall_back_to_session() {
        let messages: Vec<JsonlMessage> = vec![];
        assert_eq!(extract_title(&messages), "(session)");
    }

    #[test]
    fn no_user_message_falls_back_to_session() {
        let mut m = user(json!("hi"));
        m.message_type = "assistant".to_string();
        assert_eq!(extract_title(&[m]), "(session)");
    }

    #[test]
    fn empty_content_falls_back_to_session() {
        assert_eq!(extract_title(&[user(json!(""))]), "(session)");
    }

    #[test]
    fn image_only_array_falls_back_to_session() {
        let m = user(json!([
            {"type": "image", "source": {"type": "base64", "data": "xxxx"}}
        ]));
        assert_eq!(extract_title(&[m]), "(session)");
    }

    #[test]
    fn array_uses_first_text_block() {
        let m = user(json!([
            {"type": "text", "text": "first"},
            {"type": "text", "text": "second"}
        ]));
        assert_eq!(extract_title(&[m]), "first");
    }

    #[test]
    fn skips_assistant_to_find_user() {
        let mut assistant = user(json!("I am the model"));
        assistant.message_type = "assistant".to_string();
        let real = user(json!("real prompt"));
        assert_eq!(extract_title(&[assistant, real]), "real prompt");
    }

    #[test]
    fn is_meta_user_message_is_skipped() {
        let meta = user_with(json!("meta noise"), json!({"isMeta": true}));
        let real = user(json!("actual question"));
        assert_eq!(extract_title(&[meta, real]), "actual question");
    }

    #[test]
    fn is_compact_summary_is_skipped() {
        let summary = user_with(
            json!("compacted history"),
            json!({"isCompactSummary": true}),
        );
        let real = user(json!("the real prompt"));
        assert_eq!(extract_title(&[summary, real]), "the real prompt");
    }

    #[test]
    fn command_only_session_falls_back_to_bare_name() {
        // v2.1.193: command args are NOT read; a command-only session returns the
        // bare command name as the last-resort fallback (not `<name> <args>`).
        let m = user(json!(
            "<command-name>/review</command-name><command-args>reticulate splines</command-args>"
        ));
        assert_eq!(extract_title(&[m]), "/review");
    }

    #[test]
    fn command_without_args_falls_back_to_command_name() {
        // v2.1.193: a command-only message yields its bare name as the fallback
        // (was `(session)` under the old args-required skip).
        let m = user(json!("<command-name>/clear</command-name>"));
        assert_eq!(extract_title(&[m]), "/clear");
    }

    #[test]
    fn command_without_args_falls_through_to_next_text_block() {
        let m = user(json!([
            {"type": "text", "text": "<command-name>/clear</command-name>"},
            {"type": "text", "text": "follow-up prompt"}
        ]));
        assert_eq!(extract_title(&[m]), "follow-up prompt");
    }

    #[test]
    fn bash_input_gets_bang_prefix() {
        let m = user(json!("<bash-input>ls -la</bash-input>"));
        assert_eq!(extract_title(&[m]), "! ls -la");
    }

    #[test]
    fn skip_pattern_drops_leading_xml_tag() {
        let m = user(json!([
            {"type": "text", "text": "<ide_selection>foo.rs:1-2</ide_selection>"},
            {"type": "text", "text": "what does this do?"}
        ]));
        assert_eq!(extract_title(&[m]), "what does this do?");
    }

    #[test]
    fn skip_pattern_drops_interrupt_marker() {
        let m = user(json!("[Request interrupted by user for tool use]"));
        assert_eq!(extract_title(&[m]), "(session)");
    }

    // ---- truncate_title: side-map titles share extract_title's cap rule -----

    #[test]
    fn truncate_title_short_is_verbatim() {
        assert_eq!(truncate_title("Refactor the parser"), "Refactor the parser");
    }

    #[test]
    fn truncate_title_collapses_newlines_and_trims() {
        // Same `extractFirstPrompt` post-processing extract_title applies.
        assert_eq!(truncate_title("  line1\nline2  "), "line1 line2");
    }

    #[test]
    fn truncate_title_caps_at_200_chars_with_ellipsis() {
        let title = truncate_title(&"a".repeat(250));
        assert_eq!(title.chars().count(), 201); // 200 + '…'
        assert!(title.ends_with('…'));
        // Identical contract to extract_title's first-prompt truncation.
        assert_eq!(title, extract_title(&[user(json!("a".repeat(250)))]));
    }
}

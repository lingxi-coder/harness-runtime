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
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
#[cfg(test)]
use serde_json::Value;

/// Maximum UTF-16 code units stored for the title before truncation + ellipsis.
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

/// Max UTF-16 code units stored for a `/branch` default fork name — the binary's `I2l`
/// `.slice(0,100)` (vs the title path's 200).
pub const FORK_NAME_MAX_CHARS: usize = 100;

/// `/\s+/g` — the fork-name whitespace-run collapse in the binary's `I2l`
/// (`.replace(/\s+/g," ")`).
pub(crate) fn normalize_fork_title(value: &str) -> String {
    String::from_utf16_lossy(&collapse_js_whitespace(
        &value.encode_utf16().collect::<Vec<_>>(),
    ))
}

fn js_space(unit: u16) -> bool {
    matches!(unit, 0x0009..=0x000d | 0x0020 | 0x00a0 | 0x1680 |
        0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff)
}

fn trim_js_units(value: &[u16]) -> &[u16] {
    let start = value
        .iter()
        .position(|unit| !js_space(*unit))
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|unit| !js_space(*unit))
        .map_or(start, |index| index + 1);
    &value[start..end]
}

fn collapse_js_whitespace(value: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(value.len());
    let mut separated = false;
    for &unit in trim_js_units(value) {
        if js_space(unit) {
            separated = true;
        } else {
            if separated && !out.is_empty() {
                out.push(0x20);
            }
            out.push(unit);
            separated = false;
        }
    }
    out
}

fn find_units(haystack: &[u16], needle: &[u16]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn tag_body<'a>(value: &'a [u16], name: &str, single_line: bool) -> Option<&'a [u16]> {
    let open: Vec<_> = format!("<{name}>").encode_utf16().collect();
    let close: Vec<_> = format!("</{name}>").encode_utf16().collect();
    let mut rest = value;
    while let Some(start) = find_units(rest, &open) {
        let body = &rest[start + open.len()..];
        let end = find_units(body, &close)?;
        if !single_line
            || !body[..end]
                .iter()
                .any(|unit| matches!(unit, 0x0a | 0x0d | 0x2028 | 0x2029))
        {
            return Some(&body[..end]);
        }
        rest = &rest[start + open.len()..];
    }
    None
}

fn skip_first_prompt(value: &[u16]) -> bool {
    let marker: Vec<_> = "[Request interrupted by user".encode_utf16().collect();
    if value.starts_with(&marker) && value[marker.len()..].contains(&u16::from(b']')) {
        return true;
    }
    if value.first() != Some(&u16::from(b'<'))
        || !value.get(1).is_some_and(|unit| (97..=122).contains(unit))
    {
        return false;
    }
    let end = value[2..]
        .iter()
        .position(|unit| !matches!(unit, 48..=57 | 65..=90 | 97..=122 | 95 | 45))
        .map_or(value.len(), |index| index + 2);
    value
        .get(end)
        .is_some_and(|unit| js_space(*unit) || *unit == u16::from(b'>'))
}

// Native's pasted-content display reader removes recognized four-hex-digit
// wrappers, including at most two adjoining newlines, before title extraction.
fn unwrap_pasted_content(value: &[u16]) -> Vec<u16> {
    let opener: Vec<_> = "<pasted_content id=\"".encode_utf16().collect();
    let mut out = Vec::new();
    let mut emitted = 0;
    let mut scan = 0;
    while let Some(relative) = find_units(&value[scan..], &opener) {
        let start = scan + relative;
        let id_start = start + opener.len();
        let Some(id) = value.get(id_start..id_start + 4) else {
            break;
        };
        if !id.iter().all(|unit| matches!(unit, 48..=57 | 97..=102))
            || value.get(id_start + 4..id_start + 7) != Some(&[34, 62, 10])
        {
            scan = id_start;
            continue;
        }
        let body_start = id_start + 7;
        let mut closer: Vec<_> = "\n</pasted_content id=\"".encode_utf16().collect();
        closer.extend_from_slice(id);
        closer.extend([34, 62]);
        let Some(body_len) = find_units(&value[body_start - 1..], &closer) else {
            break;
        };
        let body_end = body_start - 1 + body_len;
        let mut before = start;
        for _ in 0..2 {
            if before > emitted && value[before - 1] == 10 {
                before -= 1;
            }
        }
        out.extend_from_slice(&value[emitted..before]);
        if body_end >= body_start {
            out.extend_from_slice(&value[body_start..body_end]);
        }
        emitted = body_end + closer.len();
        for _ in 0..2 {
            if value.get(emitted) == Some(&10) {
                emitted += 1;
            }
        }
        scan = emitted;
    }
    out.extend_from_slice(&value[emitted..]);
    out
}

fn shown_plugin_prompt(message: &JsonlMessage, content: &[u16]) -> Option<Vec<u16>> {
    let text = message.extra.string_units("/pluginPrompt/text")?;
    if text.is_empty() {
        return None;
    }
    let first_line = content.iter().position(|unit| *unit == 10)? + 1;
    let prefix = &content[..first_line];
    let start: Vec<_> = "The ".encode_utf16().collect();
    let end: Vec<_> = " plugin sent a message:\n".encode_utf16().collect();
    if prefix.len() <= 28 || !prefix.starts_with(&start) || !prefix.ends_with(&end) {
        return None;
    }
    let suffix: Vec<_> = "\n\nThis is how Claude Code surfaces a prompt a plugin submits between turns — it starts this turn in the user's place. Address the message above.".encode_utf16().collect();
    if content.len() != prefix.len() + text.len() + suffix.len()
        || content[prefix.len()..prefix.len() + text.len()] != text
        || !content.ends_with(&suffix)
    {
        return None;
    }
    let mut shown: Vec<_> = "Prompt from the ".encode_utf16().collect();
    shown.extend_from_slice(&prefix[4..prefix.len() - 24]);
    shown.extend(" plugin: ".encode_utf16());
    shown.extend(text);
    Some(shown)
}

/// Exact fork-name value. Native slices at 100 UTF-16 units and can leave a
/// dangling surrogate; the persisted title must retain that value even though
/// the Rust display API can expose only its replacement-character rendering.
fn first_meaningful_user_units(messages: &[JsonlMessage], shown: bool) -> Vec<u16> {
    let mut fallback = Vec::new();
    let mut winner = None;
    'messages: for message in messages {
        if message.message_type != "user"
            || extra_flag(message, "isMeta")
            || extra_flag(message, "isCompactSummary")
        {
            continue;
        }
        let Some(content) = message.message.get_property("content") else {
            continue;
        };
        let mut plugin = false;
        let texts = if let Some(units) = content.code_units() {
            let shown = shown.then(|| shown_plugin_prompt(message, units)).flatten();
            plugin = shown.is_some();
            vec![shown.unwrap_or_else(|| units.to_vec())]
        } else if let Some(length) = content.array_len() {
            let mut texts = Vec::new();
            for index in 0..length {
                let Some(block) = content.get_index(index) else {
                    continue;
                };
                if block.get_property("type").and_then(JsonValueView::as_str) == Some("tool_result")
                {
                    continue 'messages;
                }
                if block.get_property("type").and_then(JsonValueView::as_str) == Some("text") {
                    if let Some(units) = block
                        .get_property("text")
                        .and_then(JsonValueView::code_units)
                    {
                        texts.push(units.to_vec());
                    }
                }
            }
            texts
        } else {
            continue;
        };
        for text in texts {
            let normalized: Vec<_> = unwrap_pasted_content(&text)
                .into_iter()
                .map(|unit| if unit == 10 { 32 } else { unit })
                .collect();
            let value = trim_js_units(&normalized);
            if value.is_empty() {
                continue;
            }
            if !plugin {
                if let Some(command) = tag_body(value, "command-name", true) {
                    if fallback.is_empty() {
                        fallback = command.to_vec();
                    }
                    continue;
                }
                if let Some(bash) = tag_body(value, "bash-input", false) {
                    let mut prompt = vec![33, 32];
                    prompt.extend_from_slice(trim_js_units(bash));
                    winner = Some(prompt);
                    break 'messages;
                }
            }
            if skip_first_prompt(value) {
                continue;
            }
            winner = Some(truncate_units_with_ellipsis(value));
            break 'messages;
        }
    }
    winner.unwrap_or(fallback)
}

/// Exact native fork title, including a possible dangling surrogate at the
/// 100-unit slice boundary. Only callers displaying it should decode lossily.
pub(crate) fn derive_fork_name_projection(messages: &[JsonlMessage]) -> Utf16JsonProjection {
    let mut units = collapse_js_whitespace(&first_meaningful_user_units(messages, true));
    units.truncate(FORK_NAME_MAX_CHARS);
    while units.last().is_some_and(|unit| js_space(*unit)) {
        units.pop();
    }
    if units.is_empty() {
        units = FORK_NAME_FALLBACK.encode_utf16().collect();
    }
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("exact fork title is a JSON string")
}

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
        let Some(content) = message.message.at_pointer("/content") else {
            return false;
        };
        collect_texts(content).is_some_and(|texts| {
            texts
                .iter()
                .any(|text| js_trim(&text.replace('\n', " ")).starts_with("<tick>"))
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
    let exact = derive_fork_name_projection(messages);
    String::from_utf16_lossy(&exact.string_units("").expect("fork title is a string"))
}

/// Native's stored first-prompt path shares exact selection and 200-unit
/// truncation with fork names, while plugin display substitution is fork-only.
fn first_meaningful_user_text(messages: &[JsonlMessage]) -> Option<String> {
    let units = first_meaningful_user_units(messages, false);
    (!units.is_empty()).then(|| String::from_utf16_lossy(&units))
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(lingxi_core::host::effort::javascript_whitespace)
}

/// `extractFirstPrompt` post-processing (`sessionStorage.ts:1728-1734`):
/// `result.replace(/\n/g,' ').trim()`, then cap at 200 chars with `'…'`.
fn post_process(text: &str) -> String {
    let collapsed = text.replace('\n', " ");
    truncate_with_ellipsis(js_trim(&collapsed))
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
fn collect_texts(content: JsonValueView<'_>) -> Option<Vec<String>> {
    if let Some(s) = content.as_str() {
        Some(vec![s.to_string()])
    } else if let Some(arr) = content.array_len() {
        let mut out = Vec::new();
        for index in 0..arr {
            let Some(block) = content.get_index(index) else {
                continue;
            };
            let ty = block.get_property("type").and_then(JsonValueView::as_str);
            // First tool_result block aborts the entire message.
            if ty == Some("tool_result") {
                return None;
            }
            if ty == Some("text") {
                if let Some(t) = block.get_property("text").and_then(JsonValueView::as_str) {
                    out.push(t.to_string());
                }
            }
        }
        Some(out)
    } else {
        Some(Vec::new())
    }
}

/// Read a boolean outer field (e.g. `isMeta`, `isCompactSummary`) from
/// `JsonlMessage::extra`; missing / non-bool ⇒ `false`.
fn extra_flag(m: &JsonlMessage, key: &str) -> bool {
    m.extra
        .get_property(key)
        .and_then(JsonValueView::as_bool)
        .unwrap_or(false)
}

/// Truncate `s` to [`TITLE_MAX_CHARS`] UTF-16 units, trimming the slice and appending
/// [`TITLE_ELLIPSIS`] when anything was cut — `slice(0,200).trim() + '…'`
/// (`sessionStorage.ts:1733`).
#[must_use]
fn truncate_with_ellipsis(s: &str) -> String {
    String::from_utf16_lossy(&truncate_units_with_ellipsis(
        &s.encode_utf16().collect::<Vec<_>>(),
    ))
}

fn truncate_units_with_ellipsis(value: &[u16]) -> Vec<u16> {
    if value.len() <= TITLE_MAX_CHARS {
        return value.to_vec();
    }
    let mut units = value[..TITLE_MAX_CHARS].to_vec();
    if units
        .last()
        .is_some_and(|unit| (0xd800..=0xdbff).contains(unit))
    {
        units.pop();
    }
    units = trim_js_units(&units).to_vec();
    units.push(0x2026);
    units
}

#[cfg(test)]
#[allow(clippy::needless_pass_by_value)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_321_fork_names_preserve_exact_utf16() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/session_fork_2_1_321.json"
        ))
        .unwrap();
        for case in fixture["nameCases"].as_array().unwrap() {
            let rows = Utf16JsonProjection::parse(case["messagesJson"].as_str().unwrap()).unwrap();
            let mut lines = Vec::new();
            for index in 0..rows.at_pointer("").unwrap().array_len().unwrap() {
                lines.push(
                    rows.subprojection(&format!("/{index}"))
                        .unwrap()
                        .to_json_string()
                        .unwrap(),
                );
            }
            let messages = crate::jsonl::route_lines(&lines.join("\n")).messages_in_order;
            let actual = derive_fork_name_projection(&messages)
                .string_units("")
                .unwrap();
            let expected: Vec<u16> = case["expectedUnits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|unit| unit.as_u64().unwrap() as u16)
                .collect();
            assert_eq!(actual, expected, "{}", case["name"]);
            let stored: Vec<u16> = case["expectedStoredTitleUnits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|unit| unit.as_u64().unwrap() as u16)
                .collect();
            assert_eq!(
                extract_title(&messages),
                String::from_utf16_lossy(&stored),
                "stored {}",
                case["name"]
            );
        }
        for case in fixture["truncateCases"].as_array().unwrap() {
            let text: String = serde_json::from_str(case["textJson"].as_str().unwrap()).unwrap();
            let expected: Vec<u16> = case["expectedUnits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|unit| unit.as_u64().unwrap() as u16)
                .collect();
            assert_eq!(truncate_title(&text), String::from_utf16_lossy(&expected));
        }
    }

    fn read_message(value: Value) -> JsonlMessage {
        let source = serde_json::to_string(&value).unwrap();
        crate::jsonl::reader::route_lines(&source)
            .messages_in_order
            .into_iter()
            .next()
            .expect("message row")
    }

    /// Build a `user` JSONL message with the given inner `content`.
    fn user(content: Value) -> JsonlMessage {
        read_message(json!({
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
        read_message(base)
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

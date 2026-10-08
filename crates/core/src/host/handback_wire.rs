//! Native .286 `Aue`/`wXe`: agent-message framing with JSON-aware escaping.

use super::instruction_memory_sanitize::{
    compile_native_tag_pattern, js_trim, native_tag_opener_offsets,
};
use fancy_regex::Regex;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::LazyLock;

#[derive(Deserialize)]
struct NativeFixture {
    pattern: String,
}

static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    let fixture: NativeFixture = serde_json::from_str(include_str!(
        "../../tests/fixtures/handback_wire_2_1_286.json"
    ))
    .expect("pinned peer-envelope fixture");
    vec![compile_native_tag_pattern(&fixture.pattern)]
});

/// The Peer origin keeps the original framed body; only its model-facing value
/// gets this wrapper. Sender identity is host metadata rather than report text.
#[must_use]
pub fn render_agent_message(sender: &str, body: &str) -> String {
    let sender = sender
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;");
    format!(
        "<agent-message from=\"{sender}\">\n{}\n</agent-message>",
        neutralize_peer_frames(body)
    )
}

/// Preserve native string slicing even when its preview ends in an unmatched
/// surrogate. Neutralization changes only frame openers and inserts ASCII;
/// every original replacement character keeps its original UTF-16 unit.
#[must_use]
pub fn neutralize_peer_frames_utf16(body: &[u16]) -> Vec<u16> {
    let lossy = String::from_utf16_lossy(body);
    let mut replacement_units =
        char::decode_utf16(body.iter().copied()).filter_map(|decoded| match decoded {
            Ok('\u{fffd}') => Some(0xfffd),
            Err(error) => Some(error.unpaired_surrogate()),
            _ => None,
        });
    let neutralized = neutralize_peer_frames(&lossy);
    let mut exact = Vec::new();
    for character in neutralized.chars() {
        if character == '\u{fffd}' {
            exact.push(
                replacement_units
                    .next()
                    .expect("preserved original UTF-16 unit"),
            );
        } else {
            exact.extend(character.encode_utf16(&mut [0; 2]).iter().copied());
        }
    }
    debug_assert!(replacement_units.next().is_none());
    exact
}

/// Render the native model-facing envelope without replacing unmatched units.
#[must_use]
pub fn render_agent_message_utf16(sender: &str, body: &[u16]) -> Vec<u16> {
    let exact = neutralize_peer_frames_utf16(body);
    // The empty body supplies only the escaped sender and two wrapper lines.
    let wrapper = render_agent_message(sender, "");
    let split = wrapper
        .strip_suffix("\n</agent-message>")
        .expect("agent-message closing line")
        .len();
    let mut result: Vec<u16> = wrapper[..split].encode_utf16().collect();
    result.extend(exact);
    result.extend(wrapper[split..].encode_utf16());
    result
}

fn closing_quote(text: &str, opener: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut cursor = opener + 1;
    while let Some(relative) = text[cursor..].find('"') {
        let quote = cursor + relative;
        let mut before = quote;
        while before > 0 && bytes[before - 1] == b'\\' {
            before -= 1;
        }
        if (quote - before).is_multiple_of(2) {
            return Some(quote);
        }
        cursor = quote + 1;
    }
    None
}

fn json_like(text: &str) -> bool {
    let trimmed = js_trim(text);
    if let Some(rest) = trimmed.strip_prefix('{') {
        return js_trim(rest).starts_with(['"', '}']);
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        let rest = js_trim(rest);
        if rest.starts_with(['"', '{', '[', ']', '-'])
            || rest.as_bytes().first().is_some_and(u8::is_ascii_digit)
        {
            return true;
        }
        return ["true", "false", "null"].iter().any(|word| {
            rest.strip_prefix(word).is_some_and(|suffix| {
                !suffix
                    .as_bytes()
                    .first()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            })
        });
    }
    text.find('"')
        .filter(|index| js_trim(&text[..*index]).is_empty())
        .and_then(|index| closing_quote(text, index))
        .is_some_and(|index| js_trim(&text[index + 1..]).is_empty())
}

struct DecodedPosition {
    start: usize,
    end: usize,
    keeps_opener: bool,
}

/// Native `le` decodes string escapes with their raw start/end positions; it
/// deliberately does not require that the surrounding JSON is well formed.
fn decoded_string(
    text: &str,
    start: usize,
    end: usize,
) -> (String, BTreeMap<usize, DecodedPosition>) {
    let mut decoded = String::new();
    let mut positions = BTreeMap::new();
    let mut cursor = start;
    while cursor < end {
        let original = cursor;
        let mut character = text[cursor..].chars().next().expect("string character");
        cursor += character.len_utf8();
        if character == '\\' && cursor < end {
            let escaped = text.as_bytes()[cursor];
            let simple = match escaped {
                b'"' => Some('"'),
                b'\\' => Some('\\'),
                b'/' => Some('/'),
                b'b' => Some('\u{8}'),
                b'f' => Some('\u{c}'),
                b'n' => Some('\n'),
                b'r' => Some('\r'),
                b't' => Some('\t'),
                _ => None,
            };
            if let Some(simple) = simple {
                character = simple;
                cursor += 1;
            } else if escaped == b'u' && cursor + 5 <= end {
                let hex = &text.as_bytes()[cursor + 1..cursor + 5];
                if hex.iter().all(u8::is_ascii_hexdigit) {
                    let unit =
                        u16::from_str_radix(std::str::from_utf8(hex).expect("ASCII hex"), 16)
                            .expect("Unicode escape");
                    cursor += 5;
                    if (0xD800..=0xDBFF).contains(&unit)
                        && cursor + 6 <= end
                        && &text.as_bytes()[cursor..cursor + 2] == b"\\u"
                        && text.as_bytes()[cursor + 2..cursor + 6]
                            .iter()
                            .all(u8::is_ascii_hexdigit)
                    {
                        let low = u16::from_str_radix(&text[cursor + 2..cursor + 6], 16)
                            .expect("Unicode escape");
                        if (0xDC00..=0xDFFF).contains(&low) {
                            character = char::from_u32(
                                0x10000 + ((u32::from(unit) - 0xD800) << 10) + u32::from(low)
                                    - 0xDC00,
                            )
                            .expect("surrogate pair");
                            cursor += 6;
                        } else {
                            character = '\u{fffd}';
                        }
                    } else {
                        character = char::from_u32(u32::from(unit)).unwrap_or('\u{fffd}');
                    }
                }
            }
        }
        positions.insert(
            decoded.len(),
            DecodedPosition {
                start: original,
                end: cursor,
                keeps_opener: character == '<',
            },
        );
        decoded.push(character);
    }
    (decoded, positions)
}

/// Neutralize only the three native peer-envelope boundaries. Encoded JSON
/// strings receive two backslashes so decoding cannot reintroduce a boundary.
#[must_use]
pub fn neutralize_peer_frames(text: &str) -> String {
    let raw = native_tag_opener_offsets(text, &PATTERNS);
    if !json_like(text) {
        let mut result = String::new();
        let mut copied = 0;
        for start in raw {
            result.push_str(&text[copied..start]);
            result.push_str("<\\");
            copied = start + text[start..].chars().next().expect("opener").len_utf8();
        }
        result.push_str(&text[copied..]);
        return result;
    }
    let mut replacements = BTreeMap::new();
    for start in raw {
        replacements.insert(
            start,
            DecodedPosition {
                start,
                end: start + text[start..].chars().next().expect("opener").len_utf8(),
                keeps_opener: text.as_bytes()[start] == b'<',
            },
        );
    }
    let mut cursor = 0;
    while let Some(relative) = text[cursor..].find('"') {
        let opener = cursor + relative;
        let close = closing_quote(text, opener);
        let end = close.unwrap_or(text.len());
        let (decoded, mut positions) = decoded_string(text, opener + 1, end);
        for offset in native_tag_opener_offsets(&decoded, &PATTERNS) {
            let position = positions.remove(&offset).expect("decoded opener position");
            replacements.insert(position.start, position);
        }
        match close {
            Some(close) => cursor = close + 1,
            None => break,
        }
    }
    let mut result = String::new();
    let mut copied = 0;
    for position in replacements.into_values() {
        result.push_str(
            &text[copied..if position.keeps_opener {
                position.end
            } else {
                position.start
            }],
        );
        result.push_str(if position.keeps_opener {
            "\\\\"
        } else {
            "<\\\\"
        });
        copied = position.end;
    }
    result.push_str(&text[copied..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handback_wire_matches_actual_native_peer_renderer_and_json_neutralizer() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/handback_wire_2_1_286.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let body = case["body"].as_str().unwrap();
            assert_eq!(
                neutralize_peer_frames(body),
                case["neutralized"],
                "{}",
                case["name"]
            );
            assert_eq!(
                render_agent_message(case["sender"].as_str().unwrap(), body),
                case["expected"],
                "{}",
                case["name"]
            );
        }
        for case in fixture["utf16_cases"].as_array().unwrap() {
            let body: Vec<u16> = serde_json::from_value(case["body_units"].clone()).unwrap();
            let expected: Vec<u16> =
                serde_json::from_value(case["expected_units"].clone()).unwrap();
            let neutralized: Vec<u16> =
                serde_json::from_value(case["neutralized_units"].clone()).unwrap();
            assert_eq!(
                neutralize_peer_frames_utf16(&body),
                neutralized,
                "{}",
                case["name"]
            );
            assert_eq!(
                render_agent_message_utf16(case["sender"].as_str().unwrap(), &body),
                expected,
                "{}",
                case["name"]
            );
        }
    }
}

//! Pure native 2.1.296 normalization/filtering, also used by the differential test.
const PROMPT_SUGGESTION_XML_RE: &str = r"^<(suggestion|response|output|answer|result)>([\s\S]*)</(suggestion|response|output|answer|result)>$";
const PROMPT_SUGGESTION_LABEL_RE: &str = r"^\s*(suggested\s+(response|reply|input|prompt)|suggestion|response|reply|answer|output|result|提案|回答|返信|応答|出力|結果|建议|回复|答案|输出|结果|제안|답변|응답|출력|결과)\s*[:：]\s*";
const PROMPT_SUGGESTION_SILENCE_RE: &str = r"\bsilence is\b|\bstay(s|ing)? silent\b";
const PROMPT_SUGGESTION_BARE_SILENCE_RE: &str = r"^[^A-Za-z0-9_]*silence[^A-Za-z0-9_]*$";
const PROMPT_SUGGESTION_WRAPPER_RE: &str = r#"^(\([^\r\n\u{2028}\u{2029}]*\)|\[[^\r\n\u{2028}\u{2029}]*\]|（[^\r\n\u{2028}\u{2029}]*）|［[^\r\n\u{2028}\u{2029}]*］|【[^\r\n\u{2028}\u{2029}]*】|〔[^\r\n\u{2028}\u{2029}]*〕)$"#;
// JavaScript's non-`u` `\w` is ASCII-only; Rust regex defaults to Unicode.
const PROMPT_SUGGESTION_SPEAKER_RE: &str = r"^(?-u:\w)+:\s";
const PROMPT_SUGGESTION_MULTIPLE_SENTENCES_RE: &str = r"[.!?]\s+[A-Z]|[。！？]\s*[\p{L}\p{N}]";
const PROMPT_SUGGESTION_EVALUATIVE_RE: &str = r"thanks|thank you|looks good|sounds good|that works|that worked|that's all|nice|great|perfect|makes sense|awesome|excellent";
const PROMPT_SUGGESTION_CLAUDE_VOICE_RE: &str = r"^(let me|i'll|i've|i'm|i can|i would|i think|i notice|here's|here is|here are|that's|this is|this will|you can|you should|you could|sure,|of course|certainly)";

const PROMPT_SUGGESTION_SINGLE_WORD_ALLOWLIST: &[&str] = &[
    "yes", "yeah", "yep", "yea", "yup", "sure", "ok", "okay", "push", "commit", "deploy", "stop",
    "continue", "check", "exit", "quit", "no",
];

/// Native `W`, without the later rejection pass.
pub(crate) fn normalize_prompt_suggestion_response(raw: &str) -> String {
    static XML: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        native_regex(PROMPT_SUGGESTION_XML_RE).expect("valid xml regex")
    });
    let xml = &*XML;
    static LABELED: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        native_regex(PROMPT_SUGGESTION_LABEL_RE).expect("valid label regex")
    });
    let labeled = &*LABELED;
    let mut suggestion = raw.trim_matches(js_whitespace);
    let ascii_lower = suggestion.to_ascii_lowercase();
    if let Some(caps) = xml.captures(&ascii_lower) {
        let opening = caps.get(1).map_or("", |m| m.as_str());
        let inner = caps.get(2).map_or("", |m| &suggestion[m.range()]);
        let closing = caps.get(3).map_or("", |m| m.as_str());
        let nested_lower = format!("</{}>", opening.to_lowercase());
        let nested_upper = format!("</{}>", opening.to_ascii_uppercase());
        if opening.eq_ignore_ascii_case(closing)
            && !inner.contains(&nested_lower)
            && !inner.contains(&nested_upper)
        {
            suggestion = inner;
        }
    }
    let ascii_lower = suggestion.to_ascii_lowercase();
    if let Some(label) = labeled.find(&ascii_lower) {
        suggestion = &suggestion[label.end()..];
    }
    suggestion.trim_matches(js_whitespace).to_owned()
}

/// Native `q`: retain its first-match order for suppression telemetry.
pub(crate) fn prompt_suggestion_suppression_reason(suggestion: &str) -> Option<&'static str> {
    static SILENCE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        native_regex(PROMPT_SUGGESTION_SILENCE_RE).expect("valid silence regex")
    });
    let silence = &*SILENCE;
    static BARE_SILENCE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        native_regex(PROMPT_SUGGESTION_BARE_SILENCE_RE).expect("valid bare silence regex")
    });
    let bare_silence = &*BARE_SILENCE;
    static WRAPPERS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        native_regex(PROMPT_SUGGESTION_WRAPPER_RE).expect("valid wrapper regex")
    });
    let wrappers = &*WRAPPERS;
    static SPEAKER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        native_regex(PROMPT_SUGGESTION_SPEAKER_RE).expect("valid speaker regex")
    });
    let speaker = &*SPEAKER;
    static MULTIPLE_SENTENCES: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        native_regex(PROMPT_SUGGESTION_MULTIPLE_SENTENCES_RE)
            .expect("valid multiple-sentences regex")
    });
    static EVALUATIVE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        native_regex(PROMPT_SUGGESTION_EVALUATIVE_RE).expect("valid evaluative regex")
    });
    let evaluative = &*EVALUATIVE;
    let multiple_sentences = &*MULTIPLE_SENTENCES;
    static CLAUDE_VOICE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        native_regex(PROMPT_SUGGESTION_CLAUDE_VOICE_RE).expect("valid claude-voice regex")
    });
    let claude_voice = &*CLAUDE_VOICE;

    static CJK_DONE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        native_regex(r"^\P{L}*(完了(しました)?|完成了?|완료됨?)\P{L}*$").unwrap()
    });
    let cjk_done = &*CJK_DONE;
    static CJK_SILENCE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        native_regex(r"^\P{L}*(沈黙|沉默|静默|침묵|提案(なし|はありません)|特に(なし|ありません)|[无没沒]有?建[议議]|(제안|해당)\s*없음)\P{L}*$").unwrap()
    });
    let cjk_silence = &*CJK_SILENCE;
    static CJK_THANKS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        native_regex(r"^\P{L}*(ありがとう(ございます|ございました)?|助かりました|[谢謝][谢謝][你您]?|感谢[你您]?|感謝します|감사합니다|고마워요?)(\P{L}|的|$)").unwrap()
    });
    let cjk_thanks = &*CJK_THANKS;
    static CJK_EVALUATIVE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        native_regex(r"(良さそう|よさそう|いいですね|看起来不错|太好了|完璧|完美|좋네요)(です|ですね|だね)?\P{L}*$").unwrap()
    });
    let cjk_evaluative = &*CJK_EVALUATIVE;
    if suggestion.is_empty() {
        return Some("empty");
    }
    let lowered = suggestion.to_lowercase();
    if lowered == "done" || cjk_done.is_match(suggestion) {
        return Some("done");
    }
    if matches!(lowered.as_str(), "nothing found" | "nothing found.")
        || silence.is_match(&lowered)
        || bare_silence.is_match(&lowered)
        || lowered.starts_with("nothing to suggest")
        || lowered.starts_with("no suggestion")
        || cjk_silence.is_match(suggestion)
    {
        return Some("meta_text");
    }
    if wrappers.is_match(suggestion) {
        return Some("meta_wrapped");
    }
    if lowered.starts_with("api error:")
        || lowered.starts_with("prompt is too long")
        || lowered.starts_with("request timed out")
        || lowered.starts_with("invalid api key")
        || lowered.starts_with("image was too large")
    {
        return Some("error_message");
    }
    if speaker.is_match(suggestion) {
        return Some("prefixed_label");
    }
    let weighted_words = prompt_suggestion_word_weight(suggestion);
    if weighted_words < 2 && !suggestion.starts_with('/') {
        let cjk = prompt_suggestion_cjk_count(suggestion);
        if cjk > 0 {
            if cjk < 2 {
                return Some("too_few_words");
            }
        } else if !PROMPT_SUGGESTION_SINGLE_WORD_ALLOWLIST.contains(&lowered.as_str()) {
            return Some("too_few_words");
        }
    }
    if weighted_words > 12 {
        return Some("too_many_words");
    }
    if suggestion.encode_utf16().count() >= 100 {
        return Some("too_long");
    }
    if multiple_sentences.is_match(suggestion) {
        return Some("multiple_sentences");
    }
    if suggestion.contains('\n') || suggestion.contains('*') {
        return Some("has_formatting");
    }
    if evaluative.is_match(&lowered)
        || cjk_thanks.is_match(suggestion)
        || cjk_evaluative.is_match(suggestion)
    {
        return Some("evaluative");
    }
    if claude_voice.is_match(&suggestion.to_ascii_lowercase())
        || (suggestion.starts_with("让我") && !suggestion.starts_with("让我们"))
        || ["我来", "我会", "我将"]
            .iter()
            .any(|prefix| suggestion.starts_with(prefix))
    {
        return Some("claude_voice");
    }
    None
}

#[cfg(test)]
pub(crate) fn parse_prompt_suggestion_response(raw: &str) -> Option<String> {
    let suggestion = normalize_prompt_suggestion_response(raw);
    prompt_suggestion_suppression_reason(&suggestion)
        .is_none()
        .then_some(suggestion)
}

fn prompt_suggestion_script_counts(text: &str) -> (u32, u32, u32, u32) {
    static HAN: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| native_regex(r"\p{Han}").expect("han script"));
    let han = &*HAN;
    static PHONETIC: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        native_regex(
            r"[\p{Hiragana}\p{Katakana}\u{30FC}\u{FF70}\p{Thai}\p{Lao}\p{Khmer}\p{Myanmar}]",
        )
        .expect("phonetic scripts")
    });
    static HANGUL: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| native_regex(r"\p{Hangul}").expect("hangul script"));
    let hangul = &*HANGUL;
    let phonetic = &*PHONETIC;
    static LETTER_OR_NUMBER: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| native_regex(r"[\p{L}\p{N}]").expect("letter or number"));
    let letter_or_number = &*LETTER_OR_NUMBER;
    let mut han_n = 0;
    let mut phonetic_n = 0;
    let mut hangul_n = 0;
    let mut other_n = 0;
    let mut buf = [0u8; 4];
    for c in text.chars() {
        let s = c.encode_utf8(&mut buf);
        if han.is_match(s) {
            han_n += 1;
        } else if phonetic.is_match(s) {
            phonetic_n += 1;
        } else if hangul.is_match(s) {
            hangul_n += 1;
        } else if letter_or_number.is_match(s) {
            other_n += 1;
        }
    }
    (han_n, phonetic_n, hangul_n, other_n)
}

/// Native 2.1.296 `b`: whitespace tokens, with Han/kana/Thai weighted so CJK
/// suggestions are not dropped as "one English word".
fn prompt_suggestion_word_weight(text: &str) -> usize {
    let trimmed = text.trim_matches(js_whitespace);
    if trimmed.is_empty() {
        return 0;
    }
    let mut weight = 0.0_f64;
    for token in trimmed
        .split(js_whitespace)
        .filter(|token| !token.is_empty())
    {
        let (han, phonetic, hangul, other) = prompt_suggestion_script_counts(token);
        weight += if han == 0 && phonetic == 0 {
            1.0
        } else {
            let mixed = if other + hangul > 0 { 1.0 } else { 0.0 };
            mixed + f64::from(han) / 2.0 + f64::from(phonetic) / 4.0
        };
    }
    weight.ceil() as usize
}

/// Native 2.1.296 `y`: Han + kana/Thai + Hangul count. A CJK token with ≥2 of these
/// letters is kept even when the whitespace word count is 1.
fn prompt_suggestion_cjk_count(text: &str) -> u32 {
    let (han, phonetic, hangul, _) = prompt_suggestion_script_counts(text);
    han + phonetic + hangul
}

// ECMAScript WhiteSpace + LineTerminator; Rust's is_whitespace also accepts NEL,
// and excludes BOM, both of which change the native normalization contract.
fn js_whitespace(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

fn native_regex(pattern: &str) -> Result<regex::Regex, regex::Error> {
    // JS non-Unicode \s and \b differ from Rust's Unicode shorthand classes.
    let pattern = pattern.replace(r"\s", r"[\x09-\x0d \u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]")
        .replace(r"\b", r"(?-u:\b)");
    regex::Regex::new(&pattern)
}

#[cfg(test)]
pub(crate) fn prompt_suggestion_cache_is_cold(input: u64, output: u64, creation: u64) -> bool {
    input.saturating_add(output).saturating_add(creation) > 10_000
}

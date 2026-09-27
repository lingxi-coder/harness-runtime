//! Terminal-escape-sequence allowlist validator for a hook's `terminalSequence`
//! (#40). 1:1 port of claude-code's `wem` tokenizer + `NEo` validator
//! (`bin/claude.exe` offsets 204439967 / 204440687).
//!
//! A `PreToolUse`/etc. hook may return a top-level `terminalSequence` string
//! asking LingXi to emit a terminal escape sequence on its behalf — e.g. an
//! OSC 9 / OSC 777 desktop notification. The apply path (`szn`, BIN off
//! 205755390) validates the sequence through this allowlist and either writes it
//! to the active terminal (`BEo`) or warns + drops it:
//!
//! ```text
//! function szn(e,t){
//!   if(!e||!FL(e)||!e.terminalSequence)return;
//!   let n=NEo(e.terminalSequence);
//!   if(n!==null)BEo(n);
//!   else C(`Hook ${t} returned a terminalSequence that was rejected by the
//!     allowlist (only OSC 0/1/2/9/99/777 and BEL are permitted, and OSC 9
//!     bodies may not begin with a digit unless in the 9;4 progress form)`)
//! }
//! ```
//!
//! The allowlist accepts ONLY:
//! - BEL (`\x07`), and
//! - OSC sequences (`ESC ]` … terminator) whose `Ps` numeric code is one of
//!   `{0, 1, 2, 9, 99, 777}` — with the added constraint (claude-code `z7h`)
//!   that an OSC **9** body may not begin with a bare digit unless it is the
//!   `9;4` progress form (see [`osc9_body_allowed`]).
//!
//! Anything else — a bare `ESC` not starting an OSC, a non-numeric / out-of-set
//! `Ps`, an unterminated OSC, a control char in the input, or a total length
//! over [`MAX_TERMINAL_SEQUENCE_BYTES`] — rejects the WHOLE sequence (`null`).
//!
//! This module ports the *validation* (accept/reject) faithfully and returns the
//! sanitized sequence string on accept. The actual terminal WRITE (`BEo` —
//! `process.stdout.write` to the controlling TTY, terminal-multiplexer-aware
//! re-escaping in `aS`/`Sk`) lives in the TUI terminal writer; this crate has no
//! TTY handle, so it surfaces the validated string for that consumer to emit.

use regex::Regex;
use std::sync::LazyLock;

/// BEL control char (`\x07`, claude-code `KO`).
const BEL: char = '\u{0007}';
/// ESC control char (`\x1B`, claude-code `y5`).
const ESC: char = '\u{001B}';
/// Max UTF-8 byte length of an accepted `terminalSequence` (claude-code
/// `Cem = 4096`). A longer input rejects the whole sequence.
pub const MAX_TERMINAL_SEQUENCE_BYTES: usize = 4096;

/// Allowed OSC `Ps` numeric codes (claude-code `Eem = new Set([0,1,2,9,99,777])`).
const ALLOWED_OSC_PS: [u32; 6] = [0, 1, 2, 9, 99, 777];

/// One token parsed from a `terminalSequence` (claude-code `wem` output entry).
#[derive(Debug, Clone, PartialEq, Eq)]
enum TerminalToken {
    /// A bare BEL.
    Bel,
    /// An OSC sequence with an allowed `Ps` code and a sanitized payload.
    Osc {
        /// The numeric `Ps` (guaranteed in [`ALLOWED_OSC_PS`]).
        ps: u32,
        /// The control-char-sanitized payload (claude-code `vem`).
        payload: String,
    },
}

/// Sanitize an OSC payload by dropping control characters (claude-code `vem`,
/// BIN off 204439967): keep a char only when its code point is `>= 32`, is not
/// DEL (`127`), and is not in the C1 control range `128..=159`.
fn sanitize_payload(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            let r = c as u32;
            r >= 32 && r != 127 && !(128..=159).contains(&r)
        })
        .collect()
}

/// Validate the sanitized body of an OSC **9** sequence (claude-code `z7h`, BIN
/// off 216069691). An OSC-9 body is accepted ONLY when it is the `9;4` progress
/// form (`/^4;[0-4](;(100|\d{1,2})?)?$/`) or does NOT begin with a decimal digit
/// — after optional leading whitespace and an optional `+`/`-` sign
/// (`/^[\s᠎​]*[+-]?\p{Nd}/u`). This blocks a hook from spoofing numeric
/// OSC-9 control payloads (e.g. a fake progress/notification code) while still
/// allowing free-text desktop notifications and the real progress protocol.
///
/// `\d` in the progress pattern is ASCII-only per JS `RegExp` semantics (hence
/// `[0-9]`, not `\p{Nd}`); the leading-digit test uses Unicode `\p{Nd}` per the
/// `u` flag. The leading-whitespace class replicates JS `\s` verbatim (which
/// includes `﻿` but NOT ``, unlike Rust's `\p{White_Space}`) plus the
/// two explicit code points `᠎` and `​`.
fn osc9_body_allowed(body: &str) -> bool {
    // JS `\s` = HT LF VT FF CR SP NBSP U+1680 U+2000..U+200A LS PS U+202F U+205F
    // U+3000 U+FEFF; plus the source's explicit U+180E and U+200B. U+2000..U+200A
    // and U+200B are contiguous, collapsed to U+2000..U+200B.
    static LEADING_DIGIT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            "^[\t\n\u{0B}\u{0C}\r \u{A0}\u{1680}\u{180E}\u{2000}-\u{200B}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]*[+-]?\\p{Nd}",
        )
        .expect("static OSC-9 leading-digit regex is valid")
    });
    static PROGRESS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^4;[0-4](;(100|[0-9]{1,2})?)?$")
            .expect("static OSC-9 progress-form regex is valid")
    });
    if PROGRESS.is_match(body) {
        return true;
    }
    !LEADING_DIGIT.is_match(body)
}

/// Tokenize a `terminalSequence` (claude-code `wem`). Returns `None` (reject the
/// whole string) on ANY malformed / disallowed content; `Some(tokens)` (possibly
/// empty for an empty input is NOT possible — empty input rejects) otherwise.
fn tokenize(input: &str) -> Option<Vec<TerminalToken>> {
    // `if(e.length===0)return null` — empty input rejects.
    if input.is_empty() {
        return None;
    }
    // `if(Buffer.byteLength(e,"utf8")>Cem)return null`.
    if input.len() > MAX_TERMINAL_SEQUENCE_BYTES {
        return None;
    }
    // claude-code indexes by UTF-16 code unit; for the ASCII control chars we
    // care about (ESC/BEL/`]`/`\`/`;`/digits) a char-vec walk is equivalent and
    // avoids surrogate-pair edge cases (any non-ASCII only ever appears inside a
    // payload, which `sanitize_payload` filters).
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut n = 0usize;
    while n < chars.len() {
        let r = chars[n];
        if r == BEL {
            tokens.push(TerminalToken::Bel);
            n += 1;
            continue;
        }
        // Must be the start of an OSC: `ESC ]`. Anything else rejects.
        if r != ESC || chars.get(n + 1) != Some(&']') {
            return None;
        }
        // Scan from after `ESC ]` for the terminator: BEL (1 char) or ST
        // (`ESC \`, 2 chars). A bare ESC that is not part of ST rejects.
        let mut o = n + 2;
        let mut term_at: Option<usize> = None;
        let mut term_len = 0usize;
        while o < chars.len() {
            if chars[o] == BEL {
                term_at = Some(o);
                term_len = 1;
                break;
            }
            if chars[o] == ESC && chars.get(o + 1) == Some(&'\\') {
                term_at = Some(o);
                term_len = 2;
                break;
            }
            if chars[o] == ESC {
                // a stray ESC inside the OSC body that is not ST → reject.
                return None;
            }
            o += 1;
        }
        let Some(s) = term_at else {
            // unterminated OSC → reject.
            return None;
        };
        // Body = chars between `ESC ]` and the terminator.
        let body: String = chars[n + 2..s].iter().collect();
        // Split `Ps;payload` on the FIRST `;` (claude-code `a.indexOf(";")`).
        let (ps_str, payload) = match body.find(';') {
            Some(idx) => (body[..idx].to_string(), body[idx + 1..].to_string()),
            None => (body.clone(), String::new()),
        };
        // `if(!/^\d+$/.test(c))return null` — Ps must be a non-empty digit run.
        if ps_str.is_empty() || !ps_str.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        // `let d=Number(c);if(!Eem.has(d))return null`.
        let ps: u32 = ps_str.parse().ok()?;
        if !ALLOWED_OSC_PS.contains(&ps) {
            return None;
        }
        let payload = sanitize_payload(&payload);
        // `if(d===9&&!z7h(p))return null` — an OSC 9 body that begins with a bare
        // digit (and is not the `9;4` progress form) is rejected so a hook can't
        // spoof numeric OSC-9 control payloads.
        if ps == 9 && !osc9_body_allowed(&payload) {
            return None;
        }
        tokens.push(TerminalToken::Osc { ps, payload });
        n = s + term_len;
    }
    Some(tokens)
}

/// Validate a hook's `terminalSequence` against the OSC/BEL allowlist
/// (claude-code `NEo`, BIN off 204440687). Returns the accepted, normalized
/// sequence string to emit to the terminal, or `None` when the sequence is
/// rejected (the caller then warns and drops it).
///
/// The re-emitted string is BEL for a `Bel` token and `ESC ] Ps ; payload BEL`
/// for an `Osc` token — the canonical BEL-terminated OSC form. claude-code's
/// `aS`/`Sk` additionally re-escape for the active terminal multiplexer
/// (tmux/screen/kitty) at write time; that terminal-specific re-escaping is the
/// TUI writer's concern and is intentionally not modeled here (the accept/reject
/// decision — the security-relevant half — is byte-faithful).
#[must_use]
pub fn validate_terminal_sequence(input: &str) -> Option<String> {
    let tokens = tokenize(input)?;
    let mut out = String::new();
    for t in &tokens {
        match t {
            TerminalToken::Bel => out.push(BEL),
            TerminalToken::Osc { ps, payload } => {
                out.push(ESC);
                out.push(']');
                out.push_str(&ps.to_string());
                if !payload.is_empty() {
                    out.push(';');
                    out.push_str(payload);
                }
                out.push(BEL);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bel_alone_is_accepted() {
        assert_eq!(
            validate_terminal_sequence("\u{0007}"),
            Some("\u{0007}".to_string())
        );
    }

    #[test]
    fn empty_is_rejected() {
        assert_eq!(validate_terminal_sequence(""), None);
    }

    #[test]
    fn osc_9_notification_bel_terminated_is_accepted() {
        // ESC ] 9 ; hello BEL
        let seq = "\u{001B}]9;hello\u{0007}";
        let out = validate_terminal_sequence(seq).unwrap();
        assert_eq!(out, "\u{001B}]9;hello\u{0007}");
    }

    #[test]
    fn osc_9_body_beginning_with_a_bare_digit_is_rejected() {
        // ESC ] 9 ; 5 items done BEL — body "5 items done" begins with a digit
        // and is not the 9;4 progress form → rejected (claude-code `z7h`).
        assert_eq!(
            validate_terminal_sequence("\u{001B}]9;5 items done\u{0007}"),
            None
        );
        // A leading sign / leading whitespace before the digit is also rejected.
        assert_eq!(
            validate_terminal_sequence("\u{001B}]9;-3 left\u{0007}"),
            None
        );
        assert_eq!(validate_terminal_sequence("\u{001B}]9; 42%\u{0007}"), None);
    }

    #[test]
    fn osc_9_4_progress_form_is_accepted() {
        // The `9;4;state;progress` desktop-progress protocol is explicitly allowed
        // even though its body begins with a digit.
        for body in ["4;0", "4;1;50", "4;3;100", "4;2;", "4;4;7"] {
            let seq = format!("\u{001B}]9;{body}\u{0007}");
            assert_eq!(
                validate_terminal_sequence(&seq),
                Some(seq.clone()),
                "9;{body} progress form must be accepted"
            );
        }
    }

    #[test]
    fn osc_9_text_body_not_starting_with_a_digit_is_accepted() {
        // Free-text notifications whose body does not begin with a digit stay
        // valid — the common desktop-notification use case.
        assert_eq!(
            validate_terminal_sequence("\u{001B}]9;Build finished\u{0007}"),
            Some("\u{001B}]9;Build finished\u{0007}".to_string())
        );
    }

    #[test]
    fn osc_9_rule_does_not_affect_other_ps_codes() {
        // The digit-body constraint is OSC-9-only; OSC 0/1/2/99/777 bodies may
        // freely begin with a digit.
        assert_eq!(
            validate_terminal_sequence("\u{001B}]0;3 tabs\u{0007}"),
            Some("\u{001B}]0;3 tabs\u{0007}".to_string())
        );
    }

    #[test]
    fn osc_777_st_terminated_is_accepted_and_renormalized_to_bel() {
        // ESC ] 777 ; notify ; title ; body  ST(ESC \)
        let seq = "\u{001B}]777;notify;body\u{001B}\\";
        let out = validate_terminal_sequence(seq).unwrap();
        // payload after the first ';' is "notify;body"; re-emitted BEL-terminated.
        assert_eq!(out, "\u{001B}]777;notify;body\u{0007}");
    }

    #[test]
    fn osc_with_disallowed_ps_is_rejected() {
        // OSC 8 (hyperlink) is NOT in the allowlist.
        assert_eq!(
            validate_terminal_sequence("\u{001B}]8;;http://x\u{0007}"),
            None
        );
    }

    #[test]
    fn non_numeric_ps_is_rejected() {
        assert_eq!(validate_terminal_sequence("\u{001B}]abc\u{0007}"), None);
    }

    #[test]
    fn bare_esc_not_starting_osc_is_rejected() {
        // ESC [ (CSI) is not an OSC.
        assert_eq!(validate_terminal_sequence("\u{001B}[31m"), None);
    }

    #[test]
    fn unterminated_osc_is_rejected() {
        assert_eq!(validate_terminal_sequence("\u{001B}]9;hello"), None);
    }

    #[test]
    fn plain_text_is_rejected() {
        // A non-control string is neither BEL nor an OSC start → reject.
        assert_eq!(validate_terminal_sequence("hello"), None);
    }

    #[test]
    fn oversize_input_is_rejected() {
        let big = format!(
            "\u{001B}]0;{}\u{0007}",
            "x".repeat(MAX_TERMINAL_SEQUENCE_BYTES)
        );
        assert_eq!(validate_terminal_sequence(&big), None);
    }

    #[test]
    fn payload_control_chars_are_sanitized() {
        // a DEL (0x7f) inside the payload is dropped by `vem`.
        let seq = "\u{001B}]2;ti\u{007F}tle\u{0007}";
        let out = validate_terminal_sequence(seq).unwrap();
        assert_eq!(out, "\u{001B}]2;title\u{0007}");
    }

    #[test]
    fn multiple_tokens_concatenate() {
        let seq = "\u{0007}\u{001B}]1;x\u{0007}";
        let out = validate_terminal_sequence(seq).unwrap();
        assert_eq!(out, "\u{0007}\u{001B}]1;x\u{0007}");
    }
}

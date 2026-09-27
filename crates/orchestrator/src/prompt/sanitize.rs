//! Escaping for content interpolated into `<system-reminder>` bodies.
//!
//! New in Claude Code 2.1.238 (`CC_VER=2.1.220 oracle.sh count
//! '&lt;/system-reminder&gt;'` → 0). Every helper here is a 1:1 port of a
//! function in the oracle's `VXe`/`iFn` modules; offsets are into
//! `~/.local/share/claude/versions/2.1.238`.
//!
//! ```js
//! // @285128585 — VXe
//! function pze(e){return ktp(e.replaceAll("&","&amp;").replaceAll("<","&lt;").replaceAll(">","&gt;"))}
//! function ktp(e){return e.replace(cYb,(t)=>`&#${t.charCodeAt(0)};`)}
//! function uLt(e){return e.replaceAll("<","&lt;").replaceAll(">","&gt;")}
//! function Kae(e){return ktp(uLt(String(e??"")))}
//! function uba(e){return Kae(e).replaceAll('"',"&quot;")}
//! var cYb=/[\x00-\x1f\x7f-\x9f\u2028\u2029]/g, gFn=256;
//!
//! // @285068292 — iFn
//! function Xei(e){return e.replaceAll(/<\s*\/\s*system-reminder\s*>/gi,"&lt;/system-reminder&gt;")}
//! ```
//!
//! Why it matters: a reminder body is interpolated into
//! `` `<system-reminder>\n${body}\n</system-reminder>` `` (`NT` @296673554).
//! Tool output, a task `<result>`, or a filename containing the literal
//! `</system-reminder>` would otherwise CLOSE the envelope early and the rest of
//! the (untrusted) text would read to the model as ordinary conversation.

/// `gFn = 256` @285128933 — the output-style name length above which the
/// per-turn reminder is suppressed entirely.
pub const MAX_OUTPUT_STYLE_NAME_LEN: usize = 256;

/// `ktp(e)` @285128585 — numeric-entity-escape every C0 control, DEL, C1
/// control, and the two Unicode line separators.
///
/// `cYb = /[\x00-\x1f\x7f-\x9f\u2028\u2029]/g`. Note this INCLUDES `\n` (→
/// `&#10;`) and `\t` (→ `&#9;`), so it is only ever applied to values that are
/// meant to be a single inline token (a name, a path), never to a multi-line
/// body.
#[must_use]
fn escape_control_chars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let c = ch as u32;
        if c <= 0x1f || (0x7f..=0x9f).contains(&c) || c == 0x2028 || c == 0x2029 {
            out.push_str(&format!("&#{c};"));
        } else {
            out.push(ch);
        }
    }
    out
}

/// `Ma(e)` @283751324 — the BARE HTML-entity escape (`&`, `<`, `>`), with NO
/// control-character pass:
///
/// ```js
/// function Ma(e){return e.replaceAll("&","&amp;").replaceAll("<","&lt;").replaceAll(">","&gt;")}
/// ```
///
/// Distinct from [`escape_reminder_text`] (`pze`), which is `ktp(Ma(e))`. The
/// difference is observable: `Ma` leaves a newline alone, `pze` turns it into
/// `&#10;`. The goal check-in interstitial uses `Ma` (its goal condition and
/// task lines may legitimately span characters `ktp` would mangle).
///
/// `&` is replaced FIRST, exactly like the JS chain, so an input `&lt;` becomes
/// `&amp;lt;` rather than being double-decoded.
#[must_use]
pub fn escape_reminder_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `pze(e)` @285128585 — full HTML-entity escape (`&`, `<`, `>`) plus
/// [`escape_control_chars`]. Used on values the oracle treats as fully
/// untrusted text: the output-style name and the read-truncation banner.
#[must_use]
pub fn escape_reminder_text(s: &str) -> String {
    escape_control_chars(&escape_reminder_html(s))
}

/// `Kae(e)` @285128585 — the FILENAME/path escape: `<`/`>` only (`uLt`) plus
/// [`escape_control_chars`]. `&` is deliberately NOT escaped — paths routinely
/// contain `&` and the oracle leaves it alone here.
#[must_use]
pub fn escape_reminder_path(s: &str) -> String {
    escape_control_chars(&s.replace('<', "&lt;").replace('>', "&gt;"))
}

/// `Xei(e)` @285068292 — neutralize any literal `</system-reminder>` closing tag
/// inside interpolated content so it cannot terminate the envelope early.
///
/// The oracle's regex is `/<\s*\/\s*system-reminder\s*>/gi`: case-insensitive,
/// with optional JS `\s` runs around the `/` and before the `>`. JS `\s` is
/// the Unicode whitespace set plus `U+FEFF`, which is exactly
/// `char::is_whitespace()` plus `U+FEFF` (Rust excludes the BOM).
pub use platform_api::task_notification::escape_closing_system_reminder;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_escape_replaces_ampersand_first() {
        assert_eq!(
            escape_reminder_text("a & <b> \u{2028}"),
            "a &amp; &lt;b&gt; &#8232;"
        );
        // `&` first ⇒ an already-escaped entity is escaped again, like JS.
        assert_eq!(escape_reminder_text("&lt;"), "&amp;lt;");
    }

    /// `Ma` escapes the three entities and NOTHING else — the point of it
    /// being separate from `pze`.
    #[test]
    fn bare_html_escape_leaves_control_characters_alone() {
        assert_eq!(escape_reminder_html("a & <b>\n"), "a &amp; &lt;b&gt;\n");
        assert_eq!(escape_reminder_text("a & <b>\n"), "a &amp; &lt;b&gt;&#10;");
    }

    #[test]
    fn path_escape_leaves_ampersand_alone() {
        assert_eq!(
            escape_reminder_path("/tmp/a&b/<x>.rs"),
            "/tmp/a&b/&lt;x&gt;.rs"
        );
    }

    #[test]
    fn control_characters_become_numeric_entities() {
        assert_eq!(
            escape_reminder_path("a\nb\tc\u{7f}d\u{9f}e"),
            "a&#10;b&#9;c&#127;d&#159;e"
        );
    }

    #[test]
    fn printable_ascii_and_astral_chars_pass_through() {
        assert_eq!(escape_reminder_path("naïve — 🚀"), "naïve — 🚀");
    }

    #[test]
    fn closing_tag_is_neutralized_case_and_space_insensitively() {
        assert_eq!(
            escape_closing_system_reminder("x</system-reminder>y"),
            "x&lt;/system-reminder&gt;y"
        );
        assert_eq!(
            escape_closing_system_reminder("< / SYSTEM-Reminder\t>"),
            "&lt;/system-reminder&gt;"
        );
        assert_eq!(
            escape_closing_system_reminder("a</system-reminder>b</system-reminder>c"),
            "a&lt;/system-reminder&gt;b&lt;/system-reminder&gt;c"
        );
    }

    #[test]
    fn the_opening_tag_and_near_misses_are_untouched() {
        assert_eq!(
            escape_closing_system_reminder("<system-reminder>"),
            "<system-reminder>"
        );
        assert_eq!(
            escape_closing_system_reminder("</system-reminders>"),
            "</system-reminders>"
        );
        assert_eq!(
            escape_closing_system_reminder("</system-reminder"),
            "</system-reminder"
        );
        assert_eq!(escape_closing_system_reminder("a < b / c"), "a < b / c");
    }

    #[test]
    fn escaping_is_a_no_op_for_ordinary_text() {
        let s = "Task \"build\" completed successfully\n<result>ok</result>";
        assert_eq!(escape_closing_system_reminder(s), s);
    }
}

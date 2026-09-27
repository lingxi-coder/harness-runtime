//! OSC 8 terminal hyperlinks (cc 2.1.198 URL interaction / 2.1.196 clickable
//! file attachments).
//!
//! Claude Code emits OSC 8 hyperlink escapes so terminals (iTerm2, Warp,
//! kitty, ghostty, …) make URLs Cmd/Ctrl-clickable and select the whole URL —
//! including the scheme — on double-click. Binary evidence (2.1.198 bundle):
//!
//! - Generic wrapper: `function Bpl(e,t){return`\x1B]8;;${t}\x07${e}\x1B]8;;\x07`}`
//!   (open with BEL terminator, text, close).
//! - File attachments: `function t2(e){if(!jx())return e;
//!   return`\x1B]8;;${TQi.pathToFileURL(e).href}\x07${e}\x1B]8;;\x07`}` —
//!   a `file://` URL target so Cmd/Ctrl-click reveals the file.
//! - Support gate `jx()`: config override → npm `supports-hyperlinks` on
//!   stdout → `FORCE_HYPERLINK` → `TERM_PROGRAM`/`LC_TERMINAL` in
//!   `["ghostty","Hyper","kitty","alacritty","iTerm.app","iTerm2"]` →
//!   `TERMINAL_EMULATOR==="JetBrains-JediTerm"` → Windows Terminal
//!   (`WT_SESSION`, not inside tmux) → tmux ≥ 3.4 → `TERM` contains "kitty".
//!
//! SEAM: the ratatui backend draws through a cell-grid `Buffer` that cannot
//! carry escape sequences, so these helpers are not yet wired into the
//! `tui-rata` alternate-screen path; they lock the emitted *bytes* for the
//! raw-line print path (native-scrollback printing / final-output writes)
//! when that path lands.

/// OSC 8 open prefix (`ESC ] 8 ; ;`). The binary uses the BEL (`\x07`)
/// terminator form, not `ESC \`.
const OSC8_OPEN: &str = "\x1b]8;;";
/// BEL terminator.
const BEL: char = '\x07';

/// Wrap `text` in an OSC 8 hyperlink pointing at `url`.
/// Byte-faithful to the binary's `Bpl(text, url)`:
/// `\x1b]8;;{url}\x07{text}\x1b]8;;\x07`.
#[must_use]
pub fn hyperlink(text: &str, url: &str) -> String {
    format!("{OSC8_OPEN}{url}{BEL}{text}{OSC8_OPEN}{BEL}")
}

/// Wrap a filesystem path in an OSC 8 hyperlink with a `file://` URL target,
/// displaying the plain path (binary `t2()` — clickable file attachments,
/// cc 2.1.196: Cmd/Ctrl-click reveals the file in Finder/Explorer).
#[must_use]
pub fn file_link(path: &str) -> String {
    hyperlink(path, &path_to_file_url(path))
}

/// Minimal port of Node's `pathToFileURL(p).href` for absolute POSIX paths:
/// `file://` + percent-encoded path (RFC 3986 pchar set, `/` kept).
#[must_use]
pub fn path_to_file_url(path: &str) -> String {
    let mut out = String::from("file://");
    for b in path.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'/'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
            | b':'
            | b'@' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Detect `http(s)://` URLs in a plain text line and wrap each in an OSC 8
/// hyperlink whose target is the full URL *including the scheme* (this is
/// what makes double-click in OSC-8-aware terminals select the whole URL,
/// cc 2.1.198). Non-URL text passes through unchanged.
#[must_use]
pub fn wrap_urls(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    loop {
        let Some(found) = find_url_start(rest) else {
            out.push_str(rest);
            return out;
        };
        let (before, from) = rest.split_at(found);
        out.push_str(before);
        let end = url_end(from);
        let (url, after) = from.split_at(end);
        out.push_str(&hyperlink(url, url));
        rest = after;
    }
}

/// Byte offset of the next `http://` / `https://` in `s`, if any.
fn find_url_start(s: &str) -> Option<usize> {
    let http = s.find("http://");
    let https = s.find("https://");
    match (http, https) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Length of the URL starting at the beginning of `s`: runs until whitespace
/// or a URL-terminating character, then trims trailing punctuation that is
/// prose rather than URL (`.,;:!?"'` and unmatched closers).
fn url_end(s: &str) -> usize {
    let mut end = s.len();
    for (i, c) in s.char_indices() {
        if c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`' | '│' | '\x07' | '\x1b') {
            end = i;
            break;
        }
    }
    // Trim trailing prose punctuation (a URL rarely ends in `.` or `)` when
    // written inline; matched parens like `(…)` inside stay because we only
    // trim from the end).
    let mut trimmed = &s[..end];
    loop {
        let Some(last) = trimmed.chars().last() else {
            break;
        };
        let cut = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' => true,
            ')' => trimmed.matches('(').count() < trimmed.matches(')').count(),
            ']' => trimmed.matches('[').count() < trimmed.matches(']').count(),
            '}' => trimmed.matches('{').count() < trimmed.matches('}').count(),
            _ => false,
        };
        if !cut {
            break;
        }
        trimmed = &trimmed[..trimmed.len() - last.len_utf8()];
    }
    trimmed.len()
}

/// Pure form of the binary's `jx()` hyperlink-support gate (minus the npm
/// `supports-hyperlinks` stdout probe, which the caller passes in as
/// `stdout_supported`). `env` is a lookup closure over the environment.
pub fn supports_hyperlinks(stdout_supported: bool, env: impl Fn(&str) -> Option<String>) -> bool {
    // Terminals the binary allowlists by TERM_PROGRAM / LC_TERMINAL (`J7i`).
    const TERMS: [&str; 6] = [
        "ghostty",
        "Hyper",
        "kitty",
        "alacritty",
        "iTerm.app",
        "iTerm2",
    ];
    if env("FORCE_HYPERLINK").is_some() {
        return stdout_supported;
    }
    if stdout_supported {
        return true;
    }
    let term_program = env("TERM_PROGRAM");
    if let Some(tp) = term_program.as_deref() {
        if TERMS.contains(&tp) {
            return true;
        }
    }
    if env("TERMINAL_EMULATOR").as_deref() == Some("JetBrains-JediTerm") {
        return true;
    }
    if env("WT_SESSION").is_some()
        && term_program.as_deref() != Some("tmux")
        && env("TMUX").is_none()
    {
        return true;
    }
    if term_program.as_deref() == Some("tmux") {
        let version = env("TERM_PROGRAM_VERSION").unwrap_or_default();
        let mut parts = version.split('.');
        let major: u32 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let minor: u32 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        if major > 3 || (major == 3 && minor >= 4) {
            return true;
        }
    }
    if let Some(lc) = env("LC_TERMINAL") {
        if TERMS.contains(&lc.as_str()) {
            return true;
        }
    }
    if env("TERM").is_some_and(|t| t.contains("kitty")) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyperlink_bytes_match_binary_shape() {
        // Binary: `\x1B]8;;${url}\x07${text}\x1B]8;;\x07`.
        assert_eq!(
            hyperlink("docs", "https://example.com/docs"),
            "\x1b]8;;https://example.com/docs\x07docs\x1b]8;;\x07"
        );
    }

    #[test]
    fn file_link_targets_file_url_and_shows_plain_path() {
        assert_eq!(
            file_link("/tmp/report.txt"),
            "\x1b]8;;file:///tmp/report.txt\x07/tmp/report.txt\x1b]8;;\x07"
        );
        // Spaces and non-ASCII are percent-encoded in the URL target only.
        assert_eq!(
            file_link("/tmp/my file.txt"),
            "\x1b]8;;file:///tmp/my%20file.txt\x07/tmp/my file.txt\x1b]8;;\x07"
        );
    }

    #[test]
    fn wrap_urls_wraps_full_url_including_scheme() {
        let line = "see https://example.com/a/b for details";
        assert_eq!(
            wrap_urls(line),
            "see \x1b]8;;https://example.com/a/b\x07https://example.com/a/b\x1b]8;;\x07 for details"
        );
    }

    #[test]
    fn wrap_urls_trims_trailing_prose_punctuation() {
        assert_eq!(
            wrap_urls("read http://a.io/x."),
            "read \x1b]8;;http://a.io/x\x07http://a.io/x\x1b]8;;\x07."
        );
        assert_eq!(
            wrap_urls("(see https://a.io/p)"),
            "(see \x1b]8;;https://a.io/p\x07https://a.io/p\x1b]8;;\x07)"
        );
        // Matched parens INSIDE the URL are kept (wiki-style links).
        assert_eq!(
            wrap_urls("https://en.wikipedia.org/wiki/Rust_(language)"),
            "\x1b]8;;https://en.wikipedia.org/wiki/Rust_(language)\x07https://en.wikipedia.org/wiki/Rust_(language)\x1b]8;;\x07"
        );
    }

    #[test]
    fn wrap_urls_handles_multiple_urls_and_no_urls() {
        assert_eq!(wrap_urls("no links here"), "no links here");
        let two = wrap_urls("a http://x.io b https://y.io c");
        assert_eq!(two.matches("\x1b]8;;http").count(), 2);
        assert!(two.ends_with(" c"));
    }

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn supports_hyperlinks_gate_matches_binary_branches() {
        // stdout probe wins when true.
        assert!(supports_hyperlinks(true, env_of(&[])));
        // FORCE_HYPERLINK returns the stdout probe verbatim.
        assert!(!supports_hyperlinks(
            false,
            env_of(&[("FORCE_HYPERLINK", "1")])
        ));
        // Allowlisted TERM_PROGRAM / LC_TERMINAL (incl. iTerm2 over SSH).
        assert!(supports_hyperlinks(
            false,
            env_of(&[("TERM_PROGRAM", "ghostty")])
        ));
        assert!(supports_hyperlinks(
            false,
            env_of(&[("LC_TERMINAL", "iTerm2")])
        ));
        // JetBrains + Windows Terminal.
        assert!(supports_hyperlinks(
            false,
            env_of(&[("TERMINAL_EMULATOR", "JetBrains-JediTerm")])
        ));
        assert!(supports_hyperlinks(false, env_of(&[("WT_SESSION", "x")])));
        assert!(!supports_hyperlinks(
            false,
            env_of(&[("WT_SESSION", "x"), ("TMUX", "1")])
        ));
        // tmux ≥ 3.4 only.
        assert!(supports_hyperlinks(
            false,
            env_of(&[("TERM_PROGRAM", "tmux"), ("TERM_PROGRAM_VERSION", "3.4")])
        ));
        assert!(!supports_hyperlinks(
            false,
            env_of(&[("TERM_PROGRAM", "tmux"), ("TERM_PROGRAM_VERSION", "3.3")])
        ));
        // TERM containing kitty.
        assert!(supports_hyperlinks(
            false,
            env_of(&[("TERM", "xterm-kitty")])
        ));
        // Nothing set → unsupported.
        assert!(!supports_hyperlinks(false, env_of(&[])));
    }
}
